use super::*;
use crate::test_util::unique_temp_dir;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir -p");
    std::fs::write(path, body).expect("write file");
}

/// 索引テストの既定条件 (設定の既定値をそのまま使う — 直書きしない)。
fn test_index_opts() -> IndexOptions {
    IndexOptions::from_config(&config::Config::default())
}

/// `.gitignore` に書いたものが索引に載らないこと。
/// 以前はハードコード 10 種しか除外できず、`out/` のような
/// リポジトリ固有の生成物が ⌘P を埋め尽くしていた。
#[test]
fn file_index_respects_gitignore() {
    let base = unique_temp_dir("zaivern-app-test", "gitignore");
    write(
        &base.join(".gitignore"),
        "node_modules/\ntarget/\nout/\n*.log\n",
    );
    write(&base.join("src/main.rs"), "fn main() {}");
    write(&base.join("node_modules/pkg/index.js"), "x");
    write(&base.join("target/debug/app"), "x");
    write(&base.join("out/bundle.js"), "x");
    write(&base.join("debug.log"), "x");

    let roots = file_tree::normalize_roots(vec![base.clone()]);
    let out = build_file_index_with(&roots, &test_index_opts(), None);
    let labels: Vec<&str> = out.files.iter().map(|f| f.label.as_str()).collect();
    assert!(
        labels.contains(&"src/main.rs"),
        "実ソースは載る: {labels:?}"
    );
    for bad in ["node_modules", "target", "out", "debug.log"] {
        assert!(
            !labels.iter().any(|l| l.contains(bad)),
            "{bad} は .gitignore で除外されるべき: {labels:?}"
        );
    }
    assert!(!out.truncated, "上限にはほど遠い");

    // 設定で切れば全部載る (respect_gitignore = false)
    let opts = IndexOptions {
        respect_gitignore: false,
        ..test_index_opts()
    };
    let all = build_file_index_with(&roots, &opts, None);
    assert!(
        all.files.iter().any(|f| f.label.contains("node_modules")),
        "無効化したら除外しない"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// 上限に達したら **黙って切らず** `truncated` を立て、UI へ出す文言を作ること。
#[test]
fn file_index_reports_truncation() {
    let base = unique_temp_dir("zaivern-app-test", "truncate");
    for i in 0..40 {
        write(&base.join(format!("f{i}.txt")), "x");
    }
    let roots = file_tree::normalize_roots(vec![base.clone()]);
    let opts = IndexOptions {
        max_files: 10,
        ..test_index_opts()
    };
    let out = build_file_index_with(&roots, &opts, None);
    assert_eq!(out.files.len(), 10, "上限で止まる");
    assert!(out.truncated, "打ち切りを記録する");

    // 上限を上げれば全部載り、打ち切り表示も出ない
    let out = build_file_index_with(&roots, &test_index_opts(), None);
    assert_eq!(out.files.len(), 40);
    assert!(!out.truncated);

    std::fs::remove_dir_all(&base).ok();
}

/// 深さ上限も設定から来ること (直書きの 12 段を撤廃した)。
#[test]
fn file_index_depth_limit_comes_from_config() {
    let base = unique_temp_dir("zaivern-app-test", "depth");
    write(&base.join("a/b/c/deep.txt"), "x");
    write(&base.join("top.txt"), "x");
    let roots = file_tree::normalize_roots(vec![base.clone()]);

    let shallow = IndexOptions {
        max_depth: 2,
        ..test_index_opts()
    };
    let out = build_file_index_with(&roots, &shallow, None);
    let labels: Vec<&str> = out.files.iter().map(|f| f.label.as_str()).collect();
    assert!(labels.contains(&"top.txt"));
    assert!(!labels.iter().any(|l| l.contains("deep.txt")), "{labels:?}");

    let out = build_file_index_with(&roots, &test_index_opts(), None);
    assert!(out.files.iter().any(|f| f.rel == "a/b/c/deep.txt"));

    std::fs::remove_dir_all(&base).ok();
}

// ── タブのドラッグ並べ替え ────────────────────────────────────

/// 幅 `w` のタブを隙間なく `n` 枚並べた矩形列 (中心は w/2, 3w/2, …)。
fn tab_row(n: usize, w: f32) -> Vec<egui::Rect> {
    (0..n)
        .map(|i| {
            egui::Rect::from_min_max(
                egui::pos2(i as f32 * w, 0.0),
                egui::pos2((i + 1) as f32 * w, 24.0),
            )
        })
        .collect()
}

#[test]
fn タブの落とし先は境界でも範囲外にならない() {
    let w = 100.0_f32;
    let five = tab_row(5, w); // 中心 50,150,250,350,450
                              // 表: (タブ枚数, ポインタ x, 掴んだ添字) → 期待
    let table: &[(usize, f32, usize, Option<usize>)] = &[
        // 左端より左へ引きずる
        (5, -9999.0, 3, Some(0)),
        (5, 0.0, 4, Some(0)),
        // 右端より右へ
        (5, 9999.0, 0, Some(4)),
        (5, 1000.0, 2, Some(4)),
        // 自分自身の位置 (掴んだまま動かさない) → 変更なし
        (5, 50.0, 0, None),
        (5, 250.0, 2, None),
        (5, 449.0, 4, None),
        // 隣へ 1 つ
        (5, 160.0, 0, Some(1)),
        (5, 40.0, 1, Some(0)),
        (5, 260.0, 1, Some(2)),
        // タブ 1 枚 / 0 枚は常に None
        (1, 50.0, 0, None),
        (0, 50.0, 0, None),
        // 壊れた添字 (範囲外) でも panic しない
        (3, 50.0, 99, None),
    ];
    for (n, x, from, want) in table {
        let rects = tab_row(*n, w);
        let got = reorder_target(&rects, *x, *from);
        assert_eq!(got, *want, "reorder_target(n={n}, x={x}, from={from})");
        if let Some(t) = got {
            assert!(t < *n, "落とし先 {t} が {n} 枚の範囲外");
        }
    }
    // 幅が極端に狭く、全タブが同じ位置に重なっている場合
    let stacked: Vec<egui::Rect> = (0..6)
        .map(|_| egui::Rect::from_min_max(egui::pos2(10.0, 0.0), egui::pos2(10.0, 24.0)))
        .collect();
    for from in 0..6 {
        for x in [-1.0_f32, 9.99, 10.0, 10.01, 99.0] {
            let got = reorder_target(&stacked, x, from);
            assert!(
                got.map(|t| t < 6).unwrap_or(true),
                "重なったタブで範囲外 (from={from}, x={x}): {got:?}"
            );
        }
    }
    // 落とし先が決まれば必ず remove+insert が成立する添字であること
    for from in 0..5 {
        for x in [-100.0_f32, 0.0, 99.0, 101.0, 250.0, 499.0, 900.0] {
            if let Some(to) = reorder_target(&five, x, from) {
                let mut v: Vec<usize> = (0..5).collect();
                let e = v.remove(from);
                v.insert(to, e);
                assert_eq!(v.len(), 5, "並べ替えで枚数が変わった");
            }
        }
    }
}

#[test]
fn 並べ替えてもアクティブタブは同じものを指し続ける() {
    // 表: (要素数, from, to)。全ての active について検証する
    let cases: &[(usize, usize, usize)] = &[
        (5, 0, 4), // 先頭を末尾へ
        (5, 4, 0), // 末尾を先頭へ
        (5, 1, 2), // 隣へ
        (5, 2, 1),
        (5, 3, 3), // 動かさない
        (2, 0, 1),
        (2, 1, 0),
    ];
    for (n, from, to) in cases {
        for active in 0..*n {
            // 実際の Vec で同じ移動を行い、値が一致するかで検算する
            let mut v: Vec<usize> = (0..*n).collect();
            let want_value = v[active];
            let e = v.remove(*from);
            v.insert(*to, e);
            let new_active = reorder_active(active, *from, *to);
            assert!(new_active < *n, "アクティブ添字が範囲外");
            assert_eq!(
                v[new_active], want_value,
                "n={n} from={from} to={to} active={active}: 別のタブを指した"
            );
        }
    }
}

#[test]
fn 挿入インジケータは落とし先の側に出る() {
    let rects = tab_row(4, 100.0);
    // 手前へ落とすなら左端、後ろへ落とすなら右端
    assert_eq!(reorder_marker_x(&rects, 3, 1), Some(100.0));
    assert_eq!(reorder_marker_x(&rects, 1, 3), Some(400.0));
    assert_eq!(reorder_marker_x(&rects, 2, 2), Some(200.0));
    // 範囲外は描かない (panic しない)
    assert_eq!(reorder_marker_x(&rects, 0, 99), None);
    assert_eq!(reorder_marker_x(&[], 0, 0), None);
}

/// ドロップ後の状態を実際の `Vec` で通しで確かめる (index と ID の取り違え検出)。
#[test]
fn ドロップ後もつかんでいたタブがアクティブのまま() {
    let names = ["a.rs", "b.rs", "c.rs", "d.rs"];
    let rects = tab_row(names.len(), 100.0);
    // c.rs (添字 2) を掴んで一番左へ落とす
    let from = 2usize;
    let to = reorder_target(&rects, -50.0, from).expect("左端へ動く");
    assert_eq!(to, 0);
    let mut v: Vec<&str> = names.to_vec();
    let e = v.remove(from);
    v.insert(to, e);
    assert_eq!(v, vec!["c.rs", "a.rs", "b.rs", "d.rs"]);
    assert_eq!(
        v[reorder_active(from, from, to)],
        "c.rs",
        "掴んだタブのまま"
    );
    // 掴んだのが非アクティブ (d.rs がアクティブ) でも指し先は変わらない
    assert_eq!(v[reorder_active(3, from, to)], "d.rs");
    // a.rs (元 0) は 1 つ右へずれる
    assert_eq!(v[reorder_active(0, from, to)], "a.rs");
}

// ── フレームガードの panic ポリシー ────────────────────────────

use FrameGuardAction::{Abort, Continue, Quarantine};
use FrameOutcome::{Ok as F_OK, Panic as F_PANIC};

/// フレーム結果の並びを `step_ms` 間隔で流し込み、判断の並びを得る。
fn run_policy(seq: &[FrameOutcome], step_ms: u64) -> Vec<FrameGuardAction> {
    let mut p = FramePanicPolicy::default();
    seq.iter()
        .enumerate()
        .map(|(i, o)| p.record(*o, i as u64 * step_ms))
        .collect()
}

/// 「panic / ok」を `pairs` 組ならべる (今日の実装が永久ループする形)。
fn flapping(pairs: usize) -> Vec<FrameOutcome> {
    (0..pairs).flat_map(|_| [F_PANIC, F_OK]).collect()
}

/// DoD: フレームの panic ポリシーは表で決まる。
///
/// とくに **panic → ok → panic → ok …** (たまに成功する panic) が
/// 永久に「半分だけ描いた画面」を作り続けないこと。
/// 旧実装は 1 フレーム完走するたびにカウンタが 0 に戻るため、
/// この形を永久に検知できなかった。
#[test]
fn frame_panic_policy_table() {
    // (名前, 入力, フレーム間隔 ms, 期待する判断の並び)
    let table: Vec<(&str, Vec<FrameOutcome>, u64, Vec<FrameGuardAction>)> = vec![
        (
            "健全: 完走だけならいつまでも継続",
            vec![F_OK; 1000],
            16,
            vec![Continue; 1000],
        ),
        (
            "単発 panic は隔離まで上げない",
            vec![F_OK, F_PANIC, F_OK, F_OK],
            16,
            vec![Continue, Continue, Continue, Continue],
        ),
        (
            "3 連続 panic は従来どおり即中止",
            vec![F_PANIC, F_PANIC, F_PANIC, F_OK],
            16,
            vec![Continue, Continue, Abort, Continue],
        ),
        (
            "2 連続で止まれば中止しない",
            vec![F_PANIC, F_PANIC, F_OK, F_OK],
            16,
            vec![Continue, Continue, Continue, Continue],
        ),
        (
            "時間窓を跨いだ panic は減衰して数えない (数時間動かしても落ちない)",
            vec![F_PANIC, F_OK, F_PANIC, F_OK, F_PANIC, F_OK],
            60_000,
            vec![Continue; 6],
        ),
        (
            "ちらつく panic: 3 回目で隔離へ上げる",
            flapping(3),
            16,
            vec![Continue, Continue, Continue, Continue, Quarantine, Continue],
        ),
    ];
    for (name, seq, step, want) in table {
        assert_eq!(run_policy(&seq, step), want, "{name}");
    }
}

/// DoD: 隔離しても収まらないちらつき panic は、最後の手段として中止へ落ちる
/// = 崩れた画面を延々と描き続けることは絶対にない。
#[test]
fn frame_panic_policy_flapping_eventually_aborts() {
    let seq = flapping(40);
    let got = run_policy(&seq, 16);
    let first_q = got.iter().position(|a| *a == Quarantine);
    assert_eq!(first_q, Some(4), "3 回目の panic (添字 4) で隔離へ上げる");
    let abort = got
        .iter()
        .position(|a| *a == Abort)
        .expect("いつかは中止する");
    // 隔離 3 回 (= panic 9 回) までは粘り、その先で諦める
    assert!(
        (18..=24).contains(&abort),
        "隔離を {} 回試してから中止する想定 (中止位置={abort})",
        FRAME_PANIC_MAX_QUARANTINES
    );
    assert_eq!(
        got.iter().filter(|a| **a == Quarantine).count(),
        FRAME_PANIC_MAX_QUARANTINES as usize,
        "中止までに出す隔離の回数は上限どおり"
    );
}

/// DoD: 長く安定したら隔離回数の記憶も捨てる (無関係な panic の積み上げで
/// ある日いきなり落ちる、を防ぐ)。
#[test]
fn frame_panic_policy_forgets_after_long_calm() {
    let mut p = FramePanicPolicy::default();
    // ちらつきで隔離まで上げる
    for i in 0..5 {
        p.record(if i % 2 == 0 { F_PANIC } else { F_OK }, i as u64 * 16);
    }
    assert_eq!(p.quarantines, 1);
    // 十分に長く完走し続ける
    let base = 100_000;
    for i in 0..FRAME_CLEAN_STREAK_RESET {
        p.record(F_OK, base + i as u64 * 16);
    }
    assert_eq!(p.quarantines, 0, "落ち着いたら隔離の記憶は捨てる");
    assert!(p.recent.is_empty(), "時間窓の panic も消えている");
}

/// DoD: 印が取れないときは panic メッセージの位置情報から犯人を推測する。
#[test]
fn panic_message_attribution() {
    assert_eq!(
        subview_from_panic_message("index out of bounds at src/terminal.rs:42:9"),
        Some(Subview::Panel("terminal"))
    );
    assert_eq!(
        subview_from_panic_message("file_tree.rs:7:1: unwrap on None"),
        Some(Subview::Panel("sidebar"))
    );
    // ただの単語では誤爆しない (位置情報の形のときだけ拾う)
    assert_eq!(subview_from_panic_message("terminal is busy"), None);
    assert_eq!(subview_from_panic_message(""), None);
}

/// DoD: 印 (パンくず) は panic すると残り、完走すると消える。
#[test]
fn drawing_subview_breadcrumb_survives_panic() {
    let _ = take_drawing_subview();
    // 完走したら消える
    draw_subview(Subview::Panel("editor"), || {});
    assert_eq!(take_drawing_subview(), None);
    // panic したら残る = 犯人が分かる
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        draw_subview(Subview::Session(7), || panic!("boom"));
    }));
    assert!(r.is_err());
    assert_eq!(take_drawing_subview(), Some(Subview::Session(7)));
    assert_eq!(take_drawing_subview(), None, "取り出したら消える");

    // 入れ子: 内側が無事なら外側の印へ戻る (タイルの後にコックピットで
    // 壊れたら「コックピット」として拾えること)
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        draw_subview(Subview::Panel("cockpit"), || {
            draw_subview(Subview::Session(1), || {});
            panic!("after the tile");
        });
    }));
    assert!(r.is_err());
    assert_eq!(take_drawing_subview(), Some(Subview::Panel("cockpit")));
}

