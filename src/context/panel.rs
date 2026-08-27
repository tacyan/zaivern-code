//! Context Engine の **UI アダプタ** — パレットから開く 1 枚の窓。
//!
//! ## 何をする窓か
//!
//! 1. いま効いている設定 (使う / 畳み方 / 上限) を出す
//! 2. これまでの削減量を出す (**Multi Cockpit へ載せる値の置き場**)
//! 3. パスを 1 つ選んで、実際に畳んだ結果を見せる
//!
//! ## 守っていること
//!
//! * **描画スレッドで待たない。** 実行は
//!   [`crate::context::ContextEngine::spawn`] へ出し、UI は手元の値を描く。
//! * **閉じているフレームは 1 ピクセルも触らない** (設計原則 3)。
//! * **描画のたびにディスクを読まない。** 根の解決 (`instances` の走査) と
//!   `config.toml` の読み込みは**窓を開けた 1 回だけ**行い、以後は手元の
//!   写しを描く。素直に書くと 60fps で 1 秒に 60 回 config を読むことになる
//!   (最初にそう書いた — 設計原則 3 に真正面から反する)。
//! * **画面が突然変わらない。** 窓の外のレイアウトには一切触れない。
//! * **利用者のファイルを書き換えない。** ここから起きるのは読み取りだけ。

use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};

use eframe::egui;

// **クレート内 API はここ** — `crate::context::…` を通す
// (道具の内部パスを直に指すと、外から使える面と食い違う)。
use crate::context::{
    ContextError, ContextMetrics, ContextOrigin, ContextRequest, ContextSource, ContextStrategy,
    OptimizedContext,
};
use crate::i18n::{tr, trf};

/// 結果の本文を出す高さ。
const BODY_HEIGHT: f32 = 220.0;
/// 窓の既定幅。
const WINDOW_WIDTH: f32 = 620.0;
/// 実行中に結果を拾いに行く間隔。
const POLL_MS: u64 = 120;

#[derive(Default)]
struct Panel {
    open: bool,
    /// 窓を開けた 1 回だけ取り直す値。`None` のあいだは「まだ読んでいない」。
    env: Option<Env>,
    /// 見に行くパス (ワークスペースからの相対)。
    target: String,
    /// この 1 回だけの畳み方。
    strategy: Option<ContextStrategy>,
    /// 走っている作業。**UI スレッドは絶対に待たない。**
    pending: Option<Receiver<Result<OptimizedContext, ContextError>>>,
    summary: String,
    body: String,
    /// 直前の 1 回の測定 (削減量・かかった時間・実際の畳み方)。
    last: Option<Done>,
    error: String,
}

/// 窓を開けた時点の環境の写し。**描画中は決して取り直さない。**
struct Env {
    roots: Vec<std::path::PathBuf>,
    enabled: bool,
    /// 設定を 1 行にしたもの (毎フレーム組み直さない)。
    summary: String,
}

/// 直前の 1 回の要約。**窓が閉じても消えない**ので、開き直しても直前の
/// 結果が見える。
struct Done {
    applied: String,
    truncated: bool,
    metrics: ContextMetrics,
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
    p.env = None;
    if p.target.is_empty() {
        p.target = ".".to_string();
    }
}

/// まだ読んでいなければ 1 回だけ読む。
///
/// **読み手を引数で受ける**のは、「1 回しか呼ばない」を I/O 抜きで
/// テストに固定するため (`tests::環境は開いた1回だけ読む`)。
fn ensure_env(p: &mut Panel, load: impl FnOnce() -> Env) {
    if p.env.is_none() {
        p.env = Some(load());
    }
}

/// 窓を開けた 1 回だけ、根と設定を読む。
fn load_env() -> Env {
    let roots = vec![crate::lease::gui_workspace_root()];
    let cfg = crate::config::load(&roots, false);
    let limits = super::limits_from_config(&cfg);
    let enabled = super::enabled(&cfg);
    let summary = trf(
        "使う: {on} ／ 畳み方: {mode} ／ 出力の上限: {max} トークン ／ 一覧の上限: {results} 件",
        &[
            (
                "on",
                if enabled {
                    tr("はい")
                } else {
                    tr("いいえ")
                },
            ),
            ("mode", super::strategy_from_config(&cfg).id().to_string()),
            (
                "max",
                if limits.max_tokens == 0 {
                    tr("なし")
                } else {
                    limits.max_tokens.to_string()
                },
            ),
            ("results", limits.max_results.to_string()),
        ],
    );
    Env {
        roots,
        enabled,
        summary,
    }
}

