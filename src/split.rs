//! 事前分割 — **配る前に、衝突し得ない担当表を作る。**
//!
//! ## なぜ要るのか
//!
//! 並列エージェントの本当のコストは「実行時間」ではなく **レビュー時の
//! 衝突解決**である。既にある手は 2 つとも *起きてから* の対処になっている:
//!
//! * [`crate::conflict`] — 起きた重なりを、マージ前に見せる (検出)
//! * [`crate::lease`] — 書き込みの瞬間に止める (強制)
//!
//! そして [`crate::lease::split_plan`] は「重なったぶんを後の担当から外す」
//! だけで、**担当表そのものは作らない**。つまり「N 人へ配る前に、衝突し得ない
//! 分割を出す」という一番安い手が空いていた。ここがそれを埋める。
//!
//! ## 中核は純関数
//!
//! [`partition`] は I/O もスレッドも持たない。入力は「タスク ID → 触りそうな
//! パス集合」だけで、出力 [`Partition`] は
//!
//! * 各タスクの **専有パス集合** (互いに素)
//! * どうしても共有になるパスと、その**扱い** ([`Policy`])
//!
//! を持つ。**出力自身が [`Partition::is_disjoint`] で互いに素かを検査できる**
//! のがこの機能の価値で、テストは小さな全組合せを回して「偽になる入力が
//! 存在しない」ことを確かめている
//! ([`tests::小さな全組合せで互いに素が破れない`])。
//!
//! ## パス照合は再実装しない
//!
//! [`crate::lease::normalize_path`] / [`crate::lease::covers`] /
//! [`crate::lease::overlaps`] をそのまま使う。3 OS のパス正規化 (区切り・
//! 大小畳み・`..` の畳み込み) と glob 同士の交差判定は既にそこで実測済みで、
//! **2 実装を持つとズレる**。
//!
//! ## 決定性
//!
//! `HashMap` / `HashSet` を 1 つも使わない (`Vec` と `BTreeMap` / `BTreeSet`
//! だけ)。同点はすべて **タスク ID の辞書順**で割る。同じ入力からは、どの OS の
//! どのプロセスでも 1 バイト違わない担当表が出る。

use std::collections::{BTreeMap, BTreeSet};

use egui::{Align2, RichText};

use crate::i18n::tr;
use crate::lease::{covers, normalize_path, overlaps};
use crate::panels::space;

// ═══════════════════════════════════════════════════════════════════════════
//  1. 入出力の型
// ═══════════════════════════════════════════════════════════════════════════

/// 分割にかけるタスク 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSpec {
    /// 安定した識別子。担当表の見出しになり、同点の割り振りもこの辞書順で決まる。
    pub id: String,
    /// そのタスクが触りそうなパス / glob。正規形でなくてよい
    /// ([`partition`] が [`normalize_path`] を通す)。
    pub paths: Vec<String>,
}

/// 分割の方針。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitOpts {
    /// **追記しかしない**ファイルのパターン。ここに覆われる共有パスは
    /// [`Policy::UnionMerge`] へ回す (git の union マージで自動的に畳める)。
    ///
    /// 既定は空。**中身を決め打ちで持たない** — 何が追記専用かはリポジトリの
    /// 事情で、こちらが勝手に決めると外れたときに黙って壊れる。
    pub union_globs: Vec<String>,
    /// 1 つの共有領域を「誰か 1 人へ寄せる」ことを許すタスク数の上限。
    /// これを超えたら [`Policy::Serialize`] (順番に回す) へ倒す。
    ///
    /// 既定 2 — 3 人以上が取り合っている領域を 1 人に寄せると、残り 2 人以上が
    /// 同時に待たされる。そこは分割ではなく順番の問題なので正直にそう出す。
    pub max_owner_tasks: usize,
}

impl Default for SplitOpts {
    fn default() -> Self {
        SplitOpts {
            union_globs: Vec::new(),
            max_owner_tasks: 2,
        }
    }
}

/// 共有領域の扱い。**「分けられませんでした」で終わらせない**ための 3 択。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    /// 誰か 1 人へ寄せる (その人だけが触ってよい)。
    Owner(String),
    /// 誰にも配らず、順番に回す。
    Serialize,
    /// 追記だけなので自動マージへ回す ([`SplitOpts::union_globs`])。
    UnionMerge,
}

impl Policy {
    /// JSON に出す安定タグ。
    pub fn tag(&self) -> &'static str {
        match self {
            Policy::Owner(_) => "owner",
            Policy::Serialize => "serialize",
            Policy::UnionMerge => "union-merge",
        }
    }

    /// 寄せ先のタスク ID (寄せていなければ `None`)。
    pub fn owner(&self) -> Option<&str> {
        match self {
            Policy::Owner(id) => Some(id.as_str()),
            _ => None,
        }
    }

    /// 行頭に出す記号。
    pub fn glyph(&self) -> &'static str {
        match self {
            Policy::Owner(_) => "👤",
            Policy::Serialize => "⏭",
            Policy::UnionMerge => "➕",
        }
    }

    /// 画面と担当表に出す説明。
    pub fn label(&self) -> String {
        match self {
            Policy::Owner(id) => format!("{id} に寄せる"),
            Policy::Serialize => tr("直列 — 順番に回す"),
            Policy::UnionMerge => tr("追記だけ — 自動マージへ回す"),
        }
    }
}

/// どうしても共有になった領域 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shared {
    /// 争っているパターン (正規形・辞書順・重複なし)。
    pub paths: Vec<String>,
    /// 争っているタスク ID (辞書順)。
    pub tasks: Vec<String>,
    /// 扱い。
    pub policy: Policy,
}

/// 1 タスクぶんの専有割り当て。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assigned {
    pub id: String,
    /// このタスクだけが触ってよいパターン (正規形・辞書順)。
    pub paths: Vec<String>,
}

/// 担当表。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partition {
    /// タスク ID の辞書順。**入力の並びには依存しない**。
    pub assignments: Vec<Assigned>,
    /// 分けきれなかった領域 (先頭パスの辞書順)。
    pub shared: Vec<Shared>,
}

