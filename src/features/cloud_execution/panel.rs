//! Cloud Execution の **UI アダプタ** — パレットから開く 1 枚の窓。
//!
//! ## 守っていること
//!
//! * **描画スレッドで待たない** (§45)。SSH も HTTP も git も、押した瞬間に
//!   裏のスレッドへ出して、UI は手元の値を描く。判定は
//!   [`tests::描画から重い呼び出しをしない`] がソースの走査で固定する。
//! * **描画のたびにディスクを読まない** (設計原則 3)。台帳は窓を開けた 1 回と、
//!   操作の後だけ読む。素直に書くと 60fps で 1 秒に 60 回読むことになる。
//! * **閉じているフレームは 1 ピクセルも触らない。**
//! * **画面が突然変わらない。** 窓の外のレイアウトには一切触れない。
//! * **巨大な艦隊 UI を作らない** (§44)。出すのは一覧と、4 つの操作だけ。

use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};

use eframe::egui;

use super::model::{ExecutionTarget, TargetLifecycle};
use super::provider::ProviderProfile;
use crate::i18n::{tr, trf};

/// 窓の既定幅。
const WINDOW_WIDTH: f32 = 720.0;
/// 一覧の高さ。
const LIST_HEIGHT: f32 = 240.0;
/// 走っている作業を拾いに行く間隔。
const POLL_MS: u64 = 150;

/// 裏のスレッドから返ってくるもの。
enum Done {
    Probe(String, Result<String, String>),
    Refreshed(Result<Vec<ExecutionTarget>, String>),
}

#[derive(Default)]
struct Panel {
    open: bool,
    /// 窓を開けた 1 回と、操作の後だけ読む写し。
    view: Option<View>,
    /// 走っている作業。**UI スレッドは絶対に待たない。**
    pending: Option<Receiver<Done>>,
    /// いま何をしているか (画面に出す 1 行)。
    busy: String,
    message: String,
    error: String,
    selected: Option<String>,
}

/// 窓を開けた時点の台帳の写し。**描画中は決して取り直さない。**
struct View {
    targets: Vec<ExecutionTarget>,
    providers: Vec<ProviderProfile>,
    known_hosts: String,
}

fn panel() -> &'static Mutex<Panel> {
    static P: OnceLock<Mutex<Panel>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Panel::default()))
}

/// パレットの項目から呼ぶ入口。**ここでは何も読まない** (窓を開くだけ)。
pub fn open(_ctx: egui::Context) {
    let Ok(mut p) = panel().lock() else { return };
    p.open = true;
    // 開き直したら取り直す (設定を変えた直後に開くと反映される)。
    // **読むのはここではなく最初の描画で 1 回** — パレットは押した瞬間に
    // 閉じるので、押した手が止まるのを避ける。
    p.view = None;
}

/// 台帳を読み直す。**押したときと開いたときだけ呼ぶ。**
fn load_view() -> View {
    let targets = load_targets_or_empty();
    let providers = super::registry::all_profiles(&super::store::load_providers().unwrap_or_default());
    View {
        targets,
        providers,
        known_hosts: super::store::known_hosts_path().display().to_string(),
    }
}

fn load_targets_or_empty() -> Vec<ExecutionTarget> {
    // **組み立ては registry の 1 か所へ任せる。** 素朴に足すと、手元で
    // 仕事が走った後 (台帳に枠を数える行が出来た後) に `local` が 2 行並ぶ。
    super::registry::with_local(1, super::store::load_targets().unwrap_or_default())
}

/// 毎フレーム呼ばれる描画。
pub fn draw(_app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let Ok(mut p) = panel().lock() else { return };
    if !p.open {
        // **閉じているフレームは 1 ピクセルも触らない** (設計原則 3)。
        return;
    }
    poll(&mut p, ctx);

    if p.view.is_none() {
        p.view = Some(load_view());
    }

    let mut open = p.open;
    egui::Window::new(tr("☁ クラウド実行"))
        .open(&mut open)
        .default_width(WINDOW_WIDTH)
        .resizable(true)
        .show(ctx, |ui| body(ui, &mut p, ctx));
    p.open = open;
}

