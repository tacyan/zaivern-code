use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, RichText};

use crate::acp;
use crate::agent_input::ComposerTarget;
use crate::agent_picker::{self, AgentPicker};
use crate::agents::{self, AgentManager, SessionEvent};
use crate::breadcrumb;
use crate::checkpoint;
use crate::cli;
use crate::commander;
use crate::config::{self, Config};
use crate::conflict;
use crate::coordinator;
use crate::diagview;
use crate::editor;
use crate::editor::{combine_hash, disk_mtime, hash_str, Buffer, Editor, ExternalEvent};
use crate::editor_ops;
use crate::editor_split;
use crate::failover;
use crate::file_search;
use crate::file_tree::{
    self, Clash, DeleteRequest, FileHistory, FileOp, FileTree, TransferItem, TreeActions,
};
use crate::find_buffer;
use crate::firewall;
use crate::follow;
use crate::fswatch;
use crate::fuzzy;
use crate::git;
use crate::git_panel;
use crate::github;
use crate::highlight::Highlighter;
use crate::html;
use crate::i18n::{self, tr, trf};
use crate::ide;
use crate::kanban;
use crate::keybinds::{parse_shortcut, BindAction, Keybinds};
use crate::license;
use crate::local_history;
use crate::lsp;
use crate::markdown;
use crate::marks;
use crate::mcp;
use crate::mention;
use crate::menu_bar;
use crate::notify;
use crate::orchestration;
use crate::palette::{Action, Cmd, Item, Palette, Results};
use crate::panels::{self, space};
use crate::pathx;
use crate::pet;
use crate::pet_bubble;
use crate::plugins;
use crate::race;
use crate::recent;
use crate::remote;
use crate::session;
use crate::session_picker;
use crate::shellenv;
use crate::skills;
use crate::snippets::{self, Snippet};
use crate::sound::{self, SoundKind};
use crate::spec;
use crate::submit;
use crate::supervisor;
use crate::tailscale;
use crate::tasks;
use crate::terminal;
use crate::theme::{self, Theme};
use crate::theme_json;
use crate::tunnel;
use crate::tutorial::{self, AnchorId};
use crate::voice;
use crate::worktree;
use crate::zoom;

// エージェントデッキ (縦 1 本のエージェント管理画面) は main.rs が
// `mod deck;` で登録している。ここで `#[path]` 付きの二重登録をすると
// 型が 2 組できてしまい、片方に入れた状態がもう片方から見えなくなる。
use crate::deck;

// ---------------------------------------------------------------------------
// 問題パネル (LSP 診断) — 絞り込みとグループ化は純関数に切り出す
// ---------------------------------------------------------------------------

/// ソースを読む回帰テスト用の「app モジュールの実装部だけ」。
///
/// 分割前の `app.rs` に `strip_test_mods` を掛けたものに相当する
/// (`#[cfg(test)] mod ...` は別ファイルへ出たので、繋がなければ落ちる)。
#[cfg(test)]
pub(crate) const SRC_IMPL: &str = concat!(
    include_str!("mod.rs"),
    include_str!("startup.rs"),
    include_str!("edit_core.rs"),
    include_str!("open_prefs.rs"),
    include_str!("quota_cost.rs"),
    include_str!("agent_sessions.rs"),
    include_str!("save_files.rs"),
    include_str!("find_nav.rs"),
    include_str!("lsp_glue.rs"),
    include_str!("cmd_dispatch.rs"),
    include_str!("shortcuts.rs"),
    include_str!("top_bar_ui.rs"),
    include_str!("sidebar_ui.rs"),
    include_str!("bottom_panels.rs"),
    include_str!("cockpit.rs"),
    include_str!("kanban_deck_git.rs"),
    include_str!("editor_layout.rs"),
    include_str!("file_viewers.rs"),
    include_str!("code_editor.rs"),
    include_str!("cmd_palette.rs"),
    include_str!("file_ops.rs"),
    include_str!("whichkey_voice.rs"),
    include_str!("remote_api.rs"),
    include_str!("orchestrate.rs"),
    include_str!("frame_update.rs"),
    include_str!("helpers.rs"),
    include_str!("workbench.rs"),
    include_str!("dialog_windows.rs"),
);

/// ソースを読む回帰テスト用の「app モジュール全体」(テストコードも含む)。
///
/// 分割前は 1 ファイルを読むだけで足りていた。分割後は
/// **どれか 1 ファイルだけを読むと見落とす**ので、ここで全ファイルを繋ぐ。
/// **並びは分割前の app.rs と同じ順**にすること — 「実装 → テスト」の
/// 順に依存する検査 (`split(sig).nth(1)`) が、テスト側の文字列を先に
/// 拾って誤判定するため。新しい子モジュールを足したらこの一覧にも足すこと
/// (`app::tests::分割した子モジュールを全部srcに繋いでいる` が番人)。
#[cfg(test)]
pub(crate) const SRC: &str = concat!(
    include_str!("mod.rs"),
    include_str!("startup.rs"),
    include_str!("edit_core.rs"),
    include_str!("open_prefs.rs"),
    include_str!("quota_cost.rs"),
    include_str!("agent_sessions.rs"),
    include_str!("save_files.rs"),
    include_str!("find_nav.rs"),
    include_str!("lsp_glue.rs"),
    include_str!("cmd_dispatch.rs"),
    include_str!("shortcuts.rs"),
    include_str!("top_bar_ui.rs"),
    include_str!("sidebar_ui.rs"),
    include_str!("bottom_panels.rs"),
    include_str!("cockpit.rs"),
    include_str!("kanban_deck_git.rs"),
    include_str!("editor_layout.rs"),
    include_str!("file_viewers.rs"),
    include_str!("code_editor.rs"),
    include_str!("cmd_palette.rs"),
    include_str!("file_ops.rs"),
    include_str!("whichkey_voice.rs"),
    include_str!("remote_api.rs"),
    include_str!("orchestrate.rs"),
    include_str!("frame_update.rs"),
    include_str!("unread_cursor_tests.rs"),
    include_str!("cost_limit_wiring_tests.rs"),
    include_str!("follow_wiring_tests.rs"),
    include_str!("helpers.rs"),
    include_str!("workbench.rs"),
    include_str!("dialog_windows.rs"),
    include_str!("quick_launch_tests.rs"),
    include_str!("idle_repaint_tests.rs"),
    include_str!("idle_tag_tests.rs"),
    include_str!("tests.rs"),
    include_str!("wiring_tests.rs"),
    include_str!("super_agent_tests.rs"),
    include_str!("wave2_tests.rs"),
    include_str!("glyph_tests.rs"),
    include_str!("ui_wiring_tests.rs"),
    include_str!("tutorial_wiring_tests.rs"),
    include_str!("cockpit_layout_tests.rs"),
    include_str!("deck_wiring_tests.rs"),
    include_str!("approval_panel_tests.rs"),
    include_str!("composer_wiring_tests.rs"),
    include_str!("multi_cursor_wiring_tests.rs"),
    include_str!("encoding_wiring_tests.rs"),
    include_str!("quarantine_hole_tests.rs"),
    include_str!("split_wiring_tests.rs"),
    include_str!("crisp_text_tests.rs"),
    include_str!("quick_open_tests.rs"),
    include_str!("problems_tests.rs"),
);

/// `SRC` から切り出した「メソッド本文」の終わり (次のメソッドの手前)。
///
/// 分割前は次のメソッドが必ず `\n    fn ` で始まっていた。子モジュールへ
/// 移したメソッドには `pub(super)` が付くので、その形も目印にする —
/// 見落とすと本文がファイル末尾まで伸びて、検査が**静かに**緩む。
#[cfg(test)]
pub(crate) fn method_end(body: &str) -> usize {
    ["\n    fn ", "\n    pub(super) fn "]
        .iter()
        .filter_map(|m| body.find(m))
        .min()
        .unwrap_or(body.len())
}

/// severity 1..4 のアイコン。添字 0..3 が severity 1..4。
/// エージェント別の会話履歴を 1 エージェント × 1 フォルダあたり何件残すか。
///
/// 起動のたびに 1 行積むので、放っておくと単調増加する。一覧に出るのは
/// `session_picker::MAX_RESULTS` 件までなので、それより十分多く取って
/// 「一覧の下の方が既に消えている」を起こさない。
const HISTORY_KEEP: usize = 200;

const PROBLEM_SEV_ICONS: [&str; 4] = ["❌", "⚠", "ℹ", "💬"];
/// severity 1..4 の名前 (ホバーで出す。`tr` に通して使う)。
const PROBLEM_SEV_NAMES: [&str; 4] = ["エラー", "警告", "情報", "ヒント"];

/// 問題パネルの 1 件。
#[derive(Clone, Debug, PartialEq)]
pub struct ProblemItem {
    pub path: PathBuf,
    /// 表示名 (ファイル名だけ)。
    pub title: String,
    /// 0 起点。
    pub line: usize,
    /// 0 起点 (LSP の UTF-16 単位)。
    pub col: usize,
    /// 1=エラー 2=警告 3=情報 4=ヒント。
    pub severity: u8,
    pub message: String,
    /// そのファイルの LSP がクイックフィックスに対応しているか。
    pub can_fix: bool,
    /// エディタで開いているファイルか (開いていないものは薄く出す)。
    pub open: bool,
}

/// 問題パネルの絞り込み条件。
#[derive(Clone, Debug, PartialEq)]
pub struct ProblemsFilter {
    /// 添字 0..3 が severity 1..4。
    pub sev: [bool; 4],
    pub text: String,
}

impl Default for ProblemsFilter {
    fn default() -> Self {
        Self {
            sev: [true; 4],
            text: String::new(),
        }
    }
}

/// 問題パネルの表示行。ファイル見出しと診断本文を **1 本の列にならす**。
///
/// `ScrollArea::show_rows` に渡して「見えている分だけ描く」ため、
/// 1000 件でもフレーム時間が伸びない (`CollapsingHeader` を可変長リストに
/// 並べると ID の取り回しも面倒になる — この形なら `Button` だけで済む)。
#[derive(Clone, Debug, PartialEq)]
pub enum ProblemRow {
    Header {
        path: PathBuf,
        title: String,
        count: usize,
        /// そのファイルで最も重い severity。
        worst: u8,
    },
    Item(ProblemItem),
}

/// LSP の SymbolNode を `(名前, 開始行, 終端行, kind)` へ平らにする。
/// 行はどちらも 0 起点・**両端を含む** (`mention::Target::Symbol` と同じ約束)。
fn collect_symbol_ranges(
    nodes: &[lsp::SymbolNode],
    out: &mut Vec<(String, usize, usize, usize, u8)>,
) {
    for n in nodes {
        out.push((
            n.name.clone(),
            n.range.start.line,
            n.range.end.line.max(n.range.start.line),
            n.range.end.character,
            n.kind,
        ));
        collect_symbol_ranges(&n.children, out);
    }
}

/// severity (1..4) の日本語名。表 [`PROBLEM_SEV_NAMES`] が唯一の出所。
fn severity_word(sev: u8) -> &'static str {
    PROBLEM_SEV_NAMES[(sev.clamp(1, 4) - 1) as usize]
}

/// severity ごとの件数 (トグルのバッジ用)。添字 0..3 = severity 1..4。
/// ファイル衝突の見張りへ渡す相手を選ぶ。**素のシェルは除く。**
///
/// 見張りは「同じフォルダに 2 体以上」で走り出すので、Shell を 1 体と
/// 数えると **開いただけで未コミットの全ファイルが取り合いとして出る**
/// (実際にそう報告された: Shell 起動だけで 17 件)。人が 1 人で叩いている
/// シェルは「後から衝突を発見させる」相手ではない。
///
/// `git status` は**どのエージェントが触ったかを区別できない**ので、
/// 同居している全員に同じファイル集合が割り当たる。だからこそ
/// 「誰を同居者と数えるか」がそのまま画面に出る数字になる。
pub fn conflict_watch_rows<'a>(
    sessions: impl Iterator<Item = (u64, &'a str, PathBuf, bool)>,
) -> Vec<(u64, PathBuf, bool)> {
    sessions
        .filter(|(_, cmd, _, _)| !agents::is_plain_shell(cmd))
        .map(|(id, _, cwd, running)| (id, cwd, running))
        .collect()
}

pub fn problem_counts(items: &[ProblemItem]) -> [usize; 4] {
    let mut out = [0usize; 4];
    for it in items {
        out[(it.severity.clamp(1, 4) - 1) as usize] += 1;
    }
    out
}

/// severity トグルとテキストで絞り込む (純関数)。
///
/// テキストは **ファイル名・パス・メッセージのどれかに当たれば通す**。
/// あいまい検索は既存の [`fuzzy::PreparedQuery`] を再利用する
/// (新しいマッチャは書かない)。
pub fn filter_problems(items: &[ProblemItem], f: &ProblemsFilter) -> Vec<ProblemItem> {
    let q = f.text.trim();
    let pq = (!q.is_empty()).then(|| fuzzy::PreparedQuery::new(q));
    items
        .iter()
        .filter(|it| {
            if !f.sev[(it.severity.clamp(1, 4) - 1) as usize] {
                return false;
            }
            match &pq {
                None => true,
                Some(pq) => {
                    pq.score(&it.title).is_some()
                        || pq.score(&it.message).is_some()
                        || pq.score(&it.path.to_string_lossy()).is_some()
                }
            }
        })
        .cloned()
        .collect()
}

/// ファイルごとにまとめ、畳まれているものは中身を出さない (純関数)。
///
/// 並びは「そのファイルの最悪 severity → パス」、ファイル内は (行, 桁)。
pub fn group_problems(
    mut items: Vec<ProblemItem>,
    collapsed: &HashSet<PathBuf>,
) -> Vec<ProblemRow> {
    items.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
            .then(a.severity.cmp(&b.severity))
    });
    let mut groups: Vec<(PathBuf, String, u8, Vec<ProblemItem>)> = Vec::new();
    for it in items {
        match groups.last_mut() {
            Some(g) if g.0 == it.path => {
                g.2 = g.2.min(it.severity);
                g.3.push(it);
            }
            _ => groups.push((it.path.clone(), it.title.clone(), it.severity, vec![it])),
        }
    }
    groups.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)));
    let mut out = Vec::new();
    for (path, title, worst, list) in groups {
        out.push(ProblemRow::Header {
            path: path.clone(),
            title,
            count: list.len(),
            worst,
        });
        if !collapsed.contains(&path) {
            out.extend(list.into_iter().map(ProblemRow::Item));
        }
    }
    out
}

/// 問題が 0 件のときの空状態。
///
/// パネルの中身を空にせず、**可用領域の中央に 1 枚のカード**で示す
/// (矩形は `panels::empty_card` が決めるので、どの窓サイズでもはみ出さない)。
fn problems_empty_card(ui: &mut egui::Ui, theme: &Theme, msg: &str, sub: &str) {
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let card = panels::empty_card(avail, 0).card;
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::none()
            .fill(theme.panel_alt)
            .stroke(egui::Stroke::new(1.0_f32, theme.border))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width((card.width() - space::MD * 2.0).max(1.0));
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("✔").size(40.0).color(theme.text_dim));
                    ui.label(RichText::new(msg).size(15.0).color(theme.text));
                    ui.label(RichText::new(sub).size(12.0).color(theme.text_dim));
                });
            });
    });
}

/// 任意 2 テキストの比較結果 (「保存済みと比較」/「2 つのファイルを比較」)。
///
/// Git 基準ではないので `ReviewPanel` には載せず、**明示的に開いた
/// ウィンドウ**にだけ出す (レイアウトを勝手に押しのけない)。
struct CompareView {
    title: String,
    file: crate::diff::FileDiff,
    /// 行クリックのインラインコメント。比較でも同じレンダラを使うので必要。
    comments: crate::diff::DiffCommentStore,
}

#[derive(PartialEq, Clone, Copy)]
enum SidebarTab {
    Files,
    /// ファイル横断検索 (VS Code: ⇧⌘F)
    Search,
    Agents,
    /// フォルダごとの過去の会話 (session_picker / panels::sessions_sidebar_ui)
    Sessions,
    Plugins,
    Git,
    GitHub,
}

impl SidebarTab {
    /// セッション保存用のキー文字列。
    fn as_key(self) -> &'static str {
        match self {
            SidebarTab::Files => "files",
            SidebarTab::Search => "search",
            SidebarTab::Agents => "agents",
            SidebarTab::Sessions => "sessions",
            SidebarTab::Plugins => "plugins",
            SidebarTab::Git => "git",
            SidebarTab::GitHub => "github",
        }
    }

    /// セッションのキー文字列から復元する。未知/空なら既定の Files。
    /// 新しいタブを足しても**古い保存値はそのまま読める** (未知だけが Files へ落ちる)。
    fn from_key(s: &str) -> Self {
        match s {
            "search" => SidebarTab::Search,
            "agents" => SidebarTab::Agents,
            "sessions" => SidebarTab::Sessions,
            "plugins" => SidebarTab::Plugins,
            "git" => SidebarTab::Git,
            "github" => SidebarTab::GitHub,
            _ => SidebarTab::Files,
        }
    }
}

/// 音声入力 (プッシュトゥトーク) の実行状態。
///
/// 認識結果は対象セッションの入力欄へ「挿入するだけ」で Enter は送らない。
/// 送信するかどうかは必ずユーザーが自分で決める (誤送信防止)。
///
/// 認識中のテキストは確定を待たずに入力欄へ流し込む。話している途中の文字は
/// 変換のたびに書き換わるので、直前に書いた分を `live` に覚えておき、
/// 食い違うところだけ Backspace で消してから続きを送る。
#[derive(Default)]
struct VoiceState {
    /// 起動中の認識プロセス (None = 停止中)。⏹ を押すまで動き続ける
    session: Option<voice::Session>,
    /// マイクが開いたか (認識準備完了)
    ready: bool,
    /// 認識テキストの届け先
    target: voice::Target,
    /// 認識途中のテキスト (HUD 表示用)
    partial: String,
    /// 停止要求を出した時刻 (確定待ちのタイムアウト用)
    stopping_at: Option<Instant>,
    /// 直前に文字を送った先。宛先が変わったら区切りの空白を入れない
    last_sent_to: Option<u64>,
    /// 直前に送った文字列の末尾の 1 文字 (区切り空白を入れるか決めるのに使う)
    last_char: Option<char>,
    /// いま入力欄に書き込んである「まだ確定していない」文字列。
    /// 区切りの空白を付けたならそれも含む (差分計算をこの 1 本で完結させるため)。
    live: String,
    /// `live` の先頭に区切りの空白を付けたか
    live_space: bool,
}

/// 入力欄へ 1 回ぶん反映するための編集。
struct VoiceEdit {
    /// Backspace (0x7f) で消す文字数
    del: usize,
    /// 消したあとに書き足す文字列
    add: String,
    /// 反映後、入力欄に書いてあるはずの文字列 (区切りの空白を含む)
    want: String,
    /// `want` の先頭に区切りの空白を付けたか
    space: bool,
}

impl VoiceEdit {
    /// 送るものが無い (同じ途中経過がもう一度届いた) か。
    fn is_noop(&self) -> bool {
        self.del == 0 && self.add.is_empty()
    }

    /// 端末へ送るバイト列。`submit` なら最後に Enter まで付ける。
    fn bytes(&self, submit: bool) -> Vec<u8> {
        let mut out: Vec<u8> = vec![0x7f; self.del]; // 0x7f = DEL、端末の Backspace
        out.extend_from_slice(self.add.as_bytes());
        if submit {
            out.push(b'\r');
        }
        out
    }
}

impl VoiceState {
    /// 入力欄が空になった (送信した / ユーザーが手で消した) ときに呼ぶ。
    /// 書き込み済みの追跡を捨てるので、次の認識テキストは先頭から書き出される。
    fn reset_live(&mut self) {
        self.live.clear();
        self.live_space = false;
        self.last_char = None;
    }

    /// 認識テキスト `body` を届け先 `dest` の入力欄へ反映するための編集を組み立てる。
    /// ここでは状態を変えない — 実際に書き込めたら `commit` を呼ぶこと。
    fn plan(&self, body: &str, dest: u64) -> VoiceEdit {
        // 区切りの空白を入れるかは、その区切りの書き出し時に一度だけ決めて据え置く
        // (話している途中で変換が変わっても、空白が付いたり消えたりしないように)。
        let space = if self.live.is_empty() {
            self.last_sent_to == Some(dest) && needs_space(self.last_char, body.chars().next())
        } else {
            self.live_space
        };
        let want = if space {
            format!(" {body}")
        } else {
            body.to_string()
        };
        let (del, add) = diff_edit(&self.live, &want);
        VoiceEdit {
            del,
            add,
            want,
            space,
        }
    }

    /// 書き込めた編集を状態へ反映する。
    ///
    /// 確定した分 (`is_final`) はもう書き換えないので追跡をやめる。これで次の
    /// ひとことは前の文を消さずにその後ろへ書き足される — 2 回目以降の発話が
    /// 同じ入力欄に溜まっていくのはここが効いている。
    fn commit(&mut self, edit: VoiceEdit, is_final: bool, submit: bool, dest: u64) {
        if submit {
            // Enter まで送ったので入力欄は空。次はまた先頭から書き出す
            self.reset_live();
            self.last_sent_to = None;
            return;
        }
        self.last_sent_to = Some(dest);
        if is_final {
            self.last_char = edit.want.chars().last();
            self.live.clear();
            self.live_space = false;
        } else {
            self.live = edit.want;
            self.live_space = edit.space;
        }
    }
}

/// kind: 0 = ok(緑), 1 = warn(黄), 2 = err(赤)
/// 確認待ちの移動/コピー 1 ジョブ。
///
/// 「1 操作 = 1 ジョブ」にしてあるのは、同名衝突の確認で
/// **「すべてに適用」を 1 回だけ聞く**ため (1 件ずつ聞かない)。
/// このキューに載っているあいだ、fs はまだ 1 バイトも変わっていない。
struct TransferQueue {
    items: Vec<TransferItem>,
    /// 次に処理する項目。`items.len()` に達したら締める。
    idx: usize,
    kind: file_tree::Transfer,
    /// ドラッグ&ドロップ由来か (移動そのものの確認を出す対象)。
    from_drag: bool,
    /// フォルダ統合として展開したときの (元, 先)。後始末で空フォルダを畳む。
    merge_root: Option<(PathBuf, PathBuf)>,
    /// 移動そのものの確認を通ったか。
    move_ok: bool,
    /// 移動確認ダイアログの「今後確認しない」チェックの現在値。
    dont_ask: bool,
    /// 同名衝突への答えを残り全部へ適用する (Some(true)=置き換える)。
    apply_all: Option<bool>,
    /// 衝突ダイアログの「すべてに適用」チェックの現在値。
    all_checked: bool,
    /// いま聞いていた 1 件への答え (次の drain で 1 回だけ使われる)。
    answer: Option<bool>,
    done: usize,
    skipped: usize,
    failed: usize,
    /// 最後に動かした先 (終わったらここを選択する)。
    last: Option<PathBuf>,
    /// 実際に動かした (元, 先)。取り消し履歴へ積むために貯める。
    moved: Vec<(PathBuf, PathBuf)>,
}

impl TransferQueue {
    fn new(job: file_tree::TransferJob) -> Self {
        Self {
            items: job.items,
            idx: 0,
            kind: job.kind,
            from_drag: job.from_drag,
            merge_root: job.merge_root,
            move_ok: false,
            dont_ask: false,
            apply_all: None,
            all_checked: false,
            answer: None,
            done: 0,
            skipped: 0,
            failed: 0,
            last: None,
            moved: Vec::new(),
        }
    }
}

struct Toast {
    msg: String,
    kind: u8,
    at: Instant,
}

struct FindState {
    open: bool,
    query: String,
    focus: bool,
    /// いま選ばれているヒットの**バイト範囲**。本文が変わると照合に失敗するので
    /// 「見つからなければ現在位置なし」として扱う (古い位置へ飛ばない)。
    current: Option<(usize, usize)>,
    /// 現在位置が無いときの探索起点 (バイト)。検索バーを開いた時点のカーソル。
    anchor: usize,
    /// 直前の移動で折り返したか。`Some(true)` = 末尾から先頭へ、
    /// `Some(false)` = 先頭から末尾へ。`None` = 折り返していない。
    wrapped: Option<bool>,
    /// 置換行 (VS Code: ⌥⌘F) を表示するか
    replace_open: bool,
    replace: String,
    /// 検索バーのトグル 3 つ (大小区別 / 単語単位 / 正規表現)
    opts: find_buffer::FindOptions,
}

/// バッファ内検索のヒット一覧キャッシュ。
///
/// 鍵は**本文ハッシュ + 検索語 + トグル**。1 回の走査結果を検索バー・
/// ミニマップの印・本文のハイライトが共有する (同じ本文を 3 回走査しない)。
struct FindHitCache {
    text_hash: u64,
    query: String,
    opts: find_buffer::FindOptions,
    /// 走査に使ったマッチャ (置換の `$1` 展開でも同じものを使う)
    matcher: file_search::Matcher,
    /// Arc 共有: 毎フレームの clone は参照カウント増加のみ
    hits: std::sync::Arc<Vec<find_buffer::BufHit>>,
    /// ミニマップに出す行 (重複を潰し、minimap::MAX_HITS で打ち切り)
    mm_lines: std::sync::Arc<Vec<usize>>,
    /// [`find_buffer::MAX_HITS`] で打ち切ったか
    truncated: bool,
    /// 正規表現のコンパイルエラー。あるときヒットは空。
    error: Option<String>,
}

/// 置換フローの段階。**ドライラン → 確認 → 実行**の 3 段でしか進まない。
///
/// 「置換」を押しただけでは 1 バイトも書かない (`dry_run: true` で数えるだけ)。
/// 数えた結果を出してユーザーが「実行」を押したときだけ本番の書き込みへ進む。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ReplacePhase {
    /// 何も進行していない。
    #[default]
    Idle,
    /// ドライラン中 / 本番実行中 (バックグラウンドスレッドの待ち)。
    Running,
    /// ドライランが終わり、ユーザーの確認待ち。
    Confirm { files: usize, hits: usize },
    /// 本番の置換が終わった。
    Done { files: usize, hits: usize },
}

