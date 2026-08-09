//! `apply_patch` のパッチ本文から**書き込まれるパス**を抜く純関数。
//!
//! ## なぜ要るか
//!
//! ファイル所有リース ([`crate::lease`]) の強制は、フックの payload から
//! 対象を決めている。`Edit` / `Write` のようなツールは `tool_input` に
//! **パス欄**を持つので [`crate::agents::HookTarget::write_path_keys`] で引けるが、
//! `apply_patch` は**パス欄を持たない** — 対象は patch 本文の中に
//! `*** Update File: <path>` として書かれている。
//!
//! そのままではリースの判定に入る前に「宛先が無い」と見えるので、
//! **codex のファイル編集が丸ごと素通り**する。ここはその穴を塞ぐ。
//!
//! ## 文法 (実機で確認した)
//!
//! codex-cli 0.147.0 の実行ファイルに埋まっている文字列を採取した
//! (`strings <codex> | grep '^\*\*\* '`):
//!
//! ```text
//! *** Begin Patch
//! *** Update File: <path>
//! *** Move to: <path>          … 改名の宛先 (Update File に続く)
//! *** Add File: <path>
//! *** Delete File: <path>
//! *** End Patch
//! ```
//!
//! `*** Move to:` は**改名の宛先**なので、元と宛先の両方が書き込み対象になる
//! (`cmdwrite` が `mv` の両側を拾うのと同じ理由 — 元は消えるため)。
//!
//! ## 方針: 取りこぼしより過検出 (fail-closed 側)
//!
//! [`crate::agents::cmdwrite`] と同じ。1 件余計に拾っても、書く本人が
//! リースを持っていれば通る。逆に取りこぼすと**黙って上書きされる**。
//!
//! ## 拾わないもの
//!
//! - **差分の本文行**。`+*** Add File: 偽物` は「追加された行の中身」であって
//!   宣言ではない。ここを見落とすと、**パッチの中身に 1 行書くだけで
//!   他人が確保中のパスを名乗れて**しまう (逆に、宣言を本文と誤認すると
//!   本物の書き込みを見逃す)。行頭が `+` / `-` のものは中身として捨てる
//! - 印の後ろが空の行 (`*** Update File:` だけ)
//! - 印に一致しない `*** ...` 行 (`*** Begin Patch` / `*** End Patch` など)
//! - `echo "*** Update File: x"` のような、印で**始まっていない**行
//!
//! ## 拾えないもの (正直に書く)
//!
//! - 本文にたまたま印と同じ形の行が**字下げ付きで**現れた場合は拾ってしまう
//!   (差分の文脈行は先頭が空白 1 個なので区別が付かない)。過検出側なので
//!   そのままにしてある
//!
//! ## 使う側へ
//!
//! 入口は [`crate::agents::hook_write_targets`]。**どのエージェントの
//! どのツールの、どのキーに本文が載るか**はベンダー固有なので
//! カタログ (`agents.rs` の [`crate::agents::HookTarget::patch_tools`]) 側にある。
//! このモジュールは `apply_patch` という**書式**だけを知っていて、
//! エージェント固有値を 1 つも持たない (`cmdwrite` と同じ流儀)。

/// パスを宣言する印。この後ろが対象のパス。
const FILE_MARKERS: &[&str] = &[
    "*** Update File:",
    "*** Add File:",
    "*** Delete File:",
    // 改名の宛先。元 (`Update File`) と両方が書き込み対象になる。
    "*** Move to:",
];

/// パスを持たない枠の印。「パッチらしさ」の判定にだけ使う。
const FRAME_MARKERS: &[&str] = &["*** Begin Patch", "*** End Patch"];

/// この行は**差分の中身**か (宣言ではないか)。
///
/// apply_patch の本文では、足す行が `+`、消す行が `-` で始まる。
/// 中身を宣言と取り違えると、パッチに 1 行書くだけで他人のパスを名乗れる。
fn is_diff_body(line: &str) -> bool {
    line.starts_with('+') || line.starts_with('-')
}

/// パッチ本文から**書き込み先のパス**を全部出す (現れた順、重複なし)。
///
/// パッチでなければ空。判定は呼び出し側の責務ではなく、ここで完結する。
pub fn patch_targets(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in body.lines() {
        // Windows のチェックアウト / CRLF のヒアドキュメント対策。
        let line = raw.trim_end_matches('\r');
        if is_diff_body(line) {
            continue;
        }
        let head = line.trim_start();
        // 印以外を読みに行かない (`echo "…"` のような行をここで落とす)。
        if !head.starts_with("***") {
            continue;
        }
        for m in FILE_MARKERS {
            let Some(rest) = head.strip_prefix(m) else {
                continue;
            };
            let path = rest.trim();
            if !path.is_empty() && !out.iter().any(|x| x == path) {
                out.push(path.to_string());
            }
            break;
        }
    }
    out
}

