//! # Context Engine — AI へ渡す前に情報量を最適化する層
//!
//! ```text
//! Task / State → [Context Engine] → Agent Provider → Execution Target
//! ```
//!
//! ## 何のためにあるか
//!
//! エージェントに渡る文脈は、ほとんどが**読まれない部分**でできている。
//! 2000 行のファイルを丸ごと渡しても、必要なのは 1 つの関数だったりする。
//! ここはその差を、渡す**前に**削る層。
//!
//! ## Provider に依存しないこと (この層の中心的な約束)
//!
//! `ClaudeContextOptimizer` / `GeminiContextOptimizer` のような形にすると、
//! エージェントが 1 つ増えるたびにこの層が増える。それは基盤ではない。
//! ここは **入力の内容だけ**を見て畳み方を決め、どのエージェント向けかは
//! [`ContextOrigin`] という**分類のラベル**としてしか受け取らない。
//!
//! 言葉ではなく番人で守っている:
//!
//! * [`tests::コアはエージェント名を知らない`] — `src/context/` の
//!   ソースを走査して、エージェントの実行ファイル名が出てこないことを確かめる
//! * [`engine::tests::出自は挙動を変えない`] — 同じ入力を 6 通りの出自で
//!   流して、結果が 1 バイトも変わらないことを確かめる
//!
//! ## MCP と圧縮ロジックを分けてあること
//!
//! 元にした `token-slim-mcp` は
//! `LLM → MCP → token-slim` という積み方だった。ここでは
//! `Zaivern → Context Engine → 圧縮` に組み替えてあり、
//! **JSON-RPC / stdio / `tools/call` の包みはこの層に 1 バイトも無い**。
//! 将来 MCP アダプタを足すなら、この層の外に置く:
//!
//! ```text
//! Context Engine (ここ)
//!  ├── Zaivern 内部 API   … engine::ContextEngine
//!  ├── CLI アダプタ       … cli.rs (`zai context …`)
//!  ├── UI アダプタ        … panel.rs (パレット → 窓)
//!  └── MCP アダプタ       … 未実装 (この層を触らずに足せる)
//! ```
//!
//! ## crate から切り離すときに残る継ぎ目 (正直に)
//!
//! いま `crate::` を参照しているのは **3 か所だけ**で、いずれも小さな
//! 純関数へのもの。core crate として切り出すなら、ここを付け替える:
//!
//! | 参照先 | 使う場所 | 代わりに要るもの |
//! |---|---|---|
//! | [`crate::pathx`] | `walk.rs` | `..` の畳み込みと canonicalize |
//! | [`crate::worktree::fs_case_insensitive_at`] | `walk.rs` | 大小非区別の実 FS 検査 |
//! | [`crate::jsonc::strip_jsonc`] | `tools/json.rs` | JSONC の正規化 |
//!
//! `cli.rs` / `panel.rs` / この `mod.rs` の [`FEATURE`] は**アダプタ**なので
//! 切り出しの対象外。
//!
//! ## 勝手に何もしない (安全側の既定)
//!
//! この層は**呼ばれたときにだけ**動く。エージェントへ文字を打ち込む・
//! Enter を送る・プロンプトを書き換える・利用者のファイルを書き換える —
//! どれも**しない**。外部通信もしない。読むのはワークスペースの中だけで、
//! そこは [`walk::Workspace`] が型で強制する
//! ([`walk::tests::ワークスペースの外は読めない`])。

pub mod cli;
pub mod engine;
pub mod glob;
pub mod metrics;
pub mod optimizer;
pub mod panel;
pub mod tools;
pub mod walk;

pub use engine::{
    ContextEngine, ContextError, ContextLimits, ContextRequest, ContextSource, ContextStrategy,
    OptimizedContext,
};
// **平らに出すのは、他のモジュールが実際に名前で呼ぶものだけ。**
// 出さないもの (`ContextOperation` / `Ledger` / `DayTotals`) も
// `crate::context::metrics::…` で届く — 使われていない再エクスポートを
// 並べると「繋がっている」という嘘になる。
pub use metrics::{ContextMetrics, ContextOrigin};
pub use walk::Workspace;

