//! ブレッドクラム (VS Code のパンくずリスト) の**データと省略判断だけ**を持つ層。
//!
//! `ワークスペース › フォルダ › ファイル › シンボル階層` を 1 行で出す。
//!
//! * **パス部分は LSP 不要**。ワークスペースルートからの相対パスを分解するだけなので、
//!   サーバーが無い言語でもブレッドクラムが消えることはない。
//! * シンボル階層は `documentSymbol` の結果が**そのファイルのぶんとして届いている
//!   ときだけ**足す。届かなくても行は消えないし、後から届いても
//!   **高さは変わらない** (常に 1 行)。
//! * 幅に収まらないときは中央を「…」で省略する ([`elide`])。判断は純関数なので
//!   テーブルテストで固定できる。

use std::path::{Path, PathBuf};

/// セグメントを押したときに何が起きるか。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegKind {
    /// ワークスペースルート / 途中のフォルダ → ファイルツリーで開いて選択する。
    Folder(PathBuf),
    /// ファイル → ファイルパレットを開く。
    File(PathBuf),
    /// シンボル → その行へジャンプする (0 始まりの行番号)。
    Symbol { line: usize },
}

/// ブレッドクラムの 1 区切り。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Segment {
    pub label: String,
    pub kind: SegKind,
}

/// 区切り記号 (VS Code と同じ「›」)。
pub const SEP: &str = "›";
/// 省略記号。
pub const ELLIPSIS: &str = "…";

/// ルートの見出しに使う名前 (末尾の要素。取れなければパスそのもの)。
pub fn root_label(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.display().to_string())
}

/// パスとシンボル階層からセグメント列を組む。
///
/// * `roots` のどれかの配下なら `ルート › 途中のフォルダ… › ファイル`。
/// * どのルートにも属さないファイル (外部から開いたファイル) は
///   `親フォルダ › ファイル` に落とす — **必ずファイル名は出る**。
/// * `symbols` は `(名前, 定義行 0 始まり)` の外側→内側順。
pub fn segments(roots: &[PathBuf], path: &Path, symbols: &[(String, usize)]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let root = crate::file_tree::root_for(roots, path);
    match root {
        Some(r) => {
            out.push(Segment {
                label: root_label(r),
                kind: SegKind::Folder(r.to_path_buf()),
            });
            let rel = path.strip_prefix(r).unwrap_or(path);
            let mut acc = r.to_path_buf();
            let comps: Vec<_> = rel.components().collect();
            for (i, c) in comps.iter().enumerate() {
                acc.push(c.as_os_str());
                let label = c.as_os_str().to_string_lossy().to_string();
                let last = i + 1 == comps.len();
                out.push(Segment {
                    label,
                    kind: if last {
                        SegKind::File(acc.clone())
                    } else {
                        SegKind::Folder(acc.clone())
                    },
                });
            }
        }
        None => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                out.push(Segment {
                    label: root_label(parent),
                    kind: SegKind::Folder(parent.to_path_buf()),
                });
            }
            out.push(Segment {
                label: path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                kind: SegKind::File(path.to_path_buf()),
            });
        }
    }
    for (name, line) in symbols {
        out.push(Segment {
            label: name.clone(),
            kind: SegKind::Symbol { line: *line },
        });
    }
    out
}

/// LSP のシンボル木から、`line` (0 始まり) を含む階層を外側→内側順に取り出す。
///
/// 既存の `documentSymbol` の応答 (`app.rs` の `lsp_symbols`) をそのまま読む。
/// 新しい要求は投げない。
pub fn symbol_chain(nodes: &[crate::lsp::SymbolNode], line: usize) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut cur: &[crate::lsp::SymbolNode] = nodes;
    // 深さは実用上たかが知れているが、壊れた応答での無限ループを避けるため上限を付ける
    for _ in 0..32 {
        let Some(n) = cur
            .iter()
            .find(|n| n.range.start.line <= line && line <= n.range.end.line)
        else {
            break;
        };
        out.push((n.name.clone(), n.selection_range.start.line));
        cur = &n.children;
    }
    out
}

// ===========================================================================
// 省略 (純関数)
// ===========================================================================

/// 実際に描く要素。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shown {
    /// `widths` の添字のセグメントをそのまま描く。
    Seg(usize),
    /// 省略記号「…」。
    Ellipsis,
    /// この幅まで切り詰めて描く (最後の 1 枚すら入らないときの最終手段)。
    Truncated { index: usize, budget: f32 },
}

impl Shown {
    fn width(&self, widths: &[f32]) -> f32 {
        match self {
            Shown::Seg(i) => widths.get(*i).copied().unwrap_or(0.0),
            // 「…」の幅は `widths` に無いので total_width 側で ell_w を足す
            Shown::Ellipsis => 0.0,
            Shown::Truncated { budget, .. } => *budget,
        }
    }
}

