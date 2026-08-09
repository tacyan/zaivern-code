//! **What's New** — 更新後の初回起動で「何が変わったか」を 1 度だけ見せる。
//!
//! ## 方針
//!
//! - **材料は `CHANGELOG.md` 1 本。** `include_str!` でバイナリへ埋め込むので、
//!   実行時にファイルを探しに行かない (配置に依存せず、どの OS でも同じ)。
//!   画面に出す文章をコード側へ二重に書かない。
//! - **勝手に画面を組み替えない** (UI の原則)。開くのはウィンドウ 1 枚で、
//!   レイアウトには手を触れない。
//! - **出すのは 1 度だけ。** 見た版を `Config::last_seen_version` に覚え、
//!   次からは黙っている。ヘルプメニューからはいつでも開ける。
//! - **判断はすべて純粋関数。** 解析も「出すか否か」もファイルにも egui にも
//!   触らないので、テーブルテストで固定できる。

/// 1 つの版の変更点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// `0.9.0` のような版 (見出しから取る)。
    pub version: String,
    /// 見出しの残り (日付など)。無ければ空。
    pub date: String,
    /// 箇条書きの項目 (先頭の `- ` は落としてある)。
    pub items: Vec<String>,
}

/// 同梱の変更履歴。**画面に出す文章の唯一の出所**。
pub const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// 変更履歴を解析する (純関数)。
///
/// 拾うのは `## <版> — <日付>` の見出しと、その下の `- ` 項目だけ。
/// 版として読めない見出し (`# 変更履歴` など) は**丸ごと飛ばす**ので、
/// 前書きに何を書いても壊れない。
pub fn parse(md: &str) -> Vec<Release> {
    let mut out: Vec<Release> = Vec::new();
    for raw in md.lines() {
        // Windows のチェックアウトは CRLF。行末の \r を落としてから見る。
        let line = raw.trim_end_matches('\r');
        if let Some(head) = line.strip_prefix("## ") {
            let head = head.trim();
            // 見出しの先頭トークンが版。区切りは `—` でも `-` でも空白でもよい。
            let (ver, rest) = split_version(head);
            if ver.is_empty() {
                continue;
            }
            out.push(Release {
                version: ver,
                date: rest,
                items: Vec::new(),
            });
            continue;
        }
        if let Some(item) = line.strip_prefix("- ") {
            // 見出しより前の箇条書き (書式の説明など) は捨てる。
            if let Some(last) = out.last_mut() {
                let t = item.trim();
                if !t.is_empty() {
                    last.items.push(t.to_string());
                }
            }
        }
    }
    // 項目が 1 つも無い版は出さない (見出しだけの空セクションを描かない)。
    out.retain(|r| !r.items.is_empty());
    out
}

/// 見出しから版と残りを切り分ける。版として読めなければ空文字を返す。
fn split_version(head: &str) -> (String, String) {
    let mut it = head.splitn(2, char::is_whitespace);
    let first = it.next().unwrap_or("").trim();
    let rest = it.next().unwrap_or("").trim();
    if !is_version(first) {
        return (String::new(), String::new());
    }
    // 残りの先頭にある区切り記号を落とす (`— 2026-08-09` → `2026-08-09`)。
    let rest = rest.trim_start_matches(['—', '–', '-', ':', '·']).trim();
    (first.to_string(), rest.to_string())
}

