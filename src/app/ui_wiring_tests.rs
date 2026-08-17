use super::*;
use crate::coordinator::quota;
use crate::textenc::LineEnding;

// ── GAP1: 検索オプション → SearchOptions の対応表 ──────────────

fn gs(f: impl FnOnce(&mut GlobalSearchState)) -> GlobalSearchState {
    let mut s = GlobalSearchState::new();
    s.query = "needle".into();
    f(&mut s);
    s
}

#[test]
fn 検索オプションはそのまま_search_options_へ写る() {
    // (状態を作る手, 期待する (大小区別, 単語単位, 正規表現))
    let table: [(fn(&mut GlobalSearchState), (bool, bool, bool)); 5] = [
        (|_| {}, (false, false, false)),
        (|s| s.case_sensitive = true, (true, false, false)),
        (|s| s.whole_word = true, (false, true, false)),
        (|s| s.regex = true, (false, false, true)),
        (
            |s| {
                s.case_sensitive = true;
                s.whole_word = true;
                s.regex = true;
            },
            (true, true, true),
        ),
    ];
    for (setup, want) in table {
        let o = gs(setup).options(None);
        assert_eq!(
            (o.case_sensitive, o.whole_word, o.regex),
            want,
            "トグルの写し漏れ"
        );
        assert_eq!(o.query, "needle");
    }
}

#[test]
fn glob欄はカンマでも空白でも同じに割れる() {
    for src in [
        "*.rs, *.toml",
        "*.rs *.toml",
        " *.rs ,, *.toml , ",
        "*.rs\n*.toml",
    ] {
        assert_eq!(split_globs(src), vec!["*.rs", "*.toml"], "区切り: {src:?}");
    }
    assert!(split_globs("  ,  ").is_empty(), "空パターンを作らない");
}

#[test]
fn glob欄は_include_と_exclude_へ別々に載る() {
    let o = gs(|s| {
        s.include_globs = "src/**".into();
        s.exclude_globs = "target/**, *.lock".into();
    })
    .options(Some(PathBuf::from("/ws")));
    assert_eq!(o.include_globs, vec!["src/**"]);
    assert_eq!(o.exclude_globs, vec!["target/**", "*.lock"]);
    assert_eq!(
        o.root,
        Some(PathBuf::from("/ws")),
        "glob の基準はワークスペース"
    );
}

#[test]
fn 検索の既定値は従来の挙動を壊さない() {
    let o = GlobalSearchState::new().options(None);
    let d = file_search::SearchOptions::default();
    assert_eq!(o.max_results, d.max_results);
    assert_eq!(o.max_file_bytes, d.max_file_bytes);
    assert_eq!(o.follow_symlinks, d.follow_symlinks);
    assert!(!o.case_sensitive && !o.whole_word && !o.regex);
}

#[test]
fn 壊れた正規表現はその場でエラーになる() {
    // spawn_with_options が同期で Err を返す = UI が黙って literal に落ちない
    let o = gs(|s| {
        s.query = "(".into();
        s.regex = true;
    })
    .options(None);
    assert!(
        file_search::spawn_with_options(Vec::new(), o).is_err(),
        "閉じ括弧の無いパターンはコンパイルで弾かれる"
    );
    // 正規表現 OFF なら同じ文字列でも普通に検索できる (エラーにしない)
    let lit = gs(|s| s.query = "(".into()).options(None);
    assert!(file_search::spawn_with_options(Vec::new(), lit).is_ok());
}

// ── GAP1: 置換の確認フロー (ドライラン → 確認 → 実行) ──────────

fn run(evs: &[ReplaceEvent]) -> ReplacePhase {
    evs.iter().fold(ReplacePhase::Idle, |p, e| p.next(e))
}

#[test]
fn 置換は必ずドライランと確認を通ってから実行される() {
    use ReplaceEvent::*;
    assert_eq!(run(&[Start]), ReplacePhase::Running);
    assert_eq!(
        run(&[Start, DryRunDone { files: 3, hits: 7 }]),
        ReplacePhase::Confirm { files: 3, hits: 7 },
        "数えた結果は確認待ちとして出す"
    );
    assert_eq!(
        run(&[Start, DryRunDone { files: 3, hits: 7 }, Confirm]),
        ReplacePhase::Running
    );
    assert_eq!(
        run(&[
            Start,
            DryRunDone { files: 3, hits: 7 },
            Confirm,
            ExecuteDone { files: 3, hits: 7 },
        ]),
        ReplacePhase::Done { files: 3, hits: 7 }
    );
}

#[test]
fn 確認を飛ばした実行要求は無視される() {
    use ReplaceEvent::*;
    assert_eq!(run(&[Confirm]), ReplacePhase::Idle);
    assert_eq!(
        run(&[Start, Confirm]),
        ReplacePhase::Running,
        "ドライラン待ちのまま。実行要求では状態が動かない"
    );
    assert_eq!(
        run(&[
            Start,
            DryRunDone { files: 1, hits: 1 },
            ExecuteDone { files: 1, hits: 1 },
        ]),
        ReplacePhase::Confirm { files: 1, hits: 1 },
        "確認を経ていない完了報告で Done にしない"
    );
}