/// 置換フローを進める出来事。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReplaceEvent {
    /// 「置換」ボタン — ドライランを投げる。
    Start,
    /// ドライランの結果が届いた。
    DryRunDone { files: usize, hits: usize },
    /// 「実行」ボタン — 本番の置換を投げる。
    Confirm,
    /// 本番の置換の結果が届いた。
    ExecuteDone { files: usize, hits: usize },
    /// 「やめる」ボタン / 検索条件の変更。
    Cancel,
    /// 置換が失敗した (正規表現エラーなど)。
    Failed,
}

impl ReplacePhase {
    /// 状態遷移 (純関数)。想定外の順序の出来事は**現状維持**にして、
    /// 取りこぼしたメッセージで勝手に書き込みへ進まないようにする。
    fn next(&self, ev: &ReplaceEvent) -> ReplacePhase {
        match (self, ev) {
            // 確認待ち中に新しくドライランを投げ直すのも許す (条件を直した場合)
            (ReplacePhase::Idle, ReplaceEvent::Start)
            | (ReplacePhase::Confirm { .. }, ReplaceEvent::Start)
            | (ReplacePhase::Done { .. }, ReplaceEvent::Start) => ReplacePhase::Running,
            // 0 件なら確認を出さずに畳む (押しても何も起きない確認ボタンを出さない)
            (ReplacePhase::Running, ReplaceEvent::DryRunDone { hits: 0, .. }) => {
                ReplacePhase::Done { files: 0, hits: 0 }
            }
            (ReplacePhase::Running, ReplaceEvent::DryRunDone { files, hits }) => {
                ReplacePhase::Confirm {
                    files: *files,
                    hits: *hits,
                }
            }
            (ReplacePhase::Confirm { .. }, ReplaceEvent::Confirm) => ReplacePhase::Running,
            (ReplacePhase::Running, ReplaceEvent::ExecuteDone { files, hits }) => {
                ReplacePhase::Done {
                    files: *files,
                    hits: *hits,
                }
            }
            (_, ReplaceEvent::Cancel) | (_, ReplaceEvent::Failed) => ReplacePhase::Idle,
            // 未確認のまま実行要求が来ても書き込みへは進めない
            _ => self.clone(),
        }
    }

    /// いま画面を止めて待っているか。
    fn busy(&self) -> bool {
        matches!(self, ReplacePhase::Running)
    }
}

/// 置換ワーカーからの戻り。
type ReplaceMsg = Result<file_search::ReplaceReport, String>;

/// ファイル横断検索 (サイドバーの検索タブ) の状態。
struct GlobalSearchState {
    query: String,
    focus: bool,
    running: bool,
    results: Vec<file_search::Hit>,
    /// 表示用スニペット (`Hit.text`) の中のマッチ範囲 (バイト)。
    /// `Hit.col` / `Hit.len` は**元の行**基準なので、検索が終わった時点で
    /// 1 度だけスニペットへ当て直して覚える (毎フレームの再計算をしない)。
    marks: Vec<Vec<(usize, usize)>>,
    /// 走査したファイル数 (結果の説明用)
    scanned: usize,
    rx: Option<mpsc::Receiver<(Vec<file_search::Hit>, usize)>>,
    /// 一度でも検索したか (0 件表示と初期状態の区別)
    searched: bool,
    // ── 検索オプション (egui memory へ永続化) ──
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    /// 対象を絞る glob (カンマ / 空白区切り。例 `*.rs, src/**`)
    include_globs: String,
    /// 除外する glob (include より強い)
    exclude_globs: String,
    // ── 置換 ──
    /// 置換行を出すか (VS Code の ▸ と同じ)
    replace_open: bool,
    replace: String,
    phase: ReplacePhase,
    replace_rx: Option<mpsc::Receiver<ReplaceMsg>>,
    /// パターンのコンパイルエラー。赤字でその場に出す (黙って literal に落とさない)。
    error: Option<String>,
    /// 「まとめて開く」が押された (マルチバッファへ)。呼び出し側が読んで倒す。
    open_multi: bool,
}

impl GlobalSearchState {
    fn new() -> Self {
        Self {
            query: String::new(),
            focus: false,
            running: false,
            results: Vec::new(),
            marks: Vec::new(),
            scanned: 0,
            rx: None,
            searched: false,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            include_globs: String::new(),
            exclude_globs: String::new(),
            replace_open: false,
            replace: String::new(),
            phase: ReplacePhase::Idle,
            replace_rx: None,
            error: None,
            open_multi: false,
        }
    }

    /// 画面の状態を検索エンジンの [`file_search::SearchOptions`] へ写す。
    ///
    /// glob 欄はカンマ・空白・改行のどれで区切っても同じに読む
    /// (「`*.rs, *.toml`」と打っても「`*.rs *.toml`」と打っても同じ)。
    fn options(&self, root: Option<PathBuf>) -> file_search::SearchOptions {
        file_search::SearchOptions {
            query: self.query.trim().to_string(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
            include_globs: split_globs(&self.include_globs),
            exclude_globs: split_globs(&self.exclude_globs),
            root,
            ..file_search::SearchOptions::default()
        }
    }
}

// ─────────────────── プラン使用量の表示ルール ───────────────────
//
// 表示は次の 3 つを**必ず**守る (推測を数字で出さないため):
//
// 1. `used_fraction` が `None` の行に数字を出さない (「不明」と描く)
// 2. `confidence` が Measured 以外なら「推定」と明示する
// 3. `Projection::InsufficientData` は「データ不足」— 決して「0 分」と描かない

/// 使用率 1 行の表示文字列。
fn quota_usage_label(u: &crate::coordinator::quota::AccountUsage) -> String {
    let Some(f) = u.used_fraction else {
        // 数字が無いのだから数字は出さない (0% と書くと「まだ使っていない」に見える)
        return tr("不明");
    };
    let pct = (f.clamp(0.0, 1.0) * 100.0).round() as u32;
    if u.confidence == crate::coordinator::quota::Confidence::Measured {
        trf("{pct}%", &[("pct", pct.to_string())])
    } else {
        trf("{pct}% (推定)", &[("pct", pct.to_string())])
    }
}

/// 枯渇予測 1 行の表示文字列。
fn quota_projection_label(p: crate::coordinator::quota::Projection) -> String {
    use crate::coordinator::quota::Projection;
    match p {
        // 材料不足を「あと 0 分」と描かない
        Projection::InsufficientData => tr("データ不足"),
        Projection::NotBurning => tr("消費なし"),
        Projection::Exhaustion(d) => trf(
            "約 {mins} 分で枯渇 (推定)",
            &[("mins", (d.as_secs().div_ceil(60)).to_string())],
        ),
        Projection::ResetFirst(d) => trf(
            "約 {mins} 分でリセット",
            &[("mins", (d.as_secs().div_ceil(60)).to_string())],
        ),
    }
}

/// 深刻さ (0/1/2) → ステータスバーの絵文字。
fn quota_severity_icon(severity: u8) -> &'static str {
    match severity {
        0 => "○",
        1 => "◇",
        _ => "●",
    }
}

/// 経過時間の短い表記 (「45 秒」「12 分」「3 時間」)。
/// 桁を増やさないので、狭い幅でも行が伸びない。
fn fmt_ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        trf("{n} 秒", &[("n", s.to_string())])
    } else if s < 3600 {
        trf("{n} 分", &[("n", (s / 60).to_string())])
    } else {
        trf("{n} 時間", &[("n", (s / 3600).to_string())])
    }
}

/// セッションサイドバーの押しごたえを「実際に何をするか」へ落としたもの。
///
/// 起動やファイラ起動といった副作用の**手前**で切って純関数にしてあるので、
/// テストからプロセスを起こさずに対応表を固定できる。
#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionSidebarEffect {
    /// 何もしない (対応するプリセットが無い等)。理由はトーストに出す。
    Nothing(Option<String>),
    /// このコマンドをこの作業ディレクトリで起動する。
    Launch {
        /// 元になったプリセットの index (アイコン・env を引くのに使う)
        preset: usize,
        command: String,
        cwd: PathBuf,
    },
    /// OS のファイラで開く。
    Reveal(PathBuf),
    /// ワークスペースのルートから外す。
    RemoveRoot(PathBuf),
}

/// `bin` (実行ファイル名) に対応するプリセットを探す。
///
/// 完全一致 (`spec_for_command` が末尾要素で照合するので絶対パス指定でも当たる) を
/// 先に見て、無ければ諦める。名前の前方一致のような曖昧な照合はしない —
/// 別の CLI を再開コマンドで起動してしまうため。
fn preset_for_bin(presets: &[config::AgentPreset], bin: &str) -> Option<usize> {
    presets
        .iter()
        .position(|p| crate::agents::spec_for_command(&p.command).is_some_and(|s| s.bin == bin))
}

/// 「新しい会話」に使うプリセット。カタログ既知の CLI を優先し、
/// 無ければ先頭 (素のシェルしか登録していない構成でも動くようにする)。
fn preset_for_new_conversation(presets: &[config::AgentPreset]) -> Option<usize> {
    presets
        .iter()
        .position(|p| crate::agents::spec_for_command(&p.command).is_some())
        .or(if presets.is_empty() { None } else { Some(0) })
}

/// [`session_picker::SidebarAction`] → [`SessionSidebarEffect`] (純関数)。
fn session_sidebar_effect(
    action: &session_picker::SidebarAction,
    presets: &[config::AgentPreset],
) -> SessionSidebarEffect {
    use session_picker::SidebarAction as A;
    match action {
        A::None => SessionSidebarEffect::Nothing(None),
        A::Resume(s) => match preset_for_bin(presets, &s.agent_bin) {
            Some(i) => SessionSidebarEffect::Launch {
                preset: i,
                command: session_picker::resume_command(&presets[i].command, s),
                // 会話が走っていたフォルダで再開する (別の場所で再開すると
                // 相対パスの指示が全部ずれる)
                cwd: s.cwd.clone(),
            },
            None => SessionSidebarEffect::Nothing(Some(trf(
                "{bin} のプリセットが見つかりません (設定で追加してください)",
                &[("bin", s.agent_bin.clone())],
            ))),
        },
        A::NewConversation(dir) => match preset_for_new_conversation(presets) {
            Some(i) => SessionSidebarEffect::Launch {
                preset: i,
                command: presets[i].command.clone(),
                cwd: dir.clone(),
            },
            None => SessionSidebarEffect::Nothing(Some(tr("エージェントのプリセットがありません"))),
        },
        A::RevealFolder(dir) => SessionSidebarEffect::Reveal(dir.clone()),
        A::CloseFolder(dir) => SessionSidebarEffect::RemoveRoot(dir.clone()),
    }
}

// ---------------------------------------------------------------------------
// Cockpit のレイアウト計算 (純関数)
//
// 「割り当てられた領域に必ず収まる」ことを egui を起こさずに固定するため、
// 幾何だけを切り出してある。ここが崩れると右端でボタンが切れたり、
// 見出しの下に何百 px もの空白が残ったりする (どちらも実際に起きた)。
// ---------------------------------------------------------------------------

/// タイル同士の間隔。
const GRID_SPACING: f32 = 10.0;
/// 2 列に割るのに要る最小幅。これ未満は 1 列 (エディタと分割したときなど)。
const GRID_TWO_COL_W: f32 = 640.0;
/// タイルの最低高さ。
const GRID_MIN_CELL_H: f32 = 150.0;
/// タイル 1 枚に残したいミニターミナルの行数。
///
/// これを下回ると「何かが動いているのは分かるが、何をしているかは読めない」
/// タイルになる。6 枚以上開いたときに全部を 1 画面へ詰め込むのをやめ、
/// この行数を保ったままスクロールへ逃がすための基準。
const GRID_COMFORT_ROWS: f32 = 16.0;
/// タイルのヘッダ行 + 枠の余白 (端末の外側で使う固定の高さ)。
const GRID_TILE_CHROME_H: f32 = 46.0;
/// 快適な高さの上限。巨大フォントでもタイル 1 枚で画面を独占させない。
const GRID_COMFORT_MAX_H: f32 = 420.0;
/// 見出し行を「アイコンだけ」に縮退させる幅のしきい値。
const COCKPIT_HEADER_COMPACT_W: f32 = 820.0;

/// **中央に描くビュー。** 同時に 2 つは描かない。
///
/// 実際に起きていた不具合: Cockpit のヘッダーで「📋 看板」を押すと、その
/// フレームの**途中で** `cockpit=false / kanban=true` になり、既に描き始めて
/// いた Cockpit のタイルと、あとから出てくる看板が**重なって**描かれた。
/// 独立した bool を 3 本持つ限りこの種の重なりは構造的に起こり得るので、
/// 「今フレーム何を描くか」は必ずこの 1 個の値へ畳んでから使う。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum CenterView {
    /// 通常のエディタ
    #[default]
    Editor,
    /// 🎛 Agent Cockpit
    Cockpit,
    /// 📋 フリート看板 (下部ターミナルパネル内のタブ)
    Kanban,
    /// 🗂 エージェントデッキ
    Deck,
}

/// ズームジェスチャ (⌘+ホイール / ピンチ) を持っている中央ビューの種別。
///
/// egui は ⌘+ホイールもピンチも `zoom_delta()` にまとめてしまい、しかも
/// **消費できない** (`zoom_factor_delta` は非公開)。同じジェスチャで
/// 「画像も拡大・文字も拡大」の二重掛けが起きないよう、持ち主をここで
/// 1 つに決めてから配る ([`ZaivernApp::handle_zoom_gesture`])。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ZoomArea {
    /// エディタ本文 / Markdown プレビュー — そのファイルだけを拡大縮小する
    File,
    /// 画像ビューア — 自前で `zoom_delta` を読むので、ここでは何もしない
    Image,
}

/// 3 本のフラグから「今フレーム描くビュー」を 1 つ決める (純関数)。
///
/// 優先順は デッキ > Cockpit > 看板 > エディタ。フラグが複数立っていても
/// 返り値は必ず 1 つなので、2 つのビューが重なって描かれることはない。
fn center_view(cockpit: bool, kanban: bool, deck: bool) -> CenterView {
    if deck {
        CenterView::Deck
    } else if cockpit {
        CenterView::Cockpit
    } else if kanban {
        CenterView::Kanban
    } else {
        CenterView::Editor
    }
}

/// **未読カーソルの巡回** (純関数)。`from` の**次**から探し、端で折り返す。
///
/// * 0 件なら `None` — 呼び出し側はバッジを 1 ピクセルも描かない。
/// * `from` 自身も最後に見るので、「未読が 1 件だけでそれが今の相手」でも
///   ちゃんとその 1 件を返す (「押しても何も起きない」を作らない)。
/// * 順序は**セッションの並び順で固定**。通知の新しさで並べ替えない
///   (cmux が「⌘1-9 の割当が動き続ける」と批判された轍を踏まない)。
fn next_unread(unread: &[bool], from: usize) -> Option<usize> {
    let n = unread.len();
    if n == 0 {
        return None;
    }
    let from = from.min(n - 1);
    (1..=n).map(|step| (from + step) % n).find(|&i| unread[i])
}

/// **ボトムパネルの中身。** 同時に 2 つは描かない。
///
/// 中央ビュー ([`CenterView`]) と同じ理由でここも 1 個の値へ畳む。
/// 「🛡 承認」「🔌 MCP」「🧩 Skills」「端末」を独立した bool で持つと、
/// タブを続けて押したときに 2 つが重なって描かれる事故が構造的に起こり得る。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum BottomView {
    /// エージェントの端末 (既定)
    #[default]
    Terminal,
    /// 🛡 統合承認キュー
    Approvals,
    /// 🔌 MCP サーバ管理
    Mcp,
    /// 🧩 Skills / slash command 管理
    Skills,
    /// 📐 spec 駆動開発 (差分と陳腐化の見張り)
    Spec,
}

/// 4 本のフラグから「今フレーム描くボトムパネルの中身」を 1 つ決める (純関数)。
///
/// 優先順は 承認 > MCP > Skills > Spec > 端末。複数立っていても返り値は必ず 1 つ。
fn bottom_view(approvals: bool, mcp: bool, skills: bool, spec: bool) -> BottomView {
    if approvals {
        BottomView::Approvals
    } else if mcp {
        BottomView::Mcp
    } else if skills {
        BottomView::Skills
    } else if spec {
        BottomView::Spec
    } else {
        BottomView::Terminal
    }
}

/// エディタ本文 (`TextEdit`) の egui ID。**バッファとペインの両方**で決める。
///
/// 同じファイルを 2 つのペインで開いたとき、ID がバッファだけで決まっていると
/// 2 枚の `TextEdit` が同じ状態 (カーソル・選択) を共有してしまい、
/// 片方を触るともう片方のキャレットまで飛ぶ。ID にペインを混ぜて分ける。
/// **本文は共有・ビュー状態は別** という原則の ID 側の表れ。
fn buf_edit_id(pane: editor_split::PaneId, buf: u64) -> egui::Id {
    egui::Id::new(("zaivern-buffer", buf, pane))
}

/// 可視域ハイライトの追い付き状態を覚えておく (ペイン, バッファ) の上限。
/// 超えたら丸ごと捨てる — 作り直しは 1 フレームで済むので LRU は要らない。
const HL_STATE_CAP: usize = 256;

/// `BlameMode::Current` で `git blame` を取りに行く帯の高さ (行)。
///
/// 1 行きっかりにすると**カーソルを 1 行動かすたびに git が起きる**ので、
/// この行数の帯へ丸めて、帯の中ではキーが変わらない = git を起こさない。
/// `git::BLAME_BLOCK` (200 行) をそのまま使うと `all` と重さが変わらず、
/// 3 段にした意味が無くなる。
const BLAME_CURRENT_BAND: usize = 16;

/// `BlameMode::Current` が取りに行く行域 (**1 始まり・両端含む**)。
///
/// `git::blame_block` と同じ形の戻り値にしてあるので、呼び出し側は
/// どちらを使ったかを意識しなくてよい。
fn blame_current_range(caret0: usize, total: usize) -> (usize, usize) {
    if total == 0 {
        return (1, 1);
    }
    let caret0 = caret0.min(total - 1);
    let b = caret0 / BLAME_CURRENT_BAND;
    let start = (b * BLAME_CURRENT_BAND + 1).min(total);
    let end = ((b + 1) * BLAME_CURRENT_BAND).min(total).max(start);
    (start, end)
}

/// galley キャッシュキーへ混ぜる可視域 (`(start, end)`)。
///
/// `Highlighter::layout_job_visible` が可視域で塗り分けるのは
/// **巨大ファイルだけ**で、それ以外は可視域を無視して全文を塗る。それでも
/// 可視域をキーへ混ぜると、512 行スクロールするたびに galley を丸ごと
/// 組み直すことになる (組み直しは実測 495ms で、セクション数にほとんど
/// 依存しない)。だから**塗り分けが可視域に依存すると分かってから**混ぜる。
///
/// `windowed` は直前のフレームの `VisibleJob::scanned_lines > 0`。
fn galley_window_key(windowed: bool, win: crate::highlight::Window) -> (usize, usize) {
    if windowed {
        (win.start, win.end)
    } else {
        (0, 0)
    }
}

/// Cockpit の見出し行を縮退させるか (純関数)。
fn cockpit_header_compact(avail_w: f32) -> bool {
    avail_w < COCKPIT_HEADER_COMPACT_W
}

/// トップバー左側 (ロゴ + VS Code 準拠の 8 メニュー + ブランチ) のおおよその幅。
/// 右側のボタン群がここへ食い込むと、メニューの文字と重なって両方読めなくなる。
const TOP_BAR_LEFT_W: f32 = 620.0;
/// 右側ボタン群をラベル付きで並べるのに要る幅。
const TOP_BAR_RIGHT_W: f32 = 470.0;

/// アイコンだけに縮めた右側ボタン群の幅。
const TOP_BAR_RIGHT_ICON_W: f32 = 430.0;

/// トップバー右側の密度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopBarDensity {
    /// ラベル付き
    Full,
    /// アイコンだけ
    Compact,
    /// アイコンだけ + 装飾系 (テーマ/リモート/音声/ペット) を「⋯」へ畳む
    Overflow,
}

impl TopBarDensity {
    /// ボタンの文字を落とすか。
    fn compact(self) -> bool {
        self != TopBarDensity::Full
    }
}

