//! カタログの CLI エージェントから「使うものを選んで足す」ピッカー。
//!
//! `agents::AGENT_CATALOG` は承認モードの判定にしか使われておらず、利用者からは
//! config.toml に最初から書かれている数件しか見えていなかった。ここはその不一致を
//! 埋める層で、カタログ全件を一覧にし、選んだものだけを `cfg.agents` へ足す。
//!
//! 方針:
//! - カタログは「そこから足す元ネタ」であって、利用者のプリセット一覧の置き換えでは
//!   ない。既存の config.toml は 1 行も書き換えず、末尾に `[[agents]]` を足すだけ。
//! - 未インストールのものも隠さない。隠すと「入れれば使える」ことが伝わらないので、
//!   薄く表示して install コマンドを併記する。
//! - カタログの `note` (日本語の落とし穴メモ) をここで初めて利用者に見せる。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};

use eframe::egui;
use egui::RichText;

use crate::agents::{AgentSpec, AGENT_CATALOG};
use crate::config::AgentPreset;
use crate::i18n::{tr, trf};
use crate::theme::Theme;

/// 全自動プリセットの名前につける接尾辞 (config.toml の既定プリセットに合わせる)。
pub const AUTO_SUFFIX: &str = " (全自動)";

/// 全自動プリセットのアイコン (config.toml の既定プリセットに合わせる)。
const AUTO_ICON: &str = "⚡";

// ─── PATH 検出 ──────────────────────────────────────────────────────

/// PATH 上に見つかったカタログ CLI の集合。
///
/// UI スレッドからは毎フレーム引かれるので、参照は必ず非ブロッキングにする
/// (実際のプロセス起動はワーカースレッド側の `probe_blocking` だけが行う)。
#[derive(Default, Clone)]
pub struct Installed {
    found: HashSet<&'static str>,
    /// 一度でも検出が完了したか。未完了と「1 つも無い」を区別するために持つ。
    done: bool,
}