pub use cli::{cli_main, HELP};

use crate::feature::{Entry, Feature, Setting, SettingValue};

/// 有効かどうか。**この機能を明示的に呼んだときにしか効かない**ので、
/// 既定を `true` にしても既存の挙動は 1 つも変わらない。
pub const KEY_ENABLED: &str = "context.enabled";
/// 既定の戦略 (`auto` / `slim` / `outline` / `raw`)。
pub const KEY_MODE: &str = "context.mode";
/// 出力のトークン上限。
pub const KEY_MAX_TOKENS: &str = "context.max_tokens";
/// 検索・参照で一覧に出す件数の上限。
pub const KEY_MAX_RESULTS: &str = "context.max_results";
/// 台帳を `~/.zaivern/context/metrics.json` へ残すか。
///
/// **既定は `false`。** プロセス内の集計は常に取っているので、UI の
/// 「今日の削減」は残さなくても出る。ディスクへ書き始めるのは
/// 利用者が選んだときだけにする。
pub const KEY_PERSIST: &str = "context.persist_metrics";

/// [`KEY_ENABLED`] をその場で反転させる [`Entry::id`]。
///
/// **別の状態を作らない。** 指しているのは設定画面 (⚙) の
/// 「コンテキスト最適化を使う」と**同じ 1 つの値**で、パレットと
/// ペットメニューはそこへの近道でしかない (`notifications.toggle_sound`
/// と同じ形)。削るべきなのは「別々の状態を持つ重複」であって、
/// 同じ設定への近道ではない。
pub const ID_TOGGLE_ENABLED: &str = "context.toggle_enabled";

/// パレットから開く窓・オンオフの近道と、この機能が宣言する設定。
///
/// **実体は `panel.rs` / `cli.rs` にある。** ここは登録だけ。
/// 到達経路は 3 つ — 設定画面 (⚙) の行、パレット、ペットメニュー (🐾) の
/// 「🧠 トークンをスリム化」。**どれも [`KEY_ENABLED`] という 1 つの値を指す**
/// ので、増やしても状態が食い違わない。
pub const FEATURE: Feature = Feature {
    module: "context",
    entries: &[
        Entry {
            icon: "🧠",
            label: "コンテキストエンジン",
            id: "context.panel",
        },
        Entry {
            icon: "🧠",
            label: "トークンをスリム化 (オン/オフ)",
            id: ID_TOGGLE_ENABLED,
        },
    ],
    dispatch: |app, ctx, id| match id {
        "context.panel" => {
            panel::open(ctx.clone());
            true
        }
        ID_TOGGLE_ENABLED => {
            // いまの値は**設定から読む**。派生値 (窓が持つ写し) を真実源に
            // 使うと向きが循環するので、書き戻す側は `Config` を見る。
            let now = app.context_slim_enabled();
            app.set_context_slim(!now, ctx);
            // 窓が開いたままなら、次の描画で環境を読み直させる
            // (`Env` は開いた 1 回だけ読む写しなので、放っておくと
            //  「使う: はい」と嘘の要約を出し続ける)。
            panel::forget_env();
            true
        }
        _ => false,
    },
    draw: Some(panel::draw),
    settings: &[
        Setting {
            key: KEY_ENABLED,
            label: "コンテキスト最適化を使う",
            help: "呼ばれたときにだけ働きます。エージェントへ勝手に文字を送ることも、\
                   プロンプトを書き換えることもありません。",
            default: SettingValue::Bool(true),
        },
        Setting {
            key: KEY_MODE,
            label: "既定の畳み方",
            help: "auto = 大きくて構造のあるファイルは構造だけ、それ以外はコメントと空行を落とす / \
                   slim = コメントと空行を落とす / outline = 構造だけ / raw = そのまま。",
            default: SettingValue::Text("auto"),
        },
        Setting {
            key: KEY_MAX_TOKENS,
            label: "1 回の出力のトークン上限",
            help: "超えた分は先頭と末尾を残して中央を落とし、落としたことを本文に書きます。0 で上限なし。",
            default: SettingValue::Int(4000),
        },
        Setting {
            key: KEY_MAX_RESULTS,
            label: "検索と参照で一覧に出す件数の上限",
            help: "打ち切ったときは必ずその旨を出します。",
            default: SettingValue::Int(50),
        },
        Setting {
            key: KEY_PERSIST,
            label: "削減量を ~/.zaivern へ残す",
            help: "残さなくてもアプリを起動しているあいだの合計は出ます。\
                   残すのは日ごとの合計だけで、読んだ内容もパスも保存しません。",
            default: SettingValue::Bool(false),
        },
    ],
    ..Feature::DEFAULT
};