/// トップバー右側の密度を決める (純関数)。
///
/// 実際に起きていた不具合: 900px 幅で「実行 / ターミナル / ヘルプ」の上に
/// 「看板」「Cockpit」「既定:承認」が**重なって**描かれ、どちらも読めない。
/// egui の `right_to_left` は残り幅が足りなくても縮めてくれないので、
/// 入る形かどうかをこちらで決める。
fn top_bar_density(bar_w: f32) -> TopBarDensity {
    if bar_w >= TOP_BAR_LEFT_W + TOP_BAR_RIGHT_W {
        TopBarDensity::Full
    } else if bar_w >= TOP_BAR_LEFT_W + TOP_BAR_RIGHT_ICON_W {
        TopBarDensity::Compact
    } else {
        TopBarDensity::Overflow
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  起動バーのレイアウト (純関数 + テーブルテスト)
//
//  「どの幅でも見切れない」を証明できる形にしておく。矩形は必ず可用領域へ
//  収まり、互いに重ならない。**0 件のときは高さを 1px も取らない**。
// ═══════════════════════════════════════════════════════════════════════════

/// チップ 1 個の左右余白 + 番号バッジの幅 (px)。
const QUICK_CHIP_PAD: f32 = 26.0;
/// チップ同士の間隔 (px)。
const QUICK_CHIP_GAP: f32 = 6.0;
/// アイコンだけへ縮退したチップの幅 (px)。
const QUICK_CHIP_ICON_W: f32 = 46.0;
/// 起動バーの行の高さ (px)。0 件のときは**この高さも取らない**。
const QUICK_BAR_H: f32 = 26.0;

/// 起動バー 1 行の割り付け。
#[derive(Clone, PartialEq, Debug)]
struct QuickBarPlan {
    /// 描くチップの数 (先頭から。入り切らない分は落とす)。
    shown: usize,
    /// ラベルを落としてアイコン + 番号だけにするか。
    icons_only: bool,
    /// チップ 1 個あたりの幅 (px)。全チップ同じ幅にする。
    chip_w: f32,
    /// 行の高さ (px)。**0 件なら 0.0** — 空のセクションは高さを取らない。
    height: f32,
}

impl QuickBarPlan {
    /// `i` 番目のチップの左端 x (可用領域の左端からの相対)。
    /// 矩形が重ならないことをテーブルテストで証明するためのもの
    /// (描画は egui の `horizontal` が同じ間隔で並べる)。
    #[cfg(test)]
    fn chip_x(&self, i: usize) -> f32 {
        i as f32 * (self.chip_w + QUICK_CHIP_GAP)
    }

    /// 行全体が使う幅 (px)。
    fn used_w(&self) -> f32 {
        match self.shown {
            0 => 0.0,
            n => n as f32 * self.chip_w + (n - 1) as f32 * QUICK_CHIP_GAP,
        }
    }
}

/// 起動バーの割り付けを決める (純関数)。
///
/// * `avail_w`: 使ってよい横幅 (`ui.available_width()`)。
/// * `label_ws`: チップごとのラベル実寸 (px)。件数 = 割り当て数。
///
/// 決め方は 3 段: ①全部ラベル付きで入るか → ②アイコンのみへ縮退して入るか →
/// ③それでも入らないなら**入る個数だけ**描く (見切れさせない)。
fn quick_bar_plan(avail_w: f32, label_ws: &[f32]) -> QuickBarPlan {
    let n = label_ws.len();
    if n == 0 || avail_w <= 0.0 {
        // 空のセクションは見出しごと消す = 高さを 1px も取らない。
        return QuickBarPlan {
            shown: 0,
            icons_only: false,
            chip_w: 0.0,
            height: 0.0,
        };
    }
    let widest = label_ws.iter().cloned().fold(0.0_f32, f32::max);
    let full_w = (widest + QUICK_CHIP_PAD).max(QUICK_CHIP_ICON_W);
    let fits = |chip_w: f32, count: usize| -> bool {
        count > 0
            && count as f32 * chip_w + (count.saturating_sub(1)) as f32 * QUICK_CHIP_GAP <= avail_w
    };
    if fits(full_w, n) {
        return QuickBarPlan {
            shown: n,
            icons_only: false,
            chip_w: full_w,
            height: QUICK_BAR_H,
        };
    }
    // アイコン + 番号だけへ縮退させる。
    let icon_w = QUICK_CHIP_ICON_W;
    if fits(icon_w, n) {
        return QuickBarPlan {
            shown: n,
            icons_only: true,
            chip_w: icon_w,
            height: QUICK_BAR_H,
        };
    }
    // それでも入らない: 入る個数だけ描く (はみ出させない)。
    let mut shown = 0usize;
    while fits(icon_w, shown + 1) && shown < n {
        shown += 1;
    }
    QuickBarPlan {
        shown,
        icons_only: true,
        chip_w: icon_w,
        height: if shown == 0 { 0.0 } else { QUICK_BAR_H },
    }
}

/// 出力が止まってから「ターンが終わった」と見なすまでの静穏時間 (ms)。
///
/// 短すぎるとモデルの思考の合間で切れ、長すぎると命名が遅れる。
/// エージェントの**状態**の判定には使わない値なので、外しても害は無い
/// (誤検知 = 題名が 1 回多く付く / 取りこぼし = 従来名のまま)。
const AUTO_NAME_QUIET_MS: u64 = 4_000;

/// 自動命名を撃つかどうかの判断材料 (すべて外から与える)。
#[derive(Clone, Copy, PartialEq, Debug, Default)]
struct AutoNameSignals {
    /// 設定で有効になっているか (**既定は false**)
    enabled: bool,
    /// ターンが終わった瞬間か
    turn_ended: bool,
    /// セッションが生きているか
    running: bool,
    /// ユーザーが手で名前を付けた相手か
    manual: bool,
    /// そのセッション自身の CLI が非対話の一発実行に対応しているか
    has_generator: bool,
    /// 送れる材料 (ユーザー自身の指示文) があるか
    has_brief: bool,
    /// 同じ材料で既に命名済みか
    already_named: bool,
}

/// このセッションへ自動命名を撃つか (純関数)。
///
/// **手動名は常に勝つ**・**既定はオフ**・**同じ材料で二度は撃たない**を
/// ここ 1 か所で決める。どれか 1 つでも欠けたら撃たない。
fn should_auto_name(s: AutoNameSignals) -> bool {
    s.enabled
        && s.turn_ended
        && s.running
        && !s.manual
        && s.has_generator
        && s.has_brief
        && !s.already_named
}

/// 生成結果をタイトルへ反映する (純関数)。
///
/// * 手で付けた名前は**絶対に**上書きしない。
/// * 生成に失敗した (`None`) ら**黙って従来の名前のまま**。
fn apply_named_title(current: &str, manual: bool, generated: Option<String>) -> String {
    match (manual, generated) {
        (false, Some(t)) if !t.trim().is_empty() => t,
        _ => current.to_string(),
    }
}

/// 命名に使った指示文の指紋。同じ指示のまま次のターンが終わっても
/// もう一度は走らせないための照合キー (内容そのものは保持しない)。
fn auto_name_signature(brief: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    brief.hash(&mut h);
    h.finish()
}

/// 起動バーの割り当て編集 (右クリックメニューから)。
#[derive(Clone, Copy, PartialEq, Debug)]
enum QuickBarEdit {
    /// スロット `usize` を 1 つ左へ (番号が 1 つ小さくなる)
    MoveLeft(usize),
    /// スロット `usize` を 1 つ右へ
    MoveRight(usize),
    /// スロット `usize` を起動バーから外す
    Remove(usize),
    /// プリセット `usize` を末尾のスロットへ足す
    Add(usize),
    /// 既定 (プリセットの並びの先頭から) へ戻す
    Reset,
}

/// 1 行帯コンポーザをヘッダー行へ畳み込むのに要る最小の残り幅 (px)。
///
/// 内訳: 宛先チップ ~70 + 入力欄の下限 80 + 送信/▾ ~92 + 間隔。これを
/// 割ると入力欄が潰れて押せなくなるので、その場合だけ独立した行へ落とす。
const COMPOSER_INLINE_MIN_W: f32 = 260.0;

/// コンポーザを見出し行へ畳み込むか (純関数)。
///
/// 複数行フォーム (`expanded`) は**絶対に**畳み込まない — 横並びの 1 行へ
/// 押し込むと右端の細い帯に折り返され、見出しの下に数百 px の空白ができる
/// (実際に起きた不具合)。1 行帯でも残り幅が入力欄の下限を割るなら別行へ落とす。
fn composer_fits_header(expanded: bool, remaining_w: f32) -> bool {
    !expanded && remaining_w >= COMPOSER_INLINE_MIN_W
}

/// 見出し帯の内訳 (純関数の結果)。
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(test)]
struct HeaderLayout {
    /// タイトル・状態・右寄せボタン群 (+ 畳み込めたときはコンポーザ) の行
    row: egui::Rect,
    /// 畳み込めなかったコンポーザの行。畳み込めたときは `None` = 1 行で済む
    composer: Option<egui::Rect>,
}

#[cfg(test)]
impl HeaderLayout {
    /// 見出し帯が実際に使う高さ。
    fn height(&self) -> f32 {
        self.composer
            .map_or(self.row.height(), |c| c.bottom() - self.row.top())
    }
}

/// 見出し帯の矩形を決める (純関数)。
///
/// 密度の目標: **エージェントが居てコンポーザが未フォーカスなら 1 行**。
/// - `remaining_w`: 右寄せボタン群を置いたあとにコンポーザへ回せる幅 (実測値)
/// - `row_h`: 1 行の高さ (egui の `interact_size.y` 相当)
/// - `form_h`: 複数行フォームを開いたときの高さ
///
/// 不変条件: `row` と `composer` は重ならず、どちらも `avail` の中。
#[cfg(test)]
fn cockpit_header_layout(
    avail: egui::Rect,
    expanded: bool,
    remaining_w: f32,
    row_h: f32,
    form_h: f32,
) -> HeaderLayout {
    let row = egui::Rect::from_min_size(
        avail.min,
        egui::vec2(avail.width(), row_h.min(avail.height())),
    );
    if composer_fits_header(expanded, remaining_w) {
        // 1 行に畳み込めた = 見出し帯は行高そのまま
        return HeaderLayout {
            row,
            composer: None,
        };
    }
    let top = (row.bottom() + crate::panels::space::XS).min(avail.bottom());
    let h = form_h.min((avail.bottom() - top).max(0.0));
    HeaderLayout {
        row,
        composer: Some(egui::Rect::from_min_size(
            egui::pos2(avail.left(), top),
            egui::vec2(avail.width(), h),
        )),
    }
}

/// タイル格子の寸法。
#[derive(Clone, Copy, Debug, PartialEq)]
struct GridMetrics {
    cols: usize,
    rows: usize,
    cell_w: f32,
    cell_h: f32,
}

impl GridMetrics {
    /// 格子全体の高さ (= スクロール領域の中身の高さ)。
    fn content_h(&self) -> f32 {
        self.rows as f32 * (self.cell_h + 4.0) + GRID_SPACING * (self.rows as f32 - 1.0)
    }

    /// この寸法だと縦スクロールが要るか (可用高さ `avail_y` に対して)。
    fn scrolls(&self, avail_y: f32) -> bool {
        self.content_h() > avail_y + 0.5
    }
}

/// タイル 1 枚に確保したい高さ (= これ以上は縮めない下限)。
///
/// ミニターミナルの文字が大きいほど、同じ行数を読むのに要る高さも増えるので
/// フォントから導く。ハードコードした 1 つの値だと、フォントを上げた途端に
/// 「タイルは大きいのに 5 行しか見えない」に戻ってしまう。
fn grid_comfort_cell_h(mini_font: f32) -> f32 {
    // 行送りは epaint の等幅フォントの実測 (≒ 1.35em) に合わせた概算。
    // 正確な値は描画時にしか判らないが、ここで要るのは「読める高さ」の目安。
    (mini_font * 1.35 * GRID_COMFORT_ROWS + GRID_TILE_CHROME_H)
        .clamp(GRID_MIN_CELL_H, GRID_COMFORT_MAX_H)
}

/// 与えられた領域に n 枚のタイルを敷くときの寸法 (純関数)。
///
/// 不変条件:
/// * **総幅は `avail.x` を超えない** — 超えると右端のタイルが切れる。
/// * **タイルは `comfort` より低くしない** — 枚数が増えたときに全部を 1 画面へ
///   詰め込むと、6 枚あたりから中身が読めなくなる。縮めるのをやめて縦へ伸ばし、
///   はみ出したぶんはスクロールで見せる ([`GridMetrics::scrolls`])。
///   副次効果として、7 枚目を足しても既存タイルの高さが変わらない
///   = 既存 PTY のリサイズが起きない。
/// * ただし**窓そのものが低いとき**は 1 枚が窓に収まる高さまで譲る
///   (タイル 1 枚しか無いのにスクロールさせない)。
fn cockpit_grid_metrics(avail: egui::Vec2, n: usize, comfort: f32) -> GridMetrics {
    let cols = if n <= 1 || avail.x < GRID_TWO_COL_W {
        1
    } else {
        2
    };
    let rows = n.div_ceil(cols).max(1);
    let cell_w = ((avail.x - GRID_SPACING * (cols as f32 - 1.0)) / cols as f32 - 4.0).max(1.0);
    // 全部を 1 画面へ詰め込むときの高さ (旧実装の計算そのまま)。
    let fit = ((avail.y - GRID_SPACING * (rows as f32 - 1.0)) / rows as f32) - 4.0;
    // 譲れる下限。低い窓では「1 枚が窓に収まる高さ」まで下げてよい。
    let floor = comfort.min(avail.y - 4.0).max(GRID_MIN_CELL_H);
    GridMetrics {
        cols,
        rows,
        cell_w,
        cell_h: fit.max(floor),
    }
}

/// 保存前クリーンアップの本体 (純関数)。
///
/// 返り値が `None` なら本文が 1 バイトも変わっていない = 書き込みも undo 積みも
/// カーソル付け替えも省ける。`Some((本文, 選択範囲))` の選択範囲は
/// [`editor_ops::adjust_char_index_after_cleanup`] で付け替え済み
/// (行末が削れたぶんずれたカーソルが別の行へ飛ばないようにするため)。
fn save_cleanup_edit(
    text: &str,
    sel: (usize, usize),
    opts: &editor_ops::SaveCleanup,
) -> Option<(String, (usize, usize))> {
    let (cleaned, changed) = editor_ops::apply_save_cleanup_checked(text, opts);
    if !changed {
        return None;
    }
    let s = editor_ops::adjust_char_index_after_cleanup(text, &cleaned, sel.0);
    let e = editor_ops::adjust_char_index_after_cleanup(text, &cleaned, sel.1);
    Some((cleaned, (s, e)))
}

// ───────────────── インデントの切替 (ステータスバー) ─────────────────

/// ステータスバーのインデントメニューで選ばれたもの。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IndentAction {
    /// 表示だけ変える (本文は 1 文字も触らない)。
    Display(editor_ops::IndentStyle),
    /// 本文のインデントも新しい様式へ書き換える。
    Convert(editor_ops::IndentStyle),
    /// 中身から推定し直す。
    Detect,
}

/// ステータスバーのインデントメニューの中身 (純粋な描画。self を借りない)。
///
/// **選択肢は「表示だけ」と「変換する」の 2 段**。VS Code と同じく、
/// 押しただけで本文が書き換わることが無いようにする
/// (書き換える側は取り消し 1 段で戻せる)。
fn indent_menu_ui(ui: &mut egui::Ui, cur: editor_ops::IndentStyle, out: &mut Option<IndentAction>) {
    // 幅の候補。設定の tab_size とは独立の「よくある値」で、
    // 現在値がこの並びに無ければ先頭へ足して必ず選べるようにする。
    let mut widths: Vec<usize> = vec![2, 4, 8];
    if !widths.contains(&cur.width) {
        widths.push(cur.width);
        widths.sort_unstable();
    }
    let mut row = |ui: &mut egui::Ui, convert: bool| {
        // 行は必ず可用幅に収める (ボタンは短いラベルなので折り返さない)
        ui.horizontal_wrapped(|ui| {
            for w in &widths {
                let st = editor_ops::IndentStyle::new(false, *w);
                if ui
                    .selectable_label(cur == st, format!("␣{w}"))
                    .on_hover_text(trf("スペース {n}", &[("n", w.to_string())]))
                    .clicked()
                {
                    *out = Some(if convert {
                        IndentAction::Convert(st)
                    } else {
                        IndentAction::Display(st)
                    });
                    ui.close_menu();
                }
            }
            let st = editor_ops::IndentStyle::new(true, cur.width);
            if ui
                .selectable_label(cur.tabs, tr("タブ"))
                .on_hover_text(trf("タブ (幅 {n})", &[("n", cur.width.to_string())]))
                .clicked()
            {
                *out = Some(if convert {
                    IndentAction::Convert(st)
                } else {
                    IndentAction::Display(st)
                });
                ui.close_menu();
            }
        });
    };
    ui.label(RichText::new(tr("表示だけ変える")).strong());
    row(ui, false);
    ui.separator();
    ui.label(RichText::new(tr("インデントを変換する")).strong());
    row(ui, true);
    ui.separator();
    if ui.button(tr("中身から推定し直す")).clicked() {
        *out = Some(IndentAction::Detect);
        ui.close_menu();
    }
}

// ───────────────── 縦のルーラー (editor.rulers) ─────────────────

/// 設定の桁並びを描画に使える形へ正規化する。
///
/// 0 桁は落とす — 本文の左端に重なるだけで「桁の目印」にならないため。
/// 重複も落として昇順にする (描画側が毎フレーム並べ替えないで済む)。
fn normalize_rulers(cols: &[usize]) -> Vec<usize> {
    let mut v: Vec<usize> = cols.iter().copied().filter(|c| *c > 0).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// ルーラーを引く x 座標を返す (可用領域に収まるものだけ)。
///
/// 桁は**等幅の桁数**で数える — 東アジア文字の幅は数えない (VS Code と同じ)。
/// 座標は [`theme::snap_len`] で整数ピクセルへ揃える。小数のままだと
/// 100% 表示で線が隣の桁へにじみ、文字がガタガタに見えるため。
///
/// `clip` は描いてよい x の範囲 (ガターの右端 〜 表示域の右端)。
/// はみ出す桁は**返さない**ので、呼び出し側は無条件に描いてよい。
fn ruler_x_positions(
    cols: &[usize],
    text_left: f32,
    char_w: f32,
    clip: egui::Rangef,
    ppp: f32,
) -> Vec<f32> {
    if !char_w.is_finite() || char_w <= 0.0 || !text_left.is_finite() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(cols.len());
    for c in cols {
        let x = text_left + theme::snap_len(*c as f32 * char_w, ppp);
        if !x.is_finite() || x < clip.min || x > clip.max {
            continue;
        }
        out.push(x);
    }
    out
}

/// glob 欄 1 本を個々のパターンへ割る。区切りはカンマ・空白・改行のどれでもよい。
/// 空の断片は落とすので、末尾のカンマや二重空白があっても空パターンにならない
/// (空パターンは「何にも一致しない」ので、混ざると結果が黙って 0 件になる)。
fn split_globs(s: &str) -> Vec<String> {
    s.split([',', ' ', '\t', '\n', '\r'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// プラグインタブで押されたボタン類。クロージャの中では記録だけして、
/// パネル描画後に self へ反映する (TreeActions / GitActions と同じ流儀)。
#[derive(Default)]
struct PluginActions {
    new_plugin: bool,
    install: bool,
    rescan: bool,
    uninstall: Option<PathBuf>,
    theme: Option<String>,
    run: Option<(usize, usize)>,
    export: Option<usize>,
    open: Option<PathBuf>,
    /// 有効/無効の切り替え要求 (プラグイン名, 有効か)
    toggle: Option<(String, bool)>,
    /// 設定値の変更要求 (プラグイン名, キー, 値)
    setting: Option<(String, String, String)>,
    /// パネルの手動更新要求 (プラグイン名, パネルID)
    panel_refresh: Option<(String, String)>,
}

// ── 端末分割の判断を純関数へ切り出す ────────────────────────────────
// UI から呼ぶ側 (`ZaivernApp`) は eframe の CreationContext 無しには作れない
// ため、テストできる形は「状態 → 状態」の純関数だけ。app.rs の作法どおり
// 判断はここへ出し、メソッド側は self との受け渡しに徹する。

/// 「既定のエージェント」= プリセットの**先頭**。
///
/// キーボードの `NewAgent` も端末分割も、新しい 1 体はここを見る
/// (別々の場所で添字を決めると「キーと分割で違うものが立つ」が起きる)。
const DEFAULT_PRESET_IX: usize = 0;

/// 新しいペインで起動するプリセットの添字。
///
/// * エージェント指定 → **既定プリセット** ([`DEFAULT_PRESET_IX`])。
///   `NewAgent` キーや `👾 Agent ＋` からの新規起動と同じ 1 体を起こす —
///   分割かどうかで起動するものを変えない (親のプリセットは引き継がない)。
/// * シェル指定 → 素のシェル (コマンドが空のプリセット) → 無ければ既定
///
/// 「1 つも登録が無い」ときだけ `None` (呼び出し側がトーストを出す)。
fn split_preset_index(
    agents: &[config::AgentPreset],
    preset: terminal::PanePreset,
) -> Option<usize> {
    if agents.is_empty() {
        return None;
    }
    match preset {
        terminal::PanePreset::NewAgent => Some(DEFAULT_PRESET_IX),
        terminal::PanePreset::Shell => agents
            .iter()
            .position(|p| p.command.trim().is_empty())
            .or(Some(DEFAULT_PRESET_IX)),
    }
}

/// 分割レイアウト表を整える (消えたペインを落とす → 1 枚は畳む → キーを揃える)。
///
/// キーは常に木の**先頭リーフ** — 先頭ペインを閉じてもタイルが迷子にならない。
fn normalize_split_map(
    splits: HashMap<u64, terminal::SplitLayout>,
    live: &HashSet<u64>,
) -> HashMap<u64, terminal::SplitLayout> {
    let mut next: HashMap<u64, terminal::SplitLayout> = HashMap::new();
    for (_, mut layout) in splits {
        layout.heal(&mut |id| live.contains(&id));
        // ペイン 1 枚 (または 0 枚) の分割は保持しない
        // = 分割していないタイルと完全に同じ描画経路へ戻す。
        if layout.len() < 2 {
            continue;
        }
        let key = layout.leaves()[0];
        next.insert(key, layout);
    }
    next
}

/// Cockpit のグリッドに**タイルとして**並べるセッションの添字。
/// 分割の子ペインは親タイルの中に描かれるので外す。
/// 分割が 1 つも無ければ `0..ids.len()` そのまま。
fn split_tile_indices(ids: &[u64], splits: &HashMap<u64, terminal::SplitLayout>) -> Vec<usize> {
    if splits.is_empty() {
        return (0..ids.len()).collect();
    }
    let mut child: HashSet<u64> = HashSet::new();
    for (tile, layout) in splits {
        for id in layout.leaves() {
            if id != *tile {
                child.insert(id);
            }
        }
    }
    (0..ids.len())
        .filter(|i| !child.contains(&ids[*i]))
        .collect()
}

/// 保存された分割行を実行時のレイアウト表へ戻す。
/// 引けなかったリーフ (復元されなかったセッション) は黙って落ちる。
fn split_map_from_lines(
    lines: &[String],
    id_of: &mut dyn FnMut(&str) -> Option<u64>,
) -> HashMap<u64, terminal::SplitLayout> {
    let mut out: HashMap<u64, terminal::SplitLayout> = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let rec = terminal::SplitLayoutRec::from_line(line);
        if rec.is_empty() {
            continue;
        }
        let layout = rec.to_layout(id_of);
        if layout.len() >= 2 {
            out.insert(layout.leaves()[0], layout);
        }
    }
    out
}

/// 端末分割の操作キーを 1 つだけ取り出し、そのキーイベントを**消費**する。
///
/// 端末より先に呼ぶこと。`terminal::split_key_action` に当たらないキー
/// (`Ctrl+C` / `Ctrl+D` / 素の文字など) は 1 つも触らないので、
/// 今までどおり PTY へ届く。
///
/// 押下と離上の両方を落とすのは、離上だけが端末へ流れて修飾キーの状態が
/// ちぐはぐになるのを避けるため。`Ctrl+Alt` は配列によっては AltGr なので、
/// **消費したキーと同じ 1 文字**の `Text` イベントだけ道連れにする
/// (それ以外の文字入力・IME は一切触らない)。
fn take_split_key(ctx: &egui::Context) -> Option<terminal::SplitAction> {
    let mac = cfg!(target_os = "macos");
    let mut found: Option<terminal::SplitAction> = None;
    let mut letter: Option<char> = None;
    ctx.input_mut(|i| {
        i.events.retain(|e| {
            if let egui::Event::Key {
                key,
                pressed,
                modifiers,
                ..
            } = e
            {
                if terminal::split_key_action(*key, modifiers, mac).is_some() {
                    if *pressed && found.is_none() {
                        found = terminal::split_key_action(*key, modifiers, mac);
                        let n = key.name();
                        if n.chars().count() == 1 {
                            letter = n.chars().next().map(|c| c.to_ascii_lowercase());
                        }
                    }
                    return false;
                }
            }
            true
        });
        if let Some(c) = letter {
            i.events.retain(|e| match e {
                egui::Event::Text(t) => {
                    !(t.chars().count() == 1
                        && t.chars().next().map(|x| x.to_ascii_lowercase()) == Some(c))
                }
                _ => true,
            });
        }
    });
    found
}

/// Cockpit で押されたボタン類。クロージャの中では記録だけして、
/// パネル描画後に self へ反映する (PluginActions と同じ流儀)。
/// 衝突の一覧に並べる最大行数 (残りは「他 N 件」に畳む)。
/// 警告は**目立つが邪魔にならない**のが方針なので、画面を占領させない。
const CONFLICT_ROWS_MAX: usize = 6;

#[derive(Default)]
struct CockpitActions {
    launch: Option<usize>,
    focus: Option<usize>,
    /// グリッドのセルを選んだら、そのセッションをアクティブ (紫枠) にする。
    /// Cockpit は開いたままにしたいので focus (下部パネルへ移動) とは別に持つ。
    select: Option<usize>,
    restart: Option<usize>,
    remove: Option<usize>,
    cycle: Option<usize>,
    cycle_all: bool,
    /// チェックポイント一覧を開く (Cockpit ヘッダから)。
    checkpoints: bool,
    broadcast: Option<String>,
    /// **止まっているエージェントだけ**へ送る本文。
    /// 作業中のものは巻き込まない (判定は `SessionState::is_stuck`)。
    broadcast_stalled: Option<String>,
    /// **1 体だけ**へ送る `(セッション ID, 本文)`。
    /// コンポーザで宛先を指名したとき。全員へは飛ばさない
    /// (「レビューのプロンプトが全エージェントへ漏れる」問題の元栓)。
    send_to: Option<(u64, String)>,
    voice: Option<u64>,
    voice_all: bool,
    voice_stop: bool,
    /// 監視役 LLM の変更は、借用の都合でいったんここへ退避して閉じた後に適用する。
    /// 指名は (コマンド, セッションタイトル)。両方空 = なし。
    super_pick: Option<(String, String)>,
    super_enabled: Option<bool>,
    /// 端末分割の操作 `(タイルのキー, 操作)`。キー入力とヘッダの ⊞ / ✕ が積む。
    /// セッションの起動・後始末を伴うので、描画クロージャの外で適用する。
    split: Vec<(u64, terminal::SplitAction)>,
}

/// キーバインド駆動のエディタ編集操作
enum EditOp {
    ToggleComment,
    Duplicate,
    Move(bool),
    /// 本文の改行コードを揃える (ステータスバー / パレット / 編集メニュー)。
    NormalizeEol(crate::textenc::LineEnding),
    /// 選択範囲の大文字小文字を変換する。
    Case(editor_ops::CaseKind),
    /// 選択範囲の行を並べ替える (true = 降順)。
    Sort(bool),
    /// 選択範囲の重複行を削る。
    Dedupe,
    /// 選択範囲 (無選択なら本文全体) を JSON として整形する。
    FormatJson,
    /// 本文のインデントを別の様式へ変換する (ステータスバーから)。
    ConvertIndent(editor_ops::IndentStyle),
}

/// LSP サーバーのキー: (言語ID, ルート)。ルート毎に 1 プロセス起動する
/// (理由とトレードオフは `lsp::LspClient::spawn` のコメント参照)。
type LspKey = (String, PathBuf);

/// ファイル索引の 1 エントリ (マルチルート対応)。
///
/// 曖昧さ回避のため **絶対パスを正**として持つ。`rel` は所属ルートからの
/// 相対パスで、あいまい検索のマッチ品質を単一ルート時と同じに保つために使う。
/// `label` は表示用で、複数ルートに同じ `rel` が存在するときだけ
/// `<ルート名>/<rel>` の形にする (良いエディタと同じ挙動)。
#[derive(Clone)]
struct IndexedFile {
    abs: PathBuf,
    rel: String,
    label: String,
}

/// ファイル索引の走査条件 (すべて設定から来る — マジックナンバーを持たない)。
#[derive(Clone)]
struct IndexOptions {
    max_files: usize,
    max_depth: usize,
    respect_gitignore: bool,
}

impl IndexOptions {
    fn from_config(cfg: &config::Config) -> Self {
        Self {
            max_files: cfg.index_max_files,
            max_depth: cfg.index_max_depth,
            respect_gitignore: cfg.respect_gitignore,
        }
    }
}

/// 索引の走査結果。**打ち切りを黙って隠さない** — `truncated` を UI へ出す。
struct IndexOutcome {
    files: Vec<IndexedFile>,
    /// 上限 (`index_max_files`) に達して途中で止めたか。
    truncated: bool,
}

/// ⌘P で「最近開いたファイル」を索引の残りより上へ持ち上げる加点。
///
/// fuzzy の素点 (数十〜数百) は必ず超えるが、`palette` の一致段
/// (TIER_SUBSTR = 30_000) には届かない値にしてある = **入力を始めたら
/// 一致の質が最近順より必ず優先される**。
const RECENT_FILE_BONUS: i32 = 5_000;
/// 最近順 1 件ぶんの目減り。`recent::MAX_RECENT` (12) 件でも 0 にならない。
const RECENT_FILE_STEP: i32 = 100;

/// ⌘P (ファイル検索) の候補を組み立てる純粋関数。
///
/// * `recent` — 最近開いたファイルの絶対パス文字列 (先頭が直近。`recent.rs`)
/// * `active` — いま開いているファイル。**加点しない** ので、クエリが空なら
///   先頭に来るのは「直前に開いていたファイル」= ⌘P → Enter で戻れる
/// * `query`  — `ファイル名:123[:45]` を含みうる生のクエリ
fn file_mode_items(
    index: &[IndexedFile],
    recent: &[String],
    active: Option<&Path>,
    query: &str,
) -> Vec<Item> {
    let (name_q, goto) = split_path_goto(query);
    let pq = fuzzy::PreparedQuery::new(name_q.trim());
    // 実在確認 (`MenuState::files()`) はここでは通さない — パレットは毎フレーム
    // 組み直されるので、12 回の stat を毎フレーム撃たない。
    let rank: HashMap<&Path, usize> = recent
        .iter()
        .enumerate()
        .map(|(i, s)| (Path::new(s.as_str()), i))
        .collect();
    let mut out: Vec<Item> = Vec::with_capacity(index.len().min(256));
    for f in index {
        // マッチはルート相対パスに対して行い、単一ルート時と同じあいまい検索の
        // 品質を保つ。表示 (detail) は曖昧回避済みラベル、開くのは絶対パス。
        let Some(score) = pq.score(&f.rel) else {
            continue;
        };
        let name = f.rel.rsplit('/').next().unwrap_or(&f.rel).to_string();
        let bonus = match rank.get(f.abs.as_path()) {
            Some(_) if active == Some(f.abs.as_path()) => 0,
            Some(i) => RECENT_FILE_BONUS - (*i as i32) * RECENT_FILE_STEP,
            None => 0,
        };
        let (detail, action) = match goto {
            Some((line, col)) => (
                trf(
                    "{label} : {line} 行目",
                    &[("label", f.label.clone()), ("line", (line + 1).to_string())],
                ),
                Action::OpenFileAt(f.abs.clone(), line, col),
            ),
            None => (f.label.clone(), Action::OpenFile(f.abs.clone())),
        };
        out.push(Item {
            icon: file_tree::icon_for(&name).to_string(),
            label: name,
            detail,
            action,
            score: score.saturating_add(bonus),
        });
    }
    out
}

/// `foo.rs:12:3` を (名前部分, 0 起点の (行, 桁)) に割る。
///
/// 行指定として読めない `:` (Windows のドライブレター `C:\`、`foo:bar`) は
/// 名前側に残す。判定は `editor_ops::parse_goto` に委ねる — 数値の解釈を
/// 2 箇所に書かないため。左から最初に「残りが行指定として読める」`:` で割る。
fn split_path_goto(q: &str) -> (&str, Option<(usize, usize)>) {
    for (i, _) in q.match_indices(':') {
        if i == 0 {
            continue; // 先頭の `:` は行ジャンプモード側の役目
        }
        if let Some(go) = editor_ops::parse_goto(&q[i + 1..]) {
            return (&q[..i], Some(go));
        }
    }
    (q, None)
}

// ── ネイティブファイルダイアログのジョブ化 ────────────────────────────
//
// rfd の同期 API を UI スレッド (= eframe の `update` の中) で呼ぶと、winit の
// イベントループの**内側**で OS のモーダルメッセージループが回りはじめる。
// Windows ではこれが親ウィンドウを持たないモーダルループになるため、
//   * ダイアログが開いている間 eframe のウィンドウが一切再描画されない
//     (真っ白 / 描きかけのまま固まる)
//   * エージェント (PTY) のリーダースレッドが撃ち続ける `request_repaint` が
//     再入で wndproc へ届き、egui 内部の RefCell が二重借用で panic しうる
// という形で「エージェントを消す/ファイルを開くと画面が崩れて固まる」になる。
// エージェントが多いほど repaint の圧が上がるので再現しやすい。
//
// 対策はダイアログを UI スレッドから追い出すこと。ワーカースレッドで開いて
// 結果をチャネルで受け取り、次のフレームで適用する。UI スレッドは 1 フレームも
// 止まらないので、ダイアログ中もエージェントの出力が流れ続ける。

/// ダイアログの開き方。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DialogMode {
    PickFile,
    PickFolder,
    SaveFile,
}

/// ダイアログの組み立て材料。
///
/// ワーカースレッドへ送るので `Send` な素材だけを持つ (`rfd::FileDialog` 自体は
/// 親ウィンドウハンドルを抱えうるので、組み立ては向こう側でやる)。
#[derive(Clone, Debug, PartialEq, Eq)]
struct DialogSpec {
    mode: DialogMode,
    /// 最初に表示するディレクトリ (None なら OS 既定)
    directory: Option<PathBuf>,
    /// 拡張子フィルタ: (表示名, 拡張子の並び)
    filter: Option<(String, Vec<String>)>,
}

impl DialogSpec {
    fn new(mode: DialogMode) -> Self {
        Self {
            mode,
            directory: None,
            filter: None,
        }
    }
    fn pick_file() -> Self {
        Self::new(DialogMode::PickFile)
    }
    fn pick_folder() -> Self {
        Self::new(DialogMode::PickFolder)
    }
    fn save_file() -> Self {
        Self::new(DialogMode::SaveFile)
    }
    fn directory(mut self, dir: impl Into<PathBuf>) -> Self {
        self.directory = Some(dir.into());
        self
    }
    fn filter(mut self, name: impl Into<String>, exts: &[&str]) -> Self {
        self.filter = Some((name.into(), exts.iter().map(|e| (*e).to_string()).collect()));
        self
    }
}

/// 実際にネイティブダイアログを開く。**呼んだスレッドをブロックする**。
fn run_file_dialog(spec: &DialogSpec) -> Option<PathBuf> {
    let mut d = rfd::FileDialog::new();
    if let Some(dir) = &spec.directory {
        d = d.set_directory(dir);
    }
    if let Some((name, exts)) = &spec.filter {
        d = d.add_filter(name.clone(), exts);
    }
    match spec.mode {
        DialogMode::PickFile => d.pick_file(),
        DialogMode::PickFolder => d.pick_folder(),
        DialogMode::SaveFile => d.save_file(),
    }
}

/// ダイアログの用途を表すキー。**同じ用途の二重オープンを防ぐ**ために使う。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum DialogKind {
    OpenFile,
    NewWindowFolder,
    OpenFolder,
    AddFolder,
    PetImage,
    InstallPlugin,
    SaveAs,
}

/// ダイアログの用途 + 結果を適用するのに必要な情報。
#[derive(Clone, PartialEq, Eq, Debug)]
enum DialogPurpose {
    /// ファイルを開く (メニュー: ファイル > 開く)
    OpenFile,
    /// 新しいウィンドウでフォルダを開く
    NewWindowFolder,
    /// ワークスペースのフォルダを開き直す
    OpenFolder,
    /// ワークスペースへフォルダを追加する
    AddFolder,
    /// ペット画像を選ぶ
    PetImage,
    /// プラグイン (.zvplug / .zip) を選んで入れる
    InstallPlugin,
    /// 名前を付けて保存。
    ///
    /// ダイアログが返るまでにタブの並びは変わりうるので、添字ではなく
    /// **バッファ ID** で対象を指す。`close_after` / `run_hooks` は
    /// 保存が終わったあとの追加動作 (呼び出し元が同期時にやることの控え)。
    SaveAs {
        buffer_id: u64,
        close_after: bool,
        run_hooks: bool,
    },
}

impl DialogPurpose {
    fn kind(&self) -> DialogKind {
        match self {
            DialogPurpose::OpenFile => DialogKind::OpenFile,
            DialogPurpose::NewWindowFolder => DialogKind::NewWindowFolder,
            DialogPurpose::OpenFolder => DialogKind::OpenFolder,
            DialogPurpose::AddFolder => DialogKind::AddFolder,
            DialogPurpose::PetImage => DialogKind::PetImage,
            DialogPurpose::InstallPlugin => DialogKind::InstallPlugin,
            DialogPurpose::SaveAs { .. } => DialogKind::SaveAs,
        }
    }
}

/// ワーカースレッドから返る結果。`path` が None ならキャンセル。
#[derive(Clone, PartialEq, Eq, Debug)]
struct DialogOutcome {
    purpose: DialogPurpose,
    path: Option<PathBuf>,
}

/// 実行中のダイアログジョブ。
///
/// 状態遷移: `begin` (受理) → 実行中 (同じ用途の `begin` は None) →
/// `poll` で結果を取り出す → 待ちが解けて idle へ戻る。
/// キャンセルも「`path` が None の結果」として同じ道を通るので、
/// 待ちが解けたまま何も起きない = 従来どおりの挙動になる。
struct DialogJobs {
    in_flight: HashSet<DialogKind>,
    tx: mpsc::Sender<DialogOutcome>,
    rx: mpsc::Receiver<DialogOutcome>,
}

impl DialogJobs {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            in_flight: HashSet::new(),
            tx,
            rx,
        }
    }

    /// 受理したら結果送信用の口を返す。同じ用途が実行中なら None (無視する)。
    fn begin(&mut self, kind: DialogKind) -> Option<mpsc::Sender<DialogOutcome>> {
        if !self.in_flight.insert(kind) {
            return None;
        }
        Some(self.tx.clone())
    }

    /// 届いた結果を 1 件取り出し、その用途を待ち状態から外す。
    fn poll(&mut self) -> Option<DialogOutcome> {
        let out = self.rx.try_recv().ok()?;
        self.in_flight.remove(&out.purpose.kind());
        Some(out)
    }

    /// 何かのダイアログが開いているか (開いている間は少し速く回す)。
    fn busy(&self) -> bool {
        !self.in_flight.is_empty()
    }
}