#[test]
fn やめるとどの段階からも_idle_へ戻る() {
    use ReplaceEvent::*;
    for prefix in [
        vec![Start],
        vec![Start, DryRunDone { files: 2, hits: 5 }],
        vec![Start, DryRunDone { files: 2, hits: 5 }, Confirm],
    ] {
        let mut evs = prefix.clone();
        evs.push(Cancel);
        assert_eq!(run(&evs), ReplacePhase::Idle, "やめる: {prefix:?}");
    }
    assert_eq!(run(&[Start, Failed]), ReplacePhase::Idle);
}

#[test]
fn ゼロ件のドライランは確認を出さずに畳む() {
    use ReplaceEvent::*;
    assert_eq!(
        run(&[Start, DryRunDone { files: 0, hits: 0 }]),
        ReplacePhase::Done { files: 0, hits: 0 },
        "押しても何も起きない「実行」ボタンを出さない"
    );
}

#[test]
fn 確認待ちから条件を直して数え直せる() {
    use ReplaceEvent::*;
    assert_eq!(
        run(&[Start, DryRunDone { files: 2, hits: 4 }, Start]),
        ReplacePhase::Running
    );
}

#[test]
fn 置換要求の既定はドライラン() {
    // 「置換」を押した時点では 1 バイトも書かない、をエンジン側の既定で担保する
    assert!(file_search::ReplaceRequest::default().dry_run);
}

// ── GAP2: サイドバータブの保存と復元 ──────────────────────────

#[test]
fn サイドバータブは往復して同じものに戻る() {
    for t in [
        SidebarTab::Files,
        SidebarTab::Search,
        SidebarTab::Agents,
        SidebarTab::Sessions,
        SidebarTab::Plugins,
        SidebarTab::Git,
        SidebarTab::GitHub,
    ] {
        assert!(
            SidebarTab::from_key(t.as_key()) == t,
            "往復で変わった: {}",
            t.as_key()
        );
    }
}

#[test]
fn 新タブを足しても古い保存値はそのまま読める() {
    // セッションタブが無かった頃に保存された値
    for (old, want) in [
        ("files", SidebarTab::Files),
        ("search", SidebarTab::Search),
        ("agents", SidebarTab::Agents),
        ("plugins", SidebarTab::Plugins),
        ("git", SidebarTab::Git),
        ("github", SidebarTab::GitHub),
    ] {
        assert!(SidebarTab::from_key(old) == want, "旧値 {old:?} が読めない");
    }
    // 未知 / 空 (フィールドごと無いもっと古いセッション) は既定へ落ちる
    assert!(SidebarTab::from_key("") == SidebarTab::Files);
    assert!(SidebarTab::from_key("unknown-tab") == SidebarTab::Files);
    assert!(SidebarTab::from_key("sessions") == SidebarTab::Sessions);
}

// ── GAP2: SidebarAction の対応表 ──────────────────────────────

fn preset(name: &str, cmd: &str) -> config::AgentPreset {
    config::AgentPreset {
        name: name.into(),
        command: cmd.into(),
        icon: "👾".into(),
        cwd: None,
        env: HashMap::new(),
    }
}

fn past(bin: &str, id: &str, cwd: &str) -> session_picker::PastSession {
    session_picker::PastSession {
        id: id.into(),
        agent_bin: bin.into(),
        started: std::time::UNIX_EPOCH,
        modified: std::time::UNIX_EPOCH,
        summary: "…".into(),
        cwd: PathBuf::from(cwd),
    }
}

#[test]
fn サイドバーの操作は意図した効果へ落ちる() {
    use session_picker::SidebarAction as A;
    // 素のシェルを先に置いても、新規会話はカタログ既知の CLI を選ぶ
    let presets = vec![preset("Shell", ""), preset("Claude Code", "claude")];
    let dir = PathBuf::from("/ws/proj");

    assert_eq!(
        session_sidebar_effect(&A::None, &presets),
        SessionSidebarEffect::Nothing(None)
    );

    // 再開: 会話が走っていたフォルダで、再開指定付きのコマンド
    let s = past("claude", "abc-123", "/ws/other");
    match session_sidebar_effect(&A::Resume(s.clone()), &presets) {
        SessionSidebarEffect::Launch {
            preset,
            command,
            cwd,
        } => {
            assert_eq!(preset, 1, "claude のプリセットを選ぶ");
            assert_eq!(command, session_picker::resume_command("claude", &s));
            assert!(command.contains("abc-123"), "再開 ID が乗る: {command}");
            assert_eq!(cwd, PathBuf::from("/ws/other"), "cwd は会話のフォルダ");
        }
        other => panic!("再開が起動へ落ちていない: {other:?}"),
    }

    // 新規会話: 指定フォルダで、プリセットのコマンドそのまま
    assert_eq!(
        session_sidebar_effect(&A::NewConversation(dir.clone()), &presets),
        SessionSidebarEffect::Launch {
            preset: 1,
            command: "claude".into(),
            cwd: dir.clone(),
        }
    );

    assert_eq!(
        session_sidebar_effect(&A::RevealFolder(dir.clone()), &presets),
        SessionSidebarEffect::Reveal(dir.clone())
    );
    assert_eq!(
        session_sidebar_effect(&A::CloseFolder(dir.clone()), &presets),
        SessionSidebarEffect::RemoveRoot(dir)
    );
}