/// 設定から上限を組む。**設定を読むのはここ 1 か所**で、
/// [`engine`] 側は渡された値に従うだけ。
pub fn limits_from_config(cfg: &crate::config::Config) -> ContextLimits {
    let mut l = ContextLimits::default();
    let max_tokens = cfg.feature_i64(KEY_MAX_TOKENS);
    if max_tokens >= 0 {
        l.max_tokens = max_tokens as usize;
    }
    let max_results = cfg.feature_i64(KEY_MAX_RESULTS);
    if max_results > 0 {
        l.max_results = max_results as usize;
    }
    l
}

/// 設定の既定の戦略。**綴りを間違えていたら `Auto` へ落とす**が、
/// 黙って落とさずに済むよう [`ContextStrategy::parse`] は `None` を返す
/// (呼び出し側が「知らない値だった」と言えるようにするため)。
pub fn strategy_from_config(cfg: &crate::config::Config) -> ContextStrategy {
    ContextStrategy::parse(cfg.feature_str(KEY_MODE).trim()).unwrap_or_default()
}

/// 設定で有効になっているか。
pub fn enabled(cfg: &crate::config::Config) -> bool {
    cfg.feature_bool(KEY_ENABLED)
}

/// 台帳をファイルへ残す指定なら、その置き場。
pub fn metrics_store(cfg: &crate::config::Config) -> Option<std::path::PathBuf> {
    cfg.feature_bool(KEY_PERSIST)
        .then(|| metrics::store_path(&crate::config::zaivern_dir()))
}