/// `1.2.3` の形か。**数字とドットだけ**を認める (`v1.2` や `[1.2.3]` は弾く)。
fn is_version(s: &str) -> bool {
    let mut parts = 0usize;
    for seg in s.split('.') {
        if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// 版を数値の組へ (比較用)。読めない部分は 0 として扱う。
fn parts_of(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect()
}

/// `a` が `b` より新しいか (純関数)。
///
/// 桁数が違っても比べられる (`0.9` < `0.9.1`)。文字列比較にしないのは
/// `0.10.0` が `0.9.0` より**小さく**なってしまうため。
pub fn is_newer(a: &str, b: &str) -> bool {
    let (x, y) = (parts_of(a), parts_of(b));
    let n = x.len().max(y.len());
    for i in 0..n {
        let (l, r) = (
            x.get(i).copied().unwrap_or(0),
            y.get(i).copied().unwrap_or(0),
        );
        if l != r {
            return l > r;
        }
    }
    false
}

/// **見せるべき版**を新しい順で返す (純関数)。
///
/// - `last_seen` が空 = 初回起動。**何も出さない** — 入れた直後に
///   変更履歴を突き付けても意味が無く、「画面が突然変わらない」に反する。
/// - `last_seen` が `current` 以上なら空 (もう見ている)。
/// - それ以外は `last_seen` より新しく `current` 以下の版だけを返す。
pub fn unseen(releases: &[Release], last_seen: &str, current: &str) -> Vec<Release> {
    if last_seen.trim().is_empty() {
        return Vec::new();
    }
    releases
        .iter()
        .filter(|r| is_newer(&r.version, last_seen) && !is_newer(&r.version, current))
        .cloned()
        .collect()
}

/// このビルドの版。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 同梱の変更履歴を解析したもの。
pub fn releases() -> Vec<Release> {
    parse(CHANGELOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# 変更履歴\n\
        \n\
        - これは前書きの箇条書き (版の前なので捨てる)\n\
        \n\
        ## 0.9.0 — 2026-08-09\n\
        \n\
        - 指示が実行されない不具合を直した\n\
        - 文字サイズを変えられるようにした\n\
        \n\
        ## 0.8.0 — 2026-08-08\n\
        \n\
        - 初回の公開版\n";

    #[test]
    fn 見出しと項目を拾う() {
        let r = parse(SAMPLE);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].version, "0.9.0");
        assert_eq!(r[0].date, "2026-08-09");
        assert_eq!(r[0].items.len(), 2);
        assert_eq!(r[1].version, "0.8.0");
    }

    /// 版の前に置いた箇条書き (書式の説明など) を拾わない。
    #[test]
    fn 前書きの箇条書きは捨てる() {
        let r = parse(SAMPLE);
        assert!(!r
            .iter()
            .any(|x| x.items.iter().any(|i| i.contains("前書き"))));
    }

    /// CRLF のチェックアウトでも壊れない (Windows)。
    #[test]
    fn crlf_でも解析できる() {
        let crlf = SAMPLE.replace('\n', "\r\n");
        assert_eq!(parse(&crlf), parse(SAMPLE));
    }

    #[test]
    fn 版として読めない見出しは飛ばす() {
        let md = "## お知らせ\n- 何か\n## 1.0.0\n- 中身\n";
        let r = parse(md);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].version, "1.0.0");
    }

    /// 項目が 1 つも無い版は出さない (空セクションを描かない)。
    #[test]
    fn 中身の無い版は出さない() {
        assert!(parse("## 1.0.0 — 今日\n").is_empty());
    }

    /// **文字列比較にしない。** `0.10.0` は `0.9.0` より新しい。
    #[test]
    fn 版の比較は数値で行う() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        // 桁数が違っても比べられる
        assert!(is_newer("0.9.1", "0.9"));
        assert!(!is_newer("0.9", "0.9.0"));
    }

    /// 初回起動 (見た版が空) では**何も出さない**。
    /// 入れた直後に変更履歴を突き付けても意味が無い。
    #[test]
    fn 初回起動では出さない() {
        assert!(unseen(&parse(SAMPLE), "", "0.9.0").is_empty());
    }

    #[test]
    fn 見ていない版だけを新しい順で返す() {
        let got = unseen(&parse(SAMPLE), "0.8.0", "0.9.0");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].version, "0.9.0");
    }

    #[test]
    fn 既に見ていれば空() {
        assert!(unseen(&parse(SAMPLE), "0.9.0", "0.9.0").is_empty());
        // 手で未来の版を書いた config でも暴走しない
        assert!(unseen(&parse(SAMPLE), "99.0.0", "0.9.0").is_empty());
    }

    /// 動いているビルドより新しい版は出さない
    /// (履歴に次版の下書きが入っていても、まだ入っていない機能を宣伝しない)。
    #[test]
    fn 現在の版より新しいものは出さない() {
        let md = "## 2.0.0\n- 未来\n## 0.9.0\n- いま\n";
        let got = unseen(&parse(md), "0.8.0", "0.9.0");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].version, "0.9.0");
    }

    /// **同梱の CHANGELOG.md が実際に解析できること。**
    /// 書式を崩したら What's New が空になるので、ここで落とす。
    #[test]
    fn 同梱の変更履歴が解析できる() {
        let r = releases();
        assert!(!r.is_empty(), "CHANGELOG.md から 1 件も読めていない");
        assert!(
            r.iter().all(|x| !x.items.is_empty()),
            "項目の無い版が混ざっている"
        );
    }

    /// **いま動いている版が CHANGELOG に載っていること。**
    /// 版を上げたのに履歴を書き忘れる、を落とす。
    #[test]
    fn 現在の版が変更履歴に載っている() {
        let cur = current_version();
        assert!(
            releases().iter().any(|r| r.version == cur),
            "Cargo.toml の版 {cur} が CHANGELOG.md に無い"
        );
    }
}