/// 別の worktree (= 別ブランチ) の会話を再開したら、**その worktree** で起動する。
/// いま開いているフォルダで再開すると、相対パスの指示が全部ずれる。
#[test]
fn 別ブランチの会話はその作業ツリーで再開する() {
    use session_picker::SidebarAction as A;
    let presets = vec![preset("Claude Code", "claude")];
    // いま開いているのは main の worktree、会話は night の worktree のもの
    let night = "/ws/repo/.claude/worktrees/night-2026-07-26";
    let s = past("claude", "sess-night", night);
    match session_sidebar_effect(&A::Resume(s), &presets) {
        SessionSidebarEffect::Launch { cwd, command, .. } => {
            assert_eq!(
                cwd,
                PathBuf::from(night),
                "起動 cwd は会話が走っていた worktree",
            );
            assert!(command.contains("sess-night"));
        }
        other => panic!("再開が起動へ落ちていない: {other:?}"),
    }

    // 起動経路がその cwd をプリセットへ差し替えていることも固定する
    let src = &crate::app::SRC.replace("\r\n", "\n");
    assert!(
        src.contains("p.cwd = Some(cwd.display().to_string());"),
        "launch_preset_with が渡された cwd を使っていない",
    );
    assert!(
        src.contains("self.launch_preset_with(preset, command, &cwd, ctx);"),
        "SessionSidebarEffect::Launch の cwd が起動へ渡っていない",
    );
}

/// ユニバーサルプレビューが**画面から到達できる**こと。
///
/// 「作ったのに繋いでいない」を検出する番人。中央ビューと
/// `code_editor_ui` の二重防御が同じ 1 か所 (`preview_view_ui`) を
/// 通っていないと、片方だけ Kind を足し忘れたときに TextEdit へ
/// バイナリが流れ込む。
#[test]
fn プレビューの各ビューが描画へ配線されている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    for needle in [
        // 3 種のビューが実装されている
        "fn hex_viewer_ui(&mut self, ui: &mut egui::Ui, i: usize)",
        "fn media_card_ui(&mut self, ui: &mut egui::Ui, i: usize)",
        "fn archive_list_ui(&mut self, ui: &mut egui::Ui, i: usize)",
        // 振り分けが 3 種すべてを数え上げている
        "Some(PreviewTag::Hex) => self.hex_viewer_ui(ui, i),",
        "Some(PreviewTag::Media) => self.media_card_ui(ui, i),",
        "Some(PreviewTag::Archive) => self.archive_list_ui(ui, i),",
        "Some(PreviewTag::Multi) => self.multibuffer_ui(ui, i),",
        // 外部オープンとコピーが繋がっている (カードのボタンが死んでいない)
        "open_external(&p.to_string_lossy());",
        "ui.ctx().copy_text(p.to_string_lossy().to_string());",
    ] {
        assert!(src.contains(needle), "配線が切れている: {needle}");
    }
    // 数を数えるので、**この行自体が一致しないように**分けて組み立てる
    // (include_str! はテストコードも含むため、素の文字列だと 1 多く数える)
    let call = format!("self.{}(ui,", "preview_view_ui");
    assert_eq!(
        src.matches(call.as_str()).count(),
        2,
        "中央ビューと code_editor_ui の二重防御が同じ入口を通っていない"
    );
}

/// ブランチ表示は「押せるピッカー」であること (UI から到達できる)。
#[test]
fn ブランチ表示はピッカーとして配線されている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    for needle in [
        // ツールバーのブランチがボタンになっている
        "self.branch_button(ui, theme, b);",
        "let menu = ui.menu_button(RichText::new(label).color(color), |ui| {",
        // 開いている間だけ収集する = 閉じていれば git を起動しない
        "self.branch_nav.ensure_fresh(&ctx);",
        // 選択 → 判断 → 実行、の 3 段が繋がっている
        "self.branch_nav.select(t);",
        "if let Some(target) = self.branch_nav.take_request() {",
        "match snap.plan_switch(&target) {",
        "Ok(argv) => self.branch_nav.start_switch(argv, label, ctx),",
        // 断られたら「変更をレビュー」へ行ける
        "if self.branch_nav.take_review_request() {",
        "self.git_sub_review = true;",
        // 成功後に依存物を作り直す
        "self.after_branch_switch();",
    ] {
        assert!(
            src.contains(needle),
            "ブランチ切り替えの配線が無い: {needle}"
        );
    }
    // stash は絶対に使わない (worktree 間で共有されるため)
    assert!(
        !src.contains("\"stash\""),
        "ブランチ切り替えで git stash を使ってはいけない",
    );
}