impl Default for DialogJobs {
    fn default() -> Self {
        Self::new()
    }
}

// ── フレームガード (update の panic ポリシー) ──────────────────────────
//
// `update` 中の panic をアプリごと落とさず握り潰すのは正しい。ただし
// 「1 フレーム完走したらカウンタを 0 に戻す」だけだと、**たまに成功する**
// panic (panic → ok → panic → ok …) を永久に描き続けてしまう。画面は
// 半分だけ組み立てられた状態で固まり、クラッシュもしない
// = 利用者から見た「画面が崩れて動かなくなる」。
//
// そこで時間窓で panic の頻度を見て、収まらないなら段階的に手を打つ:
// 継続 → (犯人の部分ビューを隔離 + 画面に警告) → それでも駄目なら従来どおり中止。

/// 1 フレームの結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FrameOutcome {
    Ok,
    Panic,
}

/// フレームガードの判断。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FrameGuardAction {
    /// そのまま継続する (このフレームを捨てるだけ)
    Continue,
    /// 壊れている部分ビューを描画から外し、画面に警告を出す
    Quarantine,
    /// 手に負えない — 従来どおり落とす (最後の手段)
    Abort,
}

/// panic の頻度を見る時間窓 (ミリ秒)。
const FRAME_PANIC_WINDOW_MS: u64 = 10_000;
/// 時間窓の中でこの回数に達したら隔離へ上げる。
const FRAME_PANIC_QUARANTINE_AT: usize = 3;
/// 隔離をこの回数繰り返しても収まらなければ諦める。
const FRAME_PANIC_MAX_QUARANTINES: u32 = 3;
/// **連続**でこの回数 panic したら即座に諦める (従来からの挙動)。
const FRAME_PANIC_ABORT_STREAK: u32 = 3;
/// これだけ連続で完走したら健全とみなし、隔離回数の記憶も捨てる。
const FRAME_CLEAN_STREAK_RESET: u32 = 300;

/// panic の頻度から次の一手を決める状態機械。
///
/// 時刻を引数で受け取るだけの純粋な部品なので、テストから決定的に叩ける
/// (`frame_panic_policy_*` を参照)。
#[derive(Debug, Default, Clone)]
struct FramePanicPolicy {
    /// 時間窓に残っている panic の発生時刻 (ms)
    recent: Vec<u64>,
    /// 連続 panic 回数 (1 フレーム完走で 0)
    streak: u32,
    /// 連続で完走したフレーム数
    clean: u32,
    /// これまでに出した隔離指示の回数
    quarantines: u32,
}

impl FramePanicPolicy {
    /// 1 フレーム分の結果を記録して、次の一手を返す。
    fn record(&mut self, outcome: FrameOutcome, now_ms: u64) -> FrameGuardAction {
        // 時間窓から出た panic は忘れる (= カウンタの減衰)。
        // 「完走したら即 0」ではないので、ちらつく panic も取りこぼさない。
        let floor = now_ms.saturating_sub(FRAME_PANIC_WINDOW_MS);
        self.recent.retain(|t| *t >= floor);
        match outcome {
            FrameOutcome::Ok => {
                self.streak = 0;
                self.clean = self.clean.saturating_add(1);
                // 十分に落ち着いたら完全に忘れる (何時間も動かしたときに
                // 無関係な panic が積み上がって落ちるのを防ぐ)
                if self.recent.is_empty() && self.clean >= FRAME_CLEAN_STREAK_RESET {
                    self.quarantines = 0;
                }
                FrameGuardAction::Continue
            }
            FrameOutcome::Panic => {
                self.clean = 0;
                self.streak = self.streak.saturating_add(1);
                self.recent.push(now_ms);
                if self.streak >= FRAME_PANIC_ABORT_STREAK {
                    return FrameGuardAction::Abort;
                }
                if self.recent.len() >= FRAME_PANIC_QUARANTINE_AT {
                    // 隔離するので、効いたかどうかを測り直す
                    self.recent.clear();
                    self.quarantines = self.quarantines.saturating_add(1);
                    if self.quarantines > FRAME_PANIC_MAX_QUARANTINES {
                        return FrameGuardAction::Abort;
                    }
                    return FrameGuardAction::Quarantine;
                }
                FrameGuardAction::Continue
            }
        }
    }
}

/// panic の犯人を指す単位 (隔離できる粒度)。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Subview {
    /// Cockpit のエージェントタイル (セッション ID)
    Session(u64),
    /// 名前付きの領域 (エディタ・サイドバー等)
    Panel(&'static str),
}

impl Subview {
    /// 利用者へ見せる名前。
    fn label(&self) -> String {
        match self {
            Subview::Session(id) => trf("エージェント #{id} の画面", &[("id", id.to_string())]),
            Subview::Panel("editor") => tr("エディタ"),
            Subview::Panel("cockpit") => tr("コックピット"),
            Subview::Panel("deck") => tr("エージェントデッキ"),
            Subview::Panel("sidebar") => tr("サイドバー"),
            Subview::Panel("terminal") => tr("ターミナルパネル"),
            Subview::Panel(other) => (*other).to_string(),
        }
    }
}

thread_local! {
    /// いま描いている部分ビュー。panic すると後片付けが飛ばされて印が残るので、
    /// `catch_unwind` の側から「どこが壊れたか」を読み取れる。**UI スレッド専用**。
    static DRAWING_SUBVIEW: std::cell::RefCell<Option<Subview>> =
        const { std::cell::RefCell::new(None) };
}

/// 部分ビューに印を付けて描く。panic したら印を残したままにする (犯人の特定用)。
///
/// 入れ子にできる: 内側が無事に終われば外側の印へ戻すので、タイルを描き終えた
/// あとにコックピット側で panic しても「コックピット」として拾える。
fn draw_subview<R>(sv: Subview, f: impl FnOnce() -> R) -> R {
    let prev = DRAWING_SUBVIEW.with(|c| c.borrow_mut().replace(sv));
    let out = f();
    DRAWING_SUBVIEW.with(|c| *c.borrow_mut() = prev);
    out
}

/// 残っている印を取り出して消す。
fn take_drawing_subview() -> Option<Subview> {
    DRAWING_SUBVIEW.with(|c| c.borrow_mut().take())
}

/// panic のペイロードから人が読めるメッセージを取り出す。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    tr("原因不明の内部エラー")
}

/// panic のメッセージからモジュール名を拾って犯人を推測する。
///
/// 印 (`DRAWING_SUBVIEW`) が取れなかったときの保険。`src/terminal.rs:...` の
/// ような位置情報がメッセージに混ざっていれば、そこから領域を割り出す。
fn subview_from_panic_message(msg: &str) -> Option<Subview> {
    /// モジュール名 → 隔離できる領域
    const MAP: &[(&str, &str)] = &[
        ("terminal", "terminal"),
        ("editor", "editor"),
        ("file_tree", "sidebar"),
        ("git_panel", "sidebar"),
        ("agents", "cockpit"),
    ];
    for (module, panel) in MAP {
        // 位置情報の形 (`src/foo.rs` / `foo.rs:12:3`) のときだけ採る。
        // ただのメッセージ中の単語で誤爆しないようにするため。
        if msg.contains(&format!("src/{module}.rs")) || msg.contains(&format!("{module}.rs:")) {
            return Some(Subview::Panel(panel));
        }
    }
    None
}

/// フレームガードの状態: 頻度ポリシー + 隔離中の領域 + 画面に出す警告。
#[derive(Debug)]
struct FrameGuard {
    policy: FramePanicPolicy,
    /// 描画から外している部分ビュー
    quarantined: HashSet<Subview>,
    /// 画面上部に出す警告 (None なら出さない)
    banner: Option<String>,
    /// 単調時計の原点 (テストしやすいよう ms を引数で回すため)
    epoch: Instant,
}

impl Default for FrameGuard {
    fn default() -> Self {
        Self {
            policy: FramePanicPolicy::default(),
            quarantined: HashSet::new(),
            banner: None,
            epoch: Instant::now(),
        }
    }
}

impl FrameGuard {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// 1 フレームの結果を食わせ、判断を返す。隔離なら犯人を隔離リストへ入れる。
    fn observe(
        &mut self,
        outcome: FrameOutcome,
        culprit: Option<Subview>,
        now_ms: u64,
    ) -> FrameGuardAction {
        let act = self.policy.record(outcome, now_ms);
        if act == FrameGuardAction::Quarantine {
            if let Some(sv) = culprit {
                self.quarantined.insert(sv);
            }
        }
        act
    }

    fn is_quarantined(&self, sv: &Subview) -> bool {
        self.quarantined.contains(sv)
    }

    /// 利用者が「再試行」を押したとき: 隔離を解いて頻度の記憶もまっさらにする。
    fn reset(&mut self) {
        self.policy = FramePanicPolicy::default();
        self.quarantined.clear();
        self.banner = None;
    }

    /// **1 つの部分ビューだけ**隔離を解く (プレースホルダの「再試行」)。
    ///
    /// 頻度の記憶も同時に捨てる。残したままだと「再試行 → 1 回 panic →
    /// 即また隔離」となり、押しても何も直らないように見えるため。
    /// 最後の 1 つを解いたらバナーも消す (もう隔離は残っていない)。
    fn unquarantine(&mut self, sv: &Subview) {
        self.quarantined.remove(sv);
        self.policy.streak = 0;
        self.policy.recent.clear();
        self.policy.clean = 0;
        if self.quarantined.is_empty() {
            self.banner = None;
        }
    }

    /// 消えたセッションの隔離指定を捨てる。
    ///
    /// ID が再利用されたとき、新しいタイルがいきなり隔離状態 (= 黒いまま)
    /// で現れるのを防ぐ。`alive` に無い `Subview::Session` だけを外し、
    /// パネルの隔離 (`Subview::Panel`) はそのまま残す。
    fn forget_sessions(&mut self, alive: &HashSet<u64>) {
        self.quarantined.retain(|sv| match sv {
            Subview::Session(id) => alive.contains(id),
            Subview::Panel(_) => true,
        });
        if self.quarantined.is_empty() {
            self.banner = None;
        }
    }
}

// ===========================================================================
// 折りたたみ表示 (code folding) の純関数層
//
// 本文エディタは `egui::TextEdit` が `Buffer::text` を直接書き換える作りなので、
// 「畳んだ行を描かない」を実現するには **TextEdit に渡す文字列そのもの**から
// 隠す行を落とすしかない。galley だけ間引くと egui のキャレット添字 (CCursor)
// が本文とずれ、選択・貼り付け・削除がまとめて壊れる。
//
// そこで
//   1. 原文から隠す行を取り除いた「表示テキスト」を作り  ([`build_fold_view`])
//   2. TextEdit にはそれを編集させ
//   3. 変わったら共通接頭辞 / 接尾辞で差分区間を求め、原文の対応区間へ
//      差し戻す                                            ([`splice_fold_edit`])
// という往復にする。表示 → 原文の添字変換は「取り除いた区間の長さを足す」
// だけの単調写像 ([`fold_display_to_source`]) なので、キャレットも選択も
// 表示テキスト側で完結し、egui 側の状態に手を入れる必要がない。
// ===========================================================================

/// 補完ポップアップに一度に並べる最大行数 (超えた分はスクロールで出す)。
const MAX_COMPLETION_ROWS: usize = 60;
/// 補完ポップアップの見た目 (幅 / 高さ)。
const COMPLETION_POPUP_W: f32 = 520.0;
const COMPLETION_POPUP_H: f32 = 240.0;
/// ホバーをマウス位置からどれだけ下にずらすか。
const HOVER_OFFSET_Y: f32 = 18.0;
const HOVER_POPUP_W: f32 = 560.0;
const HOVER_POPUP_H: f32 = 320.0;
/// クイックフィックス (コードアクション) ポップアップの見た目。
/// 幅は補完より狭く固定する — タイトルは `lsp::one_line_label` で先に
/// 切り詰めてあるので、どの幅でも 1 行が見切れない。
const ACTION_POPUP_W: f32 = 460.0;
const ACTION_POPUP_H: f32 = 260.0;
/// シグネチャ (引数ヒント) ポップアップの幅。
const SIGNATURE_POPUP_W: f32 = 560.0;
/// シグネチャの説明文をこの文字数で 1 行に畳む。
const SIGNATURE_DOC_MAX: usize = 120;
/// 参照 / シンボル一覧の小窓の大きさ。
const REF_WINDOW_W: f32 = 420.0;
const REF_WINDOW_H: f32 = 360.0;
/// シンボル一覧に並べる最大件数。
const MAX_SYMBOL_ROWS: usize = 300;
/// リネーム入力を画面上端からどれだけ下に置くか。
const RENAME_WINDOW_Y: f32 = 120.0;
/// コミットメッセージ入力窓の幅。
const GIT_COMMIT_WINDOW_W: f32 = 460.0;
/// パレットの「コミット履歴」に読み込む件数の上限。
const GIT_HISTORY_MAX: usize = 200;

/// パレットから撃つ git 操作。`git_panel.rs` は commit / push を
/// スコープ外にしているので、実行はここが受け持つ (あちらは触らない)。
#[derive(Clone)]
enum GitJob {
    Commit { message: String, all: bool },
    Push,
    Pull,
}

impl GitJob {
    /// 画面に出す名前 (トーストの主語)。
    fn label(&self) -> String {
        match self {
            GitJob::Commit { all: false, .. } => tr("コミット"),
            GitJob::Commit { all: true, .. } => tr("すべてコミット"),
            GitJob::Push => tr("push"),
            GitJob::Pull => tr("pull"),
        }
    }

    /// `git -C <repo>` に続けて渡す引数。
    fn args(&self) -> Vec<String> {
        match self {
            GitJob::Commit { message, all } => {
                // **引数表は `git_panel::commit_args` 1 本へ寄せる。**
                // ここで別に組み立てていたときは `--cleanup=whitespace` が
                // 抜けており、`#` で始まるコミットメッセージがユーザーの
                // `commit.cleanup` 設定で**黙って落ちていた**
                // (同じ操作なのに Git パネル経由では残る、という食い違い)。
                // `--` は付けない (パスではなくメッセージなので値で確定する)。
                let mut a = crate::git_panel::commit_args(message, false);
                if *all {
                    a.insert(1, "--all".into());
                }
                a
            }
            // 追跡ブランチが無い初回でも通るように upstream を張る。
            GitJob::Push => vec![
                "push".into(),
                "--porcelain".into(),
                "--set-upstream".into(),
                "origin".into(),
                "HEAD".into(),
            ],
            // 履歴を勝手に書き換えない (merge も rebase もしない) 安全側。
            GitJob::Pull => vec!["pull".into(), "--ff-only".into()],
        }
    }
}

/// パレットから撃つ git 操作の走行状態と小窓。
#[derive(Default)]
struct GitOps {
    /// 走行中のジョブ (同時に 1 つだけ)
    job: Option<mpsc::Receiver<(String, bool)>>,
    /// 走行中ジョブの表示名
    job_label: String,
    commit_open: bool,
    commit_msg: String,
    commit_all: bool,
    commit_focus: bool,
    history_open: bool,
    history_busy: bool,
    /// (短い SHA, 「件名 — 著者 · 相対日時」)
    history: Vec<(String, String)>,
    history_rx: Option<mpsc::Receiver<Vec<(String, String)>>>,
    history_query: String,
}

/// エラー本文の先頭 `n` 行だけを取り出す (トーストを縦に伸ばさない)。
fn first_lines(s: &str, n: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    for l in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
        out.push(l);
        if out.len() >= n {
            break;
        }
    }
    out.join(" / ")
}
/// スティッキーヘッダの最大段数 (VS Code の既定と同じ 3 段)。
const STICKY_MAX_ROWS: usize = 3;
/// ガターの右端に確保する、折りたたみ記号 ▸ / ▾ の桁幅。
const FOLD_MARKER_W: f32 = 12.0;
/// テーブル表示で 1 列に見せる最大文字数 (長いセルは畳んで横幅を守る)。
const TABLE_CELL_CHARS: usize = 40;
/// 書庫一覧で見せるエントリ名の最大文字数 (超えたら省略しホバーで全文)。
const ARCHIVE_NAME_CHARS: usize = 72;

/// 折りたたみ表示の 1 バッファぶんのキャッシュ。
///
/// 表示テキストを毎フレーム作り直さないための控え。`prev` は差し戻しの
/// 基準 (直前フレームの表示テキスト) で、編集を検出したらキャッシュごと
/// 捨てて次フレームに作り直す。
struct FoldView {
    /// どのバッファのものか (`Buffer::id`)
    buf: u64,
    /// 原文ハッシュ + 畳んだ行集合から作る鍵。ずれたら作り直す。
    key: u64,
    /// TextEdit に渡す表示テキスト
    text: String,
    /// 差分区間を求めるための、編集前の表示テキスト
    prev: String,
    /// 表示行 i に対応する原文の行番号 (0 始まり)
    lines: Vec<usize>,
    /// 原文から取り除いた文字区間 (char 単位・昇順・非重複)
    cut: Vec<(usize, usize)>,
}

/// 原文と「隠す行の区間 (両端含む・0 始まり)」から表示テキストを作る。
///
/// 戻り値は `(表示テキスト, 表示行→原文行, 取り除いた char 区間)`。
/// 隠す区間が本文の末尾まで届くときは直前の改行ごと落とし、余分な空行が
/// 残らないようにする。
fn build_fold_view(
    src: &str,
    hidden: &[(usize, usize)],
) -> (String, Vec<usize>, Vec<(usize, usize)>) {
    // 行頭の char オフセット表
    let mut starts: Vec<usize> = vec![0];
    let mut total = 0usize;
    for ch in src.chars() {
        total += 1;
        if ch == '\n' {
            starts.push(total);
        }
    }
    let line_count = starts.len();

    let mut cut: Vec<(usize, usize)> = Vec::with_capacity(hidden.len());
    for &(a, b) in hidden {
        if a >= line_count {
            continue;
        }
        let b = b.min(line_count - 1);
        if b < a {
            continue;
        }
        let (cs, ce) = if b + 1 < line_count {
            (starts[a], starts[b + 1])
        } else {
            (starts[a].saturating_sub(1), total)
        };
        if ce > cs {
            cut.push((cs, ce));
        }
    }
    cut.sort_unstable();
    // 重なりを併合 (hidden_spans は併合済みだが、外から来ても壊れないように)
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(cut.len());
    for (s, e) in cut {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }

    let mut text = String::with_capacity(src.len());
    let mut lines: Vec<usize> = Vec::with_capacity(line_count);
    let mut ci = 0usize;
    let mut cut_i = 0usize;
    let mut line = 0usize;
    let mut at_line_start = true;
    for ch in src.chars() {
        while cut_i < merged.len() && ci >= merged[cut_i].1 {
            cut_i += 1;
        }
        let cutting = cut_i < merged.len() && ci >= merged[cut_i].0;
        if !cutting {
            if at_line_start {
                lines.push(line);
                at_line_start = false;
            }
            text.push(ch);
        }
        if ch == '\n' {
            line += 1;
            at_line_start = true;
        }
        ci += 1;
    }
    // 末尾が改行で終わっているときだけ「最後の空の表示行」を数える。
    // 末尾まで畳んだ場合は表示テキストが改行で終わらないので、ここで
    // 数えてしまうと存在しない行がガターに生えてしまう。
    if at_line_start && (text.is_empty() || text.ends_with('\n')) {
        lines.push(line.min(line_count.saturating_sub(1)));
    }
    (text, lines, merged)
}

