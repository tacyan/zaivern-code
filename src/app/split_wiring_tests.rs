use super::*;
use crate::terminal::{FocusDir, PanePreset, SplitAction, SplitDir, SplitLayout};

fn preset(name: &str, command: &str) -> config::AgentPreset {
    config::AgentPreset {
        name: name.to_string(),
        command: command.to_string(),
        ..Default::default()
    }
}

/// 右へ 2 回分割した木 (1 | 2 | 3)。フォーカスは最後に開いた 3。
fn tile(ids: &[u64]) -> SplitLayout {
    let mut l = SplitLayout::single(ids[0]);
    for id in &ids[1..] {
        assert!(l.split_focused(SplitDir::Horizontal, *id));
    }
    l
}

fn map(layouts: Vec<SplitLayout>) -> HashMap<u64, SplitLayout> {
    layouts.into_iter().map(|l| (l.leaves()[0], l)).collect()
}

// ── 新しいペインで起動するプリセットの決め方 ────────────────────

#[test]
fn split_preset_index_table() {
    let agents = vec![
        preset("Claude Code", "claude"),
        preset("シェル", ""),
        preset("Codex", "codex"),
    ];
    // エージェント指定は **常に既定プリセット (先頭)** —
    // 親が Codex でも Codex は引き継がない (新規起動と同じ 1 体)。
    assert_eq!(split_preset_index(&agents, PanePreset::NewAgent), Some(0));
    // シェル指定は常に素のシェル
    assert_eq!(split_preset_index(&agents, PanePreset::Shell), Some(1));

    // 素のシェルが登録されていない構成でも動く (先頭で代替する)
    let no_shell = vec![preset("Claude Code", "claude")];
    assert_eq!(split_preset_index(&no_shell, PanePreset::NewAgent), Some(0));
    assert_eq!(split_preset_index(&no_shell, PanePreset::Shell), Some(0));

    // 1 つも登録が無ければ None (呼び出し側がトーストを出す)
    assert_eq!(split_preset_index(&[], PanePreset::Shell), None);
    assert_eq!(split_preset_index(&[], PanePreset::NewAgent), None);
}

/// 分割で起こすエージェントは「新規起動」と同じ経路を通ること。
///
/// `launch_preset_with` (コマンドと cwd を差し替える再開用の口) を
/// 使ってしまうと、親の cwd・親のプリセットが混ざる。ソースを読んで
/// **新規起動と同じ `launch_preset`** を呼んでいることを固定する。
#[test]
fn split_launches_through_the_new_agent_path() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let (_, body) = src
        .split_once("SplitAction::SplitWith { dir, preset } => {")
        .expect("SplitWith の腕");
    let (arm, _) = body
        .split_once("SplitAction::ClosePane =>")
        .expect("次の腕まで");
    assert!(
        arm.contains("self.launch_preset(ix, ctx);"),
        "分割は新規起動と同じ launch_preset を通ること"
    );
    assert!(
        !arm.contains("launch_preset_with"),
        "分割で親の cwd / コマンドを差し替えないこと"
    );
    assert!(
        !arm.contains("preset_name"),
        "分割で親のプリセット名を引き継がないこと"
    );
}

// ── レイアウト表の正規化 ────────────────────────────────────────

#[test]
fn normalize_drops_dead_panes_and_collapses_single() {
    let live: HashSet<u64> = [1, 2, 4, 5].into_iter().collect();
    // 3 は死んでいる → 木から落ちる。7/8 のうち 8 が死ぬので 1 枚 → 畳む。
    let m = map(vec![tile(&[1, 2, 3]), tile(&[4, 5]), tile(&[7, 8])]);
    let live2: HashSet<u64> = [1, 2, 4, 5, 7].into_iter().collect();
    let out = normalize_split_map(m, &live2);
    assert_eq!(out.len(), 2, "1 枚に戻ったタイルは表から消える");
    assert_eq!(out[&1].leaves(), vec![1, 2]);
    assert_eq!(out[&4].leaves(), vec![4, 5]);
    assert!(!out.contains_key(&7));
    let _ = live;
}

/// 先頭ペインを閉じてもタイルが迷子にならない (キーが付け替わる)。
#[test]
fn normalize_rekeys_when_first_pane_dies() {
    let m = map(vec![tile(&[1, 2, 3])]);
    let live: HashSet<u64> = [2, 3].into_iter().collect();
    let out = normalize_split_map(m, &live);
    assert_eq!(out.len(), 1);
    assert!(
        out.contains_key(&2),
        "キーが先頭リーフへ付け替わる: {:?}",
        out.keys()
    );
    assert_eq!(out[&2].leaves(), vec![2, 3]);
    // フォーカスは生き残ったペインの中にある
    assert!(out[&2].focus().is_some_and(|f| f == 2 || f == 3));
}