/// セッションタブは「いま開いているフォルダの会話」だけを出す。
///
/// ブランチ (worktree) ごとにまとめる表示は**持たない**。同じフォルダを
/// 開いている限り一覧は常に同じ、という一本の規則にするため
/// (VS Code の Claude Code 拡張と同じ切り方)。
#[test]
fn セッション一覧はこのフォルダだけを出す() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    for needle in [
        "sess_action = self.sidebar_sessions_ui(ui, &theme);",
        "self.sess_folders = session_picker::sidebar_folders(&self.roots);",
    ] {
        assert!(src.contains(needle), "セッション一覧の配線が無い: {needle}");
    }
    // ブランチ横断の表示範囲を復活させない (見た目には出にくいので形で固定)。
    // 探すのは**製品コードだけ** — この検査自体が名前を書いているので、
    // テストモジュールまで含めると必ず自分に当たってしまう。
    let prod = src
        .split("\n#[cfg(test)]\nmod ")
        .next()
        .expect("製品コード");
    for gone in [
        "sessions_repo_wide",
        "repo_sidebar_folders",
        "folder_groups",
        "apply_default_collapse",
        "scan_repo_family",
        "repo_worktrees",
    ] {
        assert!(
            !prod.contains(gone),
            "セッション一覧がブランチ単位に戻っている: {gone}"
        );
    }
}

/// 起動しただけでは前回のエージェントが立ち上がらない。
///
/// 過去の会話へ戻る口は「💬 セッション」タブ (明示的に選んで再開) の 1 本。
#[test]
fn 前回のエージェントは既定で復元しない() {
    assert!(
        !config::Config::default().restore_agents,
        "起動しただけで前回の会話が走り出してはいけない",
    );
    let src = &crate::app::SRC.replace("\r\n", "\n");
    assert!(
        src.contains("if self.cfg.restore_agents && !sess.agents.is_empty() {"),
        "復元の入口が設定で切れる形になっていない"
    );
}

#[test]
fn 対応するプリセットが無ければ別の_cli_を起動しない() {
    use session_picker::SidebarAction as A;
    let presets = vec![preset("Claude Code", "claude")];
    match session_sidebar_effect(&A::Resume(past("codex", "x", "/ws")), &presets) {
        SessionSidebarEffect::Nothing(Some(msg)) => {
            assert!(msg.contains("codex"), "どの CLI が無いか言う: {msg}")
        }
        other => panic!("別の CLI で起動してはいけない: {other:?}"),
    }
}

#[test]
fn プリセットが空なら新規会話も起きない() {
    use session_picker::SidebarAction as A;
    let e = session_sidebar_effect(&A::NewConversation(PathBuf::from("/ws")), &[]);
    assert!(matches!(e, SessionSidebarEffect::Nothing(Some(_))));
}

#[test]
fn 既知の_cli_が無ければ先頭プリセットで新規会話を始める() {
    use session_picker::SidebarAction as A;
    let presets = vec![preset("Shell", ""), preset("bash", "bash")];
    assert_eq!(
        session_sidebar_effect(&A::NewConversation(PathBuf::from("/ws")), &presets),
        SessionSidebarEffect::Launch {
            preset: 0,
            command: String::new(),
            cwd: PathBuf::from("/ws"),
        }
    );
}

// ── GAP3: 使用量表示のルール ──────────────────────────────────

fn usage(used: Option<f32>, conf: quota::Confidence) -> quota::AccountUsage {
    quota::AccountUsage {
        account: "acct".into(),
        agents: vec!["claude".into()],
        used_fraction: used,
        confidence: conf,
        resets_at: None,
        running_agents: 1,
        events: Vec::new(),
        projection: quota::Projection::InsufficientData,
    }
}

#[test]
fn 使用率が不明な行には数字を出さない() {
    let s = quota_usage_label(&usage(None, quota::Confidence::Unknown));
    assert_eq!(s, tr("不明"));
    assert!(
        !s.chars().any(|c| c.is_ascii_digit()),
        "測っていない枠に数字を出すと「まだ使っていない」に見える: {s}"
    );
}

#[test]
fn 実測以外の使用率には推定と明記する() {
    let measured = quota_usage_label(&usage(Some(0.42), quota::Confidence::Measured));
    assert!(measured.contains("42"), "{measured}");
    assert!(
        !measured.contains("推定"),
        "実測に推定と書かない: {measured}"
    );
    for c in [quota::Confidence::Estimated, quota::Confidence::Unknown] {
        let s = quota_usage_label(&usage(Some(0.42), c));
        assert!(s.contains("42") && s.contains("推定"), "{c:?}: {s}");
    }
}

#[test]
fn 材料不足の予測はデータ不足と描く() {
    let s = quota_projection_label(quota::Projection::InsufficientData);
    assert_eq!(s, tr("データ不足"));
    assert!(!s.contains('0'), "「0 分」と誤読させない: {s}");
    assert!(!s.contains('分'), "時間として描かない: {s}");
}

#[test]
fn 枯渇予測は分で描き推定と明記する() {
    // 90 秒 → 切り上げて 2 分
    let s = quota_projection_label(quota::Projection::Exhaustion(Duration::from_secs(90)));
    assert!(s.contains('2') && s.contains("推定"), "{s}");
    let r = quota_projection_label(quota::Projection::ResetFirst(Duration::from_secs(600)));
    assert!(r.contains("10"), "{r}");
    assert_eq!(
        quota_projection_label(quota::Projection::NotBurning),
        tr("消費なし")
    );
}