/// 表示テキストの char 添字 → 原文の char 添字。
///
/// 取り除いた区間を通過するたびにその長さを足すだけの単調写像。
fn fold_display_to_source(cut: &[(usize, usize)], d: usize) -> usize {
    let mut s = d;
    for &(cs, ce) in cut {
        if s >= cs {
            s += ce - cs;
        } else {
            break;
        }
    }
    s
}

/// 原文の char 添字 → 表示テキストの char 添字。
///
/// [`fold_display_to_source`] の逆写像。隠れている位置は、その折りたたみの
/// 直前の可視位置へ丸める (畳んだ中へキャレットを置かないため)。
fn fold_source_to_display(cut: &[(usize, usize)], s: usize) -> usize {
    let mut d = s;
    for &(cs, ce) in cut {
        if s >= ce {
            d -= ce - cs;
        } else if s > cs {
            d -= s - cs;
            break;
        } else {
            break;
        }
    }
    d
}

/// 表示テキストの編集から「編集が始まった原文の行」と「増減した行数」を返す。
///
/// `FoldState::shift_lines` / `Bookmarks::shift_lines` にそのまま渡すための値。
fn fold_edit_shift(
    src: &str,
    next: &str,
    cut: &[(usize, usize)],
    old: &str,
    new: &str,
) -> (usize, isize) {
    let o: Vec<char> = old.chars().collect();
    let n: Vec<char> = new.chars().collect();
    let mut p = 0usize;
    while p < o.len() && p < n.len() && o[p] == n[p] {
        p += 1;
    }
    let a = fold_display_to_source(cut, p);
    let at = src.chars().take(a).filter(|c| *c == '\n').count();
    let delta = next.split('\n').count() as isize - src.split('\n').count() as isize;
    (at, delta)
}

/// 表示テキストへの編集を原文へ差し戻す。
///
/// `old` / `new` の共通接頭辞・接尾辞の外側だけを「変わった区間」とみなし、
/// [`fold_display_to_source`] で原文の区間へ写して置き換える。
/// 畳んだ行を跨いで選択して打ち込んだときは、その行も一緒に消える
/// (VS Code の折りたたみと同じ挙動)。
fn splice_fold_edit(src: &str, cut: &[(usize, usize)], old: &str, new: &str) -> String {
    let o: Vec<char> = old.chars().collect();
    let n: Vec<char> = new.chars().collect();
    let mut p = 0usize;
    while p < o.len() && p < n.len() && o[p] == n[p] {
        p += 1;
    }
    let mut suf = 0usize;
    while suf < o.len() - p && suf < n.len() - p && o[o.len() - 1 - suf] == n[n.len() - 1 - suf] {
        suf += 1;
    }
    let sc: Vec<char> = src.chars().collect();
    let a = fold_display_to_source(cut, p).min(sc.len());
    let b = fold_display_to_source(cut, o.len() - suf).min(sc.len());
    let b = b.max(a);
    let mut out = String::with_capacity(src.len() + new.len());
    out.extend(sc[..a].iter());
    out.extend(n[p..n.len() - suf].iter());
    out.extend(sc[b..].iter());
    out
}

/// 本文 galley の視覚行から「表示行の先頭になっている視覚行」を拾う。
///
/// 入力は視覚行ごとの「その行が改行で終わるか」。戻り値は
/// `(視覚行の添字, その行が何番目の表示行か)` の並び。折り返し ON では
/// 1 つの表示行が複数の視覚行になるので、行番号やガターの印は
/// **先頭の視覚行だけ**に出す必要がある。
fn row_line_starts(ends_with_newline: &[bool]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(ends_with_newline.len());
    let mut line = 0usize;
    let mut at_start = true;
    for (i, nl) in ends_with_newline.iter().enumerate() {
        if at_start {
            out.push((i, line));
        }
        if *nl {
            line += 1;
            at_start = true;
        } else {
            at_start = false;
        }
    }
    out
}

/// ガターのクリック位置から、折りたたみを開閉すべき原文行を求める。
///
/// `rows` は `(原文行, y の上端, y の下端)`。`fold_x` より右を押したときだけ
/// 反応する (左はブックマークの列)。折りたためない行は無視する。
fn fold_click_line(
    rows: &[(usize, f32, f32)],
    marks: &HashMap<usize, bool>,
    fold_x: f32,
    p: egui::Pos2,
) -> Option<usize> {
    if p.x < fold_x {
        return None;
    }
    rows.iter()
        .find(|(src, y0, y1)| marks.contains_key(src) && p.y >= *y0 && p.y < *y1)
        .map(|(src, ..)| *src)
}

// ─── タブのドラッグ並べ替え (純関数) ──────────────────────────────────
//
// 並べ替えの「掴んでいたものを指し続ける」という約束は、デッキの
// `DeckAction::Reorder` (`reorder_agent`) と同じ。違うのは移動の形だけで、
// デッキの ⌥↑/⌥↓ は隣どうしの swap、ドラッグは remove + insert になる。

/// ドラッグ中のタブの落とし先を求める**純関数**。
///
/// `tab_rects` は画面に並んだ順のタブ矩形、`pointer_x` はポインタの x、
/// `dragging` は掴んでいるタブの添字。戻りは**並べ替え後の添字**で、
/// 動かす必要が無ければ `None`。
///
/// 判定は「ポインタより中心が左にある**自分以外の**タブの数」。
/// 端より外へ出しても数は 0 / `len-1` で頭打ちになるので、
/// **戻り値が範囲外になることはない**。
fn reorder_target(tab_rects: &[egui::Rect], pointer_x: f32, dragging: usize) -> Option<usize> {
    let n = tab_rects.len();
    if n < 2 || dragging >= n {
        return None; // タブ 0/1 枚、または壊れた添字
    }
    let mut target = 0usize;
    for (i, r) in tab_rects.iter().enumerate() {
        if i == dragging {
            continue;
        }
        if r.center().x < pointer_x {
            target += 1;
        }
    }
    let target = target.min(n - 1);
    if target == dragging {
        None
    } else {
        Some(target)
    }
}

/// `from` のタブを `to` へ動かしたとき、アクティブタブが**同じタブを
/// 指し続ける**ための新しい添字を返す**純関数**。
///
/// 掴んだタブがアクティブなら落とし先へ、間に挟まれたタブは 1 つずれる。
fn reorder_active(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        return to;
    }
    if from < to && active > from && active <= to {
        active - 1
    } else if from > to && active >= to && active < from {
        active + 1
    } else {
        active
    }
}

/// 挿入位置インジケータを描く x 座標。落とし先より手前なら左端、
/// 後ろなら右端に線を引く。矩形が足りなければ `None` (描かない)。
fn reorder_marker_x(tab_rects: &[egui::Rect], from: usize, to: usize) -> Option<f32> {
    let r = tab_rects.get(to)?;
    Some(if to <= from { r.left() } else { r.right() })
}

/// ブックマーク系コマンドの効果。戻り値はジャンプ先の行 (0 始まり)。
///
/// 切替 / 全解除は `None` を返す (その場から動かない)。次 / 前は端で
/// 折り返す ([`crate::editor::Bookmarks`] の契約どおり)。
fn bookmark_cmd_target(
    cmd: &Cmd,
    marks: &mut crate::editor::Bookmarks,
    line: usize,
) -> Option<usize> {
    match cmd {
        Cmd::ToggleBookmark => {
            marks.toggle(line);
            None
        }
        Cmd::NextBookmark => marks.next_after(line),
        Cmd::PrevBookmark => marks.prev_before(line),
        Cmd::ClearBookmarks => {
            marks.clear_all();
            None
        }
        _ => None,
    }
}

/// テーブル表示コマンドで何をすべきか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableToggle {
    /// 表として読み直す
    Build,
    /// 素のテキストへ戻す
    Drop,
    /// CSV / TSV ではないので何もしない
    NotTable,
}

/// テーブル表示の切替判定。表示中なら必ず解除できる (拡張子を問わない)。
fn table_toggle_decision(is_table_path: bool, showing_table: bool) -> TableToggle {
    if showing_table {
        TableToggle::Drop
    } else if is_table_path {
        TableToggle::Build
    } else {
        TableToggle::NotTable
    }
}

/// 巨大ファイルの帯に並べる「効いている制限」の一覧。
///
/// editor.rs は意図的に旗しか返さないので、文言はここで組み立てる。
fn large_file_reasons(read_only: bool, highlight_off: bool) -> Vec<String> {
    let mut v = Vec::new();
    if read_only {
        v.push(tr("読み取り専用"));
    }
    if highlight_off {
        v.push(tr("強調表示と折りたたみを停止"));
    }
    v
}

/// 本文エディタが編集する対象。
///
/// egui 0.29 の `TextEdit` に「読み取り専用」は無いが、`TextBuffer` を
/// `is_mutable() == false` で実装すると **選択とコピーはできるまま編集だけ
/// 止まる**。巨大ファイル / 差分タブと、折りたたみ中の一時テキストを、
/// 同じ `TextEdit` 呼び出しで扱うための薄い包み。
enum EditTarget<'a> {
    /// 折りたたみ表示テキストへの編集 (原文ではないので履歴は取らない。
    /// 原文への差し戻しは `splice_fold_edit` の側で 1 段として積む)
    Rw(&'a mut String),
    /// 原文への通常編集。**`TextEdit` が入れた差分をその場で履歴へ積む**。
    ///
    /// フレーム終端で本文全体を突き合わせるのではなく、`TextBuffer` の
    /// 挿入 / 削除をそのまま拾う。本文のコピーを 1 本も持たずに済み、
    /// CJK / 絵文字でもバイト境界を割らない (位置は egui から char で来る)。
    Rec {
        text: &'a mut String,
        hist: &'a mut editor::History,
        ed: editor::Edit,
    },
    /// 読み取り専用 (巨大ファイル・差分タブ)
    Ro(&'a str),
}

impl EditTarget<'_> {
    fn set(&mut self, s: String) {
        match self {
            EditTarget::Rw(t) => **t = s,
            EditTarget::Rec { text, hist, ed } => {
                if let Some((at, at_chars, before, after)) = editor::diff_replace(text, &s) {
                    **text = s;
                    hist.record(at, at_chars, before, after, *ed);
                }
            }
            EditTarget::Ro(_) => {}
        }
    }
}

impl egui::TextBuffer for EditTarget<'_> {
    fn is_mutable(&self) -> bool {
        matches!(self, EditTarget::Rw(_) | EditTarget::Rec { .. })
    }
    fn as_str(&self) -> &str {
        match self {
            EditTarget::Rw(t) => t.as_str(),
            EditTarget::Rec { text, .. } => text.as_str(),
            EditTarget::Ro(t) => t,
        }
    }
    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        match self {
            EditTarget::Rw(t) => <String as egui::TextBuffer>::insert_text(t, text, char_index),
            EditTarget::Rec { text: t, hist, ed } => {
                let at = editor_ops::char_to_byte(t, char_index);
                let n = <String as egui::TextBuffer>::insert_text(*t, text, char_index);
                hist.record(at, char_index, String::new(), text.to_string(), *ed);
                n
            }
            EditTarget::Ro(_) => 0,
        }
    }
    fn delete_char_range(&mut self, char_range: std::ops::Range<usize>) {
        match self {
            EditTarget::Rw(t) => {
                <String as egui::TextBuffer>::delete_char_range(t, char_range);
            }
            EditTarget::Rec { text: t, hist, ed } => {
                let (cs, ce) = (char_range.start, char_range.end);
                let s = editor_ops::char_to_byte(t, cs);
                let e = editor_ops::char_to_byte(t, ce);
                let removed = t[s..e].to_string();
                <String as egui::TextBuffer>::delete_char_range(*t, char_range);
                hist.record(s, cs, removed, String::new(), *ed);
            }
            EditTarget::Ro(_) => {}
        }
    }
}

/// リネーム (LSP textDocument/rename) の進行状態。
///
/// prepareRename → 名前の入力 → rename → WorkspaceEdit の適用、の 4 段。
struct RenameFlow {
    key: LspKey,
    path: PathBuf,
    pos: lsp::Position,
    /// prepareRename の応答待ち
    preparing: bool,
    /// 名前の入力欄を出しているか
    open: bool,
    name: String,
    focus: bool,
    /// rename の応答待ち
    applying: bool,
}

/// LSP の CompletionItemKind → 一覧に出す短い種別ラベル。
///
/// 絵文字は環境によって豆腐 (□) になるので、フォント非依存の英字で出す。
fn completion_kind_label(kind: u8) -> &'static str {
    match kind {
        2 | 3 => "fn",
        4 => "new",
        5 | 10 => "fld",
        6 => "var",
        7 | 22 => "type",
        8 => "trait",
        9 => "mod",
        12 | 21 => "const",
        13 | 20 => "enum",
        14 => "kw",
        15 => "snip",
        17 => "file",
        19 => "dir",
        24 => "op",
        25 => "T",
        _ => "·",
    }
}

/// LSP の SymbolKind → 一覧に出す短い種別ラベル。
fn symbol_kind_label(kind: u8) -> &'static str {
    match kind {
        1 => "file",
        2 | 3 => "mod",
        4 => "pkg",
        5 => "class",
        6 => "method",
        7 => "prop",
        8 => "field",
        9 => "new",
        10 => "enum",
        11 => "trait",
        12 => "fn",
        13 => "var",
        14 => "const",
        23 => "struct",
        26 => "T",
        _ => "·",
    }
}

/// シンボル木を「深さつきの並び」へ平らにする (quick-open 風の一覧用)。
fn flatten_symbols(
    nodes: &[lsp::SymbolNode],
    depth: usize,
    out: &mut Vec<(usize, String, u8, lsp::Position)>,
) {
    for n in nodes {
        out.push((depth, n.name.clone(), n.kind, n.selection_range.start));
        flatten_symbols(&n.children, depth + 1, out);
    }
}

/// ブレッドクラムの 1 セグメント。押されたら true。
fn breadcrumb_seg(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    kind: &breadcrumb::SegKind,
) -> bool {
    let color = match kind {
        breadcrumb::SegKind::File(_) => theme.text,
        _ => theme.text_dim,
    };
    let r = ui.add(
        egui::Label::new(RichText::new(label).size(12.0).color(color))
            .selectable(false)
            .sense(egui::Sense::click()),
    );
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let hint = match kind {
        breadcrumb::SegKind::Folder(p) => trf(
            "{p} をエクスプローラーで開く",
            &[("p", p.display().to_string())],
        ),
        breadcrumb::SegKind::File(_) => tr("ファイルパレットを開く"),
        breadcrumb::SegKind::Symbol { .. } => tr("この定義へジャンプ"),
    };
    r.on_hover_text(hint).clicked()
}

/// `.vscode/tasks.json` を読み直す間隔。
///
/// これより短い間隔では**ディスクを触らない**。メニューとコマンドパレットは
/// 毎フレーム組み直されるので、この歯止めが無いと 60fps でファイルを開くことになる。
const TASKS_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// tasks.json の走査結果キャッシュ。
#[derive(Default)]
struct TasksCache {
    /// 走査したワークスペースルート。ここが変わったら TTL を待たずに読み直す。
    root: Option<PathBuf>,
    /// 最後にディスクを読んだ時刻。`None` = 一度も読んでいない。
    read_at: Option<std::time::Instant>,
    doc: tasks::TasksDoc,
}

/// キーバインド編集 UI の状態。
///
/// **記録中かどうかは 1 か所 (`recording`) だけが持つ。** bool を複数持つと
/// 「記録中なのに通常のショートカットも走る」が構造的に起こり得るため。
#[derive(Default)]
struct KeybindUi {
    /// 絞り込みの検索語 (あいまい検索は `fuzzy` を使う)
    query: String,
    /// 打鍵の記録中の行。`Some` の間は通常のショートカット消費を止める。
    recording: Option<crate::keybinds::Recorder>,
}

/// 設定画面の状態。値そのものは `Config` が持つので、ここには
/// **画面だけの状態** (検索語・絞り込み・入力途中の文字列) しか置かない。
#[derive(Default)]
struct SettingsUi {
    /// 絞り込みの検索語。`@modified` と書いても既定と違うものだけになる。
    query: String,
    /// 「既定から変えたものだけ」のチェック。
    only_modified: bool,
    /// 文字列欄の編集途中の値 (キー → 入力中の文字列)。
    /// 確定するまで `Config` へ入れないので、打つたびに config.toml を
    /// 書きに行くことがない (1 文字ごとの I/O を撃たない)。
    drafts: HashMap<String, String>,
}

/// 復元した本文とディスクが食い違っている 1 件。
///
/// **選ばせるまで消さない** — 黙ってどちらかを採ると、片方の変更が
/// 何の跡も残さずに消える。
struct HotExitConflict {
    /// 対象のファイル (競合が起きるのは名前付きのバッファだけ)。
    path: PathBuf,
    title: String,
    /// 退避しておいた未保存の本文。
    text: String,
    /// いまのディスクの本文 (読めたときだけ)。
    disk_text: Option<String>,
    state: session::DiskState,
}

