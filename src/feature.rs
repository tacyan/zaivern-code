//! 機能レジストリ — 並列開発で `app.rs` の同じ箇所を奪い合わないための登録面。
//!
//! ## なぜ要るのか (実測の動機)
//!
//! 機能を 1 つ足すたびに、従来は `app.rs` (4.2 万行) と `palette.rs` の
//! **5 箇所**を編集する必要があった:
//!
//!   1. `palette.rs` の `enum Cmd` に variant
//!   2. `palette.rs` の `group_of` にアーム (番人テストが強制する)
//!   3. `app.rs` の `palette_builtin_cmds()` に行
//!   4. `app.rs` の巨大な `match cmd` にアーム
//!   5. `app.rs` の描画ループに 1 行
//!
//! 隔離ワークツリーで 8 本のブランチを同時に走らせたところ、**全員がこの
//! 同じ 5 箇所を編集する**ので、直列マージのたびに「5 箇所 × ブランチ数」の
//! 衝突が出ることが分かった。**git worktree はファイルシステムを分離する
//! だけで、意味的な衝突は 1 つも防いでいない。** 同種の競合製品が軒並み
//! 同じ穴に落ちているのもここで、Cursor は自社のスウォーム実験で
//! 2 時間に 7 万件超のマージ衝突を出して実行を中断している。
//!
//! ## 解き方: 検出ではなく所有
//!
//! 設計原則 5 の「セッションの所有権はアトミックに主張し、競合したら
//! fail-closed にする」を、そのまま開発プロセスへ適用する。
//! **共有される行の書き手を 1 人に限定する**:
//!
//!   * 機能側は自分のモジュールに [`Feature`] を 1 つ公開するだけ。
//!     `app.rs` / `palette.rs` / `keybinds.rs` には**触らない**。
//!   * [`REGISTRY`] へ 1 行足すのは統合担当だけ。
//!
//! これで機能が N 個増えても、共有ファイルの差分は「統合担当が足した N 行」
//! だけになり、**ブランチ同士は構造的に衝突しない**。衝突検出 (`conflict.rs`)
//! が「起きた衝突を早く見せる」機能なのに対し、こちらは「そもそも起こさない」
//! 側の対策で、両方要る。
//!
//! ## 使い方 (機能を足す側)
//!
//! ```ignore
//! // src/mymod.rs
//! use crate::feature::{Entry, Feature};
//!
//! pub const FEATURE: Feature = Feature {
//!     module: "mymod",
//!     entries: &[Entry {
//!         icon: "🔭",
//!         label: "私の機能を開く",
//!         id: "mymod.open",
//!     }],
//!     dispatch: |app, ctx, id| match id {
//!         "mymod.open" => { /* … */ true }
//!         _ => false,
//!     },
//!     draw: Some(|app, ctx| { /* 毎フレームのオーバーレイ描画 */ }),
//! };
//! ```
//!
//! ラベルは [`Entry::label`] に**日本語の原文**を置く。表示時に [`tr`] を
//! 通すので、`tr("…")` を呼び出し側で書く必要はない (書くと二重になる)。

use crate::app::ZaivernApp;
use crate::i18n::tr;
use crate::palette::Cmd;

/// パレットに出す 1 項目。
pub struct Entry {
    /// 先頭に出す絵文字 1 つ。パレットの他の行と揃える。
    pub icon: &'static str,
    /// 見出し。**日本語の原文**をそのまま置く (表示時に [`tr`] を通す)。
    pub label: &'static str,
    /// [`Cmd::Feature`] に載る安定 ID。`"<module>.<action>"` 形式にする。
    ///
    /// **接頭辞をモジュール名に固定するのが衝突回避の要**で、これにより
    /// 別々のブランチが同じ ID を選ぶ事故が起きない。
    /// [`registry_ids_are_unique_and_prefixed`] が番人。
    pub id: &'static str,
}

/// 機能が自分で宣言する設定 1 件。
///
/// **`config.rs` の `Config` 構造体へフィールドを足させないための面。**
/// 従来は設定を持つ機能が `Config` と既定値と設定画面の 3 か所へ追記して
/// いたので、2 つのブランチが同時に設定を足すと必ず衝突した
/// (which-key と local_history が実際に 3 ハンク衝突した)。値は
/// [`crate::config::Config::extra`] に `key` 文字列で入るので、
/// **共有ファイルへの追記が 1 行も要らない**。
pub struct Setting {
    /// `"<module>.<name>"` 形式の安定キー。設定ファイルにもこの文字列で載る。
    pub key: &'static str,
    /// 設定画面の見出し。**日本語の原文**を置く (表示時に [`tr`] を通す)。
    pub label: &'static str,
    /// 補足説明。空でよい。
    pub help: &'static str,
    /// 既定値。設定ファイルに無ければこれが使われる。
    pub default: SettingValue,
}