#[test]
fn 深刻さは色分けの記号へ落ちる() {
    // 色だけに頼らず形でも区別する (○ 余裕 / ◇ 注意 / ● 危険)
    assert_eq!(quota_severity_icon(0), "○");
    assert_eq!(quota_severity_icon(1), "◇");
    assert_eq!(quota_severity_icon(2), "●");
    // Advice::severity() が返すのは 0/1/2 だけだが、増えても落ちない
    assert_eq!(quota_severity_icon(9), "●");
}

#[test]
fn 使用量ウォッチは毎フレーム呼んでよい() {
    // refresh_if_stale は TTL 内なら即戻る (UI スレッドから毎フレーム呼ぶ前提)
    let mut w = coordinator::QuotaWatch::new();
    w.set_running(vec![("claude".into(), 2)]);
    assert_eq!(w.running_total(), 2);
    for _ in 0..100 {
        w.refresh_if_stale();
    }
    assert_eq!(w.running_total(), 2, "走行本数の付け替えは何度呼んでも同じ");
}

// ── GAP4: 保存時クリーンアップとカーソル ──────────────────────

#[test]
fn 何も設定していなければ保存で本文を触らない() {
    let opts = editor_ops::SaveCleanup::default();
    assert!(opts.is_noop());
    assert!(save_cleanup_edit("a  \nb", (0, 0), &opts).is_none());
}

#[test]
fn 末尾空白を落とすとカーソルが行末へ寄る() {
    let opts = editor_ops::SaveCleanup {
        trim_trailing: true,
        trim_final_newlines: false,
        final_newline: false,
        target_ending: None,
    };
    //           0123456 789
    let text = "abc   \ndef\n";
    // カーソルが「消える空白の中」(char 5) にいる
    let (out, (s, e)) = save_cleanup_edit(text, (5, 5), &opts).expect("本文が変わる");
    assert_eq!(out, "abc\ndef\n");
    assert_eq!((s, e), (3, 3), "消えた空白の中にいたら新しい行末へ寄せる");
    // 2 行目のカーソルは同じ行・同じ桁のまま (行がずれない)
    let (_, (s2, _)) = save_cleanup_edit(text, (8, 8), &opts).expect("本文が変わる");
    assert_eq!(s2, 5, "2 行目の 2 文字目 (d|ef) のまま");
}

#[test]
fn 最終行の改行はカーソルを動かさない() {
    let opts = editor_ops::SaveCleanup {
        trim_trailing: false,
        trim_final_newlines: false,
        final_newline: true,
        target_ending: None,
    };
    let (out, sel) = save_cleanup_edit("abc", (2, 2), &opts).expect("改行が足される");
    assert_eq!(out, "abc\n");
    assert_eq!(sel, (2, 2));
    // 既に改行で終わっていれば何もしない (空行を増やさない)
    assert!(save_cleanup_edit("abc\n", (2, 2), &opts).is_none());
}

#[test]
fn 日本語の途中でカーソルを割らない() {
    let opts = editor_ops::SaveCleanup {
        trim_trailing: true,
        trim_final_newlines: false,
        final_newline: true,
        target_ending: None,
    };
    let text = "日本語   \nあいう";
    let (out, (s, e)) = save_cleanup_edit(text, (6, 6), &opts).expect("本文が変わる");
    assert_eq!(out, "日本語\nあいう\n");
    assert!(s <= e && e <= out.chars().count(), "範囲が本文の外へ出ない");
    // char → byte 変換が文字境界で成立する (割れていれば別の値になる)
    let b = editor_ops::char_to_byte(&out, e);
    assert!(out.is_char_boundary(b), "多バイト文字の途中を指した");
}

#[test]
fn 選択範囲は両端とも付け替える() {
    let opts = editor_ops::SaveCleanup {
        trim_trailing: true,
        trim_final_newlines: false,
        final_newline: false,
        target_ending: None,
    };
    let text = "aa  \nbb  \n";
    let (out, (s, e)) = save_cleanup_edit(text, (4, 9), &opts).expect("本文が変わる");
    assert_eq!(out, "aa\nbb\n");
    assert!(
        s < e && e <= out.chars().count(),
        "選択が潰れない: ({s}, {e})"
    );
}

