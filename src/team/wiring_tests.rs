//! **配線の番人** — 「作ったのに繋いでいない」を静的に見つける。
//!
//! CLAUDE.md の「UI から到達できない実装は未完成」を、警告だけに頼らず
//! テストで固定する。ソースを読む検査は
//! **`.replace("\r\n", "\n")` を必ず通す** (Windows のチェックアウトは
//! CRLF なので、改行をまたぐ照合は正規化しないと必ず外れる)。

/// 改行を正規化したソース。**照合の前に必ずここを通す。**
fn src(text: &str) -> String {
    text.replace("\r\n", "\n")
}

const GLUE: &str = include_str!("../app/team_glue.rs");
const FEATURE: &str = include_str!("../features/team.rs");
const CLI: &str = include_str!("../cli.rs");
const BOARD: &str = include_str!("organization_board.rs");
const INSPECTOR: &str = include_str!("inspector.rs");
const TEAM_CLI: &str = include_str!("cli.rs");
const PANEL: &str = include_str!("panel.rs");

#[test]
fn cliのteamサブコマンドが門に登録されている() {
    let s = src(CLI);
    // 登録し忘れると `zai team run` が未知語としてワークスペース指定に
    // なり、**GUI の窓が生える** (実測で発見された罠)。
    //
    // **`is_cli_subcommand` の中だけを見る。** ファイル全体で `| "team"` を
    // 探すと、`yields_to_directory` にある同じ行を拾ってしまい、門から
    // 外しても緑のままになる (わざと壊して確かめたら実際に空回りした)。
    let gate = function_body(&s, s.find("fn is_cli_subcommand").expect("門がある"));
    assert!(
        gate.contains("| \"team\""),
        "is_cli_subcommand に team が無い"
    );
    assert!(
        s.contains("\"team\" => {"),
        "dispatch に team の分岐が無い"
    );
    assert!(
        s.contains("crate::features::team::cli_main(rest)"),
        "team の実体を呼んでいない"
    );
    assert!(
        s.contains("HELP_TEAM"),
        "zai help に team のセクションが無い"
    );
}

#[test]
fn team_runはgui起動へ落ちる経路を持つ() {
    let s = src(CLI);
    // `zai team run` はヘッドレスではない。実行中インスタンスが無いときは
    // CLI を終わらせず **GUI 起動へ落ちる** (main.rs が None を見て起こす)。
    let at = s
        .find("\"team\" => {")
        .expect("team の分岐がある");
    let body = &s[at..at + 600];
    assert!(
        body.contains("EXIT_LAUNCH_GUI") && body.contains("return None"),
        "GUI 起動へ落ちる経路が無い:\n{body}"
    );
}

#[test]
fn パレットからteam画面へ到達できる() {
    let s = src(FEATURE);
    assert!(s.contains("\"team.open\""), "Team を開く経路が無い");
    assert!(s.contains("\"team.new_run\""), "New Team Run の経路が無い");
    assert!(
        s.contains("app.toggle_team_board()"),
        "team.open が何も呼んでいない"
    );
    assert!(
        s.contains("app.open_team_new_run()"),
        "team.new_run が何も呼んでいない"
    );
    // 毎フレームの駆動が繋がっていること (無いと調停ループが 1 度も回らない)
    assert!(
        s.contains("draw: Some(|app, ctx| app.team_tick(ctx))"),
        "毎フレームの駆動が繋がっていない"
    );
}

#[test]
fn 起動要求は一度だけ処理する() {
    let s = src(GLUE);
    let at = s
        .find("fn team_take_launch_request")
        .expect("起動要求の受け口がある");
    // **囲っている関数の中だけを見る。** 範囲を広げると、同じファイルの
    // 別の関数が書いた文字列を拾って空回りする (CLAUDE.md の実例)。
    let body = function_body(&s, at);
    assert!(
        body.contains("launch::take("),
        "投函箱から取り出していない (take は取り出すと同時に消す)"
    );
    assert!(
        body.contains("p.launch_poll_due(Instant::now())"),
        "毎フレーム投函箱を stat している"
    );
    assert!(
        !body.contains("launch::launch_path"),
        "投函ファイルを自分で読んでいる (二重処理の温床)"
    );
}