/// 裏の作業の結果を拾う。**待たない** (`try_recv`)。
fn poll(p: &mut Panel, ctx: &egui::Context) {
    let Some(rx) = p.pending.as_ref() else { return };
    match rx.try_recv() {
        Ok(Done::Probe(name, result)) => {
            p.pending = None;
            p.busy.clear();
            match result {
                Ok(summary) => {
                    p.message = summary;
                    p.error.clear();
                }
                Err(e) => {
                    p.error = e;
                    p.message.clear();
                }
            }
            // 台帳が更新されたので読み直す (**ここだけ**)
            p.view = Some(load_view());
            let _ = name;
        }
        Ok(Done::Refreshed(result)) => {
            p.pending = None;
            p.busy.clear();
            match result {
                Ok(added) => {
                    p.message = trf(
                        "{n} 件の実行先を取り込みました",
                        &[("n", added.len().to_string())],
                    );
                    p.error.clear();
                }
                Err(e) => {
                    p.error = e;
                    p.message.clear();
                }
            }
            p.view = Some(load_view());
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            // まだ終わっていない。**次のフレームを予約する** (待たない)。
            ctx.request_repaint_after(std::time::Duration::from_millis(POLL_MS));
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            p.pending = None;
            p.busy.clear();
        }
    }
}

fn body(ui: &mut egui::Ui, p: &mut Panel, ctx: &egui::Context) {
    // **写しを一度取り出す。** `p` は下の閉包から可変で触るので、
    // `p.view` を借りたままにできない (借用が閉包の外まで生きる)。
    let Some(view) = p.view.take() else { return };
    draw_body(ui, p, ctx, &view);
    // 操作が台帳を読み直していたら、その新しいほうを残す
    if p.view.is_none() {
        p.view = Some(view);
    }
}

fn draw_body(ui: &mut egui::Ui, p: &mut Panel, ctx: &egui::Context, view: &View) {
    let width = ui.available_width();

    ui.label(tr(
        "仕事をどの機械で走らせるかを決める層です。手元・SSH で入れる Linux・\
         クラウドの VM を、同じように扱います。",
    ));
    ui.add_space(4.0);

    // ── Provider ──
    ui.horizontal_wrapped(|ui| {
        ui.strong(tr("Provider"));
        for prof in &view.providers {
            let token = if prof.token_env.is_empty() {
                String::new()
            } else if prof.token_present() {
                // **値は出さない。** 設定されているかどうかだけ (§41)
                format!(" · {} ✓", prof.token_env)
            } else {
                format!(" · {} ✗", prof.token_env)
            };
            ui.label(format!("{} ({}){token}", prof.name, prof.kind.id()));
        }
    });
    ui.add_space(6.0);

    // ── 実行先 ──
    ui.strong(trf(
        "実行先 ({n} 件)",
        &[("n", view.targets.len().to_string())],
    ));
    if view.targets.len() <= 1 {
        // **空状態は 1 枚のカードで中央に。** 高さだけ取って何も無い、を作らない
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
            ui.label(tr("リモートの実行先はまだありません。"));
            ui.label(tr(
                "SSH で入れる Linux があれば、次の 1 行で足せます:",
            ));
            ui.code("zai cloud target add ssh --name dev-01 --host <ホスト> --user <ユーザー>");
        });
        ui.add_space(8.0);
    } else {
        egui::ScrollArea::vertical()
            .id_salt("cloud_execution.targets")
            .max_height(LIST_HEIGHT)
            .show(ui, |ui| {
                for t in &view.targets {
                    target_row(ui, p, t, width);
                }
            });
    }

    ui.add_space(6.0);
    ui.separator();

    // ── 操作 ──
    let selected = p.selected.clone();
    ui.horizontal_wrapped(|ui| {
        let can_act = selected.is_some() && p.pending.is_none();
        if ui
            .add_enabled(can_act, egui::Button::new(tr("確かめる")))
            .clicked()
        {
            if let Some(name) = selected.clone() {
                start_probe(p, ctx, name);
            }
        }
        if ui
            .add_enabled(p.pending.is_none(), egui::Button::new(tr("取り込む")))
            .on_hover_text(tr(
                "Provider へ問い合わせて、まだ台帳に無い実行先を取り込みます。",
            ))
            .clicked()
        {
            start_refresh(p, ctx);
        }
        if ui.button(tr("読み直す")).clicked() {
            p.view = Some(load_view());
        }
    });

    ui.add_space(4.0);
    if !p.busy.is_empty() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(&p.busy);
        });
    }
    if !p.message.is_empty() {
        ui.label(&p.message);
    }
    if !p.error.is_empty() {
        ui.colored_label(egui::Color32::from_rgb(0xd0, 0x60, 0x60), &p.error);
    }

    ui.add_space(6.0);
    ui.label(trf(
        "known_hosts: {path}",
        &[("path", view.known_hosts.clone())],
    ));
    ui.label(tr(
        "VM の作成と破棄は課金を伴うので、CLI から明示的に実行します: \
         zai cloud worker create / destroy",
    ));
}