/// **保存後にカーソル位置が保たれる。**
///
/// 「末尾空白を削ったせいでカーソルが行頭へ戻る」は保存時整形の
/// 典型的な事故なので、行と桁が保たれることを表で固定する。
#[test]
fn 保存後もカーソルの行と桁が保たれる() {
    let opts = editor_ops::SaveCleanup {
        trim_trailing: true,
        trim_final_newlines: true,
        final_newline: true,
        target_ending: None,
    };
    //           0123 4567 8
    let text = "ab  \ncd  \nef";
    // (元の char 位置, 期待する新しい位置, 説明)
    for (from, want, why) in [
        (0, 0, "1 行目の行頭は動かない"),
        (1, 1, "1 行目の途中はそのまま"),
        (2, 2, "消える空白の直前は行末のまま"),
        (3, 2, "消える空白の中にいたら新しい行末へ"),
        (5, 3, "2 行目の行頭 (改行のぶんだけ前へ)"),
        (6, 4, "2 行目の 2 文字目 (c|d)"),
        (10, 6, "3 行目の行頭"),
    ] {
        let (out, (s, e)) = save_cleanup_edit(text, (from, from), &opts).expect("変わる");
        assert_eq!(out, "ab\ncd\nef\n");
        assert_eq!(s, e, "{why}: 空選択のまま");
        assert_eq!(s, want, "{why}");
        // 行番号が変わっていないこと (= 別の行へ飛んでいない)
        let line_before = editor_ops::line_of_char(text, from);
        let line_after = editor_ops::line_of_char(&out, s);
        assert_eq!(line_before, line_after, "{why}: 行が変わった");
    }
    // 何も削るものが無い本文では保存でカーソルに触れない
    assert!(save_cleanup_edit("ab\ncd\n", (4, 4), &opts).is_none());
}

/// **1 保存 = 取り消し 1 段。**
///
/// 行末空白の除去・末尾の空行の削除・最終行の改行は 1 回の
/// [`editor_ops::apply_save_cleanup_checked`] で合成され、本文の
/// 書き換えも 1 回だけ。段を分けると ⌘Z を 3 回押さないと保存前へ
/// 戻れなくなる (整形が本文の一部として残る)。
#[test]
fn 保存時の整形は本文を一度だけ書き換える() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let after = src
        .split("fn apply_save_cleanup(&mut self, i: usize) {")
        .nth(1)
        .expect("保存時クリーンアップがある");
    let body = &after[..crate::app::method_end(after)];
    assert_eq!(
        body.matches(".apply_edit(").count(),
        1,
        "本文の書き換えが 2 回以上ある (取り消しが 1 段で戻らない)"
    );
    assert_eq!(
        body.matches("save_cleanup_edit(").count(),
        1,
        "整形の合成は 1 か所だけ (空白除去と最終改行を別の段にしない)"
    );
    // 書き出しの前に必ず整形を通す
    let save = src
        .split("fn save_buffer_to(&mut self, i: usize, path: PathBuf) -> bool {")
        .nth(1)
        .expect("保存がある");
    let head = save.split("write_to").next().expect("書き出しがある");
    assert!(
        head.contains("self.apply_save_cleanup(i);"),
        "整形が書き出しより後ろにある"
    );
}

// ── 縦のルーラー (editor.rulers) ──────────────────────────────

/// 設定の桁並びの正規化: 0 桁と重複を落として昇順にする。
#[test]
fn ルーラーの桁は正規化される() {
    assert_eq!(normalize_rulers(&[]), Vec::<usize>::new());
    assert_eq!(normalize_rulers(&[0]), Vec::<usize>::new(), "0 桁は落とす");
    assert_eq!(normalize_rulers(&[120, 80, 80, 0]), vec![80, 120]);
    assert_eq!(normalize_rulers(&[80]), vec![80]);
}

/// ルーラーの x 座標が可用領域からはみ出さず、整数ピクセルに揃う。
#[test]
fn ルーラーは可用領域に収まり整数ピクセルへ揃う() {
    let char_w = 7.3_f32;
    let left = 40.0_f32;
    let clip = egui::Rangef::new(40.0, 400.0);
    for ppp in [1.0_f32, 1.25, 1.5, 2.0] {
        // 複数指定: 収まるものだけが返る
        let xs = ruler_x_positions(&[10, 40, 80, 120], left, char_w, clip, ppp);
        assert!(!xs.is_empty(), "ppp={ppp}: 1 本も返らない");
        for x in &xs {
            assert!(
                *x >= clip.min && *x <= clip.max,
                "ppp={ppp}: 可用領域からはみ出した ({x})"
            );
            // 整数ピクセルに揃っている (小数のままだと桁間隔が揺れる)
            let px = x * ppp;
            assert!(
                (px - px.round()).abs() < 1e-3,
                "ppp={ppp}: 物理ピクセルが整数でない ({px})"
            );
        }
        // 昇順のまま (入力が昇順なら出力も昇順)
        assert!(xs.windows(2).all(|w| w[0] <= w[1]), "ppp={ppp}: 昇順でない");

        // 巨大な桁は 1 本も返らない (無限大や NaN にもならない)
        assert!(ruler_x_positions(&[usize::MAX], left, char_w, clip, ppp).is_empty());
        assert!(ruler_x_positions(&[1_000_000], left, char_w, clip, ppp).is_empty());
        // 0 桁は本文の左端 (= clip の左端) にちょうど乗る
        assert_eq!(ruler_x_positions(&[0], left, char_w, clip, ppp), vec![left]);
        // 文字幅が取れない状況では描かない (0 除算・NaN を作らない)
        assert!(ruler_x_positions(&[80], left, 0.0, clip, ppp).is_empty());
        assert!(ruler_x_positions(&[80], left, f32::NAN, clip, ppp).is_empty());
        // 設定が空なら 1 ピクセルも出さない
        assert!(ruler_x_positions(&[], left, char_w, clip, ppp).is_empty());
    }
}