/// 毎フレーム呼ばれる描画。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    // 状態はこのモジュールが持つので、**`app` の中身へは触らない**
    // (`ZaivernApp` にグルーを足さずに済むよう、根はインスタンス台帳から引く)。
    let _ = app;
    let Ok(mut p) = panel().lock() else { return };
    if !p.open {
        return;
    }
    ensure_env(&mut p, load_env);
    collect_result(&mut p, ctx);

    let mut open = true;
    let mut go = false;
    egui::Window::new(tr("🧠 コンテキストエンジン"))
        // 題名から ID を切り離す (題名が変わっても位置と大きさを失わない)
        .id(egui::Id::new("context.panel"))
        .collapsible(false)
        .resizable(true)
        .default_width(WINDOW_WIDTH)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_max_width(ui.available_width());
            ui.label(tr(
                "AI へ渡す前に情報量を減らします。どのエージェントでも同じ仕組みが働き、\
                 ここから勝手にエージェントへ入力することはありません。",
            ));
            ui.separator();
            if let Some(env) = &p.env {
                ui.label(&env.summary);
            }
            ui.label(tr("設定は「設定」画面の「機能」から変えられます。"));
            ui.separator();
            go = draw_run_row(ui, &mut p);
            draw_result(ui, &p);
            ui.separator();
            draw_stats(ui);
        });

    if go {
        start(&mut p, ctx.clone());
    }
    p.open = open;
}

/// 走らせた結果を拾う。**待たない** — 来ていなければ次のフレームへ回す。
fn collect_result(p: &mut Panel, ctx: &egui::Context) {
    let Some(rx) = &p.pending else { return };
    match rx.try_recv() {
        Ok(Ok(out)) => {
            p.summary = out.summary.clone();
            p.body = out.content.clone();
            p.last = Some(Done {
                applied: out.applied.clone(),
                truncated: out.truncated,
                metrics: out.metrics.clone(),
            });
            p.error.clear();
            p.pending = None;
        }
        Ok(Err(e)) => {
            p.error = e.to_string();
            p.summary.clear();
            p.body.clear();
            p.last = None;
            p.pending = None;
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            // 結果を拾うためだけに軽く回す (アイドルへは戻る)
            ctx.request_repaint_after(std::time::Duration::from_millis(POLL_MS));
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            p.pending = None;
            if p.summary.is_empty() && p.error.is_empty() {
                p.error = tr("処理を起動できませんでした");
            }
        }
    }
}

/// パス・畳み方・実行ボタンの 1 行。押されたら `true`。
///
/// **どの幅でも見切れない**ように、入力欄は残り幅から作る。
fn draw_run_row(ui: &mut egui::Ui, p: &mut Panel) -> bool {
    let busy = p.pending.is_some();
    let mut go = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(tr("見る場所"));
        let picked = p.strategy.unwrap_or(ContextStrategy::Auto);
        egui::ComboBox::from_id_salt("context.strategy")
            .selected_text(picked.id())
            .width(110.0)
            .show_ui(ui, |ui| {
                for s in ContextStrategy::ALL {
                    if ui.selectable_label(picked == s, s.id()).clicked() {
                        p.strategy = Some(s);
                    }
                }
            });
        let btn = ui.add_enabled(
            !busy,
            egui::Button::new(if busy {
                tr("実行中…")
            } else {
                tr("見る")
            }),
        );
        go = btn.clicked();
        // 残り幅を入力欄に渡す (先にボタンを置いて、余りを欄にする)
        let w = ui.available_width().max(80.0);
        let resp = ui.add_sized(
            [w, ui.spacing().interact_size.y],
            egui::TextEdit::singleline(&mut p.target),
        );
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            go = true;
        }
    });
    go && !busy
}