pub struct ZaivernApp {
    cfg: Config,
    theme: Theme,
    /// ワークスペースのルート一覧。**常に 1 件以上**。`roots[0]` が primary。
    roots: Vec<PathBuf>,
    /// 「いま作業しているフォルダ」。これ以降に起動するエージェント / ターミナルの
    /// 作業ディレクトリになる (`agent_cwd`)。`None` なら primary ルート。
    ///
    /// 起動引数のフォルダ (`zai .`) を初期値に、フォルダを開く / ワークスペースへ
    /// 追加する / そのフォルダのファイルを開く、のたびに追随する。
    /// **既に走っているセッションは動かさない** — 起動済みプロセスの cwd は
    /// 変えられないので、追随するのは「それ以降の起動」だけ。
    agent_root: Option<PathBuf>,
    tree: FileTree,
    editor: Editor,
    /// エディタの分割 (VS Code の editor group 相当)。
    ///
    /// **バッファの実体は `editor.buffers` だけ** — ここが持つのはタブの並び
    /// (バッファ ID) とペインごとのビュー状態 (スクロール・カーソル)。
    /// だから同じファイルを 2 ペインで開いても本文は 1 つで、片方の編集は
    /// 必ずもう片方にも出る。分割木そのものは端末と同じ
    /// [`terminal::SplitLayout`] を流用している (`editor_split` の頭注参照)。
    panes: editor_split::EditorPanes,
    /// **いま描いている**エディタペイン。TextEdit / ScrollArea の egui ID に
    /// 混ぜて、同じバッファを 2 ペインに出してもカーソルとスクロールが
    /// 混ざらないようにする ([`buf_edit_id`])。描画外では
    /// フォーカス中ペインを指す。
    cur_pane: editor_split::PaneId,
    /// `.vscode/tasks.json` の走査キャッシュ。
    ///
    /// メニューとコマンドパレットは毎フレーム組み直されるので、素直に読むと
    /// 1 フレームごとにディスク I/O が走る (設計原則 3: アイドル時のコストは 0)。
    /// 短い TTL とルート一致でしか読み直さない。
    tasks_cache: TasksCache,
    agents: AgentManager,
    /// Cockpit のタイル 1 枚ぶんの端末分割レイアウト。
    ///
    /// キーは**タイルの先頭ペイン**のセッション ID。ペインが 1 枚に戻った
    /// レイアウトはここから消す — 「分割していないタイル」は今日と 1 px も
    /// 変わらない経路 (`terminal::draw` 直呼び) を通したいため。
    /// 正規化はすべて [`ZaivernApp::normalize_splits`] が行う。
    splits: HashMap<u64, terminal::SplitLayout>,
    /// 各タイルを最後に描いた矩形。方向フォーカス (`focus_dir`) は幾何で
    /// 隣を選ぶので、描画後に適用されるキー操作にも矩形が要る。
    /// 描画時に書き、`splits` から消えたタイルは一緒に落とす。
    split_rect: HashMap<u64, egui::Rect>,
    palette: Palette,
    /// `#` パレット用の git worktree キャッシュ (branch, path, 追加済みか)。
    /// パレットを開いている間だけ保持し、閉じると破棄する
    /// (palette_items は毎フレーム呼ばれるため、都度 git を叩かない)。
    palette_worktrees: Option<Vec<(String, PathBuf, bool)>>,
    /// **エージェントへの指示の唯一の出口。**
    ///
    /// 以前は送信地点ごとに `format!("{text}\r")` を PTY へ 1 回で書いていたが、
    /// Ink 系 TUI (Claude Code / Codex / Gemini) は本文と CR が同じ write で
    /// 届くとまとめてペーストと判定し、**CR を改行として飲んで実行しない**。
    /// 「送ったのにエージェントが入力欄で待機している」の原因がこれ。
    /// 本文 → 待つ → 確定キー → 効いたか確認、の手順は `submit.rs` が持つ。
    outbox: Vec<submit::Pending>,
    /// 🏁 プロンプトレース (1 プロンプトを複数エージェントに並走させる) の
    /// ダッシュボード状態。描画・git 操作の実体は race.rs。
    race: race::RacePanel,
    /// worktree 隔離で起動したエージェント (セッション ID → 割り当てた worktree)。
    ///
    /// セッションの `cwd` にも worktree のフォルダは入っているが、
    /// 「どのリポジトリの worktree か / どのブランチか」はここにしか無い。
    /// 破棄時の `git worktree remove` と、セッション保存 / 復元がこれを見る。
    agent_worktrees: HashMap<u64, worktree::AgentWorktree>,
    /// **手で名前を付けたセッション** (セッション ID)。自動命名はここに
    /// 載っている相手を絶対に上書きしない (手動が常に勝つ)。
    manual_titles: std::collections::HashSet<u64>,
    /// エージェントタブのリネーム入力 (セッション ID, 入力中の文字列)。
    /// `None` = 開いていない (窓も 1px も描かない)。
    rename_agent: Option<(u64, String)>,
    /// 自動命名のターン境界検出。`auto_name_sessions` が false のときは
    /// 1 度も触らない (アイドル時のコストはゼロ)。
    turns: crate::agents::naming::TurnWatcher,
    /// 自動命名の実行係 (要求ごとに 1 スレッド、結果はチャネル)。
    namer: crate::agents::naming::Namer,
    /// セッション ID → 既に命名に使った指示文のハッシュ。
    /// 同じ指示のまま次のターンが終わっても**もう一度は走らせない**。
    named_for: HashMap<u64, u64>,
    /// 同じ作業ツリーに同居しているエージェント同士のファイル衝突の見張り。
    /// 同居が 0 なら git を 1 回も叩かない (アイドル時のコストはゼロ)。
    conflicts: worktree::ConflictWatch,
    /// 衝突バッジの詳細 (どのファイルを誰が取り合っているか) を開いているか。
    /// **既定は閉じ**。画面が勝手に開かないよう、明示的に押されたときだけ広がる。
    conflict_detail: bool,
    /// 🛰 衝突レーダー — **worktree で隔離した** エージェント同士の
    /// マージ衝突を、マージする前に見つける。`ConflictWatch` (同居のみ) の裏側。
    /// 見張る対象が 2 本未満なら git を 1 回も起こさない。
    conflict_radar: conflict::ConflictRadar,
    /// レーダーの窓が開いているか。**既定は閉じ**。
    radar_open: bool,
    /// レーダーで選んでいるワークツリーの組 (行列のマス)。`None` = 全件。
    radar_pair: Option<(usize, usize)>,
    /// 全エージェント一括停止の確認モーダルが出ているか (破壊的操作)。
    pending_stop_all: bool,
    /// 閉じたエージェントに割り当てられていた worktree の後始末待ち。
    /// `(worktree, 未コミット変更が残っているか)`。**確認なしには消さない**。
    pending_worktree: Option<(worktree::AgentWorktree, bool)>,
    /// 構文ハイライタ。プロセスで 1 つの共有インスタンス
    /// (`SyntaxSet` は数 MB あるので差分ビューと二重に持たない)。
    highlighter: &'static Highlighter,
    /// 巨大ファイルの可視域ハイライトが「正しい文脈まで追い付いたか」。
    /// 鍵は本文の `TextEdit` の ID (= **ペインとバッファの組**)、値は
    /// `(可視域と本文を表す鍵, 追い付いたか)`。
    ///
    /// `false → true` に変わった**その 1 回だけ** galley を捨てて塗り直す。
    /// 毎フレーム捨てると組み直し (実測 495ms) が毎フレーム乗る。
    ///
    /// **1 枠で持たない**のは、分割中は 1 フレームの中で `code_editor_ui` が
    /// ペインの数だけ走るため。1 枠だと 2 つのペインが毎フレーム上書きし合い、
    /// どちらも「変化した」と誤検出して galley を組み直し続ける。
    hl_ready: HashMap<egui::Id, (u64, bool)>,
    /// 直前に組んだ galley が**可視域で塗り分けられた**か
    /// (= `Highlighter::layout_job_visible` の窓が効いたか)。鍵は [`Self::hl_ready`] と同じ。
    ///
    /// 効いていない文書 (小さいファイル) で可視域を galley キーへ混ぜると、
    /// 512 行スクロールするたびに全文の galley を組み直すことになるので、
    /// **効くと分かってから**混ぜる。
    hl_windowed: HashMap<egui::Id, bool>,
    cockpit: bool,
    /// Cockpit グリッドで最後に「見える位置まで運んだ」セッション。
    ///
    /// タイルが 1 画面に収まらない (= スクロールしている) ときだけ使う。
    /// アクティブが変わったフレームにだけ追従させるための記録で、毎フレーム
    /// 運ぶとユーザーが自分でスクロールできなくなる。
    cockpit_followed: Option<u64>,
    /// **このフレームで描く中央ビュー** (毎フレーム [`center_view`] で畳む)。
    /// 描画の分岐はフラグではなく必ずこれを見る。
    center: CenterView,
    /// フリート看板 (全エージェントを状態列で俯瞰・指揮するカンバン画面)。
    /// Cockpit と同格の中央画面モードで、両方 true にはしない (切替時に他方を落とす)。
    kanban: bool,
    /// 看板画面の UI 状態 (ブロードキャスト/指示の入力バッファ等)
    kanban_state: kanban::KanbanState,
    /// エージェントデッキ (縦 1 本でエージェントを管理する画面)。
    /// Cockpit / 看板と同格の中央画面モードで、3 つ同時には出さない。
    deck: bool,
    /// デッキ画面の UI 状態 (選択・レイアウト・絞り込み・追跡)
    deck_state: deck::DeckState,
    /// デッキの副題に出す「作業ディレクトリ → git ブランチ」。
    /// 値は (ブランチ名, 取得時刻)。空文字 = repo ではない (再問い合わせは TTL 後)。
    deck_branches: HashMap<PathBuf, (String, Instant)>,
    /// いまバックグラウンドで問い合わせ中の作業ディレクトリ (二重起動よけ)
    deck_branch_pending: HashSet<PathBuf>,
    deck_branch_tx: mpsc::Sender<(PathBuf, String)>,
    deck_branch_rx: mpsc::Receiver<(PathBuf, String)>,
    /// Markdown/HTML ファイルをレンダリング表示するモード (Cockpit の編集ペインでも有効)
    md_preview: bool,
    /// プレビューが参照するローカル画像のテクスチャキャッシュ
    md_images: markdown::ImageCache,
    /// プレビュー用の変換結果キャッシュ (バッファ id, テキストハッシュ, 変換後 Markdown)
    md_pre_cache: Option<(u64, u64, String)>,
    /// 画像ビューアの明示ズーム (バッファ id → 倍率)。エントリ無し = フィット表示
    img_zoom: HashMap<u64, f32>,
    /// 透過画像用の市松模様テクスチャ ((色ペア) が変わったら作り直す)
    checker_tex: Option<((egui::Color32, egui::Color32), egui::TextureHandle)>,
    sidebar_open: bool,
    sidebar_tab: SidebarTab,
    /// 「セッション」タブ (フォルダごとの過去の会話) の表示状態 + 走査キャッシュ。
    /// 走査はこの中でバックグラウンドスレッドへ逃がされる。
    sidebar_sessions: session_picker::SidebarState,
    /// セッションタブに出すフォルダ一覧 (= いま開いているワークスペースのルート)。
    /// `sidebar_folders` は `is_dir()` を叩くので毎フレームは作り直さず、
    /// 元になるルートが変わったときだけ作り直す。
    sess_folders: Vec<PathBuf>,
    /// `sess_folders` を作った元 (ルート)。変化検知用。
    sess_folders_src: Vec<PathBuf>,
    /// ツールバーのブランチボタン (一覧・切り替え。git は全て裏で回す)。
    branch_nav: git::BranchNav,
    file_index: Vec<IndexedFile>,
    index_at: Option<Instant>,
    /// バックグラウンド索引の受け口 (世代付き)。`Some` = 走査中。
    index_rx: Option<mpsc::Receiver<(u64, IndexOutcome)>>,
    /// 走査済み件数 (索引スレッドが書き、UI が読むだけ)。
    index_progress: Arc<AtomicUsize>,
    /// 索引が上限で打ち切られたか (⌘P に必ず出す — 黙って切らない)。
    index_truncated: bool,
    /// 索引ジョブの世代。ルートが変わった後に届いた古い結果を捨てる。
    index_gen: u64,
    /// カスタムテーマ (~/.zaivern/themes + プラグイン同梱): (表示名, JSONフルパス)
    custom_themes: Vec<(String, String)>,
    find: FindState,
    /// メニューバー付随の永続状態 (最近使った項目・自動保存フラグ)
    menu_state: recent::MenuState,
    /// 自動保存 (afterDelay) の直近実行時刻
    autosave_at: Option<Instant>,
    /// ファイル所有ガードを張ってあるワークスペース。**起動直後もここが
    /// `None` なので、最初のフレームで初めて張られる。**
    lease_armed_for: Option<PathBuf>,
    /// ファイル所有ガードが有効になったことを 1 度だけ知らせたか。
    /// **毎フレーム知らせない**ため (UI 原則: 画面が突然変わらない)。
    lease_armed_notified: bool,
    /// 行/列へ移動ダイアログ (VS Code: ⌃G)
    goto_open: bool,
    goto_input: String,
    /// 問題 (LSP 診断) パネル (VS Code: ⇧⌘M)
    problems_open: bool,
    /// 問題パネルの絞り込み (severity トグル + テキスト)
    problems_filter: ProblemsFilter,
    /// 問題パネルで畳んでいるファイル
    problems_collapsed: HashSet<PathBuf>,
    /// キーバインド編集 UI (⌘K ⌘S) / バージョン情報ダイアログ
    shortcuts_open: bool,
    /// キーバインド編集 UI の状態 (検索語・記録中の行)。
    keybind_ui: KeybindUi,
    /// 設定画面 (検索できる一覧) を開いているか。
    settings_open: bool,
    /// 設定画面の状態 (検索語・@modified・文字列欄の編集途中)。
    settings_ui: SettingsUi,
    /// ベンダーフック設置操作の直近の結果メッセージ (設定画面に 1 行で出す)。
    hooks_log: String,
    /// Hot Exit: 未保存本文の退避帳。
    hotexit: session::HotExitStore,
    /// 退避を書き出す予定時刻。**変更があったときだけ入る** —
    /// 何も編集していないフレームでは触らない (アイドル時のコストはゼロ)。
    hotexit_due: Option<Instant>,
    /// 前フレームの未保存バッファの指紋。安く変化を見つけるためだけの値で、
    /// 実際に何を書くかは [`session::HotExitStore::sync`] が厳密に決める。
    hotexit_fingerprint: u64,
    /// 復元時にディスク側と食い違っていたバッファ (選ばせるまで消さない)。
    hotexit_conflicts: Vec<HotExitConflict>,
    /// 上限超過をもう伝えたバッファのタイトル。退避は数秒ごとに走るので、
    /// これが無いと同じ警告を延々と出し続ける (伝えるのは 1 回でいい)。
    hotexit_warned: HashSet<String>,
    /// chord (2 打鍵) の待機。フレームを跨ぐので `App` が持つ。
    chord: crate::keybinds::ChordState,
    /// which-key ポップアップ (chord の続きの一覧) の表示状態。
    whichkey: crate::whichkey::WhichKey,
    /// which-key に出す実データ行の実体 `(絶対パス, repo 相対パス, 状態)`。
    ///
    /// **1 フレームに 1 回だけ作り、打鍵経路と描画経路で同じものを見る。**
    /// 都度作り直すと、git のスキャンがフレームの途中で着地したときに
    /// 「画面の 3 番」と「押した 3 番」が別のファイルを指し得る。
    /// prefix を握っていないフレームでは空 (アイドルのコストはゼロ)。
    whichkey_live: Vec<(PathBuf, String, crate::git::FileStatus)>,
    about_open: bool,
    /// **What's New** に出す変更点。空でない間だけウィンドウを描く
    /// (bool を別に持つと「開いているのに中身が空」が構造的に起こり得る)。
    whats_new: Vec<crate::whats_new::Release>,
    /// ライセンス (Pro) の状態ダイアログを開いているか。
    license_open: bool,
    /// ダイアログの貼り付け欄。保存済みキーとは別に持つ (貼り直しを中断できる)。
    license_input: String,
    /// 保存済みの生キー。画面には [`license::mask_key`] を通してしか出さない。
    license_key: Option<String>,
    /// 起動時と適用時にだけ計算する検証結果。毎フレーム署名検証はしない。
    license_status: license::LicenseStatus,
    /// 疑似フルスクリーン (枠なし最大化) 中なら復帰用の元ジオメトリ (outer 左上, inner サイズ)。
    /// macOS のネイティブ全画面は縦オフセット配置のサブディスプレイでウィンドウが
    /// モニタより大きく作られ、描画と当たり判定がずれて UI 全体が効かなくなる
    /// (winit 0.30 の実測バグ)。その環境ではこちらの方式で全画面相当にする。
    fake_fullscreen: Option<(egui::Pos2, egui::Vec2)>,
    /// ネイティブ全画面が壊れると実測されたモニタサイズ (セッション内学習)。
    /// 以後の全画面切替は最初から疑似フルスクリーンを使う。
    broken_native_fs: Vec<egui::Vec2>,
    /// 壊れたネイティブ全画面から脱出中 (解除完了を待って疑似フルスクリーンへ入る)。
    fs_rescue_pending: bool,
    /// 救出開始時点の壊れた inner_rect。解除アニメーションが終わって矩形が
    /// ここから変化したことを「解除完了」の合図にする。
    fs_rescue_from: Option<egui::Rect>,
    /// 救出 (Fullscreen(false) 送信) を開始した時刻。長時間変化が無ければ
    /// 解除コマンドが取りこぼされたと判断して諦める。
    fs_rescue_at: Option<Instant>,
    /// 直前フレームで観測した inner_rect (矩形の「真の安定」検出用)。
    fs_last_rect: Option<egui::Rect>,
    /// inner_rect が最後に動いた時刻。「now との差」が安定継続時間になる。
    /// 全画面の遷移アニメーション中は毎フレーム更新され続ける。
    fs_rect_moved_at: Option<Instant>,
    /// 疑似フルスクリーン復帰の後半 (枠と位置の復元) の予約。
    /// zoom: (Maximized) と setStyleMask: (Decorations) を同一ターンで
    /// 送らないための分割 (遷移中の styleMask は AppKit が NSException)。
    fake_fs_restore: Option<(egui::Pos2, egui::Vec2, Instant)>,
    /// ネイティブ全画面の出入りを最後に指示した時刻 (連打クールダウン)。
    fs_toggle_at: Option<Instant>,
    /// 全画面ジオメトリ不一致を最初に観測した時刻 (遷移アニメ中の揺れと区別するため
    /// 0.5 秒持続してから壊れていると確定する)。
    fs_broken_since: Option<Instant>,
    /// ファイル横断検索 (サイドバーの検索タブ)
    gsearch: GlobalSearchState,
    /// ナビゲーション履歴 (パス, カーソル char)。戻る/進む用
    nav_history: Vec<(PathBuf, usize)>,
    nav_index: usize,
    /// 次フレーム冒頭でエディタへ注入する egui イベント
    /// (メニューの 元に戻す/切り取り/貼り付け などの実体)
    pending_editor_events: Vec<egui::Event>,
    /// 定義ジャンプ (F12) の応答待ち先 LSP
    awaiting_definition: Option<LspKey>,
    toasts: Vec<Toast>,
    pending_close: Option<usize>,
    /// ファイルツリーからの削除確認待ち(対象の集合と、ゴミ箱を通すかどうか)。
    /// `None` = 確認ダイアログを出さない。
    pending_delete: Option<DeleteRequest>,
    /// 確認待ちの移動/コピー。**確認を通った項目しか実行されない**。
    pending_transfer: Option<TransferQueue>,
    /// ファイル操作の取り消し履歴。
    ///
    /// **エディタ本文の取り消し (`editor::History`) とは完全に別**で、
    /// 積むのはツリー由来のリネーム/移動/新規作成/ゴミ箱行きだけ。
    /// 完全削除は戻せないので**積まない**。
    file_history: FileHistory,
    pending_select: Option<(usize, usize)>,
    pending_scroll: Option<f32>,
    /// 取り消し履歴の「連続入力」判定に使う単調時計の原点。
    /// `SystemTime` ではなく `Instant` — 時刻がずれても粒度が壊れない。
    undo_clock: Instant,
    last_row_h: f32,
    /// エディタ可視領域の高さ(前フレーム値)。PageUp/Down・検索ジャンプで使用
    last_view_h: f32,
    /// 今フレームのズームジェスチャの持ち主 (前フレームで確定した値)。
    /// ⌘+ホイール / ピンチを「ファイル単位 / 画像ビューア / 画面全体」の
    /// どこへ流すかの振り分けに使う。1 フレーム遅れだが、ポインタが
    /// 1 フレームで領域を跨ぐことは無い。
    zoom_area: Option<(egui::Rect, ZoomArea)>,
    /// 描画中に申告される次フレーム用の値。フレーム末尾で `zoom_area` へ移す。
    zoom_area_next: Option<(egui::Rect, ZoomArea)>,
    /// ホイール / ピンチの連続的な倍率変化を段送りへ均す蓄積器。
    /// 対象 (画面全体 / ファイル) が変わったら貯まりを捨てる。
    zoom_wheel: zoom::WheelAccum,
    /// 直前のホイールズームがファイル単位だったか (対象切替の検出用)。
    zoom_wheel_on_file: bool,
    /// エディタの垂直スクロール量(前フレーム値)
    last_scroll_y: f32,
    /// アクティブバッファ本文のハッシュ (code_editor_ui が line_marks 用に毎フレーム更新)
    last_text_hash: u64,
    /// バッファ内検索のヒット一覧 (検索バー / ミニマップの印 / 本文のハイライトが共有)。
    /// 本文か検索条件が変わったときだけ走査し直す。
    find_hits: Option<FindHitCache>,
    /// マルチバッファのタブごとのカーソル行 (`Buffer::id` → `rows` の添字)。
    ///
    /// `Buffer` に持たせないのは、これが**中身ではなく表示状態**だから
    /// (スクロール位置と同じ扱い。タブを閉じれば意味を失う)。
    /// 出所ごとにタブは 1 枚しか作らないので、実質 3 件までしか増えない。
    multibuffer_cursor: HashMap<u64, usize>,
    /// ブレッドクラム用に documentSymbol を投げた記録: (パス, 本文ハッシュ, 時刻)。
    /// 同じ内容へ二重に投げないためのデバウンス。
    breadcrumb_symbols_asked: Option<(PathBuf, u64, Instant)>,
    /// スマホリモートサーバ (起動失敗時は None + remote_err)
    remote: Option<remote::RemoteServer>,
    remote_err: Option<String>,
    remote_open: bool,
    /// Windows の受信許可 (これが無いとスマホからは繋がらない)。
    /// 他 OS では常に「確認不要」なので何も表示しない。
    fw: firewall::FirewallUi,
    qr_tex: Option<egui::TextureHandle>,
    /// `qr_tex` の元になった URL。変わったら作り直す
    /// (LAN ⇄ SSH トンネルで URL が入れ替わるため)
    qr_url: String,
    /// SSH リバーストンネル — スマホが同じ Wi-Fi にいなくても繋ぐ経路
    tunnel: tunnel::Tunnel,
    /// トンネルの接続先入力欄 (`user@host[:port]`)。鍵は一切保持しない
    tunnel_host: String,
    /// 接続先の書式エラー / 待ち受けの張り替え失敗 (1 行)
    tunnel_err: Option<String>,
    /// Tailscale の検出結果 (踏み台も Wi-Fi も要らない 3 本目の経路)。
    /// スレッドを持たず、📱 の画面が描かれたときだけ測り直す薄いキャッシュ。
    ts: tailscale::Probe,
    /// Cockpit のコンポーザ (複数行・宛先つき)。宛先ごとの下書きもここが持つ。
    agent_input_buf: crate::agent_input::AgentInputBuffer,
    /// `@` コンテキスト参照 (mention.rs)。添付台帳と裏の走査を持つ。
    mention: mention::Mention,
    /// `@` ピッカーへ渡す相対パス一覧 (索引が届いたときだけ作り直す)。
    mention_rels: Vec<String>,
    /// `@` ピッカーへ渡すシンボル (LSP の documentSymbol が届いたら作り直す)。
    mention_syms: Vec<mention::SymbolHit>,
    /// プラン使用量の監視 (集約・枯渇予測)。読み取りはこの中で TTL 付きの
    /// バックグラウンドスレッドへ逃がされるので、毎フレーム触ってよい。
    quota: coordinator::QuotaWatch,
    /// 使用量の詳細ウィンドウ (ステータスバーの表示をクリック / パレット)
    quota_open: bool,
    /// ステータスバーのトークン/コスト表示をエージェント別まで開くか。
    /// **既定はコンパクト (合算 1 個)**。消費ゼロならどちらでも 1px も出さない。
    token_detail: bool,
    /// コスト上限の判定結果 (最も深刻な 1 件)。**上限が未設定なら常に None**
    /// で、そのときステータスバーには 1px も出ない。
    cost_alert: Option<coordinator::quota::BudgetStatus>,
    /// 上の判定をやり直した材料 (取り込み回数, 上限の設定)。
    /// **これが変わったときだけ**推定コストを計算し直す (設計原則 3)。
    cost_stamp: Option<(u64, coordinator::quota::CostLimits)>,
    /// 最後に数えた推定コスト (このセッションぶん, 今日 (UTC) ぶん)。
    cost_spent: (f64, f64),
    /// コスト上限の通知を「段が変わった瞬間 1 度だけ」にする門番。
    cost_gate: notify::EdgeGate,
    /// レート制限時のアカウント自動フェイルオーバー。**既定は無効**。
    /// 段 (検知→候補選定→切替→再開→検証) と履歴をここが持つ。
    failover: failover::Failover,
    /// 保存時に行末の空白を落とす (`config.trim_trailing_whitespace` が種。
    /// セッション中の切替は egui memory へ覚える)
    save_trim_trailing: bool,
    /// 保存時に末尾の余分な空行を落とす (`config.trim_final_newlines` が種)
    save_trim_final_newlines: bool,
    /// 保存時に最終行へ改行を入れる (`config.insert_final_newline` が種)
    save_final_newline: bool,
    /// egui memory から永続設定を読み終えたか (最初のフレームで 1 度だけ)
    prefs_loaded: bool,
    /// ステータスバー用の改行コード判定キャッシュ
    /// (バッファ id, 本文バイト長, 判定時刻, 判定結果)。
    /// 判定は本文全走査なので、同じ長さのまま短時間に何度も数え直さない。
    le_cache: Option<(u64, usize, Instant, crate::textenc::LineEnding)>,
    gitinfo: git::GitSet,
    /// ガターの git blame (既定 OFF)。ON の間だけ可視ブロックを非同期で取る。
    /// OFF ならワーカーもキャッシュも持たない = アイドルコストはゼロ。
    blame: git::Blame,
    /// blame からクリックで開いたコミット差分タブのパース結果
    /// (バッファ id → ファイル差分)。毎フレーム parse_unified を回さないため。
    commit_diff_cache: HashMap<u64, Vec<crate::diff::FileDiff>>,
    /// チェックポイント (エージェントへ指示を送る直前の作業ツリーの写し)。
    /// git は全て裏のスレッドで走る。
    checkpoints: checkpoint::Checkpoints,
    /// 🕰 ローカルヒストリ (VCS に依らない取り消し履歴)。走査も書き出しも
    /// 裏のスレッドで、UI はここから `std::fs` を 1 度も呼ばない。
    local_history: local_history::LocalHistory,
    /// 次の配達で取るチェックポイントの `(エージェント, 指示要約)`。
    /// `queue_submit` は `egui::Context` を持たないので、ここへ predoc して
    /// `submit_tick` (ctx を持つ) が実際の取得を仕込む。一斉送信で N 体ぶん
    /// 積まれても**先頭の 1 件だけ**が残る = スナップショットは 1 回。
    checkpoint_pending: Option<(String, String)>,
    /// ドラッグ中のエディタタブの添字 (ドラッグ並べ替え)。押していない間は None。
    tab_drag: Option<usize>,
    /// ⌃Tab を**押している間**だけ生きる MRU 切替。離すと確定して None に戻る。
    tab_switcher: Option<editor_split::TabSwitcher>,
    /// ペインごとに「どのタブへ自動スクロール済みか」。同じタブへ毎フレーム
    /// スクロールを要求すると横スクロールが手で動かせなくなるため、
    /// **アクティブが変わった 1 回だけ**追従する。
    tab_scrolled: HashMap<editor_split::PaneId, u64>,
    /// Git サイドバー。単一 repo 表示なので常に primary ルートを見る。
    git_panel: git_panel::GitPanel,
    /// パレットから撃つ git 操作 (commit / push / pull / 履歴) の状態。
    git_ops: GitOps,
    /// PR 風のローカル変更レビュー。Git サイドバーのサブタブとして出す。
    review: git_panel::ReviewPanel,
    /// Git サイドバーのサブタブ: true = 「変更をレビュー」/ false = 「変更」
    git_sub_review: bool,
    /// 「比較の左側」として覚えたファイル (VS Code: Select for Compare)。
    compare_left: Option<PathBuf>,
    /// 任意 2 テキストの比較結果。**明示的な操作でしか開かない**
    /// (画面が突然変わらないよう、レイアウトを押しのけない別ウィンドウ)。
    compare_view: Option<CompareView>,
    /// 折りたたみ表示のキャッシュ (毎フレーム本文を作り直さないため)。
    /// 詳細は [`FoldView`] を参照。
    fold_view: Option<FoldView>,
    /// スティッキーヘッダのキャッシュ (鍵 = 本文ハッシュ + 最上部の可視行)。
    /// `highlight::sticky_headers` は本文全走査なのでスクロール中に毎フレーム
    /// 呼ばない。
    sticky_cache: Option<(u64, Vec<(usize, String)>)>,
    /// インデントガイドのキャッシュ。
    /// 鍵 = 本文ハッシュ + タブ幅 + キャレット行 (強調ガイドが行に依存するため)。
    /// 値 = (鍵, 行ごとの桁リスト, 強調するガイド)。
    #[allow(clippy::type_complexity)]
    guide_cache: Option<(
        u64,
        Vec<(usize, Vec<usize>)>,
        Option<crate::highlight::ActiveGuide>,
    )>,
    /// 補完ポップアップの状態 (デバウンス + 候補 + 選択)。
    lsp_completion: lsp::CompletionState,
    /// 補完を要求したバッファ id (別のタブへ移ったら候補を捨てるため)
    lsp_completion_buf: Option<u64>,
    /// ホバーポップアップの状態
    lsp_hover: lsp::HoverState,
    /// ホバー要求の飛行中 ID (HoverState は内部の ID を公開していないので控える)
    lsp_hover_flight: Option<u64>,
    /// ホバーを出す画面位置 (マウス位置)
    lsp_hover_pos: Option<egui::Pos2>,
    /// 本文描画中に求めた「マウス下の文書位置」。次フレームの
    /// `lsp_completion_tick` が拾ってホバーのデバウンスに流す。
    hover_doc_pos: Option<lsp::Position>,
    /// キャレット直下の画面位置 (補完ポップアップの基準)
    caret_screen: Option<egui::Pos2>,
    /// 「参照を検索」の結果と表示状態
    lsp_refs: Vec<lsp::ReferenceGroup>,
    lsp_refs_open: bool,
    lsp_refs_busy: bool,
    /// 「シンボルにジャンプ」の結果と表示状態
    lsp_symbols: Vec<lsp::SymbolNode>,
    lsp_symbols_open: bool,
    lsp_symbols_busy: bool,
    lsp_symbols_query: String,
    lsp_symbols_path: Option<PathBuf>,
    /// 直近の documentSymbol がブレッドクラムの背景更新か。
    /// true の間は「見つかりませんでした」のトーストを出さない
    /// (ユーザーが頼んでいない更新で通知を鳴らさないため)。
    lsp_symbols_quiet: bool,
    /// リネームの進行状態
    lsp_rename: Option<RenameFlow>,
    /// 整形の要求元 (バッファ id, 整形後に保存するか)
    lsp_format_buf: Option<(u64, bool)>,
    /// クイックフィックス (codeAction) の候補と表示状態。
    /// `lsp_actions_key` は選んだアクションの command を
    /// `workspace/executeCommand` へ返すための送り先。
    lsp_actions: Vec<lsp::CodeAction>,
    lsp_actions_open: bool,
    lsp_actions_busy: bool,
    lsp_actions_sel: usize,
    lsp_actions_key: Option<LspKey>,
    /// ポップアップを出す画面位置 (要求した時点のキャレット直下で固定する。
    /// 追従させると候補を選ぶ間に飛び回るため)
    lsp_actions_anchor: Option<egui::Pos2>,
    /// 引数ヒント (signatureHelp) の表示中の応答。
    /// 飛行中 ID は持たない — 古い応答は `lsp::LspClient` 側のスロットが
    /// 要求 ID で弾くので、UI 側で二重に見張る必要がない。
    lsp_signature: Option<lsp::SignatureHelp>,
    /// カーソル下シンボルのハイライト (documentHighlight) の状態
    lsp_highlight: lsp::HighlightState,
    /// 応答が来た時点で計算した本文の char 添字スパン。
    /// **毎フレーム計算しない** — 本文走査が要るので、応答時に 1 回だけ作る。
    lsp_highlight_spans: Vec<(usize, usize)>,
    /// 上のスパンがどのバッファのものか (別タブの位置を塗らないため)
    lsp_highlight_buf: Option<u64>,
    /// 同一シンボルのハイライトを出すか (config.lsp_highlight_occurrences の実行時値)
    lsp_highlight_on: bool,
    /// 直近フレームの本文選択範囲 (char 添字, start < end)。無選択なら None。
    /// 折りたたみ表示中は表示テキストの添字になってしまうので None にする。
    editor_sel_chars: Option<(usize, usize)>,
    /// 保存時に LSP で整形するか (`config.format_on_save` が種)。
    format_on_save: bool,
    /// 括弧を入れ子の深さごとに色分けするか (`config.bracket_colorization`)。
    bracket_colorization: bool,
    /// 縦のルーラーを引く桁 (`config.rulers`)。空なら 1 本も引かない。
    /// 昇順・重複なしに正規化して持つ (描画側で毎フレーム並べ替えないため)。
    rulers: Vec<usize>,
    /// 外部変更チェックの直近実行時刻(約1秒スロットリング)
    ext_check_at: Option<Instant>,
    /// 外部変更の見張り (描画スレッドの外)。最初のフレームで起こす。
    ///
    /// **これが生きているあいだ、家事のための定期フレームは 1 枚も要らない。**
    /// 見張りは `stat` だけを別スレッドで回し、UI が信じている mtime と
    /// 食い違ったときにだけ `request_repaint` する (`crate::fswatch`)。
    pub(super) fswatch: Option<fswatch::FsWatch>,
    keys: Keybinds,
    /// 機能レジストリ由来の打鍵表。**`BindAction` を 1 つも増やさずに**
    /// `Cmd::Feature(id)` を直に指す (`keybinds.rs` が共有の壁にならない)。
    /// 再割り当ては `keys` と同じ `[keybindings]` 表を共有する。
    feature_keys: crate::keybinds::FeatureBinds,
    /// ペットの固定位置(None=右下うろうろ)
    pet_pos: Option<egui::Pos2>,
    /// ユーザー指定ペット画像のテクスチャ
    pet_tex: Option<egui::TextureHandle>,
    /// ペットのアニメ状態(フレームを跨いで保持)
    pet_rt: pet::PetRuntime,
    /// 効果音プレイヤー(種類ごとの連続再生クールダウン付き)
    sound: sound::SoundPlayer,
    /// この時刻までペットが喜ぶ(直近のエージェント正常終了)
    pet_happy_until: Option<Instant>,
    /// この時刻までペットが落ち込む(直近のエージェント異常終了)
    pet_error_until: Option<Instant>,
    /// × で閉じた承認バブルのセッション id(承認待ち解除で自動掃除。
    /// index はセッション削除でずれるため安定 id をキーにする)
    pet_bubble_dismissed: HashSet<u64>,
    /// 承認/拒否に応答した時刻(セッション id 毎)。キー入力がプロンプトを
    /// 消すまでの3秒間はバブルの再表示を抑止する(再検出ループ対策)
    pet_bubble_answered: HashMap<u64, Instant>,
    /// 承認待ちトースト+効果音の直近通知時刻(セッションタイトル毎)。
    /// 同じプロンプトの再検出による多重通知を10秒に1回へ抑える
    pet_attention_notified: HashMap<String, Instant>,
    /// **Follow the agent** — 追従の状態機械 (オフなら git を 1 回も叩かない)。
    follow: follow::Follow,
    /// 通知を「稼働中 → 待機」の**遷移 1 点**へ絞る門番。
    work_gate: notify::WorkGate,
    /// 見張りの異常通知を「同じ内容が続く間は鳴らさない」ようにする門番。
    anomaly_gate: notify::EdgeGate,
    /// インストール済みプラグイン(~/.zaivern/plugins)
    plugins: Vec<plugins::Plugin>,
    /// プラグイン名 → そのプラグインが足した言語名 (名前順)。
    /// プラグイン一覧に「🔤 何言語増えたか」を出すためだけの表示用。
    plugin_langs: HashMap<String, Vec<String>>,
    /// プラグインコマンドのキーバインド: (shortcut, plugins index, commands index)
    plugin_keys: Vec<(egui::KeyboardShortcut, usize, usize)>,
    /// プラグインコマンド実行結果の受け渡し(ワーカースレッド → UI)
    plugin_tx: mpsc::Sender<plugins::RunOutcome>,
    plugin_rx: mpsc::Receiver<plugins::RunOutcome>,
    /// GitHub パネル (PR / Issue 一覧・PR 差分キャッシュ)
    github: panels::GithubPanel,
    /// gh CLI の実行結果の受け渡し(ワーカースレッド → UI)。
    /// gh は 1 回 0.6 秒ほどかかるので UI スレッドでは絶対に回さない。
    gh_tx: mpsc::Sender<github::GhOutcome>,
    gh_rx: mpsc::Receiver<github::GhOutcome>,
    /// 「➕ 新規プラグイン」ダイアログの入力名(None = 閉)
    new_plugin_name: Option<String>,
    /// カタログ全 CLI から選んでプリセットを足すピッカー。
    /// PATH 検出はこの中のワーカースレッドが行う(UI スレッドは待たない)。
    agent_picker: AgentPicker,
    /// 言語ID → スニペット一覧(拡張の snippet ファイル由来)
    snippets_by_lang: HashMap<String, Vec<Snippet>>,
    /// 言語ID → 起動済み LSP クライアント
    lsp: HashMap<LspKey, lsp::LspClient>,
    /// did_open 済みのパス(重複送信の防止)
    lsp_opened: HashSet<PathBuf>,
    /// 診断の変更をデバウンスするための保留(パス→(最新テキスト, 受信時刻, 言語ID))
    lsp_pending: HashMap<PathBuf, (String, Instant, LspKey)>,
    /// which() の「見つからなかった」結果のキャッシュ(実行ファイル名→最後に確認した時刻)。
    /// 肯定結果は入れない(見つかればサーバーが起動して self.lsp に載り、二度と which されない)。
    lsp_which_missing: HashMap<String, Instant>,
    /// アクティブバッファの診断件数 (エラー, 警告) — ステータスバー用
    diag_counts: (usize, usize),
    /// アクティブバッファの診断キャッシュ (行→最悪 severity + **範囲付きの診断**)。
    /// 行だけでなく範囲を持つので、ガターの印と本文の波線が同じ源から出る。
    diag_cache: diagview::DiagCache,
    /// アクティブバッファのインレイヒント (本文の char 添字へ写し済み)。
    /// 組み直すのは中身が変わったフレームだけ (設計原則 3)。
    inlay_cache: diagview::InlayCache,
    /// 対応括弧の強調キャッシュ: (本文ハッシュ, キャレット char, 塗る位置)。
    /// 位置は (char 添字, 相手がいるか)。キャレットが動かない限り本文を走査しない。
    bracket_hl: Option<(u64, usize, Vec<(usize, bool)>)>,
    /// 本文でホバー中の診断 (メッセージ, severity, 画面位置)。
    /// LSP ホバーより**優先**する (診断があるところに説明を二重で出さない)。
    diag_hover: Option<(String, u8, egui::Pos2)>,
    /// プラグインパネルの内容: (プラグイン名, パネルID) → 本文
    plugin_panels: HashMap<(String, String), String>,
    /// プラグインがステータスバーへ出した文字列(空なら非表示)
    plugin_status: String,
    /// interval フックの最終実行時刻: (プラグイン名, イベント名) → 時刻
    hook_last_run: HashMap<(String, String), Instant>,
    /// パネルの最終更新時刻: (プラグイン名, パネルID) → 時刻
    panel_last_run: HashMap<(String, String), Instant>,
    /// startup フックを起動済みか(初回フレーム後に一度だけ走らせる)
    startup_hooks_done: bool,
    /// フレームの panic を見張る番人 (頻度ポリシー・隔離・警告バナー)。
    frame_guard: FrameGuard,
    /// ネイティブファイルダイアログの実行中ジョブ (UI スレッドを止めないため)。
    dialogs: DialogJobs,
    /// 直近に観測した git ブランチ名(git_change フックの検知用)
    hook_git_branch: Option<String>,
    /// 起動待ちのフック(egui の Context が要るので update で消化する)
    pending_hooks: Vec<(plugins::HookEvent, Option<PathBuf>)>,
    /// 前フレームでプラグインタブが見えていたか(on_open パネルの検知用)
    plugins_tab_was_open: bool,
    /// 音声入力の実行状態
    voice: VoiceState,