impl Partition {
    /// **割り当てが本当に互いに素か。** ここがこの機能の value で、
    /// 出力が自分自身を検査できることに意味がある
    /// (`partition` の実装を疑わずに使える)。
    pub fn is_disjoint(&self) -> bool {
        for (i, x) in self.assignments.iter().enumerate() {
            for y in self.assignments.iter().skip(i + 1) {
                for p in &x.paths {
                    for q in &y.paths {
                        if overlaps(p, q) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    /// 共有パスが 1 件も残らなかったか (= そのまま配ってよい)。
    pub fn is_clean(&self) -> bool {
        self.shared.is_empty()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  2. 中核 — 純関数の分割
// ═══════════════════════════════════════════════════════════════════════════

/// N 個のタスクと、各タスクが触りそうなパス集合から、**互いに素な担当表**を作る。
///
/// ## 手順
///
/// 1. パスを [`normalize_path`] で正規形へ。同じ ID が 2 回出てきたら和集合。
/// 2. `(タスク, パターン)` を頂点、**別タスク同士で [`overlaps`] する**組を辺と
///    見て連結成分を取る。辺が 1 本も無いパターンは、その時点で専有が確定する。
/// 3. 各成分 (= 争っている領域) に [`Policy`] を決める。全パターンが
///    [`SplitOpts::union_globs`] に覆われるなら [`Policy::UnionMerge`]、
///    争っているタスク数が [`SplitOpts::max_owner_tasks`] 以下なら
///    [`Policy::Owner`] (その成分に最も多くパターンを出したタスク。同点は
///    **ID の辞書順**)、それ以外は [`Policy::Serialize`]。
/// 4. 専有集合 = 「どの成分にも属さないパターン」＋
///    「[`Policy::Owner`] で自分が勝った成分の、自分のパターン」。
///
/// ## なぜこれで互いに素になるか
///
/// 別タスクのパターンと重なるパターンは **必ず** どれかの成分に入る。成分から
/// 出て行けるのは勝者 1 人だけなので、残った専有集合の間に「別タスク同士で
/// 重なる組」は構造的に存在し得ない。[`Partition::is_disjoint`] はその不変条件を
/// 出力側からもう一度確かめるための番人で、テストは小さな全組合せを回して
/// 偽になる入力が無いことを確認している。
pub fn partition(tasks: &[TaskSpec], opts: &SplitOpts) -> Partition {
    // ── ① 正規化して ID でまとめる (BTreeMap なので ID 辞書順に並ぶ) ──
    let mut by_id: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in tasks {
        let id = t.id.trim();
        if id.is_empty() {
            continue;
        }
        let e = by_id.entry(id.to_string()).or_default();
        for p in &t.paths {
            // **先に trim する。** `normalize_path` は空白をセグメントとして
            // 残すので、`"  "` がそのまま 1 パターンとして台帳に載ってしまう。
            let n = normalize_path(p.trim());
            if !n.is_empty() {
                e.insert(n);
            }
        }
    }
    let ids: Vec<String> = by_id.keys().cloned().collect();

    // ── ② 頂点 = (タスク添字, パターン)。並びは ID 辞書順 → パターン辞書順 ──
    let mut nodes: Vec<(usize, String)> = Vec::new();
    for (ti, id) in ids.iter().enumerate() {
        for p in &by_id[id] {
            nodes.push((ti, p.clone()));
        }
    }

    // 連結成分。頂点数はせいぜい数百なので素朴な BFS で足りる。
    let mut comp: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut comps: Vec<Vec<usize>> = Vec::new();
    for start in 0..nodes.len() {
        if comp[start].is_some() {
            continue;
        }
        let cid = comps.len();
        let mut members: Vec<usize> = Vec::new();
        let mut queue = vec![start];
        comp[start] = Some(cid);
        while let Some(v) = queue.pop() {
            members.push(v);
            for (w, node) in nodes.iter().enumerate() {
                if comp[w].is_some() || node.0 == nodes[v].0 {
                    // 同じタスクの中の重なりは「争い」ではない (持ち主が同じ)
                    continue;
                }
                if overlaps(&nodes[v].1, &node.1) {
                    comp[w] = Some(cid);
                    queue.push(w);
                }
            }
        }
        members.sort_unstable();
        comps.push(members);
    }

    // ── ③ 成分ごとに扱いを決める ──
    let mut keep: Vec<BTreeSet<String>> = vec![BTreeSet::new(); ids.len()];
    let mut shared: Vec<Shared> = Vec::new();
    for members in &comps {
        let tasks_in: BTreeSet<usize> = members.iter().map(|&m| nodes[m].0).collect();
        if tasks_in.len() < 2 {
            // 誰とも争っていない = そのまま専有
            for &m in members {
                keep[nodes[m].0].insert(nodes[m].1.clone());
            }
            continue;
        }
        let paths: Vec<String> = {
            let set: BTreeSet<String> = members.iter().map(|&m| nodes[m].1.clone()).collect();
            set.into_iter().collect()
        };
        let names: Vec<String> = tasks_in.iter().map(|&t| ids[t].clone()).collect();
        let policy = if !opts.union_globs.is_empty()
            && paths
                .iter()
                .all(|p| opts.union_globs.iter().any(|g| covers(g, p)))
        {
            Policy::UnionMerge
        } else if tasks_in.len() <= opts.max_owner_tasks {
            Policy::Owner(ids[winner(members, nodes.as_slice(), &tasks_in)].clone())
        } else {
            Policy::Serialize
        };
        if let Policy::Owner(id) = &policy {
            // 勝者だけが自分のパターンを持ち帰る。他は落ちる。
            for &m in members {
                if &ids[nodes[m].0] == id {
                    keep[nodes[m].0].insert(nodes[m].1.clone());
                }
            }
        }
        shared.push(Shared {
            paths,
            tasks: names,
            policy,
        });
    }
    shared.sort_by(|a, b| a.paths.cmp(&b.paths));

    Partition {
        assignments: ids
            .iter()
            .enumerate()
            .map(|(ti, id)| Assigned {
                id: id.clone(),
                paths: keep[ti].iter().cloned().collect(),
            })
            .collect(),
        shared,
    }
}

/// 成分の勝者 (タスク添字)。**同点は ID の辞書順で先頭**。
///
/// 「最も多くのパターンを出した人」に寄せる。もっと凝った尺度 (パターンの
/// 広さ等) も考えられるが、glob の「広さ」に順序を入れると境界が説明できなく
/// なる。**説明できる単純な規則**を採り、気に入らなければタスク表を書き換えて
/// もらう。`tasks_in` は辞書順の `BTreeSet` なので、同点なら先に来た方が勝つ。
fn winner(members: &[usize], nodes: &[(usize, String)], tasks_in: &BTreeSet<usize>) -> usize {
    let mut best: Option<(usize, usize)> = None; // (パターン数, タスク添字)
    for &t in tasks_in {
        let n = members.iter().filter(|&&m| nodes[m].0 == t).count();
        if best.is_none_or(|(bn, _)| n > bn) {
            best = Some((n, t));
        }
    }
    best.map_or(0, |(_, t)| t)
}

/// `ID: パス パス …` の行をタスクへ。空行と `#` 始まりは無視。
///
/// 書式の実装は [`crate::lease::parse_assignments`] と **同じ 1 本**を通す
/// (担当表の書式が 2 つあると、どちらで書いたか分からなくなる)。
pub fn parse_tasks(text: &str) -> Vec<TaskSpec> {
    crate::lease::parse_assignments(text)
        .into_iter()
        .map(|a| TaskSpec {
            id: a.agent,
            paths: a.patterns,
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. プロンプトへ差し込む 1 行
// ═══════════════════════════════════════════════════════════════════════════

/// 「あなたが触ってよいのは次のパスだけです: …」の 1 行。
///
/// **空なら 1 文字も返さない** — [`crate::race::build_scoped_race_prompt`] と
/// 同じ約束で、範囲が無いときに空の制約文を足すと、エージェントは
/// 「何も触ってはいけない」と読む。
///
/// [`tr`] を通さないのは、これが**画面の文字ではなくエージェントへの指示文**
/// だから ([`crate::race::build_race_prompt`] と同じ扱い)。UI の言語設定で
/// 送る指示が揺れると、再現しない不具合の温床になる。
pub fn scope_line(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    format!(
        "あなたが触ってよいのは次のパスだけです: {}（他のエージェントが同じ時間に別のパスを持っています。\
         範囲外に触る必要が出たら、変更せずに理由だけ報告してください。）",
        paths.join(" ")
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 出力 (担当表 / JSON)
// ═══════════════════════════════════════════════════════════════════════════

/// 人が読む担当表。クリップボードにも CLI にも **同じ 1 実装**を使う。
pub fn render_table(p: &Partition) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "担当表 — {} タスク / 共有 {} 件\n互いに素: {}\n",
        p.assignments.len(),
        p.shared.len(),
        if p.is_disjoint() {
            "はい"
        } else {
            "いいえ (不具合です。報告してください)"
        }
    ));
    for a in &p.assignments {
        out.push_str(&format!("\n[{}] {} 件\n", a.id, a.paths.len()));
        for path in &a.paths {
            out.push_str(&format!("  {path}\n"));
        }
        let line = scope_line(&a.paths);
        if !line.is_empty() {
            out.push_str(&format!("  → {line}\n"));
        }
    }
    if !p.shared.is_empty() {
        out.push_str("\n共有パス (自動では分けられなかったぶん)\n");
        for s in &p.shared {
            out.push_str(&format!(
                "  {} {} — {}  ({})\n",
                s.policy.glyph(),
                s.policy.label(),
                s.paths.join(", "),
                s.tasks.join(", ")
            ));
        }
    }
    out
}

/// 機械が読む担当表。`zai split plan --json` の出力。
pub fn render_json(p: &Partition) -> String {
    let assignments: Vec<serde_json::Value> = p
        .assignments
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "paths": a.paths,
                "scope_line": scope_line(&a.paths),
            })
        })
        .collect();
    let shared: Vec<serde_json::Value> = p
        .shared
        .iter()
        .map(|s| {
            serde_json::json!({
                "paths": s.paths,
                "tasks": s.tasks,
                "policy": s.policy.tag(),
                "owner": s.policy.owner(),
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "disjoint": p.is_disjoint(),
        "clean": p.is_clean(),
        "assignments": assignments,
        "shared": shared,
    }))
    .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. CLI (`zai split …`)
// ═══════════════════════════════════════════════════════════════════════════

/// `zai split` の使い方。**書式はここだけが権威**。
pub fn usage() -> String {
    "zai split — 配る前に、衝突し得ない担当表を作る\n\
     \n\
     使い方:\n\
     \x20 zai split plan --tasks <ファイル|-> [--json] [--union <glob>]… [--max-owner <n>]\n\
     \n\
     タスクの書式 (1 行 1 タスク):\n\
     \x20 <ID>: <パス1> <パス2> …    パスは空白かカンマ区切り。glob (* ** ?) が使える\n\
     \x20 空行と # で始まる行は無視。ID を省くと #1 / #2 … が振られる\n\
     \n\
     \x20 例:  ui:   src/app.rs src/panels.rs\n\
     \x20      core: src/lease.rs src/conflict.rs\n\
     \x20      docs: docs/**\n\
     \n\
     オプション:\n\
     \x20 --json           機械可読な JSON で出す\n\
     \x20 --union <glob>   追記しかしないファイル。共有になっても自動マージへ回す (複数可)\n\
     \x20 --max-owner <n>  共有領域を 1 人へ寄せてよいタスク数の上限 (既定 2)\n\
     \n\
     終了コード: 0=互いに素な分割ができた / 1=共有パスが残った / 2=使い方の誤り\n"
        .to_string()
}

/// `zai split <sub>` の実体。argv は `"split"` の**次**から渡される。
///
/// 戻り値は終了コード。`0`=互いに素な分割ができた / `1`=共有パスが残った /
/// `2`=使い方の誤り。
//
// **`src/cli.rs` へは自分で配線しない。** ここは並列ブランチが同時に編集すると
/// `zai split <sub>` の実体。`src/cli.rs` の dispatch から呼ばれる。
pub fn cli_main(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str).unwrap_or("") {
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            0
        }
        "plan" => plan_cmd(&argv[1..]),
        "" => {
            eprintln!("{}", usage());
            2
        }
        other => {
            eprintln!("zai split: 知らないサブコマンドです: {other}\n\n{}", usage());
            2
        }
    }
}

fn usage_err(msg: &str) -> i32 {
    eprintln!("zai split: {msg}\n\n{}", usage());
    2
}

fn plan_cmd(args: &[String]) -> i32 {
    let mut src: Option<String> = None;
    let mut json = false;
    let mut opts = SplitOpts::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{}", usage());
                return 0;
            }
            "--tasks" | "-t" | "--union" | "--max-owner" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    return usage_err(&format!("{a} に値がありません"));
                };
                match a {
                    "--tasks" | "-t" => src = Some(v.clone()),
                    "--union" => opts.union_globs.push(normalize_path(v)),
                    _ => match v.parse::<usize>() {
                        Ok(n) => opts.max_owner_tasks = n,
                        Err(_) => return usage_err(&format!("--max-owner には数を渡します: {v}")),
                    },
                }
            }
            other => return usage_err(&format!("知らない引数です: {other}")),
        }
        i += 1;
    }
    let Some(src) = src else {
        return usage_err("--tasks <ファイル|-> が要ります");
    };
    let text = match read_source(&src) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("zai split: {e}");
            return 2;
        }
    };
    let tasks = parse_tasks(&text);
    if tasks.is_empty() {
        return usage_err("タスクを 1 件も読めませんでした (書式を確認してください)");
    }
    let part = partition(&tasks, &opts);
    if json {
        println!("{}", render_json(&part));
    } else {
        print!("{}", render_table(&part));
    }
    if part.is_clean() {
        0
    } else {
        1
    }
}