// ── インデントの切替 (ステータスバー) ─────────────────────────

/// ステータスバーの選択肢が「表示だけ」と「変換する」に分かれていること。
///
/// 変換は本文を書き換えるので、押しただけで走ると事故になる。
/// 経路が 2 本に分かれていることをソースの構造で固定する。
#[test]
fn インデントの切替は表示と変換で経路が分かれている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    for k in [
        "IndentAction::Display(",
        "IndentAction::Convert(",
        "IndentAction::Detect",
        "EditOp::ConvertIndent(",
        "fn indent_menu_ui(",
        "self.apply_indent_action(a, ctx)",
    ] {
        assert!(src.contains(k), "{k} が無い (ステータスバーから届かない)");
    }
}

// ── GAP4: 改行コードの変換 ────────────────────────────────────

#[test]
fn 改行コードの変換はカーソルを保つ() {
    // 変換は editor_op(EditOp::NormalizeEol) が normalize_to +
    // adjust_char_index_after_cleanup を通す。その組み合わせを固定する。
    let text = "one\ntwo\nthree";
    let out = crate::textenc::normalize_to(text, LineEnding::Crlf);
    assert_eq!(out, "one\r\ntwo\r\nthree");
    // "two" の先頭 (char 4) は CRLF 化後 char 5
    let moved = editor_ops::adjust_char_index_after_cleanup(text, &out, 4);
    let b = editor_ops::char_to_byte(&out, moved);
    assert_eq!(&out[b..b + 3], "two");
    // 戻せば元の本文・元の位置
    let back = crate::textenc::normalize_to(&out, LineEnding::Lf);
    assert_eq!(back, text);
    assert_eq!(
        editor_ops::adjust_char_index_after_cleanup(&out, &back, moved),
        4
    );
}

#[test]
fn ステータスバーの表記は文字コードと改行を並べる() {
    // 「UTF-8 / CRLF」の形。混在は内訳付き
    assert_eq!(
        crate::textenc::detect_line_ending("a\r\nb\r\n").label(),
        "CRLF"
    );
    assert_eq!(crate::textenc::detect_line_ending("a\nb\n").label(), "LF");
    let mixed = crate::textenc::detect_line_ending("a\r\nb\nc\n").label();
    assert!(
        mixed.contains("LF") && mixed.contains("混在"),
        "内訳を出す: {mixed}"
    );
}

// ── GAP5: レビューコメントの受け取りは 1 回だけ ────────────────

/// 描画結果 (シェイプ) から `needle` を含むテキストの矩形を探す。
/// ボタンの id を知らなくても「そのボタンを押す」ことができる。
fn text_rect(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<egui::Rect> {
    fn walk(sh: &egui::Shape, needle: &str, out: &mut Option<egui::Rect>) {
        match sh {
            egui::Shape::Text(t) => {
                if out.is_none() && t.galley.job.text.contains(needle) {
                    *out = Some(egui::Rect::from_min_size(t.pos, t.galley.size()));
                }
            }
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, needle, out)),
            _ => {}
        }
    }
    let mut out = None;
    for c in shapes {
        walk(&c.shape, needle, &mut out);
    }
    out
}

#[test]
fn レビューコメントの受け取りは一度きり() {
    let ctx = egui::Context::default();
    let theme = crate::theme::all()[0].clone();
    let files = crate::diff::parse_unified(
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
    );
    assert!(!files.is_empty(), "前提: 差分がパースできる");

    // 「エージェントに送る」が押せる状態のコメントストア
    let mut store = crate::diff::DiffCommentStore::default();
    store.add(
        crate::diff::CommentAnchor::new("a.rs", crate::diff::CommentSide::New, 1),
        "new",
        "ここを直して",
    );
    assert!(!store.prompt().is_empty(), "前提: 追いプロンプトが作れる");

    // panels.rs / race.rs と同じ呼び方 (diff_ui) を再現する。
    // ストアは diff_ui が自分で読む場所へ毎フレーム置き直す。
    let draw = |events: Vec<egui::Event>| -> Vec<egui::epaint::ClippedShape> {
        let raw = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let store = store.clone();
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let id = ui.id().with("zv-diff-comments");
                ui.data_mut(|d| d.insert_temp(id, store.clone()));
                crate::diff::diff_ui(ui, &theme, &files);
            });
        })
        .shapes
    };
    let click = |at: egui::Pos2, pressed: bool| -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    };

    // 1 フレーム描いてボタンの位置を得る
    let shapes = draw(Vec::new());
    let btn = text_rect(&shapes, &tr("エージェントに送る"))
        .expect("「エージェントに送る」ボタンが描かれている");

    // 押していないうちは何も積まれない (誤爆しない)
    assert!(
        crate::diff::take_pending_review_prompt(&ctx).is_none(),
        "押していないのにプロンプトが積まれた"
    );

    // 押して離す = diff_ui が ctx へ 1 件積む
    let _ = draw(click(btn.center(), true));
    let _ = draw(click(btn.center(), false));

    let first =
        crate::diff::take_pending_review_prompt(&ctx).expect("押したらプロンプトが積まれる");
    assert!(
        first.contains("ここを直して"),
        "コメント本文が入る: {first}"
    );
    assert!(first.contains("a.rs"), "対象ファイルが入る: {first}");

    // 2 回目以降は取れない = 毎フレーム拾っても入力欄へ二重に入らない
    for _ in 0..3 {
        assert!(
            crate::diff::take_pending_review_prompt(&ctx).is_none(),
            "同じレビューコメントを 2 回受け取ってはいけない"
        );
    }

    // 受け取り側 (入力欄) も 1 回ぶんだけ増える
    let mut buf = crate::agent_input::AgentInputBuffer::new();
    assert!(buf.append_prompt(&first));
    let after_one = buf.text().to_string();
    for _ in 0..3 {
        if let Some(p) = crate::diff::take_pending_review_prompt(&ctx) {
            buf.append_prompt(&p);
        }
    }
    assert_eq!(buf.text(), after_one, "追いプロンプトが増殖していない");
}