/// DoD: 隔離の判断が出たときだけ、犯人が隔離リストへ入る。
#[test]
fn frame_guard_quarantines_only_the_culprit() {
    let mut g = FrameGuard::default();
    let victim = Subview::Session(3);
    let other = Subview::Session(4);
    // ちらつき: 3 回目の panic で隔離
    let mut act = FrameGuardAction::Continue;
    for i in 0..5u64 {
        let outcome = if i % 2 == 0 { F_PANIC } else { F_OK };
        act = g.observe(outcome, Some(victim.clone()), i * 16);
    }
    assert_eq!(act, Quarantine);
    assert!(g.is_quarantined(&victim), "犯人だけ描画から外す");
    assert!(!g.is_quarantined(&other), "巻き添えにしない");
    // 「再試行」で元へ戻る
    g.reset();
    assert!(!g.is_quarantined(&victim));
    assert!(g.banner.is_none());
}

// ── ファイルダイアログのジョブ ─────────────────────────────────

/// DoD: ダイアログのジョブは 要求 → 実行中 (同じ用途の再要求は無視) →
/// 結果を適用 → idle、と一巡する。本物のダイアログは開かない。
#[test]
fn dialog_job_state_machine() {
    let mut jobs = DialogJobs::new();
    assert!(jobs.poll().is_none(), "idle では取り出すものが無い");
    assert!(!jobs.busy());

    // 要求 → 受理
    let tx = jobs
        .begin(DialogKind::OpenFile)
        .expect("最初の要求は受理される");
    assert!(jobs.busy());
    // 実行中: 同じ用途の再要求は無視 (二重に開かない)
    assert!(
        jobs.begin(DialogKind::OpenFile).is_none(),
        "同じ用途の二重オープンは無視する"
    );
    // 別の用途は同時に開ける
    let tx2 = jobs.begin(DialogKind::OpenFolder).expect("別用途は独立");

    // 結果が届く → 取り出せて、その用途だけ待ちが解ける
    tx.send(DialogOutcome {
        purpose: DialogPurpose::OpenFile,
        path: Some(PathBuf::from("/tmp/zv/a.txt")),
    })
    .expect("送信できる");
    let out = jobs.poll().expect("結果が届く");
    assert_eq!(out.purpose, DialogPurpose::OpenFile);
    assert_eq!(out.path.as_deref(), Some(Path::new("/tmp/zv/a.txt")));
    assert!(jobs.busy(), "もう一方はまだ開いている");
    assert!(
        jobs.begin(DialogKind::OpenFile).is_some(),
        "受け取ったら同じ用途をまた開ける"
    );

    // キャンセル (path=None) — 待ちは解けるが、適用すべきパスは無い
    tx2.send(DialogOutcome {
        purpose: DialogPurpose::OpenFolder,
        path: None,
    })
    .expect("送信できる");
    let cancelled = jobs.poll().expect("キャンセルも結果として届く");
    assert_eq!(cancelled.path, None, "キャンセルは何も適用しない");
    assert!(
        jobs.begin(DialogKind::OpenFolder).is_some(),
        "キャンセル後も次を開ける"
    );
}