    // ── 監視・連携 ────────────────────────────────────────────
    /// エージェントの異常 (停滞・ループ・エラー多発など) を見張る。
    supervisor: supervisor::Supervisor,

    /// エージェント間 / ユーザー宛メッセージの配達係。
    coordinator: coordinator::Coordinator,

    /// 確認が必要な介入の待ち行列。先頭をダイアログに出す。
    /// **確認を取るまで絶対に実行しない** (安全の要)。
    pending_intervention: Vec<supervisor::InterventionIntent>,

    /// 確認が必要な「前任セッションの停止」提案の待ち行列。
    pending_stop: Vec<coordinator::Proposal>,

    /// 停止を実行し、プロセスが本当に消えるのを待っているタスク (タスクID, セッションID)。
    stopping: Vec<(coordinator::TaskId, u64)>,

    /// タスク UI・メッセージ送信・発信マーカー取り込みの状態 (`orchestration`)。
    orch: orchestration::OrchState,

    /// coordinator に登録済みのセッション ID。増減の差分で登録/解除する。
    known_sessions: HashSet<u64>,

    /// スーパーバイザーが最後に見た状態。変化したときだけ coordinator へ橋渡しする。
    sup_last_state: HashMap<u64, supervisor::SessionState>,

    /// 次にサンプリングしてよい時刻 (supervisor 側の間引き間隔に合わせる)。
    sup_next_at: Option<Instant>,

    /// 音声側がまだ読んでいない「ユーザーが手入力した」フラグの持ち越し袋。
    typed_voice: HashMap<u64, bool>,

    /// スーパーバイザー側の同フラグ。
    typed_sup: HashMap<u64, bool>,

    /// 端末へ伝え済みのテーマ色 (OSC 10/11 の問い合わせ応答用)。
    report_colors: HashMap<u64, (Color32, Color32)>,

    // ── スーパーエージェント (指揮官) ───────────────────────────
    /// いま指揮官として扱っているセッション ID。指名なし・未起動なら `None`。
    /// 指名 (タイトル/コマンド) から毎フレーム引き直すので、再起動で ID が
    /// 変わっても、途中で指名を変えても追従する。
    super_agent_session: Option<u64>,

    /// セッションごとに最後に LLM へ相談した異常種別。
    /// 同じ異常で毎ティック CLI を叩かないための歯止め。
    sup_last_diag: HashMap<u64, supervisor::Anomaly>,

    // ── 指揮 (スーパーエージェントの指示をユーザーへ届ける) ────────
    /// 指揮官の出力から通知済みの指示ハッシュ (二重通知を防ぐ。有界)。
    /// 指揮官セッションは `super_agent_session` を使う。
    commander_seen: HashSet<u64>,
    /// commander_seen の挿入順 (上限到達時に古い方から追い出すため)。
    /// 丸ごと clear すると画面に残っている指示が全部再通知される。
    commander_seen_order: std::collections::VecDeque<u64>,

    /// エージェントのタブをクリックして選び直したとき、次にアクティブ端末を
    /// 描くフレームでキーボードフォーカスを移すための予約フラグ。
    /// これが無いと、タブを押しても端末をもう一度クリックするまで入力が移らない。
    term_focus_pending: bool,

    // ── 初回起動ガイドツアー (crate::tutorial) ────────────────────
    /// ツアーの状態。描画は毎フレーム最後の `overlay` 1 本だけ。
    tutorial: tutorial::Tutorial,
    /// `autostart()` を撃ったか。egui の Context が要るので `new()` では呼べず、
    /// 最初の `update_impl` で 1 回だけ呼ぶ (何度も呼ぶと位置が 0 へ戻る)。
    tutorial_autostarted: bool,

    // ── 統合承認キューのパネル (crate::agents::approvals) ─────────
    /// ボトムパネルを「承認キュー」表示に切り替えているか
    /// (「📋 看板」と同じ流儀で、端末の代わりに敷き詰める)。
    approvals_view: bool,
    /// 承認キューの中で監査ログ (`approvals.jsonl` の末尾) を見ているか。
    approvals_audit: bool,
    /// 監査ログの読み込み結果と、読んだ時刻。**毎フレーム読まない**ための控え。
    /// `None` なら次の描画で 1 回だけ読む。
    approvals_audit_cache: Option<Vec<agents::approvals::AuditEntry>>,
    /// 折りたたみを開いている承認要求の ID (詳細 / 生プロンプト抜粋)。
    approvals_expanded: HashSet<u64>,

    // ── ACP クライアント (crate::acp) ─────────────────────────────
    /// 構造化プロトコル (ACP) で駆動しているエージェント群とそのパネル。
    /// 接続 0 本のときは何も描かず、1 フレームも起こさない。
    ///
    /// `pub` にしてあるのは `crate::feature` の登録面 (`dispatch` / `draw` が
    /// `&mut ZaivernApp` を受け取る) から触れるようにするため。
    pub acp: acp::AcpManager,

    // ── MCP サーバ管理パネル (crate::mcp) ─────────────────────────
    /// ボトムパネルを「🔌 MCP」表示に切り替えているか。
    mcp_view: bool,
    /// 走査結果と展開状態。走査は**このビューを出したときだけ**行う
    /// (`~/.claude.json` は 100KB 級で、毎フレーム読む相手ではない)。
    mcp: mcp::McpPanel,

    // ── Skills / slash command 管理パネル (crate::skills) ──────────
    /// ボトムパネルを「🧩 Skills」表示に切り替えているか。
    skills_view: bool,
    /// 走査結果・検索語・展開状態。走査は**このビューを出したときだけ**行う
    /// (プラグインの木は数百ディレクトリで、毎フレーム歩く相手ではない)。
    skills: skills::SkillsPanel,

    // ── spec 駆動開発パネル (crate::spec) ───────────────────────────
    /// ボトムパネルを「📐 Spec」表示に切り替えているか。
    spec_view: bool,
    /// 仕様・差分・陳腐化の判定。走査は**このビューを出している間だけ**、
    /// しかも**裏のスレッド**で行う (git を描画スレッドで待たない)。
    spec: spec::SpecPanel,

    // ── 複数キャレット (crate::editor_ops::MultiSel) ────────────────
    /// `(バッファ ID, 選択集合)`。**タブごとに 1 つ**で、タブを切り替えると
    /// 捨てる (別のファイルのバイト位置を持ち越すと本文を壊すため)。
    multi_sel: Option<(u64, editor_ops::MultiSel)>,
    /// 上下にキャレットを伸ばすときの sticky column (表示桁)。
    /// 途中に短い行があっても押し始めた桁へ戻るために持つ。
    multi_sticky_col: Option<usize>,
    /// 矩形選択の始点 `(バッファ ID, 行, 表示桁)`。「開始」で置き、「確定」で使う。
    column_anchor: Option<(u64, usize, usize)>,

    // ── 符号化を指定して開き直す / 保存する ─────────────────────────
    /// 符号化ピッカー。`Some(true)` なら保存用、`Some(false)` なら開き直し用。
    enc_picker: Option<bool>,
    /// ピッカーの絞り込み文字列。
    enc_filter: String,

    // ── ニーモニック付きブックマーク (crate::marks) ─────────────────
    /// プロジェクト全体のブックマーク。`editor::Bookmarks` (タブ内の行集合)
    /// とは別で、`~/.zaivern/bookmarks` へ永続化し編集を跨いで行を追う。
    marks: marks::MarksState,
}

/// 指揮官に選べないセッションの理由。選べるなら `None`。
///
/// 指揮はそのセッションの画面を**読むだけ**で成立する (端末へは何も注入しない)
/// ので、CLI の種類やヘッドレス対応は問わない — **起動しているどのエージェント
/// でも指揮官にできる**。唯一の例外が素のシェル: コマンド出力のエコーに
/// `@対象:` らしき行が混ざると指示として誤検出しやすいため、選択肢に出す前に
/// ここで弾く。
fn commander_reject_reason(command: &str) -> Option<String> {
    if command.trim().is_empty() {
        return Some(tr(
            "素のシェルは指揮官にできません (画面のエコーを @指示 と誤検出しやすいためです)",
        ));
    }
    None
}

/// 指揮官セッションを 1 つ選ぶ **純関数**。`rows` は (id, running, command, title)。
///
/// - `title` が指名されていれば、**タイトル一致** (trim 済みの完全一致) で動いている
///   セッションを返す。見つからなければ `None` — 同じ CLI の別セッションへ勝手に
///   フォールバックしない (「#3 を指名したのに #1 が指揮官になる」事故を防ぐ)。
///   セッション ID は再起動で変わるが、タイトルは引き継がれるので追従できる。
/// - `title` が空なら旧来どおりコマンドで選ぶ。プリセットのコマンドは権限モードに
///   よってフラグが付け外しされるため、文字列の完全一致では取りこぼす。カタログの
///   実行ファイル名まで落として比べる。
///
/// 見つからなければ `None` (= 自己診断ガードは働かせなくてよい)。
fn pick_commander_session(
    rows: &[(u64, bool, String, String)],
    title: &str,
    command: &str,
) -> Option<u64> {
    let want_title = title.trim();
    if !want_title.is_empty() {
        return rows
            .iter()
            .find(|(_, running, _, t)| *running && t.trim() == want_title)
            .map(|(id, _, _, _)| *id);
    }
    let cmd = command.trim();
    if cmd.is_empty() {
        return None;
    }
    let want = crate::agents::spec_for_command(cmd).map(|s| s.bin);
    rows.iter()
        .find(|(_, running, c, _)| {
            if !*running {
                return false;
            }
            match (want, crate::agents::spec_for_command(c.trim())) {
                (Some(a), Some(b)) => a == b.bin,
                _ => c.trim() == cmd,
            }
        })
        .map(|(id, _, _, _)| *id)
}

/// 介入をそのまま実行してよいか、確認ダイアログへ回すか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IntentRoute {
    /// 無確認で実行してよい (記録・通知のみ)。
    Run,
    /// ユーザーの確認を取ってからでないと実行しない。
    Confirm,
}

/// **安全の要**: 確認が要る介入・破壊的な介入は、必ず確認ダイアログへ回す。
///
/// `needs_confirmation` を見るだけでなく `destructive()` も併せて見るのは、
/// 万一設定やゲートの取り違えで確認フラグが落ちても、再起動・停止だけは
/// 無確認で走らせないための二重の歯止め。
fn route_intent(it: &supervisor::InterventionIntent) -> IntentRoute {
    if it.needs_confirmation || it.action.destructive() {
        IntentRoute::Confirm
    } else {
        IntentRoute::Run
    }
}

/// 指揮官の `@対象: 指示` をユーザー宛の通知文へ変える **純関数**。
///
/// 指揮官の指示は**どのセッションの入力欄にも自動で書き込まない**。ユーザーへ
/// 見せるだけにして、実際に流すかどうかはユーザーが決める(勝手な注入の禁止)。
/// `titles` は指揮官以外のセッションのタイトル一覧。宛先がどれにも一致しない
/// `@mention` の誤爆は `None` で黙って捨てる(従来の配達時と同じふるまい)。
fn commander_notice(d: &commander::Directive, titles: &[String]) -> Option<String> {
    let body = supervisor::redact(&d.body, coordinator::INJECT_BODY_MAX);
    match &d.target {
        commander::Target::All => Some(trf("指揮官の指示 (全員宛): {body}", &[("body", body)])),
        commander::Target::Named(name) => {
            let title = titles.iter().find(|t| commander::title_matches(t, name))?;
            Some(trf(
                "指揮官の指示 ({title} 宛): {body}",
                &[("title", title.clone()), ("body", body)],
            ))
        }
    }
}

/// coordinator へ渡すセッション状態を決める **純関数**。
///
/// 誤って `Idle` と判定すると、作業中のエージェントの入力欄へ文字を流し込んで
/// 入力を壊してしまう。だから少しでも曖昧なら必ず `Unknown` に倒す
/// (`Unknown` には何も配達されない)。
fn coordinator_state(
    running: bool,
    attention: bool,
    rate_limited: bool,
    sup: Option<supervisor::SessionState>,
) -> coordinator::SessionState {
    use coordinator::SessionState as C;
    use supervisor::SessionState as S;

    // プロセスが居ない = 終了。ここに曖昧さは無い。
    if !running {
        return C::Exited;
    }
    // 承認プロンプトで止まっている。割り込ませない。
    if attention {
        return C::WaitingApproval;
    }
    // レート制限中は進めない = 停滞扱い。新しいタスクを振らず、配達もしない
    // (制限が明けて警告が画面から消えれば自動で元の状態判定へ戻る)。
    if rate_limited {
        return C::Stalled;
    }
    match sup {
        // 直近に出力が動いた = 作業中。
        Some(S::Working) => C::Working,
        // 静かでプロンプトへ戻っている = 待機。ここだけが配達可能。
        Some(S::Idle) => C::Idle,
        Some(S::WaitingApproval) => C::WaitingApproval,
        Some(S::Stalled) => C::Stalled,
        // ループ / エラー多発 / 異常終了 / 完了扱いは「いま入力を受け付けられるか」が
        // 判断できない。まだ一度も観測していない (None) 場合も同じく分からない。
        Some(S::Looping) | Some(S::Errored) | Some(S::Crashed) | Some(S::Done) | None => C::Unknown,
    }
}

mod edit_core;
mod startup;

/// 全ルートを走査してファイル索引を作る (純関数 — テスト可能)。
///
/// 除外は `.gitignore` (+ `.git/info/exclude` + `core.excludesFile`) に任せる。
/// 以前はここに `node_modules` / `target` などのハードコード 10 種しか無く、
/// リポジトリ固有の生成物 (`out/` `.turbo/` …) が全部索引に載っていた。
fn build_file_index_with(
    roots: &[PathBuf],
    opts: &IndexOptions,
    progress: Option<&Arc<AtomicUsize>>,
) -> IndexOutcome {
    {
        let mut ig = crate::ignore::Ignorer::new(opts.respect_gitignore);
        let mut out: Vec<IndexedFile> = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        // ルートを跨いで DFS。エントリは絶対パスを正として持ち、
        // 相対パスは所属ルート基準で作る (あいまい検索の品質を保つため)。
        for root in roots {
            let root_name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.to_string_lossy().to_string());
            let mut stack = vec![(root.clone(), 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                if depth >= opts.max_depth {
                    continue;
                }
                if out.len() >= opts.max_files {
                    truncated = true;
                    break;
                }
                let Ok(rd) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name == ".git" || name == ".DS_Store" {
                        continue;
                    }
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let abs = e.path();
                    scanned += 1;
                    if let Some(p) = progress {
                        // 進捗は 128 件ごとに 1 回だけ書く (アトミックの往復を減らす)
                        if scanned.is_multiple_of(128) {
                            p.store(scanned, Ordering::Relaxed);
                        }
                    }
                    if ig.is_ignored(root, &abs, is_dir) {
                        continue;
                    }
                    if is_dir {
                        if !name.starts_with('.') {
                            stack.push((abs, depth + 1));
                        }
                    } else {
                        if out.len() >= opts.max_files {
                            truncated = true;
                            break;
                        }
                        // 索引の相対パスは Windows でも / 区切りに正規化する
                        // (`ignore::rel_slash` が `Path::components` で切るので、
                        // ファイル名抽出 (rsplit('/')) やあいまい検索の入力・
                        // ラベル表示が OS 間で一致する。abs はネイティブのまま)。
                        let rel = crate::ignore::rel_slash(root, &abs)
                            .unwrap_or_else(|| abs.to_string_lossy().to_string());
                        out.push(IndexedFile {
                            abs,
                            label: format!("{root_name}/{rel}"),
                            rel,
                        });
                    }
                }
            }
        }
        // 表示ラベル: 相対パスが 2 ルート以上で衝突するときだけ
        // `<ルート名>/<rel>` にする (VS Code 等と同じ「必要なときだけ曖昧回避」)。
        if roots.len() > 1 {
            let mut seen: HashMap<&str, usize> = HashMap::new();
            for f in &out {
                *seen.entry(f.rel.as_str()).or_insert(0) += 1;
            }
            let unique: HashSet<String> = seen
                .iter()
                .filter(|(_, n)| **n == 1)
                .map(|(r, _)| (*r).to_string())
                .collect();
            for f in &mut out {
                if unique.contains(&f.rel) {
                    f.label = f.rel.clone();
                }
            }
        } else {
            for f in &mut out {
                f.label = f.rel.clone();
            }
        }
        out.sort_by(|a, b| a.label.cmp(&b.label));
        if let Some(p) = progress {
            p.store(scanned, Ordering::Relaxed);
        }
        IndexOutcome {
            files: out,
            truncated,
        }
    }
}

mod agent_sessions;
mod bottom_panels;
mod cmd_dispatch;
mod cmd_palette;
mod cockpit;
mod code_editor;
mod editor_layout;
mod file_ops;
mod file_viewers;
mod find_nav;
mod kanban_deck_git;
mod lsp_glue;
mod open_prefs;
mod orchestrate;
mod quota_cost;
mod remote_api;
mod save_files;
mod shortcuts;
mod sidebar_ui;
mod top_bar_ui;
mod whichkey_voice;

mod frame_update;

/// 未読カーソルの巡回 (純関数のテーブルテスト)。
#[cfg(test)]
mod unread_cursor_tests;

/// 通知の遷移判定 — 見張りの段を 3 値へ畳むところ。
/// コスト上限の配線 (ソース構造の回帰テスト)。
///
/// 判定そのものは `coordinator::quota` の純粋関数がテーブルテストで押さえて
/// いる。ここは **app.rs がその門を通っていること**だけを固定する
/// (egui の描画は headless で目視できないので、配線が消えたことを検出する)。
#[cfg(test)]
mod cost_limit_wiring_tests;

/// 追従の配線 (ソース構造の回帰テスト)。
///
/// 「アイドル時に git を叩かない」の**回数による門番**は
/// `follow::tests::追従がオフならgitを一度も叩かない` が持つ。ここは
/// app.rs 側がその門を通っていること (= 早期 return を消していないこと) を
/// 固定する。実時間の assert はフレーキーになるので使わない。
#[cfg(test)]
mod follow_wiring_tests;
mod helpers;
use self::helpers::*;
mod dialog_windows;
mod workbench;

/// ライセンス状態の見出し (アイコン・1 行の要約・色)。
///
/// ダイアログとトーストの両方が同じ文言を使うために切り出してある
/// (状態と文言の対応が 2 か所へ散らない)。
fn license_status_head(
    status: &license::LicenseStatus,
    theme: &theme::Theme,
) -> (&'static str, String, egui::Color32) {
    match status {
        license::LicenseStatus::Valid { .. } => ("✨", tr("Pro ライセンス 有効"), theme.ok),
        license::LicenseStatus::Expired { .. } => ("⌛", tr("期限切れ"), theme.warn),
        license::LicenseStatus::Malformed(_) => ("⚠", tr("キーの形式が不正です"), theme.err),
        license::LicenseStatus::BadSignature => ("⚠", tr("キーの署名が不正です"), theme.err),
        license::LicenseStatus::Unlicensed => ("🆓", tr("未ライセンス (無料版)"), theme.text_dim),
    }
}

/// 設定画面の 1 行ぶんの列を、決めた幅で描く。
///
/// **どの幅でも見切れない**ための唯一の入口。幅は
/// [`config::settings_columns`] が可用幅から決めた値をそのまま渡す。
fn settings_col(ui: &mut egui::Ui, w: f32, h: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(w.max(0.0), h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(w.max(0.0));
            add(ui);
        },
    );
}

/// Hot Exit の差分で LCS を諦める上限 (旧行数 × 新行数)。
/// 超えたら「どこが違うか」ではなく「違う」ことだけを出す。
const HOTEXIT_DIFF_MAX_CELLS: usize = 2_000_000;