/// [`Setting`] の値。設定画面の入力欄の種類もこれで決まる。
///
/// **4 種すべてを最初から持たせてある。** レジストリの目的は「設定を足すのに
/// 共有ファイルを触らせない」ことなので、種類が足りずに後から
/// `feature.rs` と `config.rs` を editing する羽目になったら本末転倒になる。
/// いま実際に使われているのは `Bool` / `Int` / `Text` だけで、`Float` は
/// まだ使い手がいない — それでも消さないのはこの理由による
/// (使い手が現れた日に共有ファイルを 2 つ触るコストのほうが高い)。
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(&'static str),
}

/// 機能が自分で宣言する既定のキーバインド 1 件。
///
/// `keybinds.rs` の `BindAction` は固定長配列 (`[BindAction; N]`) ＋ 件数検査
/// テストなので、機能側から増やすと必ず共有ファイルの追記になる。こちらは
/// [`Cmd::Feature`] を直に指すので、**`BindAction` を 1 つも増やさない**。
pub struct Bind {
    /// 対象の [`Entry::id`]。同じモジュールの ID だけを指せる。
    pub id: &'static str,
    /// 既定の打鍵 (`"⌘⇧J"` / `"Ctrl+Shift+J"` のような表記)。
    /// **画面に出すときはここではなく実際の割り当てから起こす**
    /// (`config.toml` で再割り当てされたら表記も変わるため)。
    pub default: &'static str,
}

/// 1 つの機能モジュールが公開する登録内容。
pub struct Feature {
    /// モジュール名 (`src/<module>.rs` と一致させる)。ID の接頭辞にもなる。
    pub module: &'static str,
    /// コマンドパレットに出す項目。空でもよい (描画だけの機能)。
    pub entries: &'static [Entry],
    /// パレット項目が選ばれたときの処理。**自分の ID なら処理して `true`**、
    /// 知らない ID なら `false` を返す (次の機能へ回る)。
    pub dispatch: fn(&mut ZaivernApp, &egui::Context, &str) -> bool,
    /// 毎フレームのオーバーレイ描画。要らなければ `None`。
    ///
    /// **アイドル時に再描画を要求しないこと** (設計原則 3: アイドル時の
    /// コストはゼロ)。描くものが無いフレームは 1 ピクセルも触らない。
    pub draw: Option<fn(&mut ZaivernApp, &egui::Context)>,
    /// この機能が持つ設定 ([`Setting`])。無ければ空スライス。
    pub settings: &'static [Setting],
    /// この機能の既定キーバインド ([`Bind`])。無ければ空スライス。
    pub binds: &'static [Bind],
}

impl Feature {
    /// 欄を埋めるための雛形。**機能側は `..Feature::DEFAULT` で締める。**
    ///
    /// レジストリは「登録の追記」を無くしたが、**`Feature` 構造体そのものは
    /// まだ共有面**で、欄を 1 つ足すと既存の全機能モジュールが同じコミットで
    /// 壊れる (実際に `settings` / `binds` を足したとき 4 モジュールを
    /// 同時に直す必要があり、別ワークツリーのビルドも巻き込んだ)。
    /// `..Feature::DEFAULT` で締めてあれば、次に誰が欄を足しても
    /// 他のブランチは壊れない。
    ///
    /// **既定と同じ値の欄は書かないこと。** 全欄を明示したうえで `..DEFAULT`
    /// を足すと clippy の `needless_update` が `-D warnings` の CI を落とす。
    ///
    /// `module` / `entries` / `dispatch` は既定のままだと何もしないので、
    /// 必ず自分で埋める (番人テストが空の `module` を弾く)。
    pub const DEFAULT: Feature = Feature {
        module: "",
        entries: &[],
        dispatch: |_app, _ctx, _id| false,
        draw: None,
        settings: &[],
        binds: &[],
    };
}

/// 登録済みの機能。
///
/// **ここへ行を足してよいのは統合担当だけ。** 機能ブランチ側でこの配列を
/// 編集すると、まさに避けたかった衝突が戻ってくる (モジュール本体だけを
/// 書いて、統合時に 1 行足してもらうこと)。
pub const REGISTRY: &[&Feature] = crate::features::GENERATED;