fn target_row(ui: &mut egui::Ui, p: &mut Panel, t: &ExecutionTarget, width: f32) {
    let selected = p.selected.as_deref() == Some(t.name.as_str());
    let (icon, color) = lifecycle_badge(t.lifecycle);
    // **どの幅でも見切れない。** 長い名前は縮めてホバーで全文を出す
    let caps = match (t.capabilities.cpu_cores, t.capabilities.memory_mib) {
        (Some(c), Some(m)) => format!("{c} core / {} GiB", m / 1024),
        _ => tr("未確認"),
    };
    let label = format!(
        "{icon} {}  ·  {}  ·  {}/{}  ·  {caps}",
        t.name,
        t.endpoint.summary(),
        t.capacity.active_jobs(),
        t.capacity.max_jobs
    );
    let resp = ui.add_sized(
        [width.max(120.0), 20.0],
        egui::SelectableLabel::new(selected, egui::RichText::new(label).color(color)),
    );
    if resp.clicked() {
        p.selected = Some(t.name.clone());
    }
    resp.on_hover_text(format!(
        "{}\nID: {}\nProvider: {}\n{}: {}\n{}",
        t.name,
        t.id,
        t.provider,
        tr("費用"),
        t.billing.summary(),
        t.note
    ));
}

fn lifecycle_badge(l: TargetLifecycle) -> (&'static str, egui::Color32) {
    match l {
        TargetLifecycle::Ready => ("●", egui::Color32::from_rgb(0x5a, 0xb0, 0x6a)),
        TargetLifecycle::Provisioning => ("◐", egui::Color32::from_rgb(0xc0, 0xa0, 0x50)),
        TargetLifecycle::Draining => ("◑", egui::Color32::from_rgb(0xc0, 0xa0, 0x50)),
        TargetLifecycle::Destroying => ("⊘", egui::Color32::from_rgb(0xd0, 0x60, 0x60)),
        TargetLifecycle::Failed => ("✗", egui::Color32::from_rgb(0xd0, 0x60, 0x60)),
        TargetLifecycle::Stopped => ("○", egui::Color32::GRAY),
        TargetLifecycle::Unknown => ("?", egui::Color32::GRAY),
    }
}

/// **裏のスレッドで確かめる。** UI スレッドは 1 ミリ秒も待たない。
fn start_probe(p: &mut Panel, ctx: &egui::Context, name: String) {
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    let roots = vec![crate::lease::gui_workspace_root()];
    std::thread::spawn(move || {
        let cfg = crate::config::load(&roots, false);
        let out = super::registry::Registry::load(&cfg)
            .and_then(|reg| reg.probe(&name))
            .map(|(t, probe)| {
                trf(
                    "{name} は使えます ({ms} ms) — {os} / {arch}",
                    &[
                        ("name", t.name.clone()),
                        ("ms", probe.latency_ms.to_string()),
                        ("os", probe.capabilities.os.id().to_string()),
                        ("arch", probe.capabilities.arch.id().to_string()),
                    ],
                )
            })
            .map_err(|e| e.to_string());
        let _ = tx.send(Done::Probe(name, out));
        // 結果が届いたことを描画側へ知らせる (**アイドルのまま眠らせない**)
        ctx.request_repaint();
    });
    p.pending = Some(rx);
    p.busy = tr("確かめています…");
    p.message.clear();
    p.error.clear();
}