impl Installed {
    pub fn is_installed(&self, bin: &str) -> bool {
        self.found.contains(bin)
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn count(&self) -> usize {
        self.found.len()
    }

    /// 検出結果を直接組み立てる (順序ロジックのテスト用)。
    #[cfg(test)]
    pub fn from_bins(bins: &[&'static str]) -> Self {
        Self {
            found: bins.iter().copied().collect(),
            done: true,
        }
    }
}

/// シェルへそのまま埋め込んで安全な実行ファイル名か。
///
/// カタログは静的なので現状すべて安全だが、将来 `;` を含む bin が足された時に
/// ログインシェルへ素通しするのは避けたいので、組み立て前に必ず通す。
fn is_shell_safe(bin: &str) -> bool {
    !bin.is_empty()
        && bin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// カタログ全件をまとめて `command -v` するシェルスクリプトを組み立てる。
///
/// GUI アプリはログインシェルの PATH を継承しないので `$SHELL -lc` 越しに引く
/// (`github::gh_available` や app.rs の `which` と同じ流儀)。ただし 1 件 1 プロセスだと
/// ログインシェルの起動コストを約 30 回払うことになるため、ループはスクリプト側に
/// 畳んでプロセスは 1 つに抑える。
fn probe_script(bins: &[&str]) -> String {
    let mut s = String::from("for b in");
    for b in bins {
        s.push(' ');
        s.push_str(b);
    }
    s.push_str("; do command -v \"$b\" >/dev/null 2>&1 && echo \"$b\"; done");
    s
}

/// スクリプトの出力 (見つかった bin が 1 行ずつ) をカタログの項目へ戻す。
/// カタログに無い名前は捨てる。
fn parse_probe_output(stdout: &str) -> HashSet<&'static str> {
    stdout
        .lines()
        .filter_map(|line| crate::agents::spec_for_bin(line.trim()).map(|s| s.bin))
        .collect()
}

/// 実際に PATH を引く。**必ずワーカースレッドから呼ぶこと。**
fn probe_blocking() -> Installed {
    let bins: Vec<&'static str> = AGENT_CATALOG
        .iter()
        .map(|s| s.bin)
        .filter(|b| is_shell_safe(b))
        .collect();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let found = std::process::Command::new(shell)
        .arg("-lc")
        .arg(probe_script(&bins))
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| parse_probe_output(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    Installed { found, done: true }
}

/// 検出をバックグラウンドで走らせる (plugins::run_async と同じ流儀)。
fn probe_async(tx: Sender<Installed>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let found = probe_blocking();
        let _ = tx.send(found);
        ctx.request_repaint();
    });
}

// ─── 行の組み立て (egui に依存しない = 単体テストできる) ──────────────

/// ピッカー 1 行分の表示状態。
pub struct Row {
    pub spec: &'static AgentSpec,
    pub installed: bool,
    /// 素のプリセットが既に cfg.agents にあるか。
    pub has_plain: bool,
    /// 全自動プリセットが既に cfg.agents にあるか。
    /// 全自動を作れない CLI (フラグも環境変数も無い) では None。
    pub has_auto: Option<bool>,
}

/// コマンド行を比較用に正規化する (前後の空白と連続空白のゆれを吸収)。
fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// プリセットの同一判定キー。
///
/// コマンドだけでは足りない。goose の全自動は環境変数だけが違って
/// コマンドは素のものと同一なので、env まで入れないと「追加済み」を誤判定する。
fn preset_key(p: &AgentPreset) -> String {
    let mut env: Vec<String> = p.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    env.sort();
    format!("{}\u{1}{}", normalize_command(&p.command), env.join("\u{2}"))
}

/// 素のプリセット (承認の既定は CLI 側にまかせる)。
pub fn plain_preset(spec: &AgentSpec) -> AgentPreset {
    AgentPreset {
        name: spec.label.to_string(),
        command: spec.bin.to_string(),
        icon: spec.icon.to_string(),
        cwd: None,
        env: HashMap::new(),
    }
}

/// 全自動プリセット。作れない CLI (auto_flag も auto_env も無い) では None。
///
/// フラグと環境変数は排他ではない。goose のようにフラグを一切持たない CLI では
/// 環境変数だけが自動承認の経路なので、**存在しないフラグを捏造してはいけない**
/// (起動して即エラーになるプリセットができあがる)。
pub fn auto_preset(spec: &AgentSpec) -> Option<AgentPreset> {
    if spec.auto_flag.is_empty() && spec.auto_env.is_empty() {
        return None;
    }
    let command = if spec.auto_flag.is_empty() {
        spec.bin.to_string()
    } else {
        format!("{} {}", spec.bin, spec.auto_flag)
    };
    Some(AgentPreset {
        name: format!("{}{AUTO_SUFFIX}", spec.label),
        command,
        icon: AUTO_ICON.to_string(),
        cwd: None,
        env: spec
            .auto_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    })
}

/// 検索文字列に引っかかるか (表示名・実行ファイル名の部分一致)。
fn matches_filter(spec: &AgentSpec, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    spec.label.to_lowercase().contains(q) || spec.bin.to_lowercase().contains(q)
}

/// 表示する行を組み立てる。インストール済みが先、同じ群の中はカタログ順のまま。
pub fn rows(installed: &Installed, presets: &[AgentPreset], filter: &str) -> Vec<Row> {
    let q = filter.trim().to_lowercase();
    let existing: HashSet<String> = presets.iter().map(preset_key).collect();
    let mut rows: Vec<Row> = AGENT_CATALOG
        .iter()
        .filter(|s| matches_filter(s, &q))
        .map(|spec| Row {
            spec,
            installed: installed.is_installed(spec.bin),
            has_plain: existing.contains(&preset_key(&plain_preset(spec))),
            has_auto: auto_preset(spec).map(|a| existing.contains(&preset_key(&a))),
        })
        .collect();
    // sort_by_key は安定ソートなので、群の中のカタログ順 (よく使う順) は保たれる。
    rows.sort_by_key(|r| !r.installed);
    rows
}

/// 既存プリセットと表示名がぶつからないようにする。
/// (コマンドが違えば別物として足せるので、名前だけ避ければよい)
pub fn unique_name(base: &str, presets: &[AgentPreset]) -> String {
    if !presets.iter().any(|p| p.name == base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|cand| !presets.iter().any(|p| &p.name == cand))
        .unwrap_or_else(|| base.to_string())
}

// ─── 状態 ───────────────────────────────────────────────────────────

pub struct AgentPicker {
    pub open: bool,
    filter: String,
    installed: Installed,
    /// 検出が飛行中か (二重に走らせない)。
    probing: bool,
    tx: Sender<Installed>,
    rx: Receiver<Installed>,
}

impl Default for AgentPicker {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            open: false,
            filter: String::new(),
            installed: Installed::default(),
            probing: false,
            tx,
            rx,
        }
    }
}