/// `-` なら標準入力、それ以外はファイル。**パスは受け取ったものだけ**を使い、
/// 既定の置き場を勝手に決めない (どの OS でも同じ挙動になる)。
fn read_source(src: &str) -> Result<String, String> {
    if src == "-" {
        std::io::read_to_string(std::io::stdin()).map_err(|e| format!("標準入力を読めません: {e}"))
    } else {
        std::fs::read_to_string(src).map_err(|e| format!("{src} を読めません: {e}"))
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. レイアウト (純関数・テーブルテストで固定する)
// ═══════════════════════════════════════════════════════════════════════════

/// 段の間隔。
pub const GAP: f32 = 8.0;
/// 入力欄に最低限残す高さ。
pub const INPUT_MIN_H: f32 = 64.0;
/// 担当表に最低限残す高さ。**入力欄を伸ばして結果を潰さない**。
pub const RESULT_MIN_H: f32 = 80.0;
/// これより狭いとボタンはアイコンだけへ縮退する。
pub const COMPACT_W: f32 = 560.0;

/// ウィンドウ内の割り付け。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// タスク入力欄。
    pub input: egui::Rect,
    /// 担当表 (結果)。
    pub result: egui::Rect,
    /// ボタンをアイコンだけにするか。
    pub compact: bool,
}

/// 可用領域から割り付けを決める **純関数**。
///
/// 約束は 2 つだけで、テーブルテストがそれを固定する:
/// **どちらの矩形も `avail` の中に収まる** / **互いに重ならない**。
pub fn layout(avail: egui::Rect) -> Layout {
    let w = avail.width().max(0.0);
    let h = avail.height().max(0.0);
    // **領域より広い隙間は取らない。** ここを固定値にすると、極端に低い
    // 領域 (縦 4px 等) で下の矩形が領域の外へ出る。
    let gap = GAP.min(h);
    let budget = (h - gap).max(0.0);
    // 入力は上から 35%。ただし結果側の下限を必ず先に確保する
    // (入力欄が伸びて担当表が 0 行になると、この機能の中身が消える)。
    let mut input_h = (h * 0.35).clamp(INPUT_MIN_H, 240.0);
    if input_h + RESULT_MIN_H > budget {
        input_h = (budget - RESULT_MIN_H).max(0.0);
    }
    input_h = input_h.min(budget);
    let ry = (avail.min.y + input_h + gap).min(avail.max.y);
    let result_h = (budget - input_h).max(0.0).min((avail.max.y - ry).max(0.0));
    Layout {
        input: egui::Rect::from_min_size(avail.min, egui::vec2(w, input_h)),
        result: egui::Rect::from_min_size(egui::pos2(avail.min.x, ry), egui::vec2(w, result_h)),
        compact: w < COMPACT_W,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. 機能レジストリと画面
// ═══════════════════════════════════════════════════════════════════════════

/// パレットからの到達経路。打鍵は割り当てない (統合担当が要ると判断したら足す)。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "split",
    entries: &[crate::feature::Entry {
        icon: "🔀",
        label: "担当分割: 衝突し得ない割り当てを作る",
        id: "split.open",
    }],
    dispatch: |_app, _ctx, id| match id {
        "split.open" => {
            open_panel();
            true
        }
        _ => false,
    },
    draw: Some(draw),
    settings: &[],
    binds: &[],
};

/// パネルの状態。**ウィンドウより長生きさせる** (設計原則 1) ため
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
/// こうすると `app.rs` を 1 バイトも触らずに機能が繋がる。
#[derive(Default)]
struct PanelState {
    open: bool,
    /// 入力 (1 行 1 タスク)。
    text: String,
    /// 追記だけのファイル (空白区切り)。
    union_text: String,
    /// 直近の計算結果。`(入力, union, 結果)` — 入力が変わったときだけ作り直す。
    computed: Option<(String, String, Partition)>,
    toast: String,
}

fn state() -> &'static std::sync::Mutex<PanelState> {
    static S: std::sync::OnceLock<std::sync::Mutex<PanelState>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn open_panel() {
    if let Ok(mut st) = state().lock() {
        st.open = true;
    }
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    let mut open = true;
    let mut copy: Option<String> = None;
    egui::Window::new(tr("🔀 担当分割 — 衝突し得ない割り当て"))
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(520.0)
        .open(&mut open)
        .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            copy = body(ui, &mut st);
        });
    if let Some(text) = copy {
        ctx.copy_text(text);
        st.toast = tr("コピーしました");
    }
    if !open {
        st.open = false;
        st.toast.clear();
    }
}