#[test]
fn 空のプロンプトは入力欄へ入れない() {
    // take_pending_review_prompt は空文字を弾く (空の追いプロンプトを作らない)
    let ctx = egui::Context::default();
    assert!(crate::diff::take_pending_review_prompt(&ctx).is_none());
    // AgentInputBuffer 側も空は拒否する = 二重の歯止め
    let mut b = crate::agent_input::AgentInputBuffer::new();
    assert!(!b.append_prompt("   \n  "));
    assert!(b.append_prompt("直して"));
    assert_eq!(b.text(), "直して");
}

// ─── 並ぶウィジェットの ID は「並び順」ではなく「そのものの ID」から作る ───
//
// egui 0.29 の `Button` / `SelectableLabel` は `allocate_*` の**自動採番**から
// ID を作る。行の途中に「状態で出たり消えたりするラベル」(◆ 未読 /
// ⏳ レート制限) や条件付きボタン (🛡 / ⊞) があると、その増減で
// **以降のウィジェットの ID が全部ずれる**。
//
// egui は押した (press) フレームの ID を離した (release) フレームで照合して
// `clicked()` を立てるので、その 2 フレームの間に印が動くと**押したのとは
// 別のウィジェットが発火する**。エージェントは出力のたびに未読印が付いたり
// 消えたりするので実際に踏む — 「Codex のタブを押したのに Claude が選ばれ、
// 打った文章が Claude へ行く」がこれ。
//
// 仕組みそのものの再現と、直ったことの確認は `e2e::widget_id_shift_tests`。
// ここは**実際の配線がその形になっているか**だけを見る (再現テストだけだと、
// 実装が元へ戻っても緑のままになる)。

/// 下部パネルのエージェントタブは、セッション ID で ID を固定する。
#[test]
fn エージェントタブのidはセッションidから作る() {
    let src = &include_str!("../app/bottom_panels.rs").replace("\r\n", "\n");
    let body = src
        .split("for (i, s) in self.agents.sessions.iter().enumerate() {")
        .nth(1)
        .expect("タブ列のループがある");
    let head = &body[..body
        .find("if let Some(i) = set_unread")
        .unwrap_or(body.len())];
    assert!(
        head.contains("ui\n                                        .push_id(s.id, |ui| {"),
        "タブの ID が並び順の自動採番のまま (未読印が 1 個増減しただけで別のタブが選ばれる)"
    );
    // 出たり消えたりするラベルが、固定した ID の**内側**にあること。
    let inner = &head[..head
        .find("})\n                                        .inner")
        .unwrap_or(head.len())];
    for mark in ["s.has_unread()", "s.rate_limited"] {
        assert!(
            inner.contains(mark),
            "{mark} の印が push_id の外にある (外に置くと結局 ID がずれる)"
        );
    }
}

/// Cockpit のタイル見出しと 💬 セッション一覧の行ボタンも同じ約束。
/// ここが緩むと「✕ を押したのに ⟳ が発火する」になる。
#[test]
fn 行ごとのボタンのidはセッションidから作る() {
    for (name, src) in [
        ("cockpit.rs", include_str!("../app/cockpit.rs")),
        ("sidebar_ui.rs", include_str!("../app/sidebar_ui.rs")),
    ] {
        let src = &src.replace("\r\n", "\n");
        // ✕ (閉じる) と ⟳ (再起動) が素の `small_button` で並んでいないこと。
        assert!(
            !src.contains("ui.small_button(\"✕\")"),
            "{name}: ✕ が自動採番のまま (隣のボタンが発火しうる)"
        );
        assert!(
            !src.contains("ui.small_button(\"⟳\")"),
            "{name}: ⟳ が自動採番のまま (隣のボタンが発火しうる)"
        );
        assert!(
            src.contains("ui.push_id((s.id, key)") || src.contains("ui.push_id((sid, key)"),
            "{name}: 行ボタンの ID をセッション ID から作っていない"
        );
    }
}