impl AgentPicker {
    /// ワーカースレッドの結果を取り込む。UI スレッドは待たない。
    pub fn poll(&mut self) {
        while let Ok(found) = self.rx.try_recv() {
            self.installed = found;
            self.probing = false;
        }
    }

    /// PATH を引き直す。飛行中なら何もしない。
    pub fn probe(&mut self, ctx: &egui::Context) {
        if self.probing {
            return;
        }
        self.probing = true;
        probe_async(self.tx.clone(), ctx.clone());
    }

    /// ピッカーを開く。まだ検出していなければ、ここで一度だけ走らせる
    /// (起動時に走らせないのは、使わない利用者にプロセスを起こさせないため)。
    pub fn open(&mut self, ctx: &egui::Context) {
        self.open = true;
        if !self.installed.done() && !self.probing {
            self.probe(ctx);
        }
    }

    /// 検出済みかどうかを問わない、安いインストール判定。
    pub fn is_installed(&self, bin: &str) -> bool {
        self.installed.is_installed(bin)
    }
}

// ─── UI ─────────────────────────────────────────────────────────────

/// ピッカーからの操作。借用の都合で、実際の反映は app.rs 側で行う。
pub enum PickerAction {
    /// プリセットを追加する (spec は未インストール時の警告に使う)。
    Add {
        preset: AgentPreset,
        spec: &'static AgentSpec,
    },
    /// PATH を引き直す。
    Reprobe,
}

/// 「エージェントを追加」ウィンドウを描く。
pub fn ui(
    picker: &mut AgentPicker,
    ctx: &egui::Context,
    theme: &Theme,
    presets: &[AgentPreset],
) -> Option<PickerAction> {
    if !picker.open {
        return None;
    }
    let mut action = None;
    let mut open = true;
    let rows = rows(&picker.installed, presets, &picker.filter);
    let (probing, done, found) = (
        picker.probing,
        picker.installed.done(),
        picker.installed.count(),
    );

    egui::Window::new(tr("👾 エージェントを追加"))
        .collapsible(false)
        .resizable(true)
        .default_width(620.0)
        .default_height(520.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("🔍");
                ui.add(
                    egui::TextEdit::singleline(&mut picker.filter)
                        .desired_width(220.0)
                        .hint_text(tr("名前で絞り込み")),
                );
                if ui
                    .button("⟳")
                    .on_hover_text(tr("PATH を引き直す (今インストールしたものを拾う)"))
                    .clicked()
                {
                    action = Some(PickerAction::Reprobe);
                }
                let status = if probing {
                    tr("検出中…")
                } else if done {
                    trf(
                        "{found} / {total} 件がインストール済み",
                        &[
                            ("found", found.to_string()),
                            ("total", AGENT_CATALOG.len().to_string()),
                        ],
                    )
                } else {
                    tr("未検出")
                };
                ui.label(RichText::new(status).size(11.5).color(theme.text_dim));
            });
            ui.label(
                RichText::new(tr(
                    "選ぶと ~/.zaivern/config.toml の末尾に追加されます。既存の設定は書き換えません。",
                ))
                .size(10.5)
                .color(theme.text_dim),
            );
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                for row in &rows {
                    agent_row(ui, theme, row, presets, &mut action);
                    ui.separator();
                }
                if rows.is_empty() {
                    ui.label(
                        RichText::new(tr("該当するエージェントがありません"))
                            .color(theme.text_dim),
                    );
                }
            });
        });

    if !open {
        picker.open = false;
    }
    action
}