/// 2 つの本文を行単位で比べ、unified diff 風のテキストにする (純粋関数)。
///
/// Hot Exit の競合で「退避とディスクのどちらを採るか」を選ぶための表示。
/// マス数が `max_cells` を超えるときは LCS を諦め、行数だけを出す —
/// 選ぶのに必要なのは「違う」ことの提示であって、完全な差分ではない。
fn unified_lines(old: &str, new: &str, max_cells: usize) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a == b {
        return tr("(行の内容に違いはありません — 改行コードだけが違う可能性があります)\n");
    }
    let (n, m) = (a.len(), b.len());
    if n.saturating_mul(m) > max_cells {
        return trf(
            "@@ 大きすぎるため差分を計算していません @@\n-{old_n} 行 (ディスク)\n+{new_n} 行 (復元した本文)\n",
            &[("old_n", n.to_string()), ("new_n", m.to_string())],
        );
    }
    // dp[i][j] = a[i..] と b[j..] の LCS 長 (計算量・記憶量とも n*m)
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if a[i] == b[j] {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push_str(&format!(" {}\n", a[i]));
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            out.push_str(&format!("-{}\n", a[i]));
            i += 1;
        } else {
            out.push_str(&format!("+{}\n", b[j]));
            j += 1;
        }
    }
    for l in &a[i..] {
        out.push_str(&format!("-{l}\n"));
    }
    for l in &b[j..] {
        out.push_str(&format!("+{l}\n"));
    }
    out
}

/// キーバインド編集 UI の 1 行ぶんの列を、決めた幅で描く。
///
/// **どの幅でも見切れない**ための唯一の入口。幅は
/// [`crate::keybinds::keybind_columns`] が可用幅から決めた値をそのまま渡す。
fn keybind_col(ui: &mut egui::Ui, w: f32, h: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(w.max(0.0), h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(w.max(0.0));
            add(ui);
        },
    );
}

/// 編集 UI に並べるアクションを、検索語で絞って返す。
///
/// あいまい検索は既存の [`fuzzy`] をそのまま使う (新しいマッチャは書かない)。
/// アクション名・config 名・現在の打鍵のどれに当たっても拾う。
/// 検索語が空なら [`crate::keybinds::ALL_ACTIONS`] の並び順のまま。
fn keybind_rows(keys: &Keybinds, query: &str) -> Vec<BindAction> {
    let q = query.trim();
    if q.is_empty() {
        return crate::keybinds::ALL_ACTIONS.to_vec();
    }
    let prepared = fuzzy::PreparedQuery::new(q);
    let mut scored: Vec<(i32, usize, BindAction)> = Vec::new();
    for (i, a) in crate::keybinds::ALL_ACTIONS.iter().enumerate() {
        let label = tr(crate::keybinds::action_label(*a));
        let name = crate::keybinds::config_name(*a);
        let keys_txt = keys.label(*a);
        let best = [label.as_str(), name, keys_txt.as_str()]
            .into_iter()
            .filter_map(|t| prepared.score(t))
            .max();
        if let Some(sc) = best {
            scored.push((sc, i, *a));
        }
    }
    // 同点は元の並び順で安定させる (毎フレーム行が入れ替わらないように)
    scored.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
    scored.into_iter().map(|(_, _, a)| a).collect()
}

/// この行に出す注記 (衝突 / OS 予約)。問題が無ければ None。
///
/// 「同じ打鍵が他のアクションにもある」「chord の prefix と単打がぶつかる」
/// 「macOS が OS 側で握っている」を 1 本の文字列にまとめる。
fn conflict_note(keys: &Keybinds, a: BindAction) -> Option<String> {
    use crate::keybinds::Conflict;
    let items = crate::keybinds::conflicts_for(keys, a, keys.binding(a));
    if items.is_empty() {
        return None;
    }
    let parts: Vec<String> = items
        .iter()
        .map(|c| match c {
            Conflict::Duplicate(other) => trf(
                "{action} と重複",
                &[("action", tr(crate::keybinds::action_label(*other)))],
            ),
            Conflict::Prefix(other) => trf(
                "{action} の 1 打鍵目と衝突",
                &[("action", tr(crate::keybinds::action_label(*other)))],
            ),
            Conflict::Reserved(why) => trf("macOS が使用中: {why}", &[("why", tr(why))]),
        })
        .collect();
    Some(parts.join(" / "))
}

/// egui の TextEdit に組み込みで、キーバインド表からは変更できないもの。
fn builtin_shortcuts() -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for (label, spec) in [
        (tr("元に戻す"), "cmd+z"),
        (tr("やり直し"), "cmd+shift+z"),
        (tr("切り取り"), "cmd+x"),
        (tr("コピー"), "cmd+c"),
        (tr("貼り付け"), "cmd+v"),
        (tr("すべて選択"), "cmd+a"),
    ] {
        if let Some(sc) = parse_shortcut(spec) {
            rows.push((label, crate::keybinds::format_shortcut(sc)));
        }
    }
    rows
}

/// About ダイアログ用のビルド環境表示 (取得できなければ "unknown")。
fn rustc_version() -> &'static str {
    option_env!("ZV_RUSTC_VERSION").unwrap_or("1.88+")
}

/// 1 行に収める galley (溢れたら末尾を「…」にする)。
///
/// 折り返すと行高が揃わなくなり `show_rows` の前提 (等高) が崩れるので、
/// **どの幅でも必ず 1 行**にする。全文はホバーで見せること。
fn truncated_galley(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let job = egui::text::LayoutJob::single_section(
        text.to_string(),
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    wrap_job_to_one_row(ui, job, max_w)
}

/// 出来合いの [`egui::text::LayoutJob`] を 1 行へ詰める (溢れたら「…」)。
fn wrap_job_to_one_row(
    ui: &egui::Ui,
    mut job: egui::text::LayoutJob,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts(|f| f.layout_job(job))
}

/// 検索結果 1 行を「行番号 + 本文 (マッチだけ強調)」の 1 枚のレイアウトにする。
///
/// `marks` は**スニペット (`Hit.text`) の中の**バイト範囲。範囲が本文の外や
/// 文字境界の外を指していても描画は落とさない (壊れた範囲は無視して素で描く)。
fn search_row_job(
    theme: &Theme,
    line_no: usize,
    text: &str,
    marks: &[(usize, usize)],
    font: FontId,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let dim = egui::TextFormat {
        font_id: font.clone(),
        color: theme.text_dim,
        ..Default::default()
    };
    let plain = egui::TextFormat {
        font_id: font.clone(),
        color: theme.text,
        ..Default::default()
    };
    let hot = egui::TextFormat {
        font_id: font,
        color: theme.bg,
        background: theme.accent,
        ..Default::default()
    };
    job.append(&format!("{:>5}  ", line_no), 0.0, dim);
    let mut at = 0usize;
    for &(s, e) in marks {
        if s < at
            || e <= s
            || e > text.len()
            || !text.is_char_boundary(s)
            || !text.is_char_boundary(e)
        {
            continue;
        }
        job.append(&text[at..s], 0.0, plain.clone());
        job.append(&text[s..e], 0.0, hot.clone());
        at = e;
    }
    job.append(&text[at..], 0.0, plain);
    job
}

/// サイドバーの「検索」タブ本体。self 全体を借りない free 関数にして、
/// サイドバー描画クロージャ内の他フィールド借用と両立させる。
/// 返り値: (検索開始要求, ジャンプ先 (パス, 0-based 行), 置換フローの進み)。
fn global_search_panel(
    ui: &mut egui::Ui,
    theme: &Theme,
    gsearch: &mut GlobalSearchState,
    file_index: &[IndexedFile],
) -> (bool, Option<(PathBuf, usize)>, Option<ReplaceEvent>) {
    let mut go = false;
    let mut replace_ev: Option<ReplaceEvent> = None;
    ui.horizontal(|ui| {
        // ▸/▾ で置換行の開閉 (VS Code と同じ位置・同じ意味)
        let arrow = if gsearch.replace_open { "▾" } else { "▸" };
        if ui
            .add(egui::Button::new(arrow).frame(false))
            .on_hover_text(tr("置換行の表示切替"))
            .clicked()
        {
            gsearch.replace_open = !gsearch.replace_open;
        }
        let resp = ui.add(
            egui::TextEdit::singleline(&mut gsearch.query)
                .desired_width((ui.available_width() - 34.0).max(80.0))
                .hint_text(tr("ワークスペース全体を検索…")),
        );
        if gsearch.focus {
            resp.request_focus();
            gsearch.focus = false;
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            go = true;
        }
        if ui.button("🔎").on_hover_text(tr("検索 (Enter)")).clicked() {
            go = true;
        }
    });
    // 検索オプション。押した瞬間に検索し直す (VS Code と同じ即時反映)
    ui.horizontal(|ui| {
        let mut changed = false;
        let toggles: [(&mut bool, &str, &str); 3] = [
            (
                &mut gsearch.case_sensitive,
                "Aa",
                "大文字と小文字を区別する",
            ),
            (&mut gsearch.whole_word, "Ab|", "単語単位で検索する"),
            (&mut gsearch.regex, ".*", "正規表現として検索する"),
        ];
        for (flag, label, hint) in toggles {
            if ui
                .selectable_label(*flag, label)
                .on_hover_text(tr(hint))
                .clicked()
            {
                *flag = !*flag;
                changed = true;
            }
        }
        if changed {
            // 条件が変わったら、確認待ちの置換件数は当てにならないので捨てる
            gsearch.phase = gsearch.phase.next(&ReplaceEvent::Cancel);
            if !gsearch.query.trim().is_empty() {
                go = true;
            }
        }
    });
    // 対象を絞る glob。2 本を横に並べるとどちらも狭くなるので縦に積む
    let glob_w = (ui.available_width() - 12.0).max(80.0);
    let inc = ui.add(
        egui::TextEdit::singleline(&mut gsearch.include_globs)
            .desired_width(glob_w)
            .hint_text(tr("含めるファイル (例: *.rs, src/**)")),
    );
    let exc = ui.add(
        egui::TextEdit::singleline(&mut gsearch.exclude_globs)
            .desired_width(glob_w)
            .hint_text(tr("除外するファイル (例: target/**)")),
    );
    for r in [&inc, &exc] {
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            go = true;
        }
    }
    if gsearch.replace_open {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut gsearch.replace)
                    .desired_width((ui.available_width() - 76.0).max(70.0))
                    .hint_text(tr("置換後の文字列")),
            );
            let can = !gsearch.query.trim().is_empty() && !gsearch.phase.busy();
            if ui
                .add_enabled(can, egui::Button::new(tr("置換")))
                .on_hover_text(tr(
                    "まず件数だけ数えます (この時点では 1 バイトも書きません)",
                ))
                .clicked()
            {
                replace_ev = Some(ReplaceEvent::Start);
            }
        });
        match &gsearch.phase {
            ReplacePhase::Running => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(tr("置換を数えています…"))
                            .size(11.5)
                            .color(theme.text_dim),
                    );
                });
            }
            ReplacePhase::Confirm { files, hits } => {
                ui.label(
                    RichText::new(trf(
                        "{files} ファイル / {hits} 箇所を置換します",
                        &[("files", files.to_string()), ("hits", hits.to_string())],
                    ))
                    .size(11.5)
                    .color(theme.warn),
                );
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new(tr("実行")).color(theme.err).strong())
                        .clicked()
                    {
                        replace_ev = Some(ReplaceEvent::Confirm);
                    }
                    if ui.button(tr("やめる")).clicked() {
                        replace_ev = Some(ReplaceEvent::Cancel);
                    }
                });
            }
            ReplacePhase::Done { files, hits } => {
                let msg = if *hits == 0 {
                    tr("置換対象は見つかりませんでした")
                } else {
                    trf(
                        "{files} ファイル / {hits} 箇所を置換しました",
                        &[("files", files.to_string()), ("hits", hits.to_string())],
                    )
                };
                ui.label(RichText::new(msg).size(11.5).color(theme.ok));
            }
            ReplacePhase::Idle => {}
        }
    }
    // パターンが壊れているときは黙って literal に落とさず、その場で赤く出す
    if let Some(e) = &gsearch.error {
        ui.label(RichText::new(format!("⛔ {e}")).size(11.5).color(theme.err));
    } else if gsearch.running {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(tr("検索中…")).color(theme.text_dim));
        });
    } else if gsearch.searched {
        let n = gsearch.results.len();
        let capped = if n >= file_search::MAX_HITS {
            tr(" (上限で打ち切り)")
        } else {
            String::new()
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(trf(
                    "{n} 件ヒット / {m} ファイル走査{capped}",
                    &[
                        ("n", n.to_string()),
                        ("m", gsearch.scanned.to_string()),
                        ("capped", capped),
                    ],
                ))
                .size(11.5)
                .color(theme.text_dim),
            );
            // 0 件のときは押しても空の面が出るだけなので出さない
            if n > 0
                && ui
                    .small_button(tr("⿴ まとめて開く"))
                    .on_hover_text(tr(
                        "全ヒットを前後の文脈つきで 1 枚の面に並べます (マルチバッファ)",
                    ))
                    .clicked()
            {
                gsearch.open_multi = true;
            }
        });
    }
    ui.separator();
    let mut jump: Option<(PathBuf, usize)> = None;
    let font = FontId::monospace(12.0);
    egui::ScrollArea::vertical()
        .id_salt("zv-gsearch")
        .auto_shrink(false)
        .show(ui, |ui| {
            let mut last_file: Option<&Path> = None;
            for (i, hit) in gsearch.results.iter().enumerate() {
                if last_file != Some(hit.path.as_path()) {
                    last_file = Some(hit.path.as_path());
                    let rel = file_index
                        .iter()
                        .find(|f| f.abs == hit.path)
                        .map(|f| f.label.clone())
                        .unwrap_or_else(|| hit.path.display().to_string());
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("📄 {rel}")).strong());
                }
                let empty: Vec<(usize, usize)> = Vec::new();
                let marks = gsearch.marks.get(i).unwrap_or(&empty);
                let job = search_row_job(theme, hit.line + 1, &hit.text, marks, font.clone());
                if ui
                    .add(
                        egui::Button::new(job)
                            .frame(false)
                            .wrap_mode(egui::TextWrapMode::Truncate),
                    )
                    .clicked()
                {
                    jump = Some((hit.path.clone(), hit.line));
                }
            }
        });
    (go, jump, replace_ev)
}

// ─── アイドル時の再描画ポリシー ─────────────────────────────────────
//
// 「何も起きていないのにフレームを回さない」ための唯一の窓口。
// ここに載っていない理由で定期フレームを予約しない。
//
// 予約が要る理由は 4 つしか無い:
//   (a) 実入力 — egui が自分で起こすのでここでは何もしない
//   (b) 別スレッドが新しいデータを届けた — 届けた側が `request_repaint` する
//       (PTY リーダ / LSP / git / リモート HTTP / 音声 は実際にそうしている)
//   (c) 本当に動いているアニメーション — 持ち主が自分の刻みで予約する
//   (d) ユーザーに**見える**期限 or 落とせない家事 — それがこの関数

/// 自動保存の刻み ([`ZaivernApp::autosave_tick`] のゲート)。
///
/// **期限の計算と同じ定数を使う。** 別々に書くと、片方を直したときに
/// 「起きたのにまだ期限が来ていない」= 空振りのフレームが増える。
pub(super) const AUTOSAVE_MS: u64 = 2000;
/// 外部変更チェックの刻み ([`ZaivernApp::check_external_changes`] のゲート)。
pub(super) const EXT_CHECK_MS: u64 = 1000;
/// LSP へ `did_change` を送るまでのデバウンス ([`ZaivernApp::flush_lsp_changes`])。
pub(super) const LSP_DEBOUNCE_MS: u64 = 250;
/// 期限つきの家事を予約するときの下限。
///
/// 期限が「もう過ぎている」(= 0) ときに `Some(0)` を返すと、家事が
/// 何かの理由で進まない局面で**毎フレーム予約し直す忙しいループ**になる。
/// 下限を置けば、進まないときでも 20fps 相当で頭打ちになる。
const IDLE_TIMER_FLOOR_MS: u64 = 50;

/// フォルダ/ファイルの外部変更を取り込む刻み (フォーカスあり)。
///
/// **これは見張りスレッドを起こせなかった環境の後退経路である。**
/// 通常は `crate::fswatch` が別スレッドで `stat` し、UI が信じている姿と
/// 食い違ったときにだけ 1 枚起こすので、ここの刻みは使われない。
///
/// なぜ降ろしたか: `check_external_changes` の中身は数十回の `stat`
/// (実測 20 パスで約 12µs) にすぎないのに、それを UI スレッドでやるために
/// **egui のフレームを丸ごと 1 枚 (実測 約 3.3ms)** 回していた。
/// 2 秒刻みでも 0.17%/コアで、画面は 1px も変わらない。
/// 見張りを別スレッドへ出すと、この 0.17% がまるごと消える。
const IDLE_HOUSEKEEP_MS: u64 = 2000;
/// 同上・背面に回っているとき。見ていない画面の鮮度は落として良い。
const IDLE_BACKGROUND_MS: u64 = 6000;
/// 同上・最小化されているとき。描いても誰も見ていない。
const IDLE_HIDDEN_MS: u64 = 10_000;
/// エージェントが走っている間の刻み (フォーカスあり)。
/// 出力が無くても状態機械 (承認待ち検出・見張り) を進めるために要る。
const IDLE_AGENT_MS: u64 = 250;
/// 同上・背面。通知は届くが、画面の更新頻度は落とす。
const IDLE_AGENT_BACKGROUND_MS: u64 = 1500;
/// UI スレッドの応答待ちがあるときの刻み。選ばれた瞬間に反応する速さ。
const IDLE_AWAITING_MS: u64 = 50;

/// [`idle_repaint_ms`] の入力。フレーム終わりの実状態から組み立てる。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleSignals {
    /// このフレームに実入力 (キー・ポインタ・スクロール等) があった
    pub had_input: bool,
    /// このフレームで誰かが既に再描画を予約した = アニメーションが飛んでいる
    pub animating: bool,
    /// UI スレッドの応答を待っている処理がある (ファイルダイアログ等)
    pub awaiting: bool,
    /// 走っているエージェントが 1 本以上ある
    pub agents_running: bool,
    /// **定期フレームで**外部の書き換えを見張る必要がある。
    ///
    /// 見張りスレッド (`crate::fswatch`) が生きていれば `false` —
    /// `stat` は別スレッドが回し、変化があったときにだけ起こしてくる。
    /// スレッドを起こせなかった環境だけ `true` になり、従来どおり
    /// [`IDLE_HOUSEKEEP_MS`] の刻みで UI スレッドが見張る。
    pub watching_files: bool,
    /// **期限を持つ家事が、あと何 ms で来るか。** 無ければ `None`。
    ///
    /// 以前は `timers_due: bool` で、真なら [`IDLE_HOUSEKEEP_MS`] (2 秒) の
    /// 刻みで回していた。**これが最大の常時再描画源だった** —
    /// 同梱プラグイン `usage-meter` の interval フックは **900 秒**に 1 回で
    /// よいのに、その期限を見張るためだけにアイドルで 2 秒ごとに 1 枚
    /// 描いていた (実測: 出所タグ `idle.timers`)。
    /// 期限そのものを渡せば、**その 1 枚まで寝られる**。
    pub timer_due_in_ms: Option<u64>,
    /// ウィンドウがフォーカスされている
    pub focused: bool,
    /// ウィンドウが見えている (最小化されていない)
    pub visible: bool,
}

/// 次に定期フレームを予約するまでの ms。`None` なら**予約しない** = 完全に寝る。
///
/// アニメーションが飛んでいる (`animating`) ときは、その持ち主が自分の刻みで
/// 予約済み。egui は複数の `request_repaint_after` の**最短**を採るので、
/// ここでさらに粗い予約を足しても意味が無い — `None` を返す。
/// ただし応答待ち (`awaiting`) はどのアニメより短い刻みが要るので先に見る。
pub fn idle_repaint_ms(s: IdleSignals) -> Option<u64> {
    // ① UI スレッドの返事を待っている — いちばん短く回す
    if s.awaiting {
        return Some(IDLE_AWAITING_MS);
    }
    // ② アニメーションの持ち主に任せる
    if s.animating {
        return None;
    }
    // ③ エージェントが走っている — 出力が無くても状態機械を進める
    if s.agents_running {
        return Some(if s.focused || s.had_input {
            IDLE_AGENT_MS
        } else {
            IDLE_AGENT_BACKGROUND_MS
        });
    }
    // ④ ここから下は「何も起きていない」。落とせない家事が無ければ 1 枚も描かない
    if !s.watching_files && s.timer_due_in_ms.is_none() {
        return None;
    }
    let housekeep = if !s.visible {
        IDLE_HIDDEN_MS
    } else if s.focused || s.had_input {
        IDLE_HOUSEKEEP_MS
    } else {
        IDLE_BACKGROUND_MS
    };
    let timer = s.timer_due_in_ms.map(|t| t.max(IDLE_TIMER_FLOOR_MS));
    Some(match (s.watching_files, timer) {
        // 見張りを UI スレッドで回すしかない環境。期限がそれより近ければ寄せる
        (true, Some(t)) => housekeep.min(t),
        (true, None) => housekeep,
        // **家事の期限だけ。そこまで寝る。** 900 秒後のフックのために
        // 2 秒ごとに描いていたのをやめるのが、この版の主眼
        (false, Some(t)) => t,
        // ③ の手前で弾いてあるので来ない
        (false, None) => housekeep,
    })
}

/// [`idle_repaint_ms`] が `Some` を返した**理由**。`perf::dump` の出所タグ。
///
/// `idle_repaint_ms` と**同じ優先順位**で降りる純関数。1 本のタグ
/// (`"schedule_idle_repaint"`) だけだと「アプリが定期フレームを回している」
/// までしか分からず、犯人 (待ち / エージェント / 家事 / 期限つきの家事) を
/// 逆算するのに `IdleSignals` の組み立てを読む必要があった。
/// `pet::repaint_tag` が状態まで割っているのと同じ考え方。
///
/// 順位がずれたら `idle_tag_tests::予約する理由とタグが必ず一致する` が落ちる
/// (2 つの関数を別々に直すと静かに嘘をつくため、全 256 通りで突き合わせる)。
pub fn idle_repaint_tag(s: IdleSignals) -> &'static str {
    if s.awaiting {
        return "idle.awaiting";
    }
    if s.animating {
        // ここへは来ない (`idle_repaint_ms` が None を返す) が、
        // 順位を写している以上、抜けを作らない
        return "idle.animating";
    }
    if s.agents_running {
        return "idle.agents";
    }
    if s.watching_files {
        return "idle.watch";
    }
    if s.timer_due_in_ms.is_some() {
        return "idle.timers";
    }
    "idle.none"
}

#[cfg(test)]
mod idle_tag_tests;

#[cfg(test)]
mod quick_launch_tests;

#[cfg(test)]
mod idle_repaint_tests;

#[cfg(test)]
mod tests;

// ─── セッション状態マッピングと確認ゲートのテスト ───────────────────
//
// どちらも「間違えると実害が出る」純関数なので、ここで縛っておく。
// - 状態マッピング: 誤った Idle 判定 = 作業中のエージェントへ文字を注入して壊す
// - 確認ゲート:     確認なしの再起動/停止 = ユーザーの作業内容を失う
#[cfg(test)]
mod wiring_tests;

// ─── 監視役 LLM (スーパーエージェント) の選択と配線のテスト ─────────────
//
// ここで縛るのは「選ばせてはいけないものを選ばせない」「選んだのに黙って
// 効かない状態にしない」「LLM の助言でも確認を飛ばさない」の 3 点。
#[cfg(test)]
mod super_agent_tests;

/// 第 2 次配線 (レビュー / 折りたたみ / ブックマーク / 表 / LSP) の配線テスト。
///
/// このファイルの他のテストと同じく **アプリ本体は組み立てない**。
/// 画面に出る手前の「判断」を純関数へ切り出してあるので、そこを固定する。
#[cfg(test)]
mod wave2_tests;

#[cfg(test)]
mod glyph_tests;

// ─── 今夜 UI へ配線した 5 機能の「配線そのもの」を固定するテスト ──────────
//
// エンジン側の振る舞いは各モジュールのテストが見ているので、ここは
// **UI が engine をどう呼ぶか**だけを見る (実際の起動やファイル書き込みはしない)。
#[cfg(test)]
mod ui_wiring_tests;

// ══════════════════════════════════════════════════════════════════════
//  第 3 次配線のテスト
//  (ガイドツアー / 統合承認キュー / コンポーザ / 複数キャレット / 符号化)
// ══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tutorial_wiring_tests;

/// Cockpit のレイアウト計算。
///
/// 実際に壊れていた 3 点を数値で固定する:
/// ① 見出しの下に数百 px の空白ができる (複数行コンポーザを横並びの見出し行へ
///    入れていたため、右端の細い帯に折り返されて行の高さが膨らんでいた)
/// ② 空状態の案内が画面のかなり下に落ちる (固定割合で押し下げていた)
/// ③ 右端でボタン・タイルが切れる
#[cfg(test)]
mod cockpit_layout_tests;

/// エージェントデッキの配線 (ソースを読む回帰テスト)。
///
/// デッキは端末を**自前で全高に描く**画面なので、下部ターミナルパネルと
/// 同時に出すと同じセッションを 1 フレームで 2 回描いてしまう (egui の Id 衝突)。
/// また、無条件の `request_repaint` を入れるとアイドル時の CPU が跳ねる。
/// どちらも見た目には出にくいので、ソースの形で固定する。
#[cfg(test)]
mod deck_wiring_tests;

#[cfg(test)]
mod approval_panel_tests;

#[cfg(test)]
mod composer_wiring_tests;

#[cfg(test)]
mod multi_cursor_wiring_tests;

#[cfg(test)]
mod encoding_wiring_tests;

// ══════════════════════════════════════════════════════════════════════
//  フレームガード: 隔離された領域が「黒い空間」にならないこと
//  (Windows の Cockpit で報告された不具合の回帰テスト)
// ══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod quarantine_hole_tests;

// ════════════════════════════════════════════════════════════════════════
// 端末分割の配線 (Cockpit のタイル ↔ terminal::SplitLayout)
// ════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod split_wiring_tests;

// ════════════════════════════════════════════════════════════════════════
// 文字のガタつき対策 (フォント列の順序 / 物理ピクセルへのスナップ)
// ════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod crisp_text_tests;

// ═══════════════════════════════════════════════════════════════════════
//  quick-open (⌘P) — 最近開いた順 / `名前:行` / 範囲外の行番号
//
//  ランキングは `file_mode_items` に閉じた純粋関数なので、App を組み立てずに
//  テーブルテストで固定できる。パスは `std::env::temp_dir()` から作り、
//  実ファイルは触らない (索引はメモリ上の値でよい)。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod quick_open_tests;

/// 問題パネルの絞り込み・グループ化 (純関数) の表テスト。
///
/// UI を描かずに固定できるところは全部ここで固定する。
#[cfg(test)]
mod problems_tests;
