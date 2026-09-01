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
const PERSISTENCE: &str = include_str!("persistence.rs");
const LAUNCH: &str = include_str!("launch.rs");
const FRAME: &str = include_str!("../app/frame_update.rs");
const STARTUP: &str = include_str!("../app/startup.rs");
const SESSIONS: &str = include_str!("../app/agent_sessions.rs");
const PLANNER: &str = include_str!("planner.rs");

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
        body.contains("launch::take_in("),
        "投函箱から取り出していない (take_in は取り出すと同時に消す)"
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
    // 既存の起動経路を使う (並行実装を作らない)。
    //
    // **`launch_preset(i, ctx)` ではなく `launch_preset_with(..., cwd, ..)`。**
    // 前者は呼んだ瞬間の `agent_cwd()` を使うので、Run を作ったあとに利用者が
    // フォルダを選び直すと、Team が面倒を見ているのとは違うところで
    // エージェントが動き出す。cwd は**要求が運んできた値**で決める。
    // **整形で改行されても外れない形で見る** (綴りそのものではなく、
    // 「どの口へ・どの cwd で・どの承認モードで」渡しているか)。
    for (needle, why) in [
        ("self.launch_preset_as(", "既存の起動経路を使っていない"),
        ("&spec.workspace_root", "Run の workspace で起こしていない"),
        ("self.team_approval()", "この Run の承認モードを渡していない"),
    ] {
        assert!(body.contains(needle), "{why}:\n{body}");
    }
    assert!(
        !body.contains("self.agent_cwd()"),
        "画面のいまのフォルダを見ている (Runtime が決めた実行先を上書きしている)"
    );
    assert!(
        body.contains("self.team_preset_table()"),
        "使えるプリセットの一覧を通していない:\n{body}"
    );
    // 一覧の作り方そのものも見る (**場所が移っただけで性質は同じ**)。
    let table = function_body(&s, s.find("fn team_preset_table").expect("プリセット一覧"));
    assert!(
        table.contains("spec_for_command"),
        "AI CLI かどうかを見ていない:\n{table}"
    );
    // **入っていない CLI を割り当てない。** 名前だけで決めると、その担当は
    // 永久に起動しない (画面には居るのに何も起きない)。
    assert!(
        table.contains("resolve_in("),
        "実体が PATH にあるかを確かめていない:\n{table}"
    );
}

#[test]
fn 指示の送信は既存の一本を通る() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_run_effects").expect("実行の橋"));
    // **本文はすぐ書く** (`Job::user`)。待って書く形 (`deferred`) にすると、
    // 起動直後の Claude Code は見張りがまだ Idle と言わず `⚠` の案内で
    // `attention` にもなるので、本文が 1 バイトも書かれないまま時間切れに
    // なる (実機で 6 体中 5 体が空のプロンプトのまま止まった)。
    //
    // 飲み込まれるのは**確定キー**のほうなので、待つのはそちらだけ
    // (`submit::COMMIT_IDLE_WAIT` — `submit::tests::忙しい相手には確定キーを撃たない`)。
    assert!(
        body.contains("crate::submit::Job::user(") && body.contains("self.queue_submit(job)"),
        "既存の送信経路 (submit) を通っていない:\n{body}"
    );
    assert!(
        !body.contains("Job::deferred("),
        "書くのを待つ形が残っている (起動直後は Idle にならず届かない):\n{body}"
    );
    // **配達の結末を受け取れる形で積む。** 目印が無いと、積めたことしか
    // 分からず、相手が消えても Runtime は「届いた」と信じ続ける。
    assert!(
        body.contains("job.tag = panel::with_panel(|p| p.delivery_tag(&key))"),
        "配達の結末を受け取る目印を付けていない:\n{body}"
    );
    // PTY へ直接書く経路を作らない (Ink 系 TUI の取りこぼし対策は submit が持つ)
    assert!(
        !s.contains("write_bytes("),
        "PTY へ直に書いている (submit を迂回している)"
    );
}