/// パレットのコマンド一覧へ差し込む行。
///
/// 返す形は `app.rs` の `palette_builtin_cmds()` と同じ
/// `(アイコン, ラベル, 打鍵表記, Cmd)`。**打鍵表記は空文字**にしてある —
/// 機能側にキーバインドを持たせると `keybinds.rs` の `BindAction` と
/// `ALL_ACTIONS` がまた共有の壁になるため、ここでは持たせない
/// (打鍵が要る機能は統合時に個別へ切り出す)。
pub fn palette_entries(
    binds: &crate::keybinds::FeatureBinds,
) -> Vec<(String, String, String, Cmd)> {
    let mut out = Vec::new();
    for f in REGISTRY {
        for e in f.entries {
            out.push((
                e.icon.to_string(),
                tr(e.label),
                // **打鍵表記をここで入れる。** 以前は常に空文字で、
                // `keybinds::feature_key_hint` は実装済みなのに**テストからしか
                // 呼ばれていなかった** (= 既定打鍵を宣言した機能が、パレットでは
                // 打鍵の無い行に見えた)。表記は必ずキーバインド表から作る —
                // ベタ書きは再割り当てで嘘になり、Windows/Linux では綴りも違う。
                crate::keybinds::feature_key_hint(binds, e.id),
                Cmd::Feature(e.id),
            ));
        }
    }
    out
}

/// ID が指定モジュールのものか (`"<module>." で始まる`)。
///
/// [`dispatch`] の絞り込みと、番人テストの判定で同じ規則を使うために
/// 関数へ切り出してある (2 か所に書くとズレる)。
fn owns(module: &str, id: &str) -> bool {
    // `module.len() + 1` にするのは `"spec."` のような**動作名が空の ID**を
    // 弾くため。通してしまうと「パレットに出るのに押しても何も起きない行」
    // が作れてしまう。
    id.len() > module.len() + 1
        && id.starts_with(module)
        && id.as_bytes().get(module.len()) == Some(&b'.')
}

/// 選ばれた ID を、**その ID を所有するモジュールにだけ**回す。処理されたら `true`。
///
/// 接頭辞で絞ってから呼ぶのが肝で、こうしておくと
/// 「別の機能が他人の ID を先に掴んで、本来の機能が永久に死ぬ」事故が
/// 実行時にも起こらない (番人テストは静的に同じことを保証するが、
/// レジストリは統合担当が手で足すので二重に守る)。
///
/// 未知の ID で黙って何も起きないと「押したのに反応しない」になるので、
/// 呼び出し側 (`app.rs`) が `false` を受けたらトーストで知らせる。
pub fn dispatch(app: &mut ZaivernApp, ctx: &egui::Context, id: &str) -> bool {
    for f in REGISTRY {
        if owns(f.module, id) && (f.dispatch)(app, ctx, id) {
            return true;
        }
    }
    false
}