/// DoD: 「名前を付けて保存」は添字ではなくバッファ ID を運ぶ
/// (ダイアログが開いている間にタブの並びが変わっても、正しいタブへ保存する)。
#[test]
fn save_as_dialog_carries_buffer_identity_and_follow_ups() {
    let purpose = DialogPurpose::SaveAs {
        buffer_id: 42,
        close_after: true,
        run_hooks: false,
    };
    assert_eq!(purpose.kind(), DialogKind::SaveAs);
    // 用途キーは同じなので、保存ダイアログは常に 1 枚しか開かない
    let other = DialogPurpose::SaveAs {
        buffer_id: 7,
        close_after: false,
        run_hooks: true,
    };
    assert_eq!(other.kind(), purpose.kind());
    let mut jobs = DialogJobs::new();
    assert!(jobs.begin(purpose.kind()).is_some());
    assert!(
        jobs.begin(other.kind()).is_none(),
        "保存ダイアログは 1 枚まで"
    );
}

/// DoD: ダイアログの組み立て材料はスレッドへ送れる (Send) 素材だけを持つ。
#[test]
fn dialog_spec_is_sendable_and_builds_what_was_asked() {
    fn assert_send<T: Send>(_: &T) {}
    let spec = DialogSpec::pick_file()
        .directory(PathBuf::from("/tmp/zv"))
        .filter("画像", &["png", "webp"]);
    assert_send(&spec);
    assert_eq!(spec.mode, DialogMode::PickFile);
    assert_eq!(spec.directory.as_deref(), Some(Path::new("/tmp/zv")));
    assert_eq!(
        spec.filter,
        Some((
            "画像".to_string(),
            vec!["png".to_string(), "webp".to_string()]
        ))
    );
    assert_eq!(DialogSpec::pick_folder().mode, DialogMode::PickFolder);
    assert_eq!(DialogSpec::save_file().mode, DialogMode::SaveFile);
    assert_send(&DialogPurpose::OpenFile);
}