#[test]
fn 積めたことを届いたことにしない() {
    // **送信経路は「積めた」と「届いた」を別の時刻に決める。**
    // 積んだ時点で冪等キーを完了にすると、そのあと相手が消えても
    // (`Act::Gone`)、入力欄が空かないまま上限に達しても (`Act::GaveUp`)、
    // Runtime は「指示は届いた」と信じたままタスクを抱え続ける
    // (完了した鍵は二度と出し直されない = 指示が消える)。
    let glue = src(GLUE);
    let body = function_body(&glue, glue.find("fn team_run_effects").expect("実行の橋"));
    let at = body
        .find("for (key, task, session, text) in instructions")
        .expect("指示の取り出し口");
    // 次の取り出し口 (停止) までが指示の区画。
    let end = body[at..]
        .find("for (key, session) in stops")
        .map(|e| at + e)
        .unwrap_or(body.len());
    let seg = &body[at..end];
    assert!(
        !seg.contains("ack_done"),
        "積めた時点で完了にしている (届かなくても消えなくなる):\n{seg}"
    );
    assert!(
        seg.contains("job.tag = panel::with_panel(|p| p.delivery_tag(&key))"),
        "配達の結末を受け取る目印を付けていない:\n{seg}"
    );

    // 配達の**終わり方**が、目印つきで必ず返ること。
    let sessions = src(SESSIONS);
    let tick = function_body(
        &sessions,
        sessions.find("fn submit_tick").expect("配達を進める唯一の経路"),
    );
    for (needle, why) in [
        ("submit::Act::Done => {", "届いたことを他と区別していない"),
        ("submit::Act::Gone => {", "相手が消えたことを他と区別していない"),
        ("outcomes.push((t, true))", "届いたことを返していない"),
        ("outcomes.push((t, false))", "届かなかったことを返していない"),
        ("self.team_note_delivery(outcomes)", "結末を頼んだ側へ返していない"),
    ] {
        assert!(tick.contains(needle), "{why}:\n{tick}");
    }
    // **届かなかった側は 3 通りある** (消えた / 諦めた / セッションごと無い)。
    assert!(
        tick.matches("outcomes.push((t, false))").count() >= 3,
        "届かなかった経路のどれかが黙って捨てている:\n{tick}"
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
    // 盤面は「中身を見る」、Inspector は「端末を開く」— **同じ性質を
    // 別のボタンで守る**ので、名前も別になる (押せない相手には出さない)。
    for (name, s, label) in [
        ("board", src(BOARD), "team.btn.show_output"),
        ("inspector", src(INSPECTOR), "team.btn.open_terminal"),
    ] {
        assert!(
            s.contains(&format!(
                "add_enabled(false, egui::Button::new(tr(\"{label}\")))"
            )),
            "{name}: 開けない相手のボタンを無効にしていない"
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

#[test]
fn 検証コマンドの解析失敗を黙って捨てない() {
    // **元の不具合そのもの。** `filter(|s| parse_command(s).is_ok())` と
    // 書くと、SPEC に書かれた検証が黙って消え、残りが 0 件になると
    // 既定へ落ちる — 利用者から見て「書いたものと違う検証が走る」。
    //
    // 見るのは `compose` の中だけ。ファイル全体を見ると、この番人の
    // 説明文や別の関数の `is_ok()` を拾って空回りする。
    let s = src(PLANNER);
    let at = s.find("pub fn compose").expect("compose が無い");
    let body = function_body(&s, at);
    assert!(
        !body.contains("is_ok()"),
        "検証コマンドの可否を `is_ok()` で選り分けている (失敗が黙って消える)"
    );
    // 断るときは、**種類を分けて**返している。
    //
    // **自動決定できないことは断る理由ではない** (素の HTML など、走らせ
    // られる検証が存在しないフォルダがある)。そちらは計画を通して
    // 「検証なし」として進み、盤面がそれを出す。振る舞いの番人は
    // `planner::tests::検証を自動決定できなくても計画は通るが検証は空のまま`。
    for want in [
        "PlanError::InvalidValidationCommand",
        "PlanError::ForbiddenValidationCommand",
    ] {
        assert!(body.contains(want), "{want} で断る経路が無い");
    }
}

#[test]
fn 検出の失敗理由を握り潰していない() {
    // **回帰そのもの。** `detect()` の戻りを `unwrap_or_default()` で畳むと、
    // 「読めない」(`DetectError::Unreadable`) が「候補なし」と同じ空配列に
    // なる。壊れた `package.json` のリポジトリが検証なしで走り、完了が
    // **レビュー承認だけ**で決まる状態のまま素通りする。
    //
    // 振る舞いの番人は `planner::tests::壊れたpackage_jsonは検証なしとして通さない`。
    // ここが見るのは**握り潰す書き方が戻っていないか**だけ。
    let s = src(PLANNER);
    let at = s.find("pub fn compose").expect("compose が無い");
    let body = function_body(&s, at);
    assert!(
        !body.contains("unwrap_or_default()"),
        "detect() の失敗理由を握り潰している"
    );
    // **variant を名指しで**分けている (文字列で理由を判定していない)。
    for want in [
        "DetectError::Undetermined",
        "DetectError::NoCandidate",
        "DetectError::Unreadable",
        "PlanError::ValidationDetectionFailed",
    ] {
        assert!(body.contains(want), "{want} を名指しで扱っていない");
    }
}

#[test]
fn 既定の検証コマンドを綴りで固定していない() {
    // `cargo` を綴りで持つのは**リポジトリ判定の中だけ**。Planner が
    // 直に持つと、Next.js のリポジトリで `cargo test` が走る。
    let s = src(PLANNER);
    assert!(
        !s.contains("cargo fmt --check"),
        "Planner が Rust の検証コマンドを直に持っている"
    );
    let at = s.find("pub fn compose").expect("compose が無い");
    let body = function_body(&s, at);
    assert!(
        body.contains("validation_defaults::detect"),
        "リポジトリを見て決める経路を通っていない"
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
        cli.contains("launch::post_in(&root, &req)"),
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
    // 投函経由 (CLI) も、フォーム経由 (GUI) も、同じ 1 本へ落ちる
    assert!(
        glue.contains("p.plan(&req.spec_text") && glue.contains("p.plan_with("),
        "CLI 経由と GUI 経由で入口が分かれている"
    );

    let panel = src(PANEL);
    // `plan` は `plan_with` へ委譲するだけ。計画を建てる本体は 1 つ。
    assert!(
        panel.contains("self.plan_with(spec_text, source, opts, Vec::new(), \"\")"),
        "plan が plan_with へ委譲していない (実装が 2 本になる)"
    );
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
fn 起動要求のフィールドはすべて使われている() {
    // **飾りのメタデータを残さない。** 構造体にあるのに実行時は無視、は
    // 「渡したつもり」の嘘になる (workspace_root がまさにそれだった)。
    let glue = src(GLUE);
    let body = function_body(
        &glue,
        glue.find("fn team_launch_agent").expect("起動の橋"),
    );
    for (field, used_as) in [
        ("spec.workspace_root", "エージェントの cwd"),
        ("spec.role", "プリセットの選択"),
        ("spec.name", "端末の名前"),
    ] {
        assert!(
            body.contains(field),
            "`{field}` を使っていない ({used_as} にならない):\n{body}"
        );
    }
    // agent_id はセッションの結び付けに使う (呼び出し側)。
    let run = function_body(&glue, glue.find("fn team_run_effects").expect("実行の橋"));
    assert!(
        run.contains("p.bind_session(&spec.agent_id, session, identity)"),
        "起動したセッションを要求のエージェントへ結び付けていない (目印つきで)"
    );
    // **adopt も使う。** 使わないと、再起動のあと同じ logical agent を
    // 2 体起こす (起動成功から保存までの間に落ちた窓)。
    assert!(
        run.contains("self.team_adopt_session(&spec)"),
        "起こす前に既存セッションを見ていない"
    );
    let adopt = function_body(
        &glue,
        glue.find("fn team_adopt_session").expect("引き取りの判断"),
    );
    assert!(
        adopt.contains("launch::adopt_choice(") && adopt.contains("bound.contains"),
        "引き取りの規則を Team 側の純関数へ通していない:\n{adopt}"
    );
}

#[test]
fn 実行コンテキストを画面のいまの値で取り直さない() {
    // **今回いちばん固定したい不変条件。**
    // Runtime が決めた実行先を、橋渡し層が current/global state から
    // 取り直して上書きしてはいけない。
    let glue = src(GLUE);
    // 起動の橋: 画面のフォルダを見ない (上の番人と対)。
    let launch = function_body(
        &glue,
        glue.find("fn team_launch_agent").expect("起動の橋"),
    );
    assert!(!launch.contains("agent_cwd"));
    // 計画の入口: SPEC の解決も Team の workspace が基準。
    let plan = function_body(
        &glue,
        glue.find("fn team_plan_from_form").expect("計画の入口"),
    );
    assert!(
        plan.contains("p.workspace().to_path_buf()"),
        "SPEC の解決に Team の workspace を使っていない:\n{plan}"
    );
    assert!(
        !plan.contains("self.agent_cwd()"),
        "画面のいまのフォルダで SPEC を解決している"
    );
    // 検証の実行先は要求が運んでくる値 (`v.cwd`)。
    let val = function_body(
        &glue,
        glue.find("fn team_spawn_validation").expect("検証の委譲先"),
    );
    assert!(
        val.contains("let cwd = v.cwd.clone();") && !val.contains("agent_cwd"),
        "検証の cwd を取り直している:\n{val}"
    );
}

#[test]
fn 別のrunのeffectを実行させない構造がある() {
    // キューを空にする偶然ではなく、**持ち主の照合**で防いでいること。
    let p = src(PANEL);
    let mine = function_body(&p, p.find("fn mine<T>").expect("選り分け"));
    // **走っている Run のどれかのもの**なら実行し、それ以外は捨てる。
    // (Run が複数走るようになったので「いまの Run だけ」では、画面に
    //  出していないチームの仕事が全部捨てられる。)
    assert!(
        mine.contains("live.contains(&owner)") && mine.contains("self.runs.iter()"),
        "持ち主を照合していない:\n{mine}"
    );
    let absorb = function_body(&p, p.find("fn absorb").expect("受け取り"));
    assert!(
        absorb.contains("rt.owner()"),
        "発行時に持ち主を焼き付けていない:\n{absorb}"
    );
    // 4 つの取り出し口すべてが `mine` を通ること。
    for f in [
        "pub fn take_launches",
        "pub fn take_instructions",
        "pub fn take_stops",
        "pub fn take_validations",
    ] {
        let body = function_body(&p, p.find(f).unwrap_or_else(|| panic!("{f} が無い")));
        assert!(body.contains("self.mine(q)"), "{f} が持ち主を見ていない");
    }
}

#[test]
fn 閉じるときに検証を置き去りにしない() {
    // 札を立てるだけでは死なない (worker ごと消えるので誰も木を落とさない)。
    let f = src(FRAME);
    let body = function_body(&f, f.find("fn on_exit").expect("終了処理"));
    assert!(
        body.contains("p.shutdown()"),
        "終了時に Team の後始末をしていない:\n{body}"
    );
    let p = src(PANEL);
    let now = function_body(
        &p,
        p.find("pub fn stop_all_validations_now").expect("その場で落とす"),
    );
    assert!(
        now.contains("crate::procx::kill_tree(pid)"),
        "既存のプロセスツリー停止を使っていない (第 2 のプロセス管理を作らない)"
    );
}

#[test]
fn 立ち上がるときに前のアプリのrunを引き継がない() {
    // **状態は `thread_local!` に居てアプリより長生きする。** 閉じる側の
    // 後始末 (`on_exit` の `shutdown`) は、前のアプリが落ちた日には通らない。
    // その日に生き残った Runtime を新しいアプリが拾うと、**もう自分のもの
    // ではないセッションへ結び付いた Run** を操作できてしまう。
    // 立ち上がる側でも断つ (閉じる側と対にする)。
    let s = src(STARTUP);
    let body = function_body(&s, s.find("pub fn new(").expect("アプリの生成"));
    assert!(
        body.contains("panel::begin_app_context()"),
        "立ち上がりで Team の引き継ぎを断っていない:\n{body}"
    );
    // 断り方の実体 — 手放して、`workspace` も空へ戻す (戻さないと
    // 「同じフォルダだから何もしない」で保存済み Run の案内すら出ない)。
    let p = src(PANEL);
    let adopt = function_body(
        &p,
        p.find("pub fn adopt_new_app_context").expect("引き継ぎの拒否"),
    );
    assert!(
        adopt.contains("self.shutdown()") && adopt.contains("self.workspace = PathBuf::new()"),
        "手放していないか、入り直せる形になっていない:\n{adopt}"
    );
}

#[test]
fn 判定した実体とosが起こす実体を一致させる() {
    // **`Command::new("rustfmt")` を残さない。** 名前を渡すと OS が
    // もう一度 PATH を引くので、`PATH=<workspace>/bin:$PATH` に偽物を
    // 置くだけで、判定したのとは別の実体が動く。
    let l = src(LAUNCH);
    let run = function_body(
        &l,
        l.find("pub fn run_validation_command_in").expect("実行"),
    );
    assert!(
        run.contains("validation_command::resolve_in(&cmd.executable, cwd, path_var, pathext)"),
        "実体を自分で解決していない:\n{run}"
    );
    assert!(
        run.contains("run_resolved(&program, &args, cwd"),
        "解決した実体をそのまま渡していない:\n{run}"
    );
    // **実体の信用区分を見ている。** 名前についた「読むだけ」の評価だけで
    // 起こすと、`~/.local/bin/rustfmt` に置かれた偽物が無承認で走る
    // (workspace の外にあることは、書き換えられないことを意味しない)。
    assert!(
        run.contains("found.trust.auto_runnable()") && run.contains("approved.contains(cmd)"),
        "実体の信用区分と承認を突き合わせていない:\n{run}"
    );
    // **入口は委譲だけ。** ここで別の起こし方を書くと、番人が見ている
    // 本体 (`run_resolved_capped`) を通らない第 2 の経路ができる。
    let entry = function_body(&l, l.find("pub fn run_resolved(").expect("入口"));
    assert!(
        entry.contains("run_resolved_capped(") && !entry.contains("Command"),
        "入口が委譲以外のことをしている:\n{entry}"
    );
    // 実際に起こしているのは本体のほう。**性質は同じ**で、場所だけが違う。
    let resolved = function_body(&l, l.find("pub fn run_resolved_capped").expect("起動"));
    assert!(
        resolved.contains("crate::procx::hidden_command(program)"),
        "解決済みのパスで起こしていない:\n{resolved}"
    );
    // **シェルを組み立てない。** `sh -c` も `cmd /C` も通さない。
    for bad in ["cmd\", \"/C", "sh -c", "\"/C\"", "needs_windows_shim"] {
        assert!(
            !resolved.contains(bad),
            "シェルを挟んでいる (`{bad}`):\n{resolved}"
        );
    }
}

#[test]
fn 実行の直前にも危険度と承認を見る() {
    // **ゲートが 1 か所にしか無い状態は、そこを迂回されたときに
    // 何も残らないということ。** `Forbidden` だけを見ていると、承認
    // ゲートを通らずに実行器へ届いた `black .` が黙って走る。
    let l = src(LAUNCH);
    let run = function_body(
        &l,
        l.find("pub fn run_validation_command_in").expect("実行"),
    );
    assert!(
        run.contains("let risk = super::graph::classify(cmd);"),
        "実行器が危険度を見ていない:\n{run}"
    );
    assert!(
        run.contains("!risk.auto_runnable() && !approved.contains(cmd)"),
        "実行器が承認の証跡を見ていない:\n{run}"
    );
    // 承認の証跡は Runtime が発行時に焼き付けて運ぶ。
    let r = src(RUNTIME);
    assert!(
        r.contains("pub approved: Vec<ValidationCommand>"),
        "承認の証跡が実行要求に載っていない"
    );
}

#[test]
fn 検証コマンドは構造のまま実行地点まで運ぶ() {
    // 文字列へ戻して割り直す場所を作らない。割り方が 1 文字違えば、
    // 判定したものと OS が実行するものがずれる。
    let r = src(RUNTIME);
    assert!(
        r.contains("pub commands: Vec<ValidationCommand>"),
        "要求が文字列のまま渡っている"
    );
    let l = src(LAUNCH);
    // 実行器は語に割り直さない。
    assert!(
        !l.contains("split_whitespace"),
        "実行器が文字列を割り直している (判定した形と別物になりうる)"
    );
    // 文字列へ戻すのは見出しだけ。
    let run = function_body(
        &l,
        l.find("pub fn run_validation_command_in").expect("実行"),
    );
    assert!(
        run.contains("let label = cmd.display();"),
        "見出しの作り方が 1 か所に無い"
    );
}

#[test]
fn 起動要求にworkspaceを決めさせない() {
    // **未信頼データに置き場と cwd を決めさせない。** 投函箱の
    // `workspace_root` を書き換えるだけで「別のフォルダを Team Run に
    // する」ができてしまう。権限を持つのは、いま開いている workspace だけ。
    //
    // ここはソースを読む番人にしている — GUI 経路 (`ZaivernApp`) は
    // ヘッドレスのテストから回せないので、これ以外に見張る場所が無い。
    let s = src(GLUE);
    let body = function_body(
        &s,
        s.find("fn team_take_launch_request").expect("受け口がある"),
    );
    assert!(
        !body.contains("attach_workspace(&req."),
        "要求の中の workspace を attach している:\n{body}"
    );
    assert!(
        body.contains("p.attach_workspace(&ws)"),
        "いま開いている workspace を使っていない:\n{body}"
    );
    // 境界の確認も、同じ「いまの workspace」で行っていること。
    assert!(
        body.contains("launch::take_in(&root, &ws, now)"),
        "境界の確認に別の値を渡している:\n{body}"
    );

    // 受け取り側の判定が、要求の中の値を基準にしていないこと。
    let l = src(LAUNCH);
    let check = function_body(
        &l,
        l.find("pub fn request_matches_workspace").expect("境界の判定"),
    );
    assert!(
        check.contains("let current = canon(workspace);")
            && check.contains("canon(&req.workspace_root) != current")
            && check.contains("canon(&req.spec_path).starts_with(&current)"),
        "要求の中の workspace を権限として扱っている:\n{check}"
    );
    // `take_in` がその判定を通っていること。
    let take = function_body(&l, l.find("pub fn take_in").expect("受け口"));
    assert!(
        take.contains("request_matches_workspace(&req, workspace)"),
        "受け取り側が境界を確かめ直していない:\n{take}"
    );
    assert!(
        !take.contains("req.spec_path.starts_with(&req.workspace_root)"),
        "要求の中の値どうしを比べている (境界にならない)"
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
    // **本体は `cli_main_in`。** `cli_main` は 1 行の委譲なので、そちらを
    // 見ると中身が空で空回りする。
    let body = function_body(&s, s.find("pub fn cli_main_in").expect("入口がある"));
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

#[test]
fn フォームの入力欄はすべて何かを変える() {
    // **押せるのに何も起きない入力欄を残さない。** New Team Run の各項目が
    // 実際にどこかへ渡っていることを、静的に固定する。
    let glue = src(GLUE);
    let body = function_body(&glue, glue.find("fn team_plan_from_form").expect("計画の橋"));
    for (field, needle) in [
        ("goal_name", "form.goal_name.clone()"),
        ("roles", "form.roles.clone()"),
        ("agents", "agent_count: form.agents"),
        ("max_attempts", "max_attempts: form.max_attempts"),
        ("review_required", "review_required: form.review_required"),
        ("spec_path", "form.spec_path"),
        ("spec_text", "form.spec_text.clone()"),
        ("from_file", "if form.from_file"),
        // 承認モードとコスト上限は**この Run のもの**として運ぶ。
        ("approval_mode", "approval_mode: form.approval_mode.clone()"),
        ("cost_limit", "cost_limit: form.cost_limit"),
    ] {
        assert!(
            body.contains(needle),
            "フォームの `{field}` がどこにも渡っていない"
        );
    }
}

#[test]
fn team_runは既存のグローバル設定を書き換えない() {
    // **Run を 1 本作る操作で、Zaivern 全体の安全設定が変わってはいけない。**
    //
    // フォームの既定は `ask` / `0` で、`0` は**このコードベースでは
    // 「上限なし」**を意味する。以前はこの値を `config::save_state` で
    // 利用者の設定へ書き戻していたので、`agent` / `25` で使っている人が
    // フォームを開いて計画しただけで、承認モードが下がり、課金の上限が
    // 永続的に外れていた。
    let glue = src(GLUE);
    for bad in [
        "crate::config::save_state",
        "config::save_state",
        "self.cfg.approval_mode =",
        "self.cfg.global_approval_mode =",
        "self.cfg.cost_limit_session =",
    ] {
        assert!(
            !glue.contains(bad),
            "Team の橋が既存のグローバル設定を書き換えている: `{bad}`"
        );
    }
    // 読むのは初期値としてだけ。**書かない。**
    let seed = function_body(&glue, glue.find("fn seed_team_form").expect("初期値の読み込み"));
    assert!(
        seed.contains("self.cfg.approval_mode") && seed.contains("self.cfg.cost_limit_session"),
        "既存設定を初期値として読んでいない:\n{seed}"
    );
    assert!(
        seed.contains("p.seed_guardrails("),
        "フォームへ渡していない:\n{seed}"
    );
    // フォームを開く入口は 2 つある。**両方で読む** (片方だけだと既定値の
    // まま計画できてしまう)。
    let opens = glue.matches("self.seed_team_form()").count();
    assert!(
        opens >= 2,
        "フォームを開く入口の一部で既存設定を読んでいない ({opens} か所)"
    );
    // 効かせ方は「締める方向だけ」。判断は純関数 1 本に置く。
    let approval = function_body(&glue, glue.find("fn team_approval").expect("承認モードの解決"));
    assert!(
        approval.contains("effective_approval(&self.cfg.approval_mode)"),
        "既存設定と突き合わせずに Run の値を使っている:\n{approval}"
    );
    let cost = function_body(
        &glue,
        glue.find("fn team_cost_block_reason").expect("コスト遮断"),
    );
    assert!(
        cost.contains("self.cost_block_reason()") && cost.contains("effective_cost_limit("),
        "既存のコスト判定を通していないか、Run 側で締めていない:\n{cost}"
    );
}

#[test]
fn テストは実ホームへ書かない() {
    // **`~/.zaivern` はユーザーのもので、別のインスタンスが動いている
    // かもしれない場所**。テストがそこへ書くと、同時に動いている実機の
    // 台帳の隣にファイルが生える (実際にこの版で `~/.zaivern/team/` を
    // 作ってしまった)。根を差し替えられる形にしてあることを固定する。
    // **根を受け取らない入口を 1 つも作らない。**
    //
    // 「テストの中を見て禁止語を探す」形にすると、同じファイル内では
    // 修飾なしで呼べる (`post(&req)`) ので素通りする — 実際にわざと壊して
    // 空回りした。**入口そのものを無くす**ほうが確実なので、根を既定で
    // 埋める `pub fn` が存在しないことを見る。
    for (name, s, forbidden) in [
        (
            "persistence",
            src(PERSISTENCE),
            &["pub fn team_dir(workspace: &Path)"][..],
        ),
        (
            "launch",
            src(LAUNCH),
            &[
                "pub fn launch_path(workspace: &Path)",
                "pub fn post(req: &TeamLaunchRequest)",
                "pub fn take(workspace: &Path",
            ][..],
        ),
    ] {
        for f in forbidden {
            assert!(
                !s.contains(f),
                "{name} に根を受け取らない入口 `{f}` がある (テストが実 ~/.zaivern へ書ける)"
            );
        }
    }
    // 差し替え口が実在すること (無ければ上の検査は空回りする)
    assert!(src(PERSISTENCE).contains("pub fn team_dir_in("));
    assert!(src(PERSISTENCE).contains("pub fn default_home()"));
    assert!(src(LAUNCH).contains("pub fn launch_path_in("));
    assert!(src(LAUNCH).contains("pub fn post_in("));
    assert!(src(LAUNCH).contains("pub fn take_in("));
    assert!(src(TEAM_CLI).contains("pub fn cli_main_in("));

    // **既定を決めるのは 1 か所だけ。** 増えると「どちらが効くのか」で
    // 迷い、片方だけ直す事故が起きる。
    let defaults: usize = [src(PERSISTENCE), src(LAUNCH), src(PANEL), src(TEAM_CLI)]
        .iter()
        .map(|s| s.matches("crate::config::zaivern_dir()").count())
        .sum();
    assert_eq!(
        defaults, 1,
        "既定の根を決めている場所が {defaults} 箇所ある (1 つにすること)"
    );
}

// ══════════════════════════════════════════════════════════════════════
//  レビューで見つかった 3 件が戻ってこないことを、ソースの形で固定する
// ══════════════════════════════════════════════════════════════════════

const RUNTIME: &str = include_str!("runtime.rs");

#[test]
fn 自己申告を正式な検証証跡にしない() {
    let s = src(RUNTIME);
    let body = function_body(&s, s.find("fn apply_accepted").expect("報告の受け口"));
    // 自己申告は参考情報の欄へ入れる。
    assert!(
        body.contains("t.reported_validation = acc.validation.clone();"),
        "自己申告を参考情報として分けていない"
    );
    // **正式な証跡 (`validation.runs`) へ自己申告を入れない。**
    assert!(
        !body.contains("t.validation.runs.push(r.clone())")
            && !body.contains("t.validation.runs = acc.validation"),
        "自己申告を正式な検証証跡へ入れている:\n{body}"
    );
    // 報告を受けた時点でレビューへ進めない。
    assert!(
        !body.contains("new_review_task"),
        "完了報告の時点でレビュータスクを作っている"
    );

    // 実測を受ける入口だけが決着をつける。
    let nv = function_body(
        &s,
        s.find("pub fn note_validation_for").expect("実測の入口"),
    );
    assert!(
        nv.contains("self.settle_validation(task)"),
        "実測を受けても決着をつけていない"
    );
    // レビュータスクを作る場所は 1 つだけ (決着の中)。
    let settle = function_body(&s, s.find("fn settle_validation").expect("決着"));
    assert!(settle.contains("self.new_review_task(&t)"));
    assert_eq!(
        s.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("self.new_review_task(&"))
            .count(),
        1,
        "レビュータスクを作る場所が 2 つ以上ある"
    );
}

#[test]
fn reassignは停止を確認してから配り直す() {
    let s = src(RUNTIME);
    let body = function_body(&s, s.find("TeamAction::ReassignTask").expect("Reassign"));
    // 旧担当が生きているかを見てから分岐する。
    assert!(
        body.contains("self.live_session_of(id)"),
        "旧担当が生きているかを見ていない"
    );
    // 生きているなら承認を求める。
    assert!(
        body.contains("DecisionKind::StopAgents") && body.contains("RequestHumanApproval"),
        "停止承認を求めていない"
    );
    // **その場で `confirm_stopped` を呼ばない。**
    assert!(
        !body.contains("confirm_stopped") && !body.contains("release_after_self_report"),
        "承認前に前任の停止を確認済みとして扱っている:\n{body}"
    );

    // 「人が押した = 停止済み」という前提が残っていない。
    assert!(
        !s.contains("fn release_coordinator"),
        "誤った前提の名前が残っている"
    );
    // `confirm_stopped` を呼ぶ場所は、自己申告後と停止確認後の 2 つだけ。
    assert_eq!(
        s.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("self.co.confirm_stopped("))
            .count(),
        2,
        "前任の停止を確認済みとする場所が 2 つを超えている"
    );
}

#[test]
fn effectは実行前に完了扱いにしない() {
    let s = src(RUNTIME);
    let body = function_body(&s, s.find("fn dispatch_effects").expect("発行"));
    assert!(
        body.contains("state: EffectState::Dispatched"),
        "発行した Effect をいきなり完了として記録している:\n{body}"
    );
    assert!(
        !body.contains("EffectState::Completed"),
        "発行の時点で完了にしている"
    );
    // 完了になるのは ACK を受けたときだけ。
    let done = function_body(&s, s.find("pub fn note_effect_done").expect("ACK"));
    assert!(done.contains("EffectState::Completed"));
    // 復元は成功済みだけを引き継ぐ。
    let restore = function_body(&s, s.find("pub fn restore(").expect("復元"));
    assert!(
        restore.contains("r.state != EffectState::Completed"),
        "復元で未完了の Effect を成功扱いにしている"
    );
    // 決着していない検証は、成功済みでも引き継がない (裏スレッドは
    // プロセスと一緒に消えているので、記録だけが残ると永久に止まる)。
    assert!(
        restore.contains("unsettled.iter().any(|p| r.key.starts_with(p))"),
        "落ちた検証の記録を回収していない"
    );
    // 刈り取りは成功済みだけ。
    let prune = function_body(&s, s.find("fn prune_effects").expect("刈り取り"));
    assert!(
        prune.contains("r.state == EffectState::Completed"),
        "未完了の Effect まで刈り取っている:\n{prune}"
    );
}

#[test]
fn 実行側は必ず成否を返す() {
    // GUI 側 (app) が「渡されたら返す」を守っていること。返さないと
    // Runtime は永久に発行済みのままになる。
    //
    // **1 つの関数の中に ACK が 1 つでもあれば緑、では番人にならない。**
    // 4 つの取り出し口はそれぞれ別の Effect を消費するので、**口ごとに**
    // 成否が返っていることを見る (実際に、起動の失敗側から `ack_failed` を
    // 消しても素通りした)。
    let glue = src(GLUE);
    let body = function_body(&glue, glue.find("fn team_run_effects").expect("実行の橋"));

    // (取り出し口のループ見出し, その区画に必ず要るもの)
    let loops: [(&str, &[&str]); 5] = [
        (
            "for (key, spec) in launches",
            &["p.ack_done(&key)", "p.ack_failed(&key)"],
        ),
        (
            // 宛先のタスクも運ぶ (セッションから引き直さない)。
            //
            // **指示だけは「積めた時点」では返さない。** 積めたことと
            // 届いたことは別の時刻に決まるので、成功は配達の結末
            // (`team_note_delivery`) が返す。ここで見るのは
            // 「積めなかったときに必ず失敗が返る」ことだけ。
            "for (key, task, session, text) in instructions",
            &["p.ack_failed(&key)"],
        ),
        (
            // 人が出した指示。**成功は指示と同じく配達の結末が返す**ので、
            // ここで見るのは届く前に落ちた 2 つ (コスト上限で止まった /
            // 送信キューへ積めなかった) だけ。素の `ack_failed` で済ませると
            // 監査は「送信キューへ追加しました」のまま結末を 1 件も持たない
            // — queued と failed が記録の上で区別できなくなる。
            "for (key, agent, session, text) in manual",
            &["p.note_manual_failed(&key,"],
        ),
        ("for (key, session) in stops", &["p.ack_done(&key)"]),
        // 検証だけは裏スレッドへ渡すので、返すのは委譲先 (下で見る)。
        (
            "for (key, v) in validations",
            &["self.team_spawn_validation(key, v)"],
        ),
    ];
    // 見出しの位置で区画に割り、**その区画の中だけ**を見る。
    let mut starts: Vec<(usize, usize)> = loops
        .iter()
        .enumerate()
        .map(|(i, (head, _))| {
            (
                body.find(head)
                    .unwrap_or_else(|| panic!("冪等キーを受け取っていない: {head}")),
                i,
            )
        })
        .collect();
    starts.sort_unstable();
    for (n, &(at, i)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map(|&(a, _)| a).unwrap_or(body.len());
        let seg = &body[at..end];
        for needle in loops[i].1 {
            assert!(
                seg.contains(needle),
                "`{}` の区画が `{needle}` を返していない:\n{seg}",
                loops[i].0
            );
        }
    }

    // 委譲先も、走らせられたときと作れなかったときの両方を返す。
    let spawn = function_body(
        &glue,
        glue.find("fn team_spawn_validation").expect("検証の委譲先"),
    );
    for needle in ["p.ack_done(&key)", "p.ack_failed(&key)"] {
        assert!(spawn.contains(needle), "検証の委譲先が `{needle}` を返していない");
    }
    // **時限と停止の札を必ず渡す。** ここが抜けると実行器は無期限に待ち、
    // 止める手も無くなる (GUI の経路なので、ここでしか見張れない)。
    for needle in [
        "Duration::from_secs(v.timeout_secs",
        "launch::new_cancel_flag()",
        "timeout_secs: v.timeout_secs",
    ] {
        assert!(
            spawn.contains(needle),
            "検証の実行に `{needle}` を渡していない:\n{spawn}"
        );
    }
}

/// **盤面に描く端末は、触れる。ただしホイールは取り合わない。**
///
/// 読むだけにしていたら `Yes, I trust this folder` のような**答えないと
/// 先へ進めない確認**に答えられず、実機でセッションが `code 1` で終了した。
/// 盤面の中で完結させるのが目的なのに、答えられないなら結局よそへ行くことになる。
///
/// 一方で `hover_scroll` を true にすると、外側の縦スクロールと端末自身の
/// スクロールが取り合いになって行が重なる (実際にそう報告された)。
#[test]
fn 盤面の端末は触れるがホイールは取り合わない() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_board_ui").expect("盤面の描画"));
    let call = body
        .split("crate::terminal::draw(")
        .nth(1)
        .and_then(|t| t.split(')').next())
        .expect("端末を描いている");
    // 引数は (ui, s, theme, font, interactive, allow_resize, hover_scroll)
    let args: Vec<&str> = call.split(',').map(str::trim).collect();
    assert_eq!(args.len(), 7, "端末の描画引数が変わった: {call}");
    assert_eq!(args[4], "true", "触れない端末になっている (確認に答えられない)");
    assert_eq!(
        args[6], "false",
        "ホイールを取り合う設定になっている (行が重なって崩れる)"
    );
}

/// **使うエージェントの選び方が、起動に効く。**
///
/// 「おまかせ」なら入っているもの全部から、選べば**その中だけ**から、
/// 役割ごとに配る (1 つだけ選べば候補が 1 つなので全員それになる)。
/// 選べるのに起動が変わらないなら、その選択肢は嘘になる (CLAUDE.md)。
#[test]
fn 使うエージェントの選択が起動に効く() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_launch_agent").expect("起動の橋"));
    // 空なら役割ごとに配る (従来の道)。
    assert!(
        body.contains("roles::preset_for_role(&table, spec.role)"),
        "おまかせの道が無い:\n{body}"
    );
    // 選ばれていれば、その中だけを候補にする。真実の在り処は Run 側。
    assert!(
        body.contains("p.pinned_agents()"),
        "Run が持っている選択を見ていない:\n{body}"
    );
    assert!(
        body.contains("pinned.iter().any(|n| *n == row.name)"),
        "選ばれた名前で候補を絞っていない:\n{body}"
    );
    // **担当を 0 体にしない。** 1 つも残らなければ、おまかせへ落ちる。
    assert!(
        body.contains("table = self.team_preset_table()"),
        "選んだものが全部消えていたとき、担当が起動しないまま止まる:\n{body}"
    );
}

/// **選べるのは、この PC に入っている AI CLI だけ。**
///
/// 入っていないものを選ばせると、その担当だけ永久に起動しない
/// (画面には居るのに何も起きない)。
#[test]
fn 選べるエージェントは入っているものだけ() {
    let s = src(GLUE);
    let body = function_body(&s, s.find("fn team_board_ui").expect("盤面の描画"));
    let list = body
        .split("let agents: Vec<String>")
        .nth(1)
        .and_then(|t| t.split(';').next())
        .expect("選択肢の一覧を作っている");
    assert!(
        list.contains("p.is_ai") && list.contains("p.available"),
        "入っているかを見ていない:\n{list}"
    );
}

/// **計画ができたら、始め方がその場に出る。**
///
/// 開始のボタンはヘッダの奥 (一時停止・停止の隣) にあるので、初めての人は
/// 「計画は出たが次に何を押せばいいか分からない」で止まる (実際にそう報告
/// された)。ただし**勝手には始めない** — 始めた瞬間に費用が発生するので、
/// 1 回の明示的な操作は残す。
#[test]
fn 計画ができたら始め方がその場に出る() {
    let s = src(BOARD);
    let body = function_body(&s, s.find("fn ready_to_start_row").expect("案内の関数"));
    // 出るのは「始められる」ときだけ。走っている盤面に出し続けない。
    assert!(
        body.contains("GoalStatus::Ready"),
        "始められるときだけ出す形になっていない:\n{body}"
    );
    assert!(
        body.contains("BoardAction::Start"),
        "押しても始まらない案内になっている:\n{body}"
    );
    // **自動では始めない。** 押されたときだけ `Start` を積むこと。
    let auto = body.split("BoardAction::Start").next().unwrap_or_default();
    assert!(
        auto.contains(".clicked()"),
        "押していないのに始めている:\n{body}"
    );
    // 盤面の本体から呼ばれていること (関数だけ作って繋がない、を防ぐ)。
    assert!(
        s.contains("ready_to_start_row(ui, theme, snap, acts)"),
        "案内が盤面から呼ばれていない"
    );
}