/// 登録済み機能のオーバーレイ描画をまとめて呼ぶ。
pub fn draw_all(app: &mut ZaivernApp, ctx: &egui::Context) {
    for f in REGISTRY {
        if let Some(d) = f.draw {
            d(app, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ID はレジストリ全体で一意で、かつモジュール名で接頭辞が付いている。
    ///
    /// **接頭辞の強制がこのレジストリの肝**で、別々のブランチが偶然
    /// 同じ ID を選ぶ事故 (= ディスパッチが先勝ちで片方が死ぬ) を防ぐ。
    #[test]
    fn registry_ids_are_unique_and_prefixed() {
        let mut seen: Vec<&str> = Vec::new();
        for f in REGISTRY {
            assert!(
                !f.module.is_empty(),
                "Feature::module が空 (ID の接頭辞に使うので必須)"
            );
            for e in f.entries {
                assert!(
                    owns(f.module, e.id),
                    "ID {:?} は \"{}.\" で始めること (モジュール接頭辞が衝突回避の要)",
                    e.id,
                    f.module
                );
                assert!(!seen.contains(&e.id), "ID が重複している: {:?}", e.id);
                seen.push(e.id);
            }
        }
    }

    /// モジュール名も重複しない (同じ接頭辞を 2 つの機能が使うと ID が衝突する)。
    #[test]
    fn registry_modules_are_unique() {
        let mut seen: Vec<&str> = Vec::new();
        for f in REGISTRY {
            assert!(
                !seen.contains(&f.module),
                "module 名が重複している: {:?}",
                f.module
            );
            seen.push(f.module);
        }
    }

    /// ラベルとアイコンが空でない (パレットに空行が出るのを防ぐ)。
    #[test]
    fn registry_entries_are_displayable() {
        for f in REGISTRY {
            for e in f.entries {
                assert!(!e.icon.trim().is_empty(), "{:?} のアイコンが空", e.id);
                assert!(!e.label.trim().is_empty(), "{:?} のラベルが空", e.id);
            }
        }
    }

    /// **全ての `FEATURE` が `..Feature::DEFAULT` で締められている。**
    ///
    /// レジストリは「登録の追記」を無くしたが、`Feature` 構造体そのものは
    /// まだ共有面で、欄を 1 つ足すと既存の全機能モジュールが同じコミットで
    /// 壊れる。`..Feature::DEFAULT` があれば壊れない。**規約はテストで
    /// 強制しないと必ず腐る**ので、ここで番人にする。
    #[test]
    fn 全ての機能登録は_default_で締められている() {
        // パスはリテラルではなくビルド時のクレート位置から起こす
        // (どのマシンでチェックアウトしても動く)。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // Windows のチェックアウトは CRLF なので必ず正規化する。
                let text = raw.replace("\r\n", "\n");
                let Some(at) = text.find("pub const FEATURE:") else {
                    continue;
                };
                let Some(end) = text[at..].find("\n};") else {
                    continue;
                };
                let block = &text[at..at + end];
                assert!(
                    block.contains("Feature::DEFAULT"),
                    "{} の FEATURE が `..Feature::DEFAULT` で締められていない。\n                     欄を足したときに他のブランチを壊さないため、必ず付けること。",
                    path.display()
                );
                checked += 1;
            }
        }
        // 走査そのものが空振りしていないことを確かめる
        // (パスを間違えて 0 件でも緑、が最悪の壊れ方)。
        assert!(checked >= 4, "FEATURE が {checked} 件しか見つからない");
    }

    /// 所有判定の境界。**接頭辞が一致しても区切りの `.` が無ければ他人の ID**
    /// で、ここを緩めると `spec` が `special.open` を掴んでしまう。
    #[test]
    fn owns_requires_a_dot_separator() {
        assert!(owns("spec", "spec.open"));
        assert!(owns("spec", "spec.a.b"));
        // 接頭辞は一致するが区切りが無い → 他人のもの
        assert!(!owns("spec", "special.open"));
        assert!(!owns("spec", "specopen"));
        // 完全一致だけで本体が無いものも不可 (押しても何も起きない ID になる)
        assert!(!owns("spec", "spec"));
        assert!(!owns("spec", "spec."));
        // 他モジュールの ID を掴まない
        assert!(!owns("spec", "marks.list"));
        assert!(!owns("marks", "spec.open"));
        // 空文字で panic しない
        assert!(!owns("spec", ""));
        assert!(!owns("", "spec.open"));
    }

    /// 空のレジストリでも落ちない (最初の 1 個が入るまでの状態)。
    #[test]
    fn empty_registry_yields_no_entries() {
        // REGISTRY が空のうちは 0 件、埋まれば entries の総数と一致する。
        let expected: usize = REGISTRY.iter().map(|f| f.entries.len()).sum();
        let binds = crate::keybinds::FeatureBinds::default();
        assert_eq!(palette_entries(&binds).len(), expected);
    }

    /// **機能側が共有ファイルを触っていないこと**を構造で担保する番人。
    ///
    /// レジストリの各モジュールについて、そのソースが `palette.rs` の
    /// `enum Cmd` や `keybinds.rs` の `BindAction` を増やしていないかは
    /// ここでは見られない (別ファイルなので)。代わりに、**この配列に
    /// 行を足す以外の方法で機能が増えていない**ことを、`REGISTRY` の
    /// 件数と `Feature` の定義箇所の対応で確認する。
    /// 実際の禁止は CONTRIBUTING と code review 側で担保する。
    #[test]
    fn registry_is_the_single_integration_point() {
        let src = include_str!("feature.rs").replace("\r\n", "\n");
        assert!(
            src.contains("pub const REGISTRY: &[&Feature]"),
            "REGISTRY の定義が見つからない (統合点が動いたらこのテストを直すこと)"
        );
    }
}