/// 1 行ぶんを描く。
fn agent_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    row: &Row,
    presets: &[AgentPreset],
    action: &mut Option<PickerAction>,
) {
    let spec = row.spec;
    // 未インストールは「薄く出す」。隠すと入れれば使えることが伝わらない。
    let title_color = if row.installed {
        theme.text
    } else {
        theme.text_dim
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} {}", spec.icon, spec.label)).color(title_color));
        ui.label(
            RichText::new(format!("`{}`", spec.bin))
                .size(10.5)
                .color(theme.text_dim),
        );
        if row.installed {
            ui.label(RichText::new(tr("✅ インストール済み")).size(10.5).color(theme.ok));
        } else {
            ui.label(RichText::new(tr("⚠ 未インストール")).size(10.5).color(theme.warn));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 全自動から先に置く (右詰めなので、描画順と見た目の順が逆になる)
            if let Some(has_auto) = row.has_auto {
                let auto_btn = if has_auto {
                    ui.add_enabled(false, egui::Button::new(tr("追加済み (全自動)")))
                } else {
                    ui.button(tr("＋ 全自動"))
                };
                if auto_btn.clicked() {
                    if let Some(mut p) = auto_preset(spec) {
                        p.name = unique_name(&p.name, presets);
                        *action = Some(PickerAction::Add { preset: p, spec });
                    }
                }
            }
            let plain_btn = if row.has_plain {
                ui.add_enabled(false, egui::Button::new(tr("追加済み")))
            } else {
                ui.button(tr("＋ 追加"))
            };
            if plain_btn.clicked() {
                let mut p = plain_preset(spec);
                p.name = unique_name(&p.name, presets);
                *action = Some(PickerAction::Add { preset: p, spec });
            }
        });
    });

    // カタログに書かれている落とし穴メモ。ここが利用者から見える唯一の場所。
    if !spec.note.is_empty() {
        ui.label(
            RichText::new(format!("⚠ {}", tr(spec.note)))
                .size(10.5)
                .color(theme.warn),
        );
    }
    // 未インストールなら入れ方を併記する。
    if !row.installed && !spec.install.is_empty() {
        ui.label(
            RichText::new(trf("インストール: {cmd}", &[("cmd", spec.install.to_string())]))
                .size(10.5)
                .color(theme.text_dim),
        );
    }
}