/// セッションが全滅したら表は空 (空タイルは「閉じたエージェント」と同じ)。
#[test]
fn normalize_empties_when_all_panes_gone() {
    let out = normalize_split_map(map(vec![tile(&[1, 2])]), &HashSet::new());
    assert!(out.is_empty());
}

// ── グリッドに並べるタイル ──────────────────────────────────────

#[test]
fn tiles_exclude_child_panes_and_are_identity_without_splits() {
    let ids = vec![10, 11, 12, 13];
    // 分割ゼロ → 今日とまったく同じ並び
    assert_eq!(split_tile_indices(&ids, &HashMap::new()), vec![0, 1, 2, 3]);
    // 11 と 13 が 10 のタイルの子ペイン
    let m = map(vec![tile(&[10, 11, 13])]);
    assert_eq!(split_tile_indices(&ids, &m), vec![0, 2]);
}

// ── 保存と復元 ──────────────────────────────────────────────────

/// 保存 → 復元の往復。**復元されなかったセッション**のリーフは黙って落ち、
/// 残りだけで開き直す (壊れた保存ファイルでも panic しない)。
#[test]
fn persist_round_trip_drops_missing_sessions() {
    let mut l = tile(&[1, 2, 3]);
    assert!(l.zoom_focused());
    let keys: HashMap<u64, &str> = [(1, "/logs/a.log"), (2, "/logs/b.log"), (3, "/logs/c.log")]
        .into_iter()
        .collect();
    let line = l
        .to_rec(&mut |id| keys.get(&id).map(|s| s.to_string()))
        .to_line();
    assert!(!line.is_empty());

    // 1) 全部戻る
    let back = split_map_from_lines(&[line.clone()], &mut |k| {
        keys.iter().find(|(_, v)| **v == k).map(|(id, _)| *id)
    });
    assert_eq!(back.len(), 1);
    assert_eq!(back[&1].leaves(), vec![1, 2, 3]);
    assert!(back[&1].zoomed(), "ズーム状態も戻る");

    // 2) 2 番が復元されなかった (素のシェル等) → そのリーフだけ落ちる
    let partial = split_map_from_lines(&[line.clone()], &mut |k| match k {
        "/logs/a.log" => Some(1),
        "/logs/c.log" => Some(3),
        _ => None,
    });
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[&1].leaves(), vec![1, 3]);

    // 3) 1 本しか戻らなければ分割は成立しない → 表に入れない
    let one = split_map_from_lines(&[line.clone()], &mut |k| (k == "/logs/a.log").then_some(1));
    assert!(one.is_empty());

    // 4) 空行・壊れた行は無視する (panic しない)
    assert!(split_map_from_lines(&[String::new()], &mut |_| Some(1)).is_empty());
    assert!(split_map_from_lines(&["ごみ".to_string()], &mut |_| Some(1)).is_empty());
}

/// 分割していないタイルの保存は**空文字** = 既存のセッションファイルと同形。
#[test]
fn undivided_tile_persists_as_empty_string() {
    let l = SplitLayout::single(1);
    let line = l.to_rec(&mut |_| Some("/logs/a.log".into())).to_line();
    assert_eq!(
        split_map_from_lines(&[line], &mut |_| Some(1)).len(),
        0,
        "1 枚は分割として保存しない"
    );
}

// ── キーの振り分け ──────────────────────────────────────────────

fn run_keys(events: Vec<egui::Event>) -> (Option<SplitAction>, Vec<egui::Event>) {
    let ctx = egui::Context::default();
    let mut got = None;
    let mut left = Vec::new();
    let _ = ctx.run(
        egui::RawInput {
            events,
            ..Default::default()
        },
        |ctx| {
            got = take_split_key(ctx);
            left = ctx.input(|i| i.events.clone());
        },
    );
    (got, left)
}

fn key(k: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key: k,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    }
}

fn split_mods(shift: bool) -> egui::Modifiers {
    if cfg!(target_os = "macos") {
        egui::Modifiers {
            alt: true,
            ctrl: false,
            shift,
            mac_cmd: true,
            command: true,
        }
    } else {
        egui::Modifiers {
            alt: true,
            ctrl: true,
            shift,
            mac_cmd: false,
            command: true,
        }
    }
}