/// 表示列の総幅。`elide` の結果は必ず `avail` 以下になる。
pub fn total_width(shown: &[Shown], widths: &[f32], sep_w: f32, ell_w: f32) -> f32 {
    if shown.is_empty() {
        return 0.0;
    }
    let mut w = sep_w * (shown.len() - 1) as f32;
    for s in shown {
        w += match s {
            Shown::Ellipsis => ell_w,
            other => other.width(widths),
        };
    }
    w
}

/// 幅に収まる表示列を決める。
///
/// 収まらないときは**中央を省略**する (`workspace › … › file.rs › fn foo`)。
/// どれだけ狭くても**最後のセグメント (ファイル名 / いちばん内側のシンボル) は必ず残る**。
/// 返り値の総幅 ([`total_width`]) は必ず `avail` 以下。
pub fn elide(widths: &[f32], sep_w: f32, ell_w: f32, avail: f32) -> Vec<Shown> {
    let n = widths.len();
    if n == 0 {
        return Vec::new();
    }
    let avail = if avail.is_finite() {
        avail.max(0.0)
    } else {
        0.0
    };
    let fits = |v: &[Shown]| total_width(v, widths, sep_w, ell_w) <= avail + 1e-3;

    // 1. 全部入る
    let all: Vec<Shown> = (0..n).map(Shown::Seg).collect();
    if fits(&all) {
        return all;
    }
    // 2. 先頭 (ワークスペース) + … + 入るだけの末尾
    for k in 1..n {
        let mut v = vec![Shown::Seg(0), Shown::Ellipsis];
        v.extend((k..n).map(Shown::Seg));
        if fits(&v) {
            return v;
        }
    }
    // 3. 先頭も落として … + 入るだけの末尾
    for k in 1..n {
        let mut v = vec![Shown::Ellipsis];
        v.extend((k..n).map(Shown::Seg));
        if fits(&v) {
            return v;
        }
    }
    // 4. 最後の 1 枚だけ
    let last = vec![Shown::Seg(n - 1)];
    if fits(&last) {
        return last;
    }
    // 5. 最後の 1 枚を切り詰める (これ以上は縮められない)
    vec![Shown::Truncated {
        index: n - 1,
        budget: avail,
    }]
}