#[test]
fn エージェント起動は既存経路を通る() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_launch_agent").expect("起動の橋がある"));
    assert!(
        body.contains("self.launch_preset("),
        "既存の起動経路を使っていない (並行実装を作らない)"
    );
    assert!(
        body.contains("spec_for_command"),
        "AI CLI のプリセットを選んでいない"
    );
}

#[test]
fn 指示の送信は既存の一本を通る() {
    let s = src(GLUE);
    assert!(
        s.contains("self.queue_submit(crate::submit::Job::deferred("),
        "既存の送信経路 (submit) を通っていない"
    );
    // PTY へ直接書く経路を作らない (Ink 系 TUI の取りこぼし対策は submit が持つ)
    assert!(
        !s.contains("write_bytes("),
        "PTY へ直に書いている (submit を迂回している)"
    );
}

#[test]
fn 停止は承認ゲートを通る() {
    let s = src(GLUE);
    let body = function_body(
        &s,
        s.find("fn team_apply_board_action").expect("操作の受け口がある"),
    );
    // Stop は Runtime へ渡すだけ。**その場で kill しない。**
    assert!(
        body.contains("BoardAction::Stop => panel::with_panel(|p| p.act(TeamAction::Stop))"),
        "Stop が Runtime を経由していない"
    );
    let stop_at = body.find("BoardAction::Stop").expect("Stop の分岐");
    let stop_arm = &body[stop_at..stop_at + 200];
    assert!(
        !stop_arm.contains("close_agent"),
        "承認前に kill している:\n{stop_arm}"
    );
}

#[test]
fn エージェントをクリックすると実際の端末が開く() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_open_terminal").expect("端末を開く橋がある"));
    assert!(
        body.contains("self.focus_agent_in_place(i)"),
        "既存の選択経路を使っていない"
    );
    assert!(
        body.contains("iter().position(|s| s.id == session)"),
        "セッション ID から実体を引いていない"
    );
}

#[test]
fn 報告されたサブエージェントの端末ボタンは無効になる() {
    for (name, s) in [("board", src(BOARD)), ("inspector", src(INSPECTOR))] {
        assert!(
            s.contains("add_enabled(false, egui::Button::new(tr(\"team.btn.open_terminal\")))"),
            "{name}: 開けない端末のボタンを無効にしていない"
        );
        assert!(
            s.contains("on_disabled_hover_text"),
            "{name}: 無効にした理由を出していない"
        );
    }
    // 判定は 1 か所 (can_open_terminal) を見ること
    assert!(
        src(BOARD).contains("a.can_open_terminal"),
        "board が can_open_terminal を見ていない"
    );
    assert!(
        src(INSPECTOR).contains("a.can_open_terminal"),
        "inspector が can_open_terminal を見ていない"
    );
}

#[test]
fn 描画から副作用を起こしていない() {
    // 盤面と Inspector は **読むだけ**。プロセスも保存も走らせない。
    for (name, s) in [("board", src(BOARD)), ("inspector", src(INSPECTOR))] {
        for forbidden in [
            "std::process::Command",
            "std::fs::write",
            "std::fs::remove_file",
            "std::thread::spawn",
            "persistence::save",
        ] {
            assert!(
                !s.contains(forbidden),
                "{name} が描画中に {forbidden} を呼んでいる"
            );
        }
    }
}

#[test]
fn 走査は間隔を空けてから行う() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_tick").expect("駆動がある"));
    assert!(
        body.contains("p.scan_due(Instant::now())"),
        "毎フレーム画面を舐めている"
    );
    assert!(
        body.contains("if self.team_is_active()"),
        "走っていないときも再描画を頼んでいる"
    );
}