/// 入力が変わっていたら計算し直す。純関数なので同期で呼んでよい
/// (git も I/O も 1 回も起こさない)。
fn recompute(st: &mut PanelState) {
    let fresh = st
        .computed
        .as_ref()
        .is_some_and(|(t, u, _)| t == &st.text && u == &st.union_text);
    if fresh {
        return;
    }
    let opts = SplitOpts {
        union_globs: st
            .union_text
            .split([',', ' ', '\t', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(normalize_path)
            .collect(),
        ..SplitOpts::default()
    };
    let part = partition(&parse_tasks(&st.text), &opts);
    st.computed = Some((st.text.clone(), st.union_text.clone(), part));
}

/// 中身。返り値はクリップボードへ流す文字列 (描画の中で I/O をしない)。
fn body(ui: &mut egui::Ui, st: &mut PanelState) -> Option<String> {
    let lay = layout(ui.available_rect_before_wrap());
    let mut copy = None;

    ui.label(
        RichText::new(tr(
            "1 行 1 タスク。「ID: パス パス …」 — glob (* ** ?) が使えます",
        ))
        .small()
        .weak(),
    )
    // 書式の権威は 1 箇所 ([`usage`])。画面と CLI で説明がズレないようにする。
    .on_hover_text(usage());
    ui.add_sized(
        [lay.input.width(), lay.input.height().max(INPUT_MIN_H)],
        egui::TextEdit::multiline(&mut st.text)
            .hint_text("ui:   src/app.rs src/panels.rs\ncore: src/lease.rs\ndocs: docs/**")
            .code_editor(),
    );
    ui.horizontal(|ui| {
        ui.label(RichText::new(tr("追記だけのファイル")).small().weak());
        ui.add(
            egui::TextEdit::singleline(&mut st.union_text)
                .hint_text(tr("CHANGELOG.md docs/log/**"))
                .desired_width(ui.available_width().max(80.0)),
        )
        .on_hover_text(tr(
            "ここに覆われた共有パスは「追記だけ」として自動マージへ回します。",
        ));
    });
    ui.add_space(space::XS);

    recompute(st);
    let Some((_, _, part)) = &st.computed else {
        return copy;
    };

    if part.assignments.is_empty() {
        // 空状態は **高さを取らない 1 行**。大きなカードで場所を潰さない
        // (中身より空状態を見せている時間の方が長いパネルにしない)。
        ui.label(
            RichText::new(tr(
                "🔀 タスクを貼ると、互いに素な担当表とプロンプト 1 行を作ります",
            ))
            .small()
            .weak(),
        );
        return copy;
    }

    // ── 見出し: 分けきれたかどうかを最初に言う ──
    ui.horizontal(|ui| {
        let (txt, col) = if part.is_clean() {
            (
                tr("✅ 互いに素な担当表ができました。そのまま配れます"),
                ui.visuals().hyperlink_color,
            )
        } else {
            (
                tr("⚠ 共有パスが残っています。下の扱いを確認してください"),
                ui.visuals().warn_fg_color,
            )
        };
        ui.label(RichText::new(txt).strong().color(col));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if lay.compact {
                "📋"
            } else {
                "📋 担当表を全部コピー"
            };
            if ui
                .button(tr(label))
                .on_hover_text(tr("担当表をテキストでコピーします"))
                .clicked()
            {
                copy = Some(render_table(part));
            }
        });
    });
    if !st.toast.is_empty() {
        ui.label(RichText::new(st.toast.as_str()).small().weak());
    }

    egui::ScrollArea::vertical()
        .id_salt("zv-split-result")
        .max_height(lay.result.height().max(RESULT_MIN_H))
        .show(ui, |ui| {
            for a in &part.assignments {
                // 可変長リストの中の `CollapsingHeader` 等は使わない。
                // 使うなら `ui.push_id` が要る (`panels.rs` の構造検査が番人)。
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("[{}]", a.id)).strong().monospace());
                    ui.label(
                        RichText::new(format!("{} 件", a.paths.len()))
                            .small()
                            .weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if lay.compact { "📋" } else { "📋 プロンプト行" };
                        let line = scope_line(&a.paths);
                        let btn = ui.add_enabled(!line.is_empty(), egui::Button::new(tr(label)));
                        if btn.clicked() {
                            copy = Some(line.clone());
                        }
                        if line.is_empty() {
                            btn.on_hover_text(tr("専有パスがありません"));
                        } else {
                            btn.on_hover_text(line);
                        }
                    });
                });
                if a.paths.is_empty() {
                    ui.label(
                        RichText::new(tr("  (専有パスなし — 共有側を見てください)"))
                            .small()
                            .weak(),
                    );
                } else {
                    let joined = a.paths.join("  ");
                    // 長い行は省略してホバーで全文 (どの幅でも見切れない)。
                    ui.add(
                        egui::Label::new(RichText::new(joined.as_str()).small().monospace())
                            .truncate(),
                    )
                    .on_hover_text(joined.clone());
                }
                ui.add_space(space::XS);
            }
            if !part.shared.is_empty() {
                ui.separator();
                ui.label(
                    RichText::new(tr("共有パス (自動では分けられなかったぶん)"))
                        .small()
                        .strong()
                        .weak(),
                );
                for s in &part.shared {
                    let line = format!(
                        "{} {} — {}  ({})",
                        s.policy.glyph(),
                        s.policy.label(),
                        s.paths.join(", "),
                        s.tasks.join(", ")
                    );
                    ui.add(egui::Label::new(RichText::new(line.as_str()).small()).truncate())
                        .on_hover_text(line.clone());
                }
            }
        });
    copy
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, paths: &[&str]) -> TaskSpec {
        TaskSpec {
            id: id.into(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 指定タスクの専有パス (テストの読みやすさのためだけの補助)。
    fn paths_of<'a>(p: &'a Partition, id: &str) -> &'a [String] {
        p.assignments
            .iter()
            .find(|a| a.id == id)
            .map_or(&[][..], |a| a.paths.as_slice())
    }

    fn ids_and_paths(p: &Partition) -> Vec<(String, Vec<String>)> {
        p.assignments
            .iter()
            .map(|a| (a.id.clone(), a.paths.clone()))
            .collect()
    }

    // -----------------------------------------------------------------
    // 分割 (テーブルテスト)
    // -----------------------------------------------------------------

    #[test]
    fn 分割のテーブル() {
        let opts = SplitOpts::default();

        // ── 空 ──
        let p = partition(&[], &opts);
        assert!(p.assignments.is_empty());
        assert!(p.shared.is_empty());
        assert!(p.is_disjoint());
        assert!(p.is_clean());

        // ── 1 タスク (争う相手が居ない) ──
        let p = partition(&[task("a", &["src/x.rs", "src/**"])], &opts);
        assert_eq!(
            ids_and_paths(&p),
            vec![(
                "a".to_string(),
                vec!["src/**".to_string(), "src/x.rs".to_string()]
            )],
            "同一タスク内の重なりは争いではない"
        );
        assert!(p.is_clean());

        // ── 完全に独立 ──
        let p = partition(
            &[task("b", &["src/b.rs"]), task("a", &["src/a.rs"])],
            &opts,
        );
        assert_eq!(
            ids_and_paths(&p),
            vec![
                ("a".to_string(), vec!["src/a.rs".to_string()]),
                ("b".to_string(), vec!["src/b.rs".to_string()]),
            ],
            "並びは入力順ではなく ID 辞書順"
        );
        assert!(p.is_clean() && p.is_disjoint());

        // ── 全部同じファイル (2 人) → 誰かに寄せる ──
        let p = partition(
            &[task("zz", &["src/app.rs"]), task("aa", &["src/app.rs"])],
            &opts,
        );
        assert_eq!(p.shared.len(), 1);
        assert_eq!(p.shared[0].tasks, vec!["aa".to_string(), "zz".to_string()]);
        assert_eq!(
            p.shared[0].policy,
            Policy::Owner("aa".into()),
            "同点は ID 辞書順で先頭が勝つ"
        );
        assert_eq!(paths_of(&p, "aa"), ["src/app.rs".to_string()]);
        assert!(paths_of(&p, "zz").is_empty());
        assert!(p.is_disjoint());
        assert!(!p.is_clean());

        // ── 部分重なり (独立なぶんは残る) ──
        let p = partition(
            &[
                task("a", &["src/a.rs", "src/shared.rs"]),
                task("b", &["src/b.rs", "src/shared.rs"]),
            ],
            &opts,
        );
        assert_eq!(
            paths_of(&p, "a"),
            ["src/a.rs".to_string(), "src/shared.rs".into()]
        );
        assert_eq!(paths_of(&p, "b"), ["src/b.rs".to_string()]);
        assert_eq!(p.shared.len(), 1);
        assert_eq!(p.shared[0].paths, vec!["src/shared.rs".to_string()]);
        assert!(p.is_disjoint());

        // ── glob 同士の重なり ──
        let p = partition(
            &[task("a", &["src/**"]), task("b", &["src/ui/panel.rs"])],
            &opts,
        );
        assert_eq!(p.shared.len(), 1);
        assert_eq!(
            p.shared[0].paths,
            vec!["src/**".to_string(), "src/ui/panel.rs".to_string()]
        );
        assert!(p.is_disjoint());

        // ── glob が重ならない (`*` は `/` を越えない) ──
        let p = partition(
            &[task("a", &["src/*.rs"]), task("b", &["src/sub/a.rs"])],
            &opts,
        );
        assert!(p.is_clean(), "重ならないので分けきれる: {p:?}");

        // ── 同点 (どちらも 2 件) → ID 辞書順 ──
        let p = partition(
            &[
                task("y", &["core/a.rs", "core/b.rs"]),
                task("x", &["core/a.rs", "core/b.rs"]),
            ],
            &opts,
        );
        assert_eq!(p.shared[0].policy, Policy::Owner("x".into()));

        // ── その領域に多くのパターンを出した方が勝つ (辞書順より優先) ──
        // `core/**` は 1 本、`core/a.rs`+`core/b.rs` は 2 本。辞書順なら "a" が
        // 勝つところを、件数が上回る "z" が取る。
        let p = partition(
            &[
                task("a", &["core/**"]),
                task("z", &["core/a.rs", "core/b.rs"]),
            ],
            &opts,
        );
        assert_eq!(
            p.shared[0].policy,
            Policy::Owner("z".into()),
            "その領域に多く出した方へ寄せる"
        );
        assert_eq!(
            paths_of(&p, "z"),
            ["core/a.rs".to_string(), "core/b.rs".to_string()]
        );
        assert!(paths_of(&p, "a").is_empty());
        assert!(p.is_disjoint());

        // ── 3 タスク以上が跨る → 直列 (誰にも配らない) ──
        let p = partition(
            &[
                task("a", &["src/app.rs"]),
                task("b", &["src/app.rs"]),
                task("c", &["src/app.rs"]),
            ],
            &opts,
        );
        assert_eq!(p.shared.len(), 1);
        assert_eq!(p.shared[0].policy, Policy::Serialize);
        assert_eq!(p.shared[0].tasks.len(), 3);
        assert!(p.assignments.iter().all(|a| a.paths.is_empty()));
        assert!(p.is_disjoint());

        // ── 連鎖 (a—b, b—c だが a と c は重ならない) も 1 つの領域 ──
        let p = partition(
            &[
                task("a", &["src/x.rs"]),
                task("b", &["src/**"]),
                task("c", &["src/y.rs"]),
            ],
            &opts,
        );
        assert_eq!(p.shared.len(), 1, "連結成分は 1 つ: {p:?}");
        assert_eq!(p.shared[0].tasks.len(), 3);
        assert_eq!(p.shared[0].policy, Policy::Serialize);

        // ── 上限を上げれば 3 人でも寄せられる ──
        let p = partition(
            &[
                task("a", &["src/app.rs"]),
                task("b", &["src/app.rs"]),
                task("c", &["src/app.rs"]),
            ],
            &SplitOpts {
                max_owner_tasks: 3,
                ..SplitOpts::default()
            },
        );
        assert_eq!(p.shared[0].policy, Policy::Owner("a".into()));
        assert!(p.is_disjoint());
    }

    #[test]
    fn 追記だけのファイルは自動マージへ回す() {
        let opts = SplitOpts {
            union_globs: vec![normalize_path("CHANGELOG.md")],
            ..SplitOpts::default()
        };
        let p = partition(
            &[
                task("a", &["CHANGELOG.md", "src/a.rs"]),
                task("b", &["CHANGELOG.md", "src/b.rs"]),
            ],
            &opts,
        );
        assert_eq!(p.shared.len(), 1);
        assert_eq!(p.shared[0].policy, Policy::UnionMerge);
        // 追記マージへ回した領域は誰の専有にもならない
        assert_eq!(paths_of(&p, "a"), ["src/a.rs".to_string()]);
        assert_eq!(paths_of(&p, "b"), ["src/b.rs".to_string()]);
        assert!(p.is_disjoint());
        // 一致しないファイルは通常どおり
        let p2 = partition(&[task("a", &["src/app.rs"]), task("b", &["src/app.rs"])], &opts);
        assert_eq!(p2.shared[0].policy, Policy::Owner("a".into()));
    }

    #[test]
    fn 同じidが二回出たら和集合にする() {
        let p = partition(
            &[task("a", &["src/a.rs"]), task("a", &["src/b.rs"])],
            &SplitOpts::default(),
        );
        assert_eq!(p.assignments.len(), 1);
        assert_eq!(
            paths_of(&p, "a"),
            ["src/a.rs".to_string(), "src/b.rs".into()]
        );
        // 空 ID とパス無しは落とす (押しても何も起きない行を作らない)
        let p = partition(
            &[task("  ", &["src/a.rs"]), task("b", &[]), task("c", &["  "])],
            &SplitOpts::default(),
        );
        assert_eq!(p.assignments.len(), 2, "空 ID だけ落ちる: {p:?}");
        assert!(p.assignments.iter().all(|a| a.paths.is_empty()));
    }

    /// **この機能の価値そのもの**: `is_disjoint()` が偽になる入力が存在しない。
    ///
    /// 小さなパターン集合とタスク数で全組合せを回す (プロパティテスト風)。
    /// 3 タスク × 各 0〜2 パターン × 上限 3 通り = 1 万件強。
    #[test]
    fn 小さな全組合せで互いに素が破れない() {
        let universe = ["src/a.rs", "src/b.rs", "src/**", "src/*.rs", "docs/x.md"];
        // 各タスクが取り得るパターン集合 (0 個・1 個・2 個)
        let mut choices: Vec<Vec<&str>> = vec![Vec::new()];
        for (i, a) in universe.iter().enumerate() {
            choices.push(vec![*a]);
            for b in universe.iter().skip(i + 1) {
                choices.push(vec![*a, *b]);
            }
        }
        let ids = ["a", "b", "c"];
        let mut checked = 0usize;
        for max_owner in 1..=3usize {
            let opts = SplitOpts {
                max_owner_tasks: max_owner,
                ..SplitOpts::default()
            };
            for x in &choices {
                for y in &choices {
                    for z in &choices {
                        let tasks = vec![
                            task(ids[0], x),
                            task(ids[1], y),
                            task(ids[2], z),
                        ];
                        let p = partition(&tasks, &opts);
                        assert!(
                            p.is_disjoint(),
                            "互いに素が破れた: max_owner={max_owner} {x:?} {y:?} {z:?} → {p:?}"
                        );
                        // 落としたパターンは必ず共有側に記録が残る (黙って消さない)
                        let kept: BTreeSet<&str> = p
                            .assignments
                            .iter()
                            .flat_map(|a| a.paths.iter().map(String::as_str))
                            .collect();
                        let shared: BTreeSet<&str> = p
                            .shared
                            .iter()
                            .flat_map(|s| s.paths.iter().map(String::as_str))
                            .collect();
                        for pat in x.iter().chain(y).chain(z) {
                            let n = normalize_path(pat);
                            assert!(
                                kept.contains(n.as_str()) || shared.contains(n.as_str()),
                                "{n} が担当表からも共有一覧からも消えた: {p:?}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 10_000, "組合せが少なすぎる: {checked}");
    }

    // -----------------------------------------------------------------
    // 書式・出力
    // -----------------------------------------------------------------

    #[test]
    fn タスク行を読む() {
        let text = "# コメント\n\nui: src/app.rs, src/panels.rs\ncore:\tsrc/lease.rs\ndocs/**\n";
        let tasks = parse_tasks(text);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, "ui");
        assert_eq!(tasks[0].paths, vec!["src/app.rs", "src/panels.rs"]);
        assert_eq!(tasks[1].id, "core");
        assert_eq!(tasks[2].id, "#3", "ID を省いた行にも安定した名前を振る");
        assert_eq!(tasks[2].paths, vec!["docs/**"]);
    }

    #[test]
    fn プロンプト行は空なら一文字も出さない() {
        assert_eq!(scope_line(&[]), "");
        let line = scope_line(&["src/a.rs".to_string(), "docs/**".into()]);
        assert!(line.starts_with("あなたが触ってよいのは次のパスだけです: "));
        assert!(line.contains("src/a.rs docs/**"));
        // 画面の言語設定で送る指示が揺れない (tr を通していない)
        assert!(!line.is_empty());
    }

    #[test]
    fn 担当表とjsonが同じ結論を出す() {
        let p = partition(
            &[
                task("a", &["src/a.rs", "src/app.rs"]),
                task("b", &["src/b.rs", "src/app.rs"]),
            ],
            &SplitOpts::default(),
        );
        let table = render_table(&p);
        assert!(table.contains("互いに素: はい"), "{table}");
        assert!(table.contains("[a] 2 件"), "{table}");
        assert!(table.contains("共有パス"), "{table}");
        assert!(table.contains("あなたが触ってよいのは次のパスだけです"), "{table}");

        let v: serde_json::Value = serde_json::from_str(&render_json(&p)).expect("JSON");
        assert_eq!(v["disjoint"], serde_json::json!(true));
        assert_eq!(v["clean"], serde_json::json!(false));
        assert_eq!(v["assignments"][0]["id"], serde_json::json!("a"));
        assert_eq!(v["shared"][0]["policy"], serde_json::json!("owner"));
        assert_eq!(v["shared"][0]["owner"], serde_json::json!("a"));
    }

    // -----------------------------------------------------------------
    // CLI
    // -----------------------------------------------------------------

    #[test]
    fn cliの終了コード() {
        let dir = crate::test_util::unique_temp_dir("zv-split", "cli");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let clean = dir.join("clean.txt");
        std::fs::write(&clean, "a: src/a.rs\nb: src/b.rs\n").expect("write");
        let dirty = dir.join("dirty.txt");
        std::fs::write(&dirty, "a: src/app.rs\nb: src/app.rs\n").expect("write");
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();

        let ok = ["plan".to_string(), "--tasks".into(), s(&clean)];
        assert_eq!(cli_main(&ok), 0, "互いに素な分割ができた");
        let ng = ["plan".to_string(), "--tasks".into(), s(&dirty), "--json".into()];
        assert_eq!(cli_main(&ng), 1, "共有パスが残った");

        // 使い方の誤りは 2
        assert_eq!(cli_main(&[]), 2);
        assert_eq!(cli_main(&["nope".to_string()]), 2);
        assert_eq!(cli_main(&["plan".to_string()]), 2, "--tasks が無い");
        assert_eq!(
            cli_main(&["plan".to_string(), "--tasks".into()]),
            2,
            "値が無い"
        );
        assert_eq!(
            cli_main(&[
                "plan".to_string(),
                "--tasks".into(),
                s(&clean),
                "--max-owner".into(),
                "many".into()
            ]),
            2
        );
        assert_eq!(
            cli_main(&[
                "plan".to_string(),
                "--tasks".into(),
                s(&dir.join("no-such-file.txt"))
            ]),
            2,
            "読めないファイル"
        );
        // ヘルプは 0
        assert_eq!(cli_main(&["--help".to_string()]), 0);
        assert_eq!(cli_main(&["plan".to_string(), "--help".into()]), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 使い方に書式と終了コードが載っている() {
        let u = usage();
        for needle in [
            "--tasks",
            "--json",
            "--union",
            "--max-owner",
            "終了コード",
            "<ID>:",
        ] {
            assert!(u.contains(needle), "使い方に {needle} が無い:\n{u}");
        }
    }

    // -----------------------------------------------------------------
    // レイアウト
    // -----------------------------------------------------------------

    #[test]
    fn どの幅でも矩形が領域内で重ならない() {
        let sizes = [
            (900.0f32, 700.0f32),
            (1200.0, 300.0),
            (720.0, 520.0),
            (420.0, 240.0),
            (320.0, 120.0),
            (200.0, 50.0),
            (0.0, 0.0),
        ];
        for (w, h) in sizes {
            // 原点をずらしても成り立つこと (ウィンドウは画面の任意の位置に出る)
            for origin in [egui::pos2(0.0, 0.0), egui::pos2(137.0, 91.0)] {
                let avail = egui::Rect::from_min_size(origin, egui::vec2(w, h));
                let lay = layout(avail);
                assert!(
                    avail.contains_rect(lay.input),
                    "入力欄がはみ出す {w}x{h}: {lay:?}"
                );
                assert!(
                    avail.contains_rect(lay.result),
                    "結果がはみ出す {w}x{h}: {lay:?}"
                );
                // 高さのある矩形どうしは決して重ならない
                // (どちらかが潰れている極小領域は「重なり」を問わない)
                if lay.input.height() > 0.0 && lay.result.height() > 0.0 {
                    assert!(
                        !lay.input.intersects(lay.result),
                        "2 つが重なる {w}x{h}: {lay:?}"
                    );
                }
                assert!(lay.input.height() >= 0.0 && lay.result.height() >= 0.0);
            }
        }
        // 普通の大きさでは両方に実用的な高さが残る
        let big = layout(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 700.0),
        ));
        assert!(big.input.height() >= INPUT_MIN_H);
        assert!(big.result.height() >= RESULT_MIN_H);
        assert!(!big.compact);
        let wide_short = layout(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 300.0),
        ));
        assert!(wide_short.input.height() >= INPUT_MIN_H);
        assert!(wide_short.result.height() >= RESULT_MIN_H);
        // 狭いときはボタンをアイコンだけへ縮退させる
        assert!(layout(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 240.0))).compact);
    }

    /// 共有ファイルへ 1 バイトも追記していないこと (レジストリの約束)。
    #[test]
    fn 登録は共有ファイルを触らずに済んでいる() {
        let reg = include_str!("features/split.rs").replace("\r\n", "\n");
        assert!(
            reg.contains("pub use imp::{cli_main, FEATURE};"),
            "features/split.rs は再エクスポートだけにする:\n{reg}"
        );
        // FEATURE の ID はモジュール接頭辞付き (feature::tests が全体を検査する)
        assert_eq!(FEATURE.module, "split");
        assert!(FEATURE.entries.iter().all(|e| e.id.starts_with("split.")));
        assert!(FEATURE.draw.is_some(), "ウィンドウを自分で描く");
    }
}