/// 分割の和音は端末へ流さない (押下も離上も、同じ文字の Text も)。
#[test]
fn split_chords_are_consumed_before_the_pty() {
    let m = split_mods(false);
    let (got, left) = run_keys(vec![
        key(egui::Key::W, true, m),
        egui::Event::Text("w".into()),
        key(egui::Key::W, false, m),
    ]);
    assert_eq!(got, Some(SplitAction::ClosePane));
    assert!(left.is_empty(), "端末へ漏れた: {left:?}");

    // Shift 付き (分割・幅調整) も同じ
    let ms = split_mods(true);
    let (got, left) = run_keys(vec![key(egui::Key::ArrowRight, true, ms)]);
    assert_eq!(
        got,
        Some(SplitAction::SplitWith {
            dir: SplitDir::Horizontal,
            preset: PanePreset::NewAgent
        })
    );
    assert!(left.is_empty());
    let (got, _) = run_keys(vec![key(egui::Key::L, true, ms)]);
    assert_eq!(got, Some(SplitAction::Resize { grow: true }));
    let (got, _) = run_keys(vec![key(egui::Key::J, true, split_mods(true))]);
    assert_eq!(got, None, "表に無い Shift 和音は端末のもの");
}

/// **回帰の要**: `Ctrl+C` / `Ctrl+D` は必ず PTY へ届く。
/// ここを奪うとシェルもエージェントも中断できなくなる。
#[test]
fn ctrl_c_and_ctrl_d_still_reach_the_pty() {
    let ctrl = egui::Modifiers {
        alt: false,
        ctrl: true,
        shift: false,
        mac_cmd: false,
        command: true,
    };
    for k in [egui::Key::C, egui::Key::D, egui::Key::W, egui::Key::Z] {
        let (got, left) = run_keys(vec![key(k, true, ctrl)]);
        assert_eq!(got, None, "{k:?} を分割が奪った");
        assert_eq!(left.len(), 1, "{k:?} のイベントが消えた");
    }
    // 素の文字入力も一切触らない
    let (got, left) = run_keys(vec![
        key(egui::Key::W, true, egui::Modifiers::NONE),
        egui::Event::Text("w".into()),
    ]);
    assert_eq!(got, None);
    assert_eq!(left.len(), 2);
    // 日本語の確定文字も落とさない
    let (_, left) = run_keys(vec![egui::Event::Text("あ".into())]);
    assert_eq!(left.len(), 1);
}

// ── フォーカス / ズーム / 等分 (ディスパッチャが呼ぶモデル操作) ──

/// `apply_split_action` が各アクションで呼ぶモデル操作を、同じ順序で辿る。
/// (`ZaivernApp` は eframe の CreationContext 無しには作れないため、
///  ここではセッション起動を伴わない全アクションを直接確かめる)
#[test]
fn dispatcher_actions_move_focus_zoom_and_equalize() {
    let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 300.0));
    let mut l = tile(&[1, 2, 3]);
    assert_eq!(l.focus(), Some(3));

    // Focus: 幾何的な隣へ
    assert!(l.focus_dir(FocusDir::Left, area, terminal::GUTTER));
    assert_eq!(l.focus(), Some(2));

    // Resize: フォーカス中ペインが広がる / 狭まる
    let before = l.rects(area, terminal::GUTTER);
    assert!(l.resize_focused(terminal::RESIZE_STEP));
    let after = l.rects(area, terminal::GUTTER);
    assert_ne!(before, after, "幅が動いていない");

    // Equalize: 面積が揃う
    l.equalize();
    let rs = l.rects(area, terminal::GUTTER);
    let w0 = rs[0].1.width();
    // 差はガター 1 本ぶん以内 (仕切りの本数がペインごとに違うため)
    for (id, r) in &rs {
        assert!(
            (r.width() - w0).abs() <= terminal::GUTTER + 1.0,
            "ペイン {id} の幅が揃わない ({rs:?})"
        );
    }

    // Zoom: 見えるのはフォーカス中の 1 枚だけ / もう一度で戻る
    assert!(l.zoom_focused());
    assert_eq!(l.rects(area, terminal::GUTTER), vec![(2, area)]);
    assert!(!l.zoom_focused());
    assert_eq!(l.rects(area, terminal::GUTTER).len(), 3);

    // ClosePane: 先に木から外してから reap する (フォーカスは兄弟へ)
    assert!(l.close_leaf(2));
    assert_eq!(l.leaves(), vec![1, 3]);
    assert!(l.focus().is_some_and(|f| f == 1 || f == 3));
}