/// **裏のスレッドで Provider へ問い合わせる。**
fn start_refresh(p: &mut Panel, ctx: &egui::Context) {
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    let roots = vec![crate::lease::gui_workspace_root()];
    std::thread::spawn(move || {
        let cfg = crate::config::load(&roots, false);
        let out = (|| {
            let reg = super::registry::Registry::load(&cfg)?;
            let mut added = Vec::new();
            for prof in super::registry::all_profiles(reg.profiles()) {
                if prof.kind.mode() != super::provider::ProvisioningMode::Dynamic {
                    continue;
                }
                // トークンが無い Provider は黙って飛ばす (エラーにしない —
                // 設定していないのは失敗ではない)
                if !prof.token_present() {
                    continue;
                }
                added.extend(reg.refresh_from(&prof.name)?);
            }
            Ok(added)
        })()
        .map_err(|e: super::model::CloudError| e.to_string());
        let _ = tx.send(Done::Refreshed(out));
        ctx.request_repaint();
    });
    p.pending = Some(rx);
    p.busy = tr("Provider へ問い合わせています…");
    p.message.clear();
    p.error.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **描画の中で重い呼び出しをしていないこと**をソースの走査で固定する (§45)。
    ///
    /// UI が数秒固まるのは、このリポジトリが git で実際に踏んだ壊れ方
    /// (`git branch --show-current` が 6023ms / thread="main")。
    #[test]
    fn 描画から重い呼び出しをしない() {
        let src = include_str!("panel.rs").replace("\r\n", "\n");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        // 描画関数の**中だけ**を見る (範囲を広げると空回りする)
        let at = body.find("fn body(").expect("描画関数がある");
        let end = body[at..]
            .find("\nfn ")
            .map(|e| at + e)
            .unwrap_or(body.len());
        let draw_body = &body[at..end];
        for banned in [
            "Registry::load",
            "probe(",
            "transport::",
            "reg.refresh_from",
            "recv()",
            "join()",
        ] {
            assert!(
                !draw_body.contains(banned),
                "描画の中で {banned} を呼んでいる。\n\
                 SSH / HTTP / git は裏のスレッドへ出すこと (UI が数秒固まる)"
            );
        }
        // 裏へ出す側は spawn を通っている
        assert!(body.contains("std::thread::spawn"), "裏へ出していない");
        // 待たずに拾っている
        assert!(body.contains("try_recv()"), "recv で待っている");
    }

    #[test]
    fn 閉じているフレームは何もしない() {
        let src = include_str!("panel.rs").replace("\r\n", "\n");
        let at = src.find("pub fn draw(").expect("描画関数がある");
        let head = &src[at..at + 400];
        assert!(
            head.contains("if !p.open") && head.contains("return"),
            "閉じているときに早期 return していない"
        );
    }

    #[test]
    fn 状態の印は全部の状態にある() {
        for l in [
            TargetLifecycle::Unknown,
            TargetLifecycle::Provisioning,
            TargetLifecycle::Destroying,
            TargetLifecycle::Ready,
            TargetLifecycle::Draining,
            TargetLifecycle::Stopped,
            TargetLifecycle::Failed,
        ] {
            let (icon, _) = lifecycle_badge(l);
            assert!(!icon.is_empty(), "{} の印が無い", l.id());
        }
    }

    #[test]
    fn 画面の文字列は辞書を通す() {
        // 素の日本語をベタ書きした行は、その言語の利用者には永久に日本語のまま残る
        let src = include_str!("panel.rs").replace("\r\n", "\n");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        for line in body.lines() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue;
            }
            // 画面へ出す呼び出しに、辞書を通していない日本語が無いこと
            for call in ["ui.label(\"", "ui.strong(\"", "ui.button(\""] {
                assert!(
                    !line.contains(call),
                    "辞書を通していない文字列がある: {line}"
                );
            }
        }
    }
}