// ─── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn no_presets() -> Vec<AgentPreset> {
        Vec::new()
    }

    #[test]
    fn every_catalog_agent_is_offered() {
        let rows = rows(&Installed::default(), &no_presets(), "");
        assert_eq!(
            rows.len(),
            AGENT_CATALOG.len(),
            "カタログ全件がピッカーに並ばなければならない"
        );
        for spec in AGENT_CATALOG {
            assert!(
                rows.iter().any(|r| r.spec.bin == spec.bin),
                "{} がピッカーに出ていない",
                spec.bin
            );
        }
    }

    #[test]
    fn catalog_has_twenty_nine_agents() {
        // 「29 件すべてを選べる」が要件なので、件数そのものを固定する。
        assert_eq!(AGENT_CATALOG.len(), 29);
    }

    #[test]
    fn installed_agents_sort_first() {
        let installed = Installed::from_bins(&["aider", "qwen"]);
        let rows = rows(&installed, &no_presets(), "");
        let first_two: Vec<&str> = rows.iter().take(2).map(|r| r.spec.bin).collect();
        assert_eq!(first_two, vec!["qwen", "aider"], "インストール済みが先頭に来る");
        assert!(
            rows.iter().skip(2).all(|r| !r.installed),
            "3 件目以降に未インストールが混ざる順序になっている"
        );
    }

    #[test]
    fn installed_group_keeps_catalog_order() {
        // 安定ソートなので、インストール済み同士はカタログ順のまま。
        let installed = Installed::from_bins(&["claude", "codex", "aider"]);
        let rows = rows(&installed, &no_presets(), "");
        let head: Vec<&str> = rows.iter().take(3).map(|r| r.spec.bin).collect();
        assert_eq!(head, vec!["claude", "codex", "aider"]);
    }

    #[test]
    fn already_added_agent_is_not_offered_again() {
        let presets = vec![plain_preset(crate::agents::spec_for_bin("claude").unwrap())];
        let rows = rows(&Installed::default(), &presets, "");
        let claude = rows.iter().find(|r| r.spec.bin == "claude").unwrap();
        assert!(claude.has_plain, "既にある素のプリセットは追加済み扱い");
        assert_eq!(
            claude.has_auto,
            Some(false),
            "全自動はまだ無いので追加できるまま"
        );
        // 他のエージェントは巻き込まれない
        let codex = rows.iter().find(|r| r.spec.bin == "codex").unwrap();
        assert!(!codex.has_plain);
    }

    #[test]
    fn duplicate_detection_survives_whitespace_noise() {
        let mut p = plain_preset(crate::agents::spec_for_bin("codex").unwrap());
        p.command = "  codex   ".into();
        let rows = rows(&Installed::default(), &[p], "");
        let codex = rows.iter().find(|r| r.spec.bin == "codex").unwrap();
        assert!(codex.has_plain, "空白のゆれで重複を見逃してはいけない");
    }

    #[test]
    fn default_config_presets_are_marked_as_added() {
        // 利用者の既定 config.toml にあるものは、最初から「追加済み」に見えるべき。
        let cfg = crate::config::Config::default();
        let rows = rows(&Installed::default(), &cfg.agents, "");
        for bin in ["claude", "codex", "agy"] {
            let r = rows.iter().find(|r| r.spec.bin == bin).unwrap();
            assert!(r.has_plain, "{bin} の素のプリセットが追加済みにならない");
            assert_eq!(r.has_auto, Some(true), "{bin} の全自動が追加済みにならない");
        }
    }

    #[test]
    fn auto_variant_uses_env_route_when_there_is_no_flag() {
        // goose は一括自動承認フラグを持たない。存在しないフラグを付けたプリセットを
        // 作ると起動即エラーになるので、環境変数の経路だけを使うこと。
        let goose = crate::agents::spec_for_bin("goose").unwrap();
        assert!(goose.auto_flag.is_empty(), "前提: goose にフラグは無い");
        let auto = auto_preset(goose).expect("goose は環境変数経由で全自動にできる");
        assert_eq!(auto.command, "goose", "コマンドにフラグを捏造してはいけない");
        assert_eq!(auto.env.get("GOOSE_MODE").map(String::as_str), Some("auto"));
    }

    #[test]
    fn auto_variant_carries_env_for_aider() {
        // aider はフラグと環境変数の両方を持つ。env を落とすと、フラグを
        // 受け付けない経路 (設定ファイル起動など) で自動承認が効かなくなる。
        let aider = crate::agents::spec_for_bin("aider").unwrap();
        let auto = auto_preset(aider).expect("aider は全自動にできる");
        assert_eq!(
            auto.env.get("AIDER_YES_ALWAYS").map(String::as_str),
            Some("1"),
            "auto_env を落としてはいけない"
        );
        assert!(auto.command.contains("--yes-always"));
    }

    #[test]
    fn auto_variant_is_absent_when_the_cli_has_no_auto_route() {
        // auggie / crush はフラグも環境変数も持たない。嘘の全自動を出さないこと。
        for bin in ["auggie", "crush"] {
            let spec = crate::agents::spec_for_bin(bin).unwrap();
            assert!(
                auto_preset(spec).is_none(),
                "{bin} に全自動プリセットを作ってはいけない"
            );
        }
        let rows = rows(&Installed::default(), &no_presets(), "");
        let crush = rows.iter().find(|r| r.spec.bin == "crush").unwrap();
        assert_eq!(crush.has_auto, None);
    }

    #[test]
    fn every_auto_preset_is_either_flag_or_env_backed() {
        for spec in AGENT_CATALOG {
            let Some(auto) = auto_preset(spec) else {
                continue;
            };
            let flagged = auto.command.trim() != spec.bin;
            assert!(
                flagged || !auto.env.is_empty(),
                "{}: 全自動なのにフラグも環境変数も無い",
                spec.bin
            );
            if !spec.auto_flag.is_empty() {
                assert!(
                    auto.command.contains(spec.auto_flag),
                    "{}: auto_flag が入っていない",
                    spec.bin
                );
            }
        }
    }

    #[test]
    fn plain_preset_command_is_just_the_binary() {
        for spec in AGENT_CATALOG {
            let p = plain_preset(spec);
            assert_eq!(p.command, spec.bin);
            assert!(!p.icon.is_empty());
            assert!(!p.name.is_empty());
        }
    }

    #[test]
    fn added_preset_is_recognised_by_the_approval_layer() {
        // 足したプリセットが承認モードの対象として認識されないと、
        // 「追加したのに全自動が効かない」壊れた項目になる。
        for spec in AGENT_CATALOG {
            let p = plain_preset(spec);
            assert!(
                crate::agents::spec_for_command(&p.command).is_some(),
                "{}: 追加したプリセットを承認レイヤが認識できない",
                spec.bin
            );
            if let Some(a) = auto_preset(spec) {
                assert!(crate::agents::spec_for_command(&a.command).is_some());
            }
        }
    }

    #[test]
    fn filter_matches_label_and_bin() {
        let installed = Installed::default();
        let by_label = rows(&installed, &no_presets(), "rovo");
        assert_eq!(by_label.len(), 1);
        assert_eq!(by_label[0].spec.bin, "acli");

        let by_bin = rows(&installed, &no_presets(), "cursor-agent");
        assert_eq!(by_bin.len(), 1);
        assert_eq!(by_bin[0].spec.label, "Cursor");

        // 大文字小文字は問わない
        assert_eq!(rows(&installed, &no_presets(), "GOOSE").len(), 1);
        assert!(rows(&installed, &no_presets(), "ぜったいにない").is_empty());
    }

    #[test]
    fn unique_name_avoids_collisions() {
        let mut presets = vec![AgentPreset {
            name: "Grok".into(),
            ..Default::default()
        }];
        assert_eq!(unique_name("Grok", &presets), "Grok (2)");
        assert_eq!(unique_name("Kimi", &presets), "Kimi");
        presets.push(AgentPreset {
            name: "Grok (2)".into(),
            ..Default::default()
        });
        assert_eq!(unique_name("Grok", &presets), "Grok (3)");
    }

    // ---- 検出 ----

    #[test]
    fn probe_targets_the_binary_not_the_subcommand() {
        // `codex exec` / `acli rovodev run` / `goose run -t` のようなサブコマンド型でも、
        // PATH を引くのは先頭の実行ファイル名でなければならない。
        let script = probe_script(&["codex", "acli", "goose"]);
        for bin in ["codex", "acli", "goose"] {
            assert!(script.contains(&format!(" {bin}")), "{bin} を探していない");
        }
        for sub in ["exec", "rovodev", "run", "-t"] {
            assert!(
                !script.contains(&format!(" {sub};")) && !script.contains(&format!(" {sub} ")),
                "サブコマンド {sub} を実行ファイル名として探している"
            );
        }
        // headless にサブコマンドが書かれている CLI でも、bin は 1 トークン。
        for spec in AGENT_CATALOG {
            assert!(
                !spec.bin.contains(char::is_whitespace),
                "{}: bin にサブコマンドが混ざっている",
                spec.bin
            );
        }
    }

    #[test]
    fn probe_script_covers_every_catalog_bin() {
        let bins: Vec<&str> = AGENT_CATALOG.iter().map(|s| s.bin).collect();
        let script = probe_script(&bins);
        for b in &bins {
            assert!(script.contains(&format!(" {b}")), "{b} が検出対象から漏れた");
        }
        assert!(script.starts_with("for b in "));
        assert!(script.contains("command -v"));
    }

    #[test]
    fn catalog_bins_are_shell_safe() {
        for spec in AGENT_CATALOG {
            assert!(
                is_shell_safe(spec.bin),
                "{}: シェルへ素通しできない文字を含む",
                spec.bin
            );
        }
        assert!(!is_shell_safe("foo; rm -rf /"));
        assert!(!is_shell_safe(""));
        assert!(is_shell_safe("cursor-agent"));
    }

    #[test]
    fn parse_probe_output_keeps_only_catalog_bins() {
        let found = parse_probe_output("claude\ncodex\n\n  cursor-agent  \nnot-an-agent\n");
        assert_eq!(found.len(), 3);
        for b in ["claude", "codex", "cursor-agent"] {
            assert!(found.contains(b));
        }
        assert!(!found.contains("not-an-agent"));
        assert!(parse_probe_output("").is_empty());
    }

    #[test]
    fn installed_distinguishes_unprobed_from_empty() {
        let fresh = Installed::default();
        assert!(!fresh.done(), "未検出と「1 つも無い」は区別する");
        assert_eq!(fresh.count(), 0);
        let empty = Installed::from_bins(&[]);
        assert!(empty.done());
        assert_eq!(empty.count(), 0);
    }

    /// このマシンの PATH を実際に引いて、見つかったものを出す。
    ///
    /// ログインシェルを起こすので通常のテスト走行からは外す (`--ignored` で実行)。
    /// 検出まわりを直した時に、本物の PATH で確かめるための口。
    #[test]
    #[ignore = "ログインシェルを起動するので手動実行 (cargo test -- --ignored)"]
    fn probe_reports_what_is_actually_installed() {
        let found = probe_blocking();
        assert!(found.done());
        let mut lines: Vec<String> = AGENT_CATALOG
            .iter()
            .map(|s| {
                let mark = if found.is_installed(s.bin) { "OK  " } else { "--  " };
                format!("{mark}{:<14} {}", s.bin, s.label)
            })
            .collect();
        lines.sort();
        println!(
            "検出結果 {} / {} 件\n{}",
            found.count(),
            AGENT_CATALOG.len(),
            lines.join("\n")
        );
    }

    #[test]
    fn picker_does_not_probe_twice_at_once() {
        let picker = AgentPicker::default();
        assert!(!picker.open);
        assert!(!picker.is_installed("claude"));
    }
}