fn draw_result(ui: &mut egui::Ui, p: &Panel) {
    if !p.error.is_empty() {
        ui.colored_label(egui::Color32::from_rgb(220, 90, 90), &p.error);
        return;
    }
    // **空のセクションは高さを取らない** (中身が無ければ見出しごと出さない)
    if p.summary.is_empty() {
        return;
    }
    ui.label(&p.summary);
    if let Some(d) = &p.last {
        ui.label(trf(
            "畳み方 {applied} ／ 節約 ~{saved} トークン ({pct}%) ／ {ms} ミリ秒{capped}",
            &[
                ("applied", d.applied.clone()),
                ("saved", d.metrics.saved_tokens().to_string()),
                ("pct", format!("{:.0}", d.metrics.reduction_percent())),
                ("ms", d.metrics.elapsed_ms.to_string()),
                (
                    "capped",
                    if d.truncated {
                        tr(" ／ 上限で中央を落としました")
                    } else {
                        String::new()
                    },
                ),
            ],
        ));
    }
    if p.body.is_empty() {
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("context.body")
        .max_height(BODY_HEIGHT)
        .show(ui, |ui| {
            // 折り返して出す。横に見切れる出力を作らない
            ui.monospace(&p.body);
        });
}

/// これまでの削減量。**Multi Cockpit へ載せるならこの値。**
fn draw_stats(ui: &mut egui::Ui) {
    let live = super::metrics::snapshot().total();
    if live.operations == 0 {
        ui.label(tr("まだ何も最適化していません。"));
        return;
    }
    ui.label(trf(
        "このセッション: {n} 回で ~{saved} トークン節約 ({pct}% 削減)",
        &[
            ("n", live.operations.to_string()),
            ("saved", live.saved_tokens().to_string()),
            ("pct", format!("{:.0}", live.reduction_percent())),
        ],
    ));
}

/// 実行を裏のスレッドへ出す。**押されたときにだけ設定を読む**
/// (描画のたびではない)。
fn start(p: &mut Panel, ctx: egui::Context) {
    p.error.clear();
    p.summary.clear();
    p.body.clear();
    p.last = None;
    let Some(env) = &p.env else { return };
    if !env.enabled {
        p.error = tr("コンテキスト最適化は設定で無効になっています。");
        return;
    }
    let roots = env.roots.clone();
    let cfg = crate::config::load(&roots, false);
    let engine = match super::engine_for(&roots, &cfg) {
        Ok(e) => e,
        Err(e) => {
            p.error = e.to_string();
            return;
        }
    };
    let target = std::path::PathBuf::from(if p.target.trim().is_empty() {
        "."
    } else {
        p.target.trim()
    });
    let strategy = p
        .strategy
        .unwrap_or_else(|| super::strategy_from_config(&cfg));
    // ディレクトリなら地図、ファイルなら中身。**利用者に選ばせるほどの差では
    // ないので、指したものに合わせる。**
    let source = if engine
        .workspace()
        .resolve(&target)
        .map(|sp| sp.as_path().is_dir())
        .unwrap_or(false)
    {
        ContextSource::Directory {
            path: target,
            params: Default::default(),
        }
    } else {
        ContextSource::File {
            path: target,
            params: Default::default(),
        }
    };
    let req = ContextRequest::new(source)
        .with_strategy(strategy)
        // 出自は**集計のラベル**。ここでどのエージェント向けかを決めない。
        .with_origin(ContextOrigin {
            session: Some("ui".to_string()),
            ..ContextOrigin::unknown()
        });
    p.pending = Some(engine.spawn(req, move || ctx.request_repaint()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窓は**開くまで何もしない**。パレットから呼んでも、その場では
    /// ファイルを 1 つも読まない (設計原則 3)。
    #[test]
    fn 開くだけでは何も読まない() {
        let ctx = egui::Context::default();
        open(ctx);
        let p = panel().lock().unwrap();
        assert!(p.open);
        assert_eq!(p.target, ".");
        assert!(p.pending.is_none(), "開いた時点で作業が走っている");
        assert!(p.summary.is_empty());
    }

    /// 結果の受け取りは**待たない**。切れた通路は「起動できなかった」として
    /// 畳み、窓が実行中のまま固まらない。
    #[test]
    fn 通路が切れても実行中のまま固まらない() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        let mut p = Panel {
            pending: Some(rx),
            ..Panel::default()
        };
        collect_result(&mut p, &ctx);
        assert!(p.pending.is_none());
        assert!(!p.error.is_empty(), "理由が出ていない");

        // 結果が来ていないうちは何も変えない
        let (_tx, rx) = std::sync::mpsc::channel::<Result<OptimizedContext, ContextError>>();
        let mut p = Panel {
            pending: Some(rx),
            ..Panel::default()
        };
        collect_result(&mut p, &ctx);
        assert!(p.pending.is_some());
        assert!(p.error.is_empty());
    }

    /// **描画のたびにディスクを読まない。** 素直に書くと 60fps で
    /// 1 秒に 60 回 `config.toml` を読むことになる (最初にそう書いた)。
    #[test]
    fn 環境は開いた1回だけ読む() {
        let mut loads = 0usize;
        let mut mk = |p: &mut Panel| {
            ensure_env(p, || {
                loads += 1;
                Env {
                    roots: vec![std::path::PathBuf::from("/w")],
                    enabled: true,
                    summary: "x".into(),
                }
            })
        };
        let mut p = Panel::default();
        for _ in 0..120 {
            mk(&mut p);
        }
        assert_eq!(loads, 1, "描画のたびに読んでいる");

        // 開き直したら取り直す (設定を変えた直後に開くと反映される)
        open(egui::Context::default());
        assert!(
            panel().lock().unwrap().env.is_none(),
            "開き直しても古い写しが残る"
        );
    }

    /// 失敗は理由ごと出す (握り潰さない)。
    #[test]
    fn 失敗の理由をそのまま出す() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(ContextError::OutsideWorkspace {
            path: "/etc/passwd".into(),
            roots: vec!["/w".into()],
        }))
        .unwrap();
        let mut p = Panel {
            pending: Some(rx),
            summary: "古い結果".into(),
            body: "古い本文".into(),
            ..Panel::default()
        };
        collect_result(&mut p, &ctx);
        assert!(p.error.contains("/etc/passwd"), "{}", p.error);
        assert!(p.summary.is_empty(), "古い結果が残っている");
        assert!(p.body.is_empty());
    }
}