/// DoD: 隔離エージェントは再起動後も**同じ worktree** に戻る。
/// セッション記録 → 割り当ての復元が壊れると、次の起動で本体ツリーへ
/// 落ちて隔離が黙って無効になる (一番気付きにくい壊れ方)。
#[test]
fn 隔離エージェントは再起動後も同じworktreeへ戻る() {
    let base = crate::test_util::unique_temp_dir("zaivern-app-test", "restore-wt");
    let wt_dir = base.join("repo-agent-claude-code-1");
    std::fs::create_dir_all(&wt_dir).expect("worktree のふり");
    let rec = session::AgentSessionRec {
        preset_name: "Claude Code".into(),
        cwd: wt_dir.to_string_lossy().into_owned(),
        worktree_repo: base.join("repo").to_string_lossy().into_owned(),
        worktree_branch: "agent/claude-code-1".into(),
        ..Default::default()
    };
    let got = restored_worktree(&rec).expect("隔離として復元される");
    assert_eq!(got.dir, wt_dir, "前回と同じ worktree へ戻る");
    assert_eq!(got.branch, "agent/claude-code-1");
    assert_eq!(got.repo, base.join("repo"));

    // 隔離していない記録は None (通常起動のまま)
    let plain = session::AgentSessionRec {
        cwd: wt_dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    assert!(restored_worktree(&plain).is_none());

    // worktree を手で消した後の記録は None。
    // ここで Some を返すと、実体の無いフォルダへ `git worktree remove` を撃つ。
    std::fs::remove_dir_all(&wt_dir).ok();
    assert!(
        restored_worktree(&rec).is_none(),
        "消えた worktree を掴んでいる"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// DoD: エージェントは「直近に開いたフォルダ」で起動する。
/// 起動引数のフォルダ (`zai .`) も、あとから開き直したフォルダも同じ扱い。
#[test]
fn agent_cwd_follows_the_folder_you_opened() {
    let base = unique_temp_dir("zaivern-app-test", "agent-cwd");
    let a = base.join("alpha");
    let b = base.join("beta");
    std::fs::create_dir_all(a.join("src")).expect("mkdir a");
    std::fs::create_dir_all(&b).expect("mkdir b");
    let roots = file_tree::normalize_roots(vec![a.clone(), b.clone()]);
    assert_eq!(roots.len(), 2, "別ツリーの 2 ルート");
    let (a, b) = (roots[0].clone(), roots[1].clone());

    // 未指定なら従来どおり primary ルート
    assert_eq!(agent_cwd_from(&roots, None), a);
    // 開いたフォルダ (= ルート) がそのまま起動先になる
    assert_eq!(agent_cwd_from(&roots, Some(&b)), b);
    // ルート配下のサブフォルダも有効 (そのフォルダで起動する)
    let sub = a.join("src");
    assert_eq!(agent_cwd_from(&roots, Some(&sub)), sub);
    // ワークスペースから外したフォルダは採用しない (primary へ落とす)
    assert_eq!(agent_cwd_from(&roots[..1], Some(&b)), a);
    // 消えたフォルダも採用しない — 存在しない cwd では起動できない
    std::fs::remove_dir_all(&b).expect("rmdir b");
    assert_eq!(agent_cwd_from(&roots, Some(&b)), a);
    // ルートが空でも "." に落ちて起動先を必ず 1 つ返す
    assert_eq!(agent_cwd_from(&[], None), PathBuf::from("."));

    std::fs::remove_dir_all(&base).ok();
}

/// DoD: `zai <フォルダ>` で開いたフォルダは、セッション復元でルートが増えても
/// primary のまま = エージェントの起動先のまま。
#[test]
fn restoring_a_wider_workspace_keeps_the_launched_folder_primary() {
    let a = PathBuf::from("/ws/alpha");
    let b = PathBuf::from("/ws/beta");
    let c = PathBuf::from("/ws/gamma");

    // `zai beta` 起動 + 保存構成 [alpha, beta] → beta を先頭に戻して復元する
    let restored = restored_roots(std::slice::from_ref(&b), vec![a.clone(), b.clone()])
        .expect("より広い構成は復元する");
    assert_eq!(
        restored,
        vec![b.clone(), a.clone()],
        "開いたフォルダが primary"
    );

    // 現在のルートを含まない保存構成は別ワークスペース扱いで復元しない
    assert!(restored_roots(std::slice::from_ref(&c), vec![a.clone(), b.clone()]).is_none());
    // 同じ広さ / より狭い保存構成も触らない
    assert!(restored_roots(std::slice::from_ref(&b), vec![b.clone()]).is_none());
    assert!(restored_roots(&[b.clone(), a.clone()], vec![a.clone(), b.clone()]).is_none());
}

/// DoD: 2 つのルートに同じ相対パス (`src/main.rs`) があっても、
/// あいまい検索から「正しい方のファイル」が開けること。
#[test]
fn two_roots_with_same_relative_path_resolve_to_distinct_files() {
    let base = unique_temp_dir("zaivern-app-test", "collide");
    let a = base.join("alpha");
    let b = base.join("beta");
    write(&a.join("src/main.rs"), "fn main() { /* ALPHA */ }");
    write(&b.join("src/main.rs"), "fn main() { /* BETA */ }");
    // 片方にしか無いファイルは曖昧でないのでラベルにルート名が付かない
    write(&a.join("only_in_alpha.rs"), "x");

    let roots = file_tree::normalize_roots(vec![a.clone(), b.clone()]);
    assert_eq!(roots.len(), 2, "別ツリーの 2 ルートは畳まれない");
    let index = build_file_index_with(&roots, &test_index_opts(), None).files;

    // 衝突する rel は両方ともルート名付きラベルになる
    let mains: Vec<&IndexedFile> = index.iter().filter(|f| f.rel == "src/main.rs").collect();
    assert_eq!(mains.len(), 2, "両方のルートから索引される");
    let mut labels: Vec<&str> = mains.iter().map(|f| f.label.as_str()).collect();
    labels.sort();
    assert_eq!(labels, ["alpha/src/main.rs", "beta/src/main.rs"]);

    // 衝突しない rel はルート名を付けない (単一ルート時と同じ見え方)
    let only = index
        .iter()
        .find(|f| f.rel == "only_in_alpha.rs")
        .expect("indexed");
    assert_eq!(only.label, "only_in_alpha.rs", "曖昧でなければ素の相対パス");

    // ★ 本丸: ラベルから開くと「その」ファイルの中身が読める
    for f in &mains {
        assert!(f.abs.is_absolute(), "索引は絶対パスを正として持つ");
        let body = std::fs::read_to_string(&f.abs).expect("read indexed file");
        let expected = if f.label.starts_with("alpha/") {
            "ALPHA"
        } else {
            "BETA"
        };
        assert!(
            body.contains(expected),
            "{} を開くと {} 側の中身であるべき (実際: {body})",
            f.label,
            expected,
        );
    }

    // あいまい検索は相対パスに対して効く (単一ルート時と同じ品質)
    let hits: Vec<&IndexedFile> = index
        .iter()
        .filter(|f| fuzzy::score("srcmain", &f.rel).is_some())
        .collect();
    assert_eq!(hits.len(), 2, "両方が候補に出る");

    std::fs::remove_dir_all(&base).ok();
}

/// 単一ルートでは索引のラベルが従来どおり素の相対パスであること (非退行)。
#[test]
fn single_root_index_labels_are_plain_relative_paths() {
    let base = unique_temp_dir("zaivern-app-test", "single");
    write(&base.join("src/main.rs"), "fn main() {}");
    write(&base.join("README.md"), "# hi");

    let roots = file_tree::normalize_roots(vec![base.clone()]);
    let index = build_file_index_with(&roots, &test_index_opts(), None).files;
    let mut labels: Vec<&str> = index.iter().map(|f| f.label.as_str()).collect();
    labels.sort();
    assert_eq!(labels, ["README.md", "src/main.rs"]);
    assert!(index.iter().all(|f| f.label == f.rel));
    assert!(index.iter().all(|f| f.abs.is_absolute()));

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn roots_label_shortens_for_many_roots() {
    assert_eq!(roots_label(&[]), "");
    assert_eq!(roots_label(&[PathBuf::from("/x/alpha")]), "alpha");
    assert_eq!(
        roots_label(&[PathBuf::from("/x/alpha"), PathBuf::from("/y/beta")]),
        "alpha, beta"
    );
    assert_eq!(
        roots_label(&[
            PathBuf::from("/x/alpha"),
            PathBuf::from("/y/beta"),
            PathBuf::from("/z/gamma"),
            PathBuf::from("/w/delta"),
        ]),
        "alpha, beta (+2)"
    );
    assert_eq!(
        workspace_title(&[PathBuf::from("/x/alpha")]),
        "Zaivern Code — alpha"
    );
}

#[test]
fn parses_gitdir_from_worktree_dot_git_file() {
    let ws = Path::new("/repo/.claude/worktrees/feature");

    // linked worktree の `.git` ファイル (git が書く形式は絶対パス + 末尾改行)
    let abs = "gitdir: /repo/.git/worktrees/feature\n";
    assert_eq!(
        parse_gitdir_file(abs, ws),
        Some(PathBuf::from("/repo/.git/worktrees/feature"))
    );

    // 相対パスは workspace 基準で解決する
    let rel = "gitdir: ../../../.git/worktrees/feature\n";
    assert_eq!(
        parse_gitdir_file(rel, ws),
        Some(ws.join("../../../.git/worktrees/feature"))
    );

    // gitdir 行が無い / 空なら None (通常の .git ディレクトリへフォールバックする)
    assert_eq!(parse_gitdir_file("ref: refs/heads/main\n", ws), None);
    assert_eq!(parse_gitdir_file("gitdir:   \n", ws), None);
    assert_eq!(parse_gitdir_file("", ws), None);
}

#[test]
fn git_head_path_falls_back_to_dot_git_dir() {
    // `.git` が存在しない (=ファイルでない) 場合は従来どおり <ws>/.git/HEAD
    let ws = Path::new("/no/such/workspace");
    assert_eq!(git_head_path(ws), ws.join(".git").join("HEAD"));
}

#[test]
fn which_cache_rechecks_when_never_checked() {
    // 未確認なら必ず which() を実行する(初回は元の挙動と同じ)
    let now = Instant::now();
    assert!(!which_result_is_fresh(None, now, WHICH_MISS_TTL));
}

#[test]
fn which_cache_suppresses_repeat_within_ttl() {
    // TTL 以内の再確認は省く = 毎フレームのサブプロセス生成が消える
    let now = Instant::now();
    let just_now = now - Duration::from_millis(1);
    assert!(which_result_is_fresh(Some(just_now), now, WHICH_MISS_TTL));
}

#[test]
fn which_cache_expires_after_ttl() {
    // TTL を過ぎたら再確認する = 起動後にインストールしても いずれ 認識される
    let now = Instant::now();
    let old = now - WHICH_MISS_TTL - Duration::from_millis(1);
    assert!(!which_result_is_fresh(Some(old), now, WHICH_MISS_TTL));
    // 境界(ちょうど TTL)も再確認側に倒す
    assert!(!which_result_is_fresh(
        Some(now - WHICH_MISS_TTL),
        now,
        WHICH_MISS_TTL
    ));
}

#[test]
fn which_cache_ttl_is_short_enough_to_feel_immediate() {
    assert!(WHICH_MISS_TTL <= Duration::from_secs(5));
}

#[test]
fn joins_japanese_without_spaces() {
    // 息継ぎごとに区切って書き足しても、日本語は分かち書きにならない
    assert!(!needs_space(Some('る'), Some('修')));
    assert!(!needs_space(Some('。'), Some('あ')));
    assert!(!needs_space(Some('た'), Some('。')));
}

#[test]
fn separates_english_words() {
    assert!(needs_space(Some('o'), Some('w')));
    assert!(needs_space(Some('.'), Some('T')));
}

#[test]
fn no_space_at_the_start_or_next_to_existing_space() {
    // 先頭 (まだ何も送っていない)
    assert!(!needs_space(None, Some('a')));
    assert!(!needs_space(Some('a'), None));
    // すでに空白があるところへ重ねない
    assert!(!needs_space(Some(' '), Some('a')));
}

#[test]
fn mixed_scripts_follow_the_japanese_side() {
    // 日本語と英語が隣り合うときは詰める (「Rustで」を割らない)
    assert!(!needs_space(Some('t'), Some('で')));
    assert!(!needs_space(Some('を'), Some('R')));
}

#[test]
fn streaming_appends_only_the_new_tail() {
    // 話し進めているだけの間は、増えたぶんを足すだけで消さない
    assert_eq!(diff_edit("", "こん"), (0, "こん".into()));
    assert_eq!(diff_edit("こん", "こんにちは"), (0, "にちは".into()));
}

#[test]
fn streaming_rewrites_only_what_changed() {
    // 変換が確定して後ろが書き換わったケース。共通する先頭は残す
    // (「きょうは」まで同じ → 「いいてんき」3 文字を消して「良い天気」を書く)
    assert_eq!(
        diff_edit("きょうはいい", "きょうは良い"),
        (2, "良い".into())
    );
    // 文字数は「バイト数」ではなく「文字数」で数える (日本語が壊れないこと)
    let (del, add) = diff_edit("あいうえお", "あい");
    assert_eq!((del, add.as_str()), (3, ""));
}

#[test]
fn streaming_is_a_noop_when_nothing_changed() {
    // 同じ partial が続けて届いても端末へは何も送らない
    assert_eq!(diff_edit("こんにちは", "こんにちは"), (0, String::new()));
}

#[test]
fn streaming_erases_everything_when_the_head_changes() {
    // 先頭から変わったら全部消して書き直す
    assert_eq!(diff_edit("abc", "xyz"), (3, "xyz".into()));
}

#[test]
fn streaming_handles_the_separator_space_as_part_of_the_text() {
    // 区切りの空白も live に含めて数えるので、書き換えても空白が消えない
    assert_eq!(diff_edit(" and", " and then"), (0, " then".into()));
}

/// 届け先セッションの id (テスト用の適当な値)
const DEST: u64 = 1;

#[test]
fn second_utterance_continues_in_the_same_field() {
    let mut v = VoiceState::default();

    // 1 回目 — 話しながら partial が伸びていく。増えたぶんだけ書き足す
    let e = v.plan("こん", DEST);
    assert_eq!((e.del, e.add.as_str()), (0, "こん"));
    v.commit(e, false, false, DEST);
    let e = v.plan("こんにちは", DEST);
    assert_eq!((e.del, e.add.as_str()), (0, "にちは"));
    v.commit(e, false, false, DEST);

    // 確定。中身は最後の partial と同じで送るバイトは無いが、
    // ここで追跡を締めないと 2 回目の発話が 1 回目を消してしまう
    let e = v.plan("こんにちは", DEST);
    assert!(e.is_noop());
    v.commit(e, true, false, DEST);
    assert!(v.live.is_empty(), "確定した分は書き換え対象から外れること");

    // 2 回目 — 前の文を 1 文字も消さずに、その後ろへ書き足す
    let e = v.plan("さようなら", DEST);
    assert_eq!((e.del, e.add.as_str()), (0, "さようなら"));
}

#[test]
fn second_utterance_is_spaced_in_english_and_stays_spaced() {
    let mut v = VoiceState::default();
    let e = v.plan("hello", DEST);
    v.commit(e, true, false, DEST);

    // 続きの発話は単語がつながらないよう空白を挟む
    let e = v.plan("world", DEST);
    assert_eq!((e.del, e.add.as_str()), (0, " world"));
    v.commit(e, false, false, DEST);

    // 途中で認識が変わっても区切りの空白は据え置き (" world" → " word")
    let e = v.plan("word", DEST);
    assert_eq!((e.del, e.add.as_str()), (2, "d"));
    assert_eq!(e.want, " word");
}

#[test]
fn submitting_starts_the_next_utterance_from_scratch() {
    let mut v = VoiceState::default();
    let e = v.plan("送ります", DEST);
    v.commit(e, true, true, DEST);
    // Enter を送ったので入力欄は空 — 消す文字も区切りの空白も無い
    assert!(v.live.is_empty());
    assert_eq!(v.last_char, None);
    assert_eq!(v.last_sent_to, None);
    let e = v.plan("次の話", DEST);
    assert_eq!((e.del, e.add.as_str()), (0, "次の話"));
}

#[test]
fn switching_destination_does_not_backspace_the_new_one() {
    let mut v = VoiceState::default();
    let e = v.plan("前の宛先へ", DEST);
    v.commit(e, false, false, DEST);

    // 宛先が変わったら追跡を捨てる (apply_voice_text がやること)
    v.live.clear();
    v.last_char = None;
    // 別セッションへは先頭から書き出す。空白も Backspace も入らない
    let e = v.plan("新しい宛先へ", 2);
    assert_eq!((e.del, e.add.as_str()), (0, "新しい宛先へ"));
}

/// テスト用の入力欄シミュレータ。端末へ送ったバイト列を実際に当ててみる。
/// 0x7f で末尾を 1 文字消し、残りは書き足す (`\r` は送信 = 空になる)。
fn apply_bytes(field: &mut String, bytes: &[u8]) {
    let del = bytes.iter().take_while(|b| **b == 0x7f).count();
    for _ in 0..del {
        field.pop();
    }
    let rest = &bytes[del..];
    if rest.last() == Some(&b'\r') {
        field.clear();
        return;
    }
    field.push_str(std::str::from_utf8(rest).unwrap());
}

#[test]
fn dictation_lands_in_the_field_as_spoken() {
    // 実際の認識の流れを再現する: 話しながら変換が書き換わり、息継ぎで確定し、
    // 2 回目の発話がその後ろへ続く。入力欄に残る文字列を突き合わせる。
    let mut v = VoiceState::default();
    let mut field = String::new();
    let step = |v: &mut VoiceState, field: &mut String, text: &str, is_final: bool| {
        let e = v.plan(text, DEST);
        apply_bytes(field, &e.bytes(false));
        v.commit(e, is_final, false, DEST);
    };

    // 1 回目 — 「せかい」が「世界」へ変換されても二重にならない
    step(&mut v, &mut field, "こんにちは", false);
    assert_eq!(field, "こんにちは");
    step(&mut v, &mut field, "こんにちはせかい", false);
    assert_eq!(field, "こんにちはせかい");
    step(&mut v, &mut field, "こんにちは世界", false);
    assert_eq!(field, "こんにちは世界");
    // 確定 — 中身は直前と同じなので端末へは何も送らない
    step(&mut v, &mut field, "こんにちは世界", true);
    assert_eq!(field, "こんにちは世界");

    // 2 回目 — 1 回目を 1 文字も消さずに後ろへ続く
    step(&mut v, &mut field, "これは", false);
    assert_eq!(field, "こんにちは世界これは");
    step(&mut v, &mut field, "これは二回目です", false);
    step(&mut v, &mut field, "これは二回目です", true);
    assert_eq!(field, "こんにちは世界これは二回目です");

    // 3 回目まで続けても崩れない
    step(&mut v, &mut field, "さらに三回目", false);
    step(&mut v, &mut field, "さらに三回目も", true);
    assert_eq!(field, "こんにちは世界これは二回目ですさらに三回目も");
}

#[test]
fn english_dictation_keeps_words_apart() {
    let mut v = VoiceState::default();
    let mut field = String::new();
    for (text, is_final) in [
        ("hello", false),
        ("hello", true),
        ("world", false),
        ("world", true),
    ] {
        let e = v.plan(text, DEST);
        apply_bytes(&mut field, &e.bytes(false));
        v.commit(e, is_final, false, DEST);
    }
    assert_eq!(field, "hello world");
}

#[test]
fn edit_bytes_are_backspaces_then_text() {
    let e = VoiceEdit {
        del: 2,
        add: "は".into(),
        want: "は".into(),
        space: false,
    };
    let mut want = b"\x7f\x7f".to_vec();
    want.extend_from_slice("は".as_bytes());
    assert_eq!(e.bytes(false), want);
    // 合図キーワードで送信するときだけ Enter が付く
    want.push(b'\r');
    assert_eq!(e.bytes(true), want);
}

#[test]
fn reset_live_forgets_what_was_written() {
    // ユーザーが手で Enter を押した後などに呼ぶ。次は先頭から書き出す
    let mut v = VoiceState {
        live: "書きかけ".into(),
        live_space: true,
        last_char: Some('け'),
        ..Default::default()
    };
    v.reset_live();
    assert!(v.live.is_empty());
    assert!(!v.live_space);
    assert_eq!(v.last_char, None);
    // 追跡を捨てた直後は区切りの空白も入らない
    assert!(!needs_space(v.last_char, Some('a')));
}

// ── lsp_server_for: 言語ID → LSP 起動コマンド ──────────────────

#[test]
fn lsp_server_for_maps_known_languages() {
    assert_eq!(lsp_server_for("rust"), Some("rust-analyzer"));
    assert_eq!(
        lsp_server_for("typescriptreact"),
        Some("typescript-language-server --stdio")
    );
    assert_eq!(lsp_server_for("python"), Some("pyright-langserver --stdio"));
    assert_eq!(lsp_server_for("go"), Some("gopls"));
}

#[test]
fn lsp_server_for_rejects_unknown_and_empty() {
    assert_eq!(lsp_server_for("cobol"), None);
    assert_eq!(lsp_server_for(""), None);
}

#[test]
fn lsp_server_for_is_case_sensitive() {
    // 言語IDは小文字で届く前提。大文字は別物として弾く (現挙動の固定)
    assert_eq!(lsp_server_for("Rust"), None);
}

// ── strip_trailing_keyword: 音声の合図キーワード除去 ─────────────

#[test]
fn strip_trailing_keyword_strips_exact_tail() {
    assert_eq!(
        strip_trailing_keyword("これを直して送信", "送信"),
        Some("これを直して".to_string())
    );
}

#[test]
fn strip_trailing_keyword_trims_space_before_keyword() {
    // キーワード直前の空白は本文に残さない
    assert_eq!(
        strip_trailing_keyword("fix the bug send", "send"),
        Some("fix the bug".to_string())
    );
}

#[test]
fn strip_trailing_keyword_ignores_trailing_punctuation() {
    // 音声認識が勝手に付ける末尾の句読点・記号は無視して判定する
    assert_eq!(
        strip_trailing_keyword("直して送信。", "送信"),
        Some("直して".to_string())
    );
    assert_eq!(
        strip_trailing_keyword("fix it send!? ", "send"),
        Some("fix it".to_string())
    );
    // ただし本文側 (キーワードより前) の句読点はそのまま残る
    assert_eq!(
        strip_trailing_keyword("直して。送信", "送信"),
        Some("直して。".to_string())
    );
}

#[test]
fn strip_trailing_keyword_requires_the_keyword_at_the_end() {
    // キーワードが無い / 末尾以外に現れるだけなら合図ではない
    assert_eq!(strip_trailing_keyword("こんにちは", "送信"), None);
    assert_eq!(strip_trailing_keyword("送信して直す", "送信"), None);
    assert_eq!(strip_trailing_keyword("", "送信"), None);
}

#[test]
fn strip_trailing_keyword_alone_yields_empty_text() {
    // キーワードだけを言ったら本文は空 (空送信の扱いは呼び出し側の判断)
    assert_eq!(strip_trailing_keyword("送信", "送信"), Some(String::new()));
    assert_eq!(
        strip_trailing_keyword(" 送信。", "送信"),
        Some(String::new())
    );
}

// ── キーバインド編集 UI の行組み立て ─────────────────────────────

#[test]
fn keybind_rows_の全行にラベルと打鍵がある() {
    let keys = Keybinds::default();
    let rows = keybind_rows(&keys, "");
    assert_eq!(rows.len(), crate::keybinds::ALL_ACTIONS.len());
    for a in rows {
        assert!(
            !tr(crate::keybinds::action_label(a)).is_empty(),
            "{a:?}: ラベルが空"
        );
        assert!(!keys.label(a).is_empty(), "{a:?}: 打鍵表記が空");
    }
}

#[test]
fn keybind_rows_のラベルは重複しない() {
    // 同じラベルが 2 行あると、どちらのキーか読み分けられない
    let keys = Keybinds::default();
    let mut seen = HashSet::new();
    for a in keybind_rows(&keys, "") {
        let l = tr(crate::keybinds::action_label(a));
        assert!(seen.insert(l.clone()), "ラベル重複: {l}");
    }
}

#[test]
fn keybind_rows_はあいまい検索で絞り込める() {
    let keys = Keybinds::default();
    // config 名でも当たる
    let rows = keybind_rows(&keys, "palette_commands");
    assert!(rows.contains(&BindAction::PaletteCommands));
    assert!(
        rows.len() < crate::keybinds::ALL_ACTIONS.len(),
        "絞れていない"
    );
    // 打鍵表記でも当たる (⌘K ⌘S を貼って探せる)
    let spec = keys.label(BindAction::KeybindEditor);
    assert!(keybind_rows(&keys, &spec).contains(&BindAction::KeybindEditor));
    // 一致しない語では空になり、空状態の分岐へ落ちる
    assert!(keybind_rows(&keys, "zzzqqqxxxyyy").is_empty());
}

#[test]
fn 既定のキーバインドに衝突の注記は出ない() {
    // 出荷時の割り当ては重複も prefix 衝突も OS 予約も踏んでいないこと。
    let keys = Keybinds::default();
    let bad: Vec<String> = crate::keybinds::ALL_ACTIONS
        .iter()
        .filter_map(|a| conflict_note(&keys, *a).map(|n| format!("{a:?}: {n}")))
        .collect();
    assert!(bad.is_empty(), "既定に衝突がある: {bad:?}");
}

#[test]
fn 再割り当てすると衝突の注記が出る() {
    let mut keys = Keybinds::default();
    // 保存を「コマンドパレット」と同じ打鍵にする
    keys.set(BindAction::Save, keys.binding(BindAction::PaletteCommands));
    let note = conflict_note(&keys, BindAction::Save).expect("重複の注記が出ていない");
    assert!(
        note.contains(&tr(crate::keybinds::action_label(
            BindAction::PaletteCommands
        ))),
        "衝突相手のアクション名が出ていない: {note}"
    );
    // chord の prefix (⌘K) と単打がぶつかる場合も出る
    let prefix = keys.binding(BindAction::KeybindEditor).first();
    keys.set(
        BindAction::NewFile,
        crate::keybinds::Binding::Single(prefix),
    );
    let note = conflict_note(&keys, BindAction::NewFile).expect("prefix 衝突の注記が無い");
    assert!(
        note.contains(&tr(crate::keybinds::action_label(
            BindAction::KeybindEditor
        ))),
        "prefix の相手が出ていない: {note}"
    );
}

#[test]
fn 内蔵ショートカットの一覧は空でない() {
    let rows = builtin_shortcuts();
    assert_eq!(rows.len(), 6);
    for (label, keys) in &rows {
        assert!(!label.is_empty());
        assert!(!keys.is_empty(), "{label}: 打鍵表記が空");
    }
}

// ── resolve_theme: テーマ名 / テーマJSONパスの解決 ───────────────

#[test]
fn resolve_theme_builtin_names_match_theme_by_name() {
    for want in theme::all() {
        let got = resolve_theme(&want.name);
        assert_eq!(got.name, want.name);
        assert_eq!(got.label, want.label);
        assert_eq!(got.dark, want.dark);
        assert_eq!(got.bg, want.bg);
    }
}

#[test]
fn resolve_theme_missing_json_path_falls_back_to_builtin_default() {
    // 存在しない JSON パスは読み込み失敗 → ビルトイン名として解決を試み、
    // 該当なしなので既定テーマに落ちる (起動不能にはならない)
    let got = resolve_theme("/no/such/dir/zaivern-missing-theme.json");
    assert_eq!(got.name, theme::by_name("そんな名前は無い").name);
    assert_eq!(got.name, "zaivern-dark");
}

#[test]
fn resolve_theme_empty_name_is_the_default_theme() {
    assert_eq!(resolve_theme("").name, "zaivern-dark");
}

// ── root_name: ルートの表示名 ────────────────────────────────

#[test]
fn root_name_returns_last_component() {
    assert_eq!(root_name(Path::new("/a/b")), "b");
    assert_eq!(root_name(Path::new("/deep/nested/dir")), "dir");
}

#[test]
fn root_name_ignores_a_trailing_slash() {
    assert_eq!(root_name(Path::new("/a/b/")), "b");
}

#[test]
fn root_name_falls_back_to_the_full_path_for_root() {
    // "/" や ".." にはフォルダ名が無いのでフルパス表示に落ちる
    assert_eq!(root_name(Path::new("/")), "/");
    assert_eq!(root_name(Path::new("..")), "..");
}

#[test]
fn root_name_works_for_relative_paths() {
    assert_eq!(root_name(Path::new("src")), "src");
    assert_eq!(root_name(Path::new("src/deep")), "deep");
}

/// `SRC` / `SRC_IMPL` の一覧が実ファイルから漏れていないか。
///
/// 漏れるとソースを読む回帰テスト 60 本以上が**静かに**そのファイルを
/// 見落とす (落ちないので気付けない)。
#[test]
fn 分割した子モジュールを全部srcに繋いでいる() {
    let decl = include_str!("mod.rs");
    let cut = decl
        .find("pub(crate) const SRC: &str")
        .expect("SRC の宣言がある");
    let (impl_decl, all_decl) = decl.split_at(cut);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app");
    let (mut miss_all, mut miss_impl): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for e in std::fs::read_dir(&dir).expect("src/app を読める") {
        let name = e.expect("項目").file_name().to_string_lossy().into_owned();
        if !name.ends_with(".rs") {
            continue;
        }
        let needle = format!("include_str!(\"{name}\")");
        if !all_decl.contains(&needle) {
            miss_all.push(name.clone());
        }
        // 実装ファイル (テストモジュールでないもの) は SRC_IMPL にも要る
        if !name.ends_with("_tests.rs") && name != "tests.rs" && !impl_decl.contains(&needle) {
            miss_impl.push(name);
        }
    }
    miss_all.sort();
    miss_impl.sort();
    assert!(miss_all.is_empty(), "SRC に繋がっていない: {miss_all:?}");
    assert!(
        miss_impl.is_empty(),
        "SRC_IMPL に繋がっていない: {miss_impl:?}"
    );
}