/// `at` から始まる関数の本体 (対応する `}` まで) を返す。
///
/// **囲っている関数の中だけ**を見るための道具。範囲を「直前 N 行」の
/// ように取ると、同じファイルの別のテストや別の関数が書いた文字列を
/// 拾って、わざと壊しても緑のままになる (CLAUDE.md の実例)。
fn function_body(s: &str, at: usize) -> String {
    let open = s[at..].find('{').map(|i| at + i).unwrap_or(at);
    let mut depth = 0i32;
    let mut end = open;
    for (i, b) in s.bytes().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    // コメント行は落とす (自分のコメント文を拾って誤検出しないため)
    s[open..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod meta {
    use super::function_body;

    /// **番人そのものを検査する。** 範囲の切り出しが壊れていたら、
    /// 上の検査は全部空回りする。
    #[test]
    fn 関数の本体だけを切り出す() {
        let s = "fn a() {\n    let x = 1;\n}\nfn b() {\n    let y = 2;\n}\n";
        let body = function_body(s, s.find("fn a").unwrap());
        assert!(body.contains("let x = 1;"));
        assert!(!body.contains("let y = 2;"), "隣の関数まで拾っている");
    }

    #[test]
    fn コメント行は落とす() {
        let s = "fn a() {\n    // let x = 1;\n    let z = 3;\n}\n";
        let body = function_body(s, 0);
        assert!(!body.contains("let x = 1;"), "コメントを拾っている");
        assert!(body.contains("let z = 3;"));
    }

    #[test]
    fn 入れ子の括弧でも閉じ位置を間違えない() {
        let s = "fn a() {\n    if x { y() }\n    let z = 3;\n}\nfn b() { w() }\n";
        let body = function_body(s, 0);
        assert!(body.contains("let z = 3;"));
        assert!(!body.contains("w()"), "隣の関数まで拾っている");
    }
}

#[test]
fn cli起動とgui起動は同じruntimeを通る() {
    // **別々の実装を作らない。** CLI は起動要求を投函するだけで、計画も実行も
    // GUI 側の 1 本 (`TeamPanel::plan` → `TeamRuntime`) を通る。
    let cli = src(TEAM_CLI);
    assert!(
        !cli.contains("TeamRuntime::from_plan"),
        "CLI が独自に Runtime を建てている (GUI と二重実装になる)"
    );
    assert!(
        cli.contains("launch::post(&req)"),
        "CLI が起動要求を投函していない"
    );
    // 計画の入口は Planner 1 本
    assert!(
        cli.contains("StaticPlanner") && cli.contains("plan_schema::TeamPlan"),
        "CLI が Planner 境界を通っていない"
    );

    let glue = src(GLUE);
    assert!(
        glue.contains("p.plan(&req.spec_text"),
        "GUI が投函された SPEC で計画していない"
    );
    // 投函経由も、GUI のフォーム経由も、同じ `TeamPanel::plan` を通る
    assert!(
        glue.matches("p.plan(").count() >= 2,
        "CLI 経由と GUI 経由で入口が分かれている"
    );

    let panel = src(PANEL);
    assert!(
        panel.contains("TeamRuntime::from_plan"),
        "Runtime を建てるのが TeamPanel::plan の 1 か所でない"
    );
    assert_eq!(
        panel.matches("TeamRuntime::from_plan").count(),
        1,
        "Runtime を建てる場所が 2 つ以上ある"
    );
}

#[test]
fn 起動要求はteam画面を選ぶ() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_take_launch_request").expect("受け口がある"));
    assert!(body.contains("p.open = true"), "Team 画面を開いていない");
    assert!(
        body.contains("p.tab = BoardTab::Organization"),
        "Organization タブを選んでいない"
    );
    assert!(
        body.contains("p.form.open = false"),
        "Plan Preview ではなくフォームを出してしまう"
    );
    // `--yes` は **Start Team の確認だけ**を省く
    assert!(
        body.contains("if r.is_ok() && auto") && body.contains("TeamAction::Start"),
        "--yes が Start Team を省く経路になっていない"
    );
}

#[test]
fn yesで省けるのはstart_teamの確認だけ() {
    let s = src(TEAM_CLI);
    let body = function_body(&s, s.find("pub fn cli_main").expect("入口がある"));
    // `--yes` が権限昇格・破壊的操作・push/merge/deploy を素通ししないこと。
    // それらは計画の検証 (graph::check_command) で止まるので、CLI 側で
    // `--yes` を見て緩める分岐があってはいけない。
    assert!(
        !body.contains("opts.yes && "),
        "--yes を別の判断に混ぜている:\n{body}"
    );
    // reset だけは削除の確認に --yes を使う (消す対象を出したうえで)
    assert!(
        body.contains("if opts.dry_run || !opts.yes"),
        "reset が確認なしで消せてしまう"
    );
}