/// 「これはパッチ本文らしい」か。
///
/// [`patch_targets`] が空を返したときに、**パッチなのに宛先が取れなかった**のか
/// **そもそもパッチではない**のかを分けるために使う。前者は監査に残す価値がある
/// (`HookWrite::opaque`)。
pub fn looks_like_patch(body: &str) -> bool {
    body.lines().any(|raw| {
        let line = raw.trim_end_matches('\r');
        if is_diff_body(line) {
            return false;
        }
        let head = line.trim_start();
        FRAME_MARKERS.iter().any(|m| head.starts_with(m))
            || FILE_MARKERS.iter().any(|m| head.starts_with(m))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実際に codex が出す形。改名は元と宛先の両方が対象になる。
    const REAL: &str = "*** Begin Patch\n\
                        *** Update File: src/app.rs\n\
                        @@ fn main()\n\
                        -old\n\
                        +new\n\
                        *** Add File: src/new_mod.rs\n\
                        +fn hello() {}\n\
                        *** Delete File: src/old_mod.rs\n\
                        *** Update File: src/from.rs\n\
                        *** Move to: src/to.rs\n\
                        *** End Patch";

    #[test]
    fn パッチ本文から更新と追加と削除と改名の全パスが出る() {
        assert_eq!(
            patch_targets(REAL),
            vec![
                "src/app.rs",
                "src/new_mod.rs",
                "src/old_mod.rs",
                "src/from.rs",
                "src/to.rs",
            ]
        );
        assert!(looks_like_patch(REAL));
    }

    #[test]
    fn パスらしくない行を拾わない() {
        // (入力, 期待するパス) — 拾ってはいけない形を並べる
        let cases: &[(&str, &[&str])] = &[
            // 枠の印はパスを持たない
            ("*** Begin Patch\n*** End Patch", &[]),
            // 印の後ろが空
            ("*** Update File:\n*** Add File:   ", &[]),
            // 知らない `***` 行
            ("*** Note: これはパッチではない\n*** 何か", &[]),
            // **差分の中身**。ここを拾うと他人のパスを名乗れてしまう
            (
                "*** Begin Patch\n+*** Add File: 偽物.rs\n-*** Delete File: 偽物2.rs",
                &[],
            ),
            // 印で始まっていない行 (シェルの引用の中など)
            (
                "echo \"*** Update File: not-a-patch.rs\"\ngrep '*** Add File: x'",
                &[],
            ),
            // 空・空白のみ
            ("", &[]),
            ("\n\n   \n", &[]),
            // 印の途中までしか無い
            ("*** Update Fil: src/x.rs", &[]),
        ];
        for (body, want) in cases {
            assert_eq!(
                patch_targets(body),
                *want,
                "拾ってはいけない行を拾った: {body:?}"
            );
        }
    }

    #[test]
    fn 同じパスは一度しか出さない() {
        let body = "*** Update File: a.rs\n*** Update File: a.rs\n*** Delete File: a.rs";
        assert_eq!(patch_targets(body), vec!["a.rs"]);
    }

    #[test]
    fn 改行コードや空白入りのパスでも外れない() {
        // CRLF (Windows のチェックアウト / ヒアドキュメント) でも外れない
        assert_eq!(
            patch_targets("*** Begin Patch\r\n*** Update File: src/a b.rs\r\n"),
            vec!["src/a b.rs"],
            "CRLF とスペース入りのパスで外れる"
        );
        // 印の前後の余分な空白は落とす
        assert_eq!(
            patch_targets("   *** Add File:    docs/x.md   "),
            vec!["docs/x.md"]
        );
    }

    #[test]
    fn パッチでない文字列はパッチと見なさない() {
        // `opaque` を立てる判定が、ただのシェル行で立たないこと
        for body in ["cargo test", "", "printf 'hello' > a.txt", "@@ -1 +1 @@"] {
            assert!(!looks_like_patch(body), "パッチ扱いされた: {body:?}");
            assert!(patch_targets(body).is_empty());
        }
        // 宛先の取れないパッチは「パッチらしい」= 監査に残す価値がある
        assert!(looks_like_patch("*** Begin Patch\n*** End Patch"));
        assert!(patch_targets("*** Begin Patch\n*** End Patch").is_empty());
    }
}