/// ラベルを `budget` の幅へ切り詰める (末尾に「…」を付ける)。
/// `char_w` は 1 文字あたりの概算幅。0 以下なら空文字を返す。
pub fn truncate_label(label: &str, budget: f32, char_w: f32) -> String {
    if char_w <= 0.0 || budget <= 0.0 {
        return String::new();
    }
    let max_chars = (budget / char_w).floor() as usize;
    if max_chars == 0 {
        return String::new();
    }
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    if max_chars == 1 {
        return ELLIPSIS.to_string();
    }
    let keep = max_chars - 1;
    let mut s: String = label.chars().take(keep).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{Position, Range, SymbolNode};

    fn node(name: &str, s: usize, e: usize, children: Vec<SymbolNode>) -> SymbolNode {
        SymbolNode {
            name: name.into(),
            detail: String::new(),
            kind: 12,
            range: Range::new(Position::new(s, 0), Position::new(e, 0)),
            selection_range: Range::new(Position::new(s, 3), Position::new(s, 9)),
            deprecated: false,
            children,
        }
    }

    // ── セグメントの組み立て ──────────────────────────────────────

    #[test]
    fn segments_from_workspace_relative_path() {
        let root = PathBuf::from("/w/proj");
        let p = PathBuf::from("/w/proj/src/ui/panel.rs");
        let segs = segments(&[root.clone()], &p, &[]);
        let labels: Vec<&str> = segs.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["proj", "src", "ui", "panel.rs"]);
        assert_eq!(segs[0].kind, SegKind::Folder(root));
        assert_eq!(segs[1].kind, SegKind::Folder(PathBuf::from("/w/proj/src")));
        assert_eq!(segs[3].kind, SegKind::File(p));
    }

    #[test]
    fn segments_without_lsp_still_show_the_file() {
        let segs = segments(&[PathBuf::from("/w")], &PathBuf::from("/w/a.txt"), &[]);
        assert_eq!(segs.len(), 2, "LSP が無くてもパスは必ず出る");
        assert!(matches!(segs.last().unwrap().kind, SegKind::File(_)));
    }

    #[test]
    fn segments_outside_roots_fall_back_to_parent_and_name() {
        let segs = segments(&[PathBuf::from("/w")], &PathBuf::from("/other/x/y.rs"), &[]);
        let labels: Vec<&str> = segs.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["x", "y.rs"]);
    }

    #[test]
    fn segments_append_symbol_chain() {
        let segs = segments(
            &[PathBuf::from("/w")],
            &PathBuf::from("/w/a.rs"),
            &[("Foo".into(), 3), ("bar".into(), 9)],
        );
        let labels: Vec<&str> = segs.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["w", "a.rs", "Foo", "bar"]);
        assert_eq!(segs[3].kind, SegKind::Symbol { line: 9 });
    }

    #[test]
    fn symbol_chain_picks_the_innermost_container() {
        let tree = vec![
            node("Alpha", 0, 20, vec![node("inner", 5, 9, vec![])]),
            node("Beta", 21, 40, vec![]),
        ];
        assert_eq!(
            symbol_chain(&tree, 7),
            vec![("Alpha".into(), 0), ("inner".into(), 5)]
        );
        assert_eq!(symbol_chain(&tree, 15), vec![("Alpha".into(), 0)]);
        assert_eq!(symbol_chain(&tree, 30), vec![("Beta".into(), 21)]);
        assert!(symbol_chain(&tree, 100).is_empty(), "範囲外は空");
        assert!(symbol_chain(&[], 3).is_empty());
    }

    // ── 省略 ──────────────────────────────────────────────────────

    /// 極端な幅でも「可用幅に収まる」「最後のセグメントが残る」が必ず成り立つ。
    #[test]
    fn elide_table_always_fits_and_keeps_the_file_name() {
        let sep = 10.0f32;
        let ell = 12.0f32;
        // (セグメント幅, 可用幅, 期待する表示列)
        let cases: Vec<(Vec<f32>, f32)> = vec![
            (vec![60.0, 40.0, 80.0], 900.0),
            (vec![60.0, 40.0, 80.0], 200.0),
            (vec![60.0, 40.0, 80.0], 120.0),
            (vec![60.0, 40.0, 80.0], 90.0),
            (vec![60.0, 40.0, 80.0], 80.0),
            (vec![60.0, 40.0, 80.0], 40.0),
            (vec![60.0, 40.0, 80.0], 0.0),
            (vec![300.0], 200.0),
            (vec![300.0], 1200.0),
            (vec![70.0, 55.0, 55.0, 90.0, 60.0, 45.0, 120.0], 1200.0),
            (vec![70.0, 55.0, 55.0, 90.0, 60.0, 45.0, 120.0], 300.0),
            (vec![70.0, 55.0, 55.0, 90.0, 60.0, 45.0, 120.0], 200.0),
            (vec![70.0, 55.0, 55.0, 90.0, 60.0, 45.0, 120.0], 130.0),
            (vec![70.0, 55.0, 55.0, 90.0, 60.0, 45.0, 120.0], 20.0),
        ];
        for (widths, avail) in &cases {
            let v = elide(widths, sep, ell, *avail);
            let w = total_width(&v, widths, sep, ell);
            assert!(
                w <= *avail + 1e-3,
                "幅 {avail} / {widths:?} で可用幅を超えた ({w}): {v:?}"
            );
            assert!(!v.is_empty(), "幅 {avail} で全部消えた");
            let last = widths.len() - 1;
            assert!(
                v.iter().any(|s| matches!(s, Shown::Seg(i) if *i == last)
                    || matches!(s, Shown::Truncated { index, .. } if *index == last)),
                "幅 {avail} で最後のセグメントが消えた: {v:?}"
            );
            // 「…」は 1 個まで・先頭要素は必ず添字 0 か「…」
            assert!(
                v.iter().filter(|s| matches!(s, Shown::Ellipsis)).count() <= 1,
                "省略記号が 2 個以上出た: {v:?}"
            );
            // 添字は昇順で重複しない
            let idx: Vec<usize> = v
                .iter()
                .filter_map(|s| match s {
                    Shown::Seg(i) => Some(*i),
                    Shown::Truncated { index, .. } => Some(*index),
                    Shown::Ellipsis => None,
                })
                .collect();
            assert!(idx.windows(2).all(|w| w[0] < w[1]), "順序が壊れた: {v:?}");
        }
    }

    #[test]
    fn elide_keeps_workspace_head_when_it_can() {
        let widths = vec![60.0, 40.0, 40.0, 80.0];
        // 60 + 10 + 12 + 10 + 80 = 172
        let v = elide(&widths, 10.0, 12.0, 180.0);
        assert_eq!(v, vec![Shown::Seg(0), Shown::Ellipsis, Shown::Seg(3)]);
    }

    #[test]
    fn elide_empty_input_is_empty() {
        assert!(elide(&[], 10.0, 12.0, 500.0).is_empty());
    }

    #[test]
    fn elide_rejects_non_finite_width() {
        let v = elide(&[80.0], 10.0, 12.0, f32::NAN);
        assert_eq!(
            v,
            vec![Shown::Truncated {
                index: 0,
                budget: 0.0
            }]
        );
    }

    #[test]
    fn truncate_label_never_exceeds_budget() {
        let cw = 8.0;
        for budget in [0.0f32, 4.0, 8.0, 16.0, 40.0, 400.0] {
            let s = truncate_label("very_long_symbol_name", budget, cw);
            assert!(
                s.chars().count() as f32 * cw <= budget + 1e-3,
                "budget={budget} で溢れた: {s:?}"
            );
        }
        assert_eq!(truncate_label("abc", 400.0, 8.0), "abc", "収まるなら無加工");
        assert_eq!(truncate_label("abc", 400.0, 0.0), "", "文字幅 0 は空");
    }
}