/// 設定を反映したエンジンを組む。**CLI もパネルもここを通る**
/// (2 か所で組むと、片方だけ設定を無視する形でずれる)。
pub fn engine_for(
    roots: &[std::path::PathBuf],
    cfg: &crate::config::Config,
) -> Result<ContextEngine, ContextError> {
    Ok(ContextEngine::new(Workspace::new(roots)?)
        .with_limits(limits_from_config(cfg))
        .with_metrics_store(metrics_store(cfg)))
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! テスト用の実験場。**実 `~/.zaivern` にも実リポジトリにも触らない。**

    use std::path::{Path, PathBuf};

    use super::engine::{ContextEngine, ContextError};
    use super::optimizer::JsonLimits;
    use super::tools::{self, Rendered, ToolContext};
    use super::walk::Workspace;
    use super::ContextStrategy;

    pub struct Lab {
        root: PathBuf,
        ws: Workspace,
    }

    impl Lab {
        pub fn new(tag: &str) -> Self {
            let root =
                crate::pathx::canonical(&crate::test_util::unique_temp_dir("zaivern-ctx", tag));
            let ws = Workspace::new(std::slice::from_ref(&root)).expect("実験場を根にできる");
            Self { root, ws }
        }

        pub fn root(&self) -> &Path {
            &self.root
        }

        pub fn write(&self, rel: &str, body: &str) {
            let p = self.root.join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).expect("実験場のディレクトリ");
            }
            std::fs::write(p, body).expect("実験場へ書ける");
        }

        pub fn engine(&self) -> ContextEngine {
            ContextEngine::new(self.ws.clone())
        }

        fn cx(&self) -> ToolContext<'_> {
            ToolContext {
                workspace: &self.ws,
                limits: LIMITS.get_or_init(super::ContextLimits::default),
            }
        }

        pub fn read(
            &self,
            rel: &str,
            params: tools::read::ReadParams,
            s: ContextStrategy,
        ) -> Rendered {
            self.read_result(rel, params, s).expect("読める")
        }

        pub fn read_result(
            &self,
            rel: &str,
            params: tools::read::ReadParams,
            s: ContextStrategy,
        ) -> Result<Rendered, ContextError> {
            tools::read::run(&self.cx(), Path::new(rel), params, s)
        }

        pub fn grep(&self, p: &tools::grep::SearchParams) -> Rendered {
            self.grep_result(p).expect("検索できる")
        }

        pub fn grep_result(&self, p: &tools::grep::SearchParams) -> Result<Rendered, ContextError> {
            tools::grep::run(&self.cx(), Path::new("."), p)
        }

        pub fn refs(&self, p: &tools::refs::RefsParams) -> Rendered {
            self.refs_result(p).expect("辿れる")
        }

        pub fn refs_result(&self, p: &tools::refs::RefsParams) -> Result<Rendered, ContextError> {
            tools::refs::run(&self.cx(), Path::new("."), p)
        }

        pub fn map(&self, p: tools::directory::MapParams) -> Rendered {
            self.map_result(Path::new("."), p).expect("地図が作れる")
        }

        pub fn map_result(
            &self,
            path: &Path,
            p: tools::directory::MapParams,
        ) -> Result<Rendered, ContextError> {
            tools::directory::run(&self.cx(), path, p)
        }

        pub fn json_text(&self, text: &str, lim: Option<JsonLimits>) -> Rendered {
            self.json_text_result(text, lim).expect("JSON が読める")
        }

        pub fn json_text_result(
            &self,
            text: &str,
            lim: Option<JsonLimits>,
        ) -> Result<Rendered, ContextError> {
            tools::json::run(&self.cx(), tools::json::JsonInput::Text(text), lim)
        }

        pub fn json_file(&self, rel: &str, lim: Option<JsonLimits>) -> Rendered {
            self.json_file_result(rel, lim).expect("JSON が読める")
        }

        pub fn json_file_result(
            &self,
            rel: &str,
            lim: Option<JsonLimits>,
        ) -> Result<Rendered, ContextError> {
            tools::json::run(
                &self.cx(),
                tools::json::JsonInput::File(Path::new(rel)),
                lim,
            )
        }

        pub fn text(&self, text: &str, level: tools::text::TextLevel) -> Rendered {
            tools::text::run(&self.cx(), tools::text::TextInput::Text(text), level).expect("畳める")
        }

        pub fn text_file(&self, rel: &str, level: tools::text::TextLevel) -> Rendered {
            self.text_file_result(rel, level).expect("畳める")
        }

        pub fn text_file_result(
            &self,
            rel: &str,
            level: tools::text::TextLevel,
        ) -> Result<Rendered, ContextError> {
            tools::text::run(
                &self.cx(),
                tools::text::TextInput::File(Path::new(rel)),
                level,
            )
        }

        pub fn count(&self, text: &str) -> Rendered {
            tools::text::count(&self.cx(), tools::text::TextInput::Text(text)).expect("数えられる")
        }
    }

    static LIMITS: std::sync::OnceLock<super::ContextLimits> = std::sync::OnceLock::new();

    impl Drop for Lab {
        fn drop(&mut self) {
            // 自分が作ったものだけを消す (パターン検索で見つけたものは消さない)
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 走査の対象にする部分だけを切り出す。
    ///
    /// **見るのは「製品として動くコード」だけ**にする。2 つ落とす:
    ///
    /// * コメント — 「Claude / Codex から使える」という**説明**は分岐ではない
    /// * `#[cfg(test)]` 以降 — 出自で結果が変わらないことを確かめるテストは、
    ///   まさにエージェント名を並べる必要がある
    ///
    /// コメント落としに [`optimizer::strip_comments`] を**使わない**のは、
    /// あちらが「LLM へ渡す文脈」用の粗い実装で、`&'static str` の
    /// ライフタイム記号を**文字列の開き引用符と読む**ため。そこから先の
    /// コメントが 1 つも落ちない (`metrics.rs` で実際に踏んだ)。
    /// 番人が見たいのは行単位の粗い形で足りるので、ここは自前で持つ。
    fn product_code(src: &str) -> String {
        // Windows のチェックアウトは CRLF なので必ず正規化する
        let src = src.replace("\r\n", "\n");
        let body = match src.find("#[cfg(test)]") {
            Some(at) => &src[..at],
            None => &src[..],
        };
        let mut out = Vec::new();
        let mut in_block = false;
        for line in body.lines() {
            let t = line.trim_start();
            if in_block {
                if let Some(rest) = t.split_once("*/") {
                    in_block = false;
                    out.push(rest.1.to_string());
                }
                continue;
            }
            if t.starts_with("//") {
                continue;
            }
            if let Some((before, after)) = t.split_once("/*") {
                match after.split_once("*/") {
                    Some((_, tail)) => out.push(format!("{before}{tail}")),
                    None => {
                        in_block = true;
                        out.push(before.to_string());
                    }
                }
                continue;
            }
            // 行末コメントも落とす (文字列の中の `//` まで落とすが、
            // 番人の粗さとしてはそれで構わない — 見逃す側ではなく
            // 咎めすぎる側へ倒れるので、嘘の緑にはならない)
            out.push(match t.split_once("//") {
                Some((code, _)) => code.to_string(),
                None => t.to_string(),
            });
        }
        out.join("\n")
    }

    /// `src` が `name` を**識別子として**含むか (`omp` が `compress` に
    /// 当たらないよう、前後が語の文字でないことを見る)。
    fn mentions_identifier(src: &str, name: &str) -> bool {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let bytes = src.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(name) {
            let at = from + rel;
            let before_ok =
                at == 0 || !(bytes[at - 1] as char).is_ascii() || !is_word(bytes[at - 1] as char);
            let after = at + name.len();
            let after_ok = after >= bytes.len()
                || !(bytes[after] as char).is_ascii()
                || !is_word(bytes[after] as char);
            if before_ok && after_ok {
                return true;
            }
            from = at + name.len();
        }
        false
    }

    /// **コアがエージェントを知らないことを、ソースの走査で固定する。**
    ///
    /// `if agent.bin == "claude"` の類が 1 つでも入ると、この層は
    /// 「共通基盤」ではなくエージェントごとの分岐の置き場になる。
    /// 規約はテストで強制しないと必ず腐るので、番人にする。
    ///
    /// 対象は**コアだけ** — アダプタ (`cli.rs` / `panel.rs` / この `mod.rs`)
    /// はこの層の外側なので除く。
    #[test]
    fn コアはエージェント名を知らない() {
        // エージェントの実行ファイル名はカタログが真実の在り処。
        // 写経すると増えたときにずれるので、カタログから起こす。
        // **3 文字以下の名前は見ない。** `pi` / `cn` / `amp` / `omp` は実在の
        // エージェント名だが、同時に**どのコードにも出てくる普通の識別子**でも
        // ある (`glob.rs` のパターン添字が実際に `pi` だった)。短い名前まで
        // 咎めると、番人を黙らせるために変数名を歪めることになる。
        // 見逃す代わりに、**見ている名前の数を数で固定する**。
        const MIN_DISTINCTIVE: usize = 4;
        let bins: Vec<&'static str> = crate::agents::AGENT_CATALOG
            .iter()
            .map(|a| a.bin)
            .filter(|b| b.len() >= MIN_DISTINCTIVE)
            .collect();
        assert!(bins.len() >= 20, "見ている名前が {} 件しかない", bins.len());
        assert!(
            bins.contains(&"claude") && bins.contains(&"codex") && bins.contains(&"cursor-agent")
        );

        // **番人が空回りしていないことを、先に証明する。**
        // わざと壊した入力で赤にならないなら、以下の走査は何も守っていない。
        let broken = "fn pick(bin: &str) -> u8 { if bin == \"claude\" { 1 } else { 0 } }";
        assert!(
            bins.iter()
                .any(|b| mentions_identifier(&product_code(broken), b)),
            "番人が空回りしている (分岐を仕込んでも捕まらない)"
        );
        // 逆に、コメントと `#[cfg(test)]` 以降は落ちる
        let ok =
            "fn f() {}\n// claude からも使える\n#[cfg(test)]\nmod t { const A: &str = \"codex\"; }";
        assert!(
            !bins
                .iter()
                .any(|b| mentions_identifier(&product_code(ok), b)),
            "説明とテストまで咎めている"
        );

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/context");
        // アダプタとこの番人自身は対象外
        const ADAPTERS: &[&str] = &["cli.rs", "panel.rs", "mod.rs"];
        let mut checked = 0usize;
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)
                .expect("src/context を読める")
                .flatten()
            {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if ADAPTERS.contains(&name) {
                    continue;
                }
                let src = product_code(&std::fs::read_to_string(&p).expect("読める"));
                checked += 1;
                for bin in &bins {
                    assert!(
                        !mentions_identifier(&src, bin),
                        "{} に {bin:?} が出てくる。\n\
                         Context Engine は Provider に依存しない層で、\n\
                         エージェントごとの分岐はここに置けない\n\
                         (出自が要るなら ContextOrigin をラベルとして受けること)",
                        p.display()
                    );
                }
            }
        }
        // 走査が空振りしていないこと (パスを間違えて 0 件でも緑、が最悪)
        assert!(checked >= 8, "コアのファイルが {checked} 件しか無い");
    }

    /// **代表入力での削減率が床を下回らない。**
    ///
    /// 元にした `token-slim-mcp` と同じ入力・同じ既定で比べた実測
    /// (`tools/context-bench.sh`、2026-08-26):
    ///
    /// | 入力 | token-slim | Zaivern |
    /// |---|---|---|
    /// | 400 関数の Rust (auto) | -91% | -91% |
    /// | 同 (slim) | -92% | -92% |
    /// | 2000 件の JSON | -99% | -99% |
    /// | 4000 行のログ (aggressive) | -99% | -99% |
    ///
    /// 出力の中身も**打ち切りの印の文言以外はバイト単位で同じ**だった。
    /// その比較は外部リポジトリが要るので CI では回せない — だから
    /// **床だけをここで固定する**。落ちたら、削減が退化している。
    #[test]
    fn 削減率は代表入力で床を下回らない() {
        use engine::{ContextRequest, ContextSource};
        use tools::text::TextLevel;

        let lab = tests_support::Lab::new("bench-floor");
        let mut code = String::new();
        for i in 0..400 {
            code.push_str(&format!(
                "/// 関数 {i} の説明。outline では落ちる行。\n\
                 pub fn f{i}(a: u32, b: u32) -> u32 {{\n\
                 \x20   // 途中の説明\n\
                 \x20   let scaled = a.wrapping_mul({i}).wrapping_add(b);\n\
                 \x20   let clamped = scaled.clamp(0, u32::MAX / 2);\n\
                 \x20   let mut acc = 0u32;\n\
                 \x20   for step in 0..clamped.min(8) {{\n\
                 \x20       acc = acc.wrapping_add(step).wrapping_mul(3);\n\
                 \x20   }}\n\
                 \x20   acc.wrapping_add(clamped)\n\
                 }}\n\n"
            ));
        }
        lab.write("big.rs", &code);

        let pad = "x".repeat(300);
        let items: Vec<serde_json::Value> = (0..2000)
            .map(|i| serde_json::json!({"id": i, "name": format!("item-{i}"), "note": pad}))
            .collect();
        lab.write(
            "big.json",
            &serde_json::json!({ "items": items }).to_string(),
        );

        let mut log = String::new();
        for i in 0..4000 {
            log.push_str("   INFO   waiting for the lock   \n");
            if i % 500 == 0 {
                log.push_str(&format!("STEP {i}\n"));
            }
        }
        lab.write("big.log", &log);

        let engine = lab.engine();
        let cases: Vec<(&str, ContextSource, f32)> = vec![
            (
                "read(auto)",
                ContextSource::File {
                    path: "big.rs".into(),
                    params: Default::default(),
                },
                85.0,
            ),
            (
                "json",
                ContextSource::JsonFile {
                    path: "big.json".into(),
                    limits: None,
                },
                95.0,
            ),
            (
                "text(aggressive)",
                ContextSource::TextFile {
                    path: "big.log".into(),
                    level: TextLevel::Aggressive,
                },
                95.0,
            ),
        ];
        for (name, source, floor) in cases {
            let out = engine.run(&ContextRequest::new(source)).expect(name);
            let got = out.metrics.reduction_percent();
            assert!(
                got >= floor,
                "{name}: 削減率が {got:.1}% ({floor}% を下回った / {} → {})",
                out.metrics.original_tokens,
                out.metrics.optimized_tokens
            );
        }

        // slim も outline も、素直に読むより必ず小さい
        for s in [ContextStrategy::Slim, ContextStrategy::Outline] {
            let out = engine
                .run(
                    &ContextRequest::new(ContextSource::File {
                        path: "big.rs".into(),
                        params: Default::default(),
                    })
                    .with_strategy(s),
                )
                .expect("読める");
            assert!(
                out.metrics.reduction_percent() >= 85.0,
                "{}: {:.1}%",
                s.id(),
                out.metrics.reduction_percent()
            );
        }
    }

    /// 設定は宣言どおりに読める。**既定は「呼んだときだけ働く」ので
    /// 有効で始めてよい** (既存の挙動を 1 つも変えない)。
    #[test]
    fn 設定の既定と読み出し() {
        let cfg = crate::config::Config::default();
        assert!(enabled(&cfg), "既定で使えること");
        assert_eq!(strategy_from_config(&cfg), ContextStrategy::Auto);
        assert_eq!(limits_from_config(&cfg).max_tokens, 4000);
        assert_eq!(limits_from_config(&cfg).max_results, 50);
        assert!(metrics_store(&cfg).is_none(), "既定でディスクへ書いている");
    }

    /// 設定を変えると効く。**綴りを間違えた戦略は `Auto` へ落ちる**
    /// (そこで panic すると設定ファイルの 1 文字で起動しなくなる)。
    #[test]
    fn 設定を変えると効く() {
        let mut cfg = crate::config::Config::default();
        assert!(cfg.set_feature(
            KEY_MODE,
            crate::config::SettingValue::Text("outline".into())
        ));
        assert_eq!(strategy_from_config(&cfg), ContextStrategy::Outline);
        assert!(cfg.set_feature(
            KEY_MODE,
            crate::config::SettingValue::Text("でたらめ".into())
        ));
        assert_eq!(strategy_from_config(&cfg), ContextStrategy::Auto);

        assert!(cfg.set_feature(KEY_MAX_TOKENS, crate::config::SettingValue::Int(0)));
        assert_eq!(limits_from_config(&cfg).max_tokens, 0, "0 = 上限なし");
        assert!(cfg.set_feature(KEY_MAX_TOKENS, crate::config::SettingValue::Int(-5)));
        assert_eq!(limits_from_config(&cfg).max_tokens, 4000, "負値は既定へ");
        assert!(cfg.set_feature(KEY_MAX_RESULTS, crate::config::SettingValue::Int(7)));
        assert_eq!(limits_from_config(&cfg).max_results, 7);

        assert!(cfg.set_feature(KEY_ENABLED, crate::config::SettingValue::Bool(false)));
        assert!(!enabled(&cfg));
        // 型違いは受け付けない (受けると読み出しが既定へ落ちて「効かない」になる)
        assert!(!cfg.set_feature(KEY_ENABLED, crate::config::SettingValue::Int(1)));
    }

    /// 宣言した設定キーが全て `context.` 接頭辞つきで、レジストリから引けること。
    #[test]
    fn 設定キーは宣言と一致する() {
        for s in FEATURE.settings {
            assert!(s.key.starts_with("context."), "{:?} に接頭辞が無い", s.key);
            assert!(
                crate::config::feature_setting(s.key).is_some(),
                "{:?} がレジストリから引けない",
                s.key
            );
            assert!(!s.label.trim().is_empty());
            assert!(!s.help.trim().is_empty(), "{:?} の説明が空", s.key);
        }
        assert_eq!(FEATURE.settings.len(), 5);
    }

    /// 囲っている関数の中だけを切り出す (行単位)。
    ///
    /// **窓 (「前後 N 行」) では見ない。** 同じファイルの**別の関数**が書いた
    /// 文字列を拾って、わざと壊しても緑になる番人が実際に生まれた
    /// (`cli::tests::改行をまたぐ照合は…` の最初の 2 版)。
    fn enclosing_fn(src: &str, sig: &str) -> Option<String> {
        let lines: Vec<&str> = src.lines().collect();
        let start = lines.iter().position(|l| l.contains(sig))?;
        let is_fn_head = |l: &str| {
            let t = l.trim_start();
            t.starts_with("fn ")
                || t.starts_with("pub fn ")
                || t.starts_with("pub(super) fn ")
                || t.starts_with("pub(crate) fn ")
        };
        let end = lines[start + 1..]
            .iter()
            .position(|l| is_fn_head(l))
            .map(|i| start + 1 + i)
            .unwrap_or(lines.len());
        Some(lines[start..end].join("\n"))
    }

    /// ペットメニューの「トークンをスリム化」が、設定画面 (⚙) と
    /// **同じ 1 つの値**を指していること。
    ///
    /// 到達経路を増やすこと自体は良い (同じ真実源への近道)。禁じたいのは
    /// **別の状態を作ること** — ペット側に専用の bool を持たせると、設定画面と
    /// 食い違って「オンにしたのにコンテキストが畳まれない」が出る。しかも
    /// 食い違いは画面のどちらか片方を見ているあいだは観測できない。
    #[test]
    fn ペットメニューの切り替えは設定と同じ値を指す() {
        // Windows のチェックアウトは CRLF なので必ず正規化してから照合する
        let src = include_str!("../app/top_bar_ui.rs").replace("\r\n", "\n");
        let body = enclosing_fn(&src, "fn top_bar_pet_menu").expect("ペットメニューの関数がある");

        /// 真実源が 1 つか — 読み出しは `Config` 由来の glue、書き込みは
        /// レジストリの ID 経由。どちらか一方でも欠けたら偽。
        fn single_source(body: &str) -> bool {
            body.contains("self.context_slim_enabled()") && body.contains("ID_TOGGLE_ENABLED")
        }

        assert!(
            single_source(&body),
            "ペットメニューは `context_slim_enabled()` で読み、\
             `ID_TOGGLE_ENABLED` を投げること (設定画面と同じ 1 つの値を指すため)"
        );
        // **空回りする番人を残さない。** 専用の状態を持たせた版を作って、
        // この検査が実際に赤へ倒れることを同じテストの中で確かめる。
        let broken = body.replace("self.context_slim_enabled()", "self.cfg.pet_token_slim");
        assert_ne!(
            broken, body,
            "仕込みが当たっていない (照合する文字列がずれた)"
        );
        assert!(
            !single_source(&broken),
            "専用の状態を持たせても緑になる = 番人が空回りしている"
        );
    }
}
