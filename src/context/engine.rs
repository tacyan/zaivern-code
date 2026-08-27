//! Context Engine — **AI へ渡す前の情報量を最適化する層**。
//!
//! ```text
//! ContextRequest → ContextEngine → ContextStrategy → OptimizedContext
//! ```
//!
//! ## Provider から独立していること
//!
//! ここには「どのエージェント向けか」という分岐が 1 つも無い。
//! `Claude` / `Codex` / `Gemini` は入力の**ラベル**
//! ([`ContextOrigin`]) としてしか登場せず、処理を変えない。
//! `ClaudeContextOptimizer` のような形にすると、エージェントが 1 つ増える
//! たびにこの層が増える — それは基盤ではなく、機能の寄せ集めになる。
//!
//! この性質は言葉ではなく**番人**で守っている:
//!
//! * [`crate::context::tests::コアはエージェント名を知らない`] —
//!   `src/context/` のソースを走査して、エージェントの実行ファイル名が
//!   出てこないことを確かめる
//! * [`tests::出自は挙動を変えない`] — 同じ入力を違う出自で流して、
//!   本文が 1 バイトも変わらないことを確かめる
//!
//! ## UI スレッドを止めないこと
//!
//! ファイル読み・grep・走査・JSON はどれも数秒返らないことがある
//! (このリポジトリでは同期 git が 6023ms かかった実測がある)。
//! 描画から呼ぶ経路は必ず [`ContextEngine::spawn`] を通す。

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use super::metrics::{
    self, estimate_tokens, reduction_percent, truncate_tokens, ContextMetrics, ContextOperation,
    ContextOrigin,
};
use super::optimizer::JsonLimits;
use super::tools::{self, ToolContext};
use super::walk::Workspace;

/// 情報量の畳み方。**Provider ではなく内容で決まる。**
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ContextStrategy {
    /// 内容に応じて [`ContextStrategy::Outline`] か [`ContextStrategy::Slim`]。
    #[default]
    Auto,
    /// コメントを外し、空行を畳む。
    Slim,
    /// 構造の行だけ。
    Outline,
    /// そのまま。
    Raw,
}

impl ContextStrategy {
    /// 設定ファイルと CLI に載る安定 ID。
    pub fn id(self) -> &'static str {
        match self {
            ContextStrategy::Auto => "auto",
            ContextStrategy::Slim => "slim",
            ContextStrategy::Outline => "outline",
            ContextStrategy::Raw => "raw",
        }
    }

    /// 安定 ID からの逆引き。知らない語は `None`
    /// (**黙って `Auto` へ落とさない** — 綴りを間違えた設定が
    ///  「効いている」ように見えるのがいちばん困る)。
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => ContextStrategy::Auto,
            "slim" => ContextStrategy::Slim,
            "outline" => ContextStrategy::Outline,
            "raw" => ContextStrategy::Raw,
            _ => return None,
        })
    }

    /// 設定画面と CLI が出す一覧。
    pub const ALL: [ContextStrategy; 4] = [
        ContextStrategy::Auto,
        ContextStrategy::Slim,
        ContextStrategy::Outline,
        ContextStrategy::Raw,
    ];
}

/// 何を文脈にするか。
pub enum ContextSource {
    /// 1 つのファイル。
    File {
        path: PathBuf,
        params: tools::read::ReadParams,
    },
    /// 木を検索する。
    Search {
        root: PathBuf,
        params: tools::grep::SearchParams,
    },
    /// 記号の参照を辿る。
    Symbol {
        root: PathBuf,
        params: tools::refs::RefsParams,
    },
    /// ディレクトリの地図。
    Directory {
        path: PathBuf,
        params: tools::directory::MapParams,
    },
    /// その場の JSON 文字列。
    Json {
        text: String,
        limits: Option<JsonLimits>,
    },
    /// ファイルの JSON。
    JsonFile {
        path: PathBuf,
        limits: Option<JsonLimits>,
    },
    /// その場のテキスト。
    Text {
        text: String,
        level: tools::text::TextLevel,
    },
    /// ファイルのテキスト。
    TextFile {
        path: PathBuf,
        level: tools::text::TextLevel,
    },
    /// トークン数を数えるだけ (その場の文字列)。
    Count(String),
    /// トークン数を数えるだけ (ファイル)。
    CountFile(PathBuf),
}

impl ContextSource {
    /// この入力がどの操作に当たるか (メトリクスの分類軸)。
    pub fn operation(&self) -> ContextOperation {
        match self {
            ContextSource::File { .. } => ContextOperation::Read,
            ContextSource::Search { .. } => ContextOperation::Search,
            ContextSource::Symbol { .. } => ContextOperation::Refs,
            ContextSource::Directory { .. } => ContextOperation::Directory,
            ContextSource::Json { .. } | ContextSource::JsonFile { .. } => ContextOperation::Json,
            ContextSource::Text { .. } | ContextSource::TextFile { .. } => ContextOperation::Text,
            ContextSource::Count(_) | ContextSource::CountFile(_) => ContextOperation::Count,
        }
    }
}

/// 上限の束。**環境変数も設定ファイルもここでは読まない** — 値を決めるのは
/// アダプタ (CLI / パネル) の仕事で、この層は渡されたものに従う。
#[derive(Clone, Copy, Debug)]
pub struct ContextLimits {
    /// 出力のトークン上限。0 なら上限なし。
    pub max_tokens: usize,
    /// 検索・参照で一覧に出す件数の上限。
    pub max_results: usize,
    /// 地図で降りる段数。
    pub dir_depth: usize,
    /// 地図で出す件数の上限。
    pub dir_max_entries: usize,
    /// JSON を刈る上限。
    pub json: JsonLimits,
    /// `Auto` が outline を選び始めるトークン数の下限。
    pub auto_outline_min_tokens: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            max_tokens: 4000,
            max_results: 50,
            dir_depth: 3,
            dir_max_entries: 300,
            json: JsonLimits::default(),
            auto_outline_min_tokens: 400,
        }
    }
}

/// 1 回ぶんの要求。
pub struct ContextRequest {
    pub source: ContextSource,
    pub strategy: ContextStrategy,
    /// この要求だけの上限 (指定が無ければエンジンの既定)。
    pub max_tokens: Option<usize>,
    /// 誰のための要求か。**分類のラベルであって分岐の材料ではない。**
    pub origin: ContextOrigin,
}

impl ContextRequest {
    /// 既定の戦略・既定の上限・出自不明の要求。
    pub fn new(source: ContextSource) -> Self {
        Self {
            source,
            strategy: ContextStrategy::default(),
            max_tokens: None,
            origin: ContextOrigin::unknown(),
        }
    }

    /// 戦略を指定する。
    pub fn with_strategy(mut self, s: ContextStrategy) -> Self {
        self.strategy = s;
        self
    }

    /// 出自を添える。
    pub fn with_origin(mut self, o: ContextOrigin) -> Self {
        self.origin = o;
        self
    }

    /// この要求だけのトークン上限。
    pub fn with_max_tokens(mut self, n: usize) -> Self {
        self.max_tokens = Some(n);
        self
    }
}

/// 最適化の結果。
#[derive(Debug)]
pub struct OptimizedContext {
    /// 渡す本文。**ヘッダは含まない** ([`OptimizedContext::render`] が付ける)。
    pub content: String,
    /// 1 行のヘッダ。何をどれだけ畳んだかを名乗る。
    pub summary: String,
    /// 実際に使われた戦略の名前 (`outline(auto)` のように、自動で降りた
    /// 先まで分かる形)。
    pub applied: String,
    /// 上限で途中を落としたか。
    pub truncated: bool,
    /// 測った結果。
    pub metrics: ContextMetrics,
}

impl OptimizedContext {
    /// 実際に文脈へ渡す形 (ヘッダ + 本文)。
    ///
    /// ヘッダを本文と分けてあるのは、**メトリクスが本文だけを測る**ため。
    /// ヘッダ自身のトークン数を削減率に混ぜると、測っている値が自分の
    /// 出力に依存して動く (小さな入力ほど「増えた」と出る)。
    pub fn render(&self) -> String {
        if self.content.is_empty() {
            return self.summary.clone();
        }
        format!("{}\n{}", self.summary, self.content)
    }
}

/// 失敗の種類。**理由を握り潰さない** — どれも呼び出し側が対処を選べる形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    /// 触ってよい範囲の外を指した。
    OutsideWorkspace { path: String, roots: Vec<String> },
    /// 根が 1 つも無い。
    NoWorkspace,
    /// 引数が受け付けられない (壊れた正規表現・空の記号・JSON でない等)。
    BadRequest(String),
    /// 読み書きに失敗した。
    Io(String),
    /// 2 進ファイル。
    Binary(String),
    /// 大きすぎる。
    TooLarge { path: String, bytes: u64 },
    /// 裏のスレッドを起こせなかった。
    Spawn(String),
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextError::OutsideWorkspace { path, roots } => write!(
                f,
                "{path} is outside the workspace (roots: {})",
                roots.join(", ")
            ),
            ContextError::NoWorkspace => write!(f, "no workspace root given"),
            ContextError::BadRequest(m) => write!(f, "{m}"),
            ContextError::Io(m) => write!(f, "{m}"),
            ContextError::Binary(p) => write!(f, "{p}: binary file"),
            ContextError::TooLarge { path, bytes } => {
                write!(f, "{path}: file too large ({bytes} bytes)")
            }
            ContextError::Spawn(m) => write!(f, "could not start background work: {m}"),
        }
    }
}

impl std::error::Error for ContextError {}

/// 最適化の入口。
#[derive(Clone, Debug)]
pub struct ContextEngine {
    workspace: Workspace,
    limits: ContextLimits,
    /// 台帳の置き場。`None` なら**プロセス内にだけ**積む。
    metrics_store: Option<PathBuf>,
}

impl ContextEngine {
    /// 触ってよい範囲を与えて作る。
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            limits: ContextLimits::default(),
            metrics_store: None,
        }
    }

    /// 上限を差し替える。
    pub fn with_limits(mut self, limits: ContextLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 台帳をファイルへも残す。
    pub fn with_metrics_store(mut self, path: Option<PathBuf>) -> Self {
        self.metrics_store = path;
        self
    }

    /// 触ってよい範囲。
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// 同期で走らせる。**描画スレッドから呼ばない** ([`Self::spawn`] を使う)。
    pub fn run(&self, req: &ContextRequest) -> Result<OptimizedContext, ContextError> {
        let started = std::time::Instant::now();
        let cx = ToolContext {
            workspace: &self.workspace,
            limits: &self.limits,
        };
        let rendered = match &req.source {
            ContextSource::File { path, params } => {
                tools::read::run(&cx, path, *params, req.strategy)?
            }
            ContextSource::Search { root, params } => tools::grep::run(&cx, root, params)?,
            ContextSource::Symbol { root, params } => tools::refs::run(&cx, root, params)?,
            ContextSource::Directory { path, params } => tools::directory::run(&cx, path, *params)?,
            ContextSource::Json { text, limits } => {
                tools::json::run(&cx, tools::json::JsonInput::Text(text), *limits)?
            }
            ContextSource::JsonFile { path, limits } => {
                tools::json::run(&cx, tools::json::JsonInput::File(path), *limits)?
            }
            ContextSource::Text { text, level } => {
                tools::text::run(&cx, tools::text::TextInput::Text(text), *level)?
            }
            ContextSource::TextFile { path, level } => {
                tools::text::run(&cx, tools::text::TextInput::File(path), *level)?
            }
            ContextSource::Count(text) => {
                tools::text::count(&cx, tools::text::TextInput::Text(text))?
            }
            ContextSource::CountFile(path) => {
                tools::text::count(&cx, tools::text::TextInput::File(path))?
            }
        };

        let cap = req.max_tokens.unwrap_or(self.limits.max_tokens);
        let (content, truncated) = truncate_tokens(&rendered.body, cap);
        let optimized_tokens = estimate_tokens(&content);
        let operation = req.source.operation();
        let metrics = ContextMetrics {
            operation,
            original_tokens: rendered.original_tokens,
            optimized_tokens,
            origin: req.origin.clone(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        let summary = format!(
            "[context] {} {} ~{}→~{} tok ({}){}{}",
            operation.id(),
            rendered.detail,
            metrics.original_tokens,
            optimized_tokens,
            percent_label(metrics.original_tokens, optimized_tokens),
            if truncated { " [capped]" } else { "" },
            rendered.hint,
        );
        let applied = applied_label(&rendered.detail, req.strategy);

        metrics::record(&metrics);
        if let Some(store) = &self.metrics_store {
            // 統計が書けないことで最適化そのものを失敗させない (fail-open)。
            let _ = metrics::persist(store, &metrics);
        }

        Ok(OptimizedContext {
            content,
            summary,
            applied,
            truncated,
            metrics,
        })
    }

    /// 裏のスレッドで走らせる。**描画から呼ぶ経路はこちら。**
    ///
    /// `ready` は結果が出たあとに 1 度だけ呼ばれる (egui なら
    /// `ctx.request_repaint` を渡す)。起こせなかったときも受信側は
    /// `Disconnected` を見るので、UI が「実行中」のまま固まらない。
    pub fn spawn(
        &self,
        req: ContextRequest,
        ready: impl FnOnce() + Send + 'static,
    ) -> Receiver<Result<OptimizedContext, ContextError>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = self.clone();
        let started = std::thread::Builder::new()
            .name("zaivern-context".into())
            .spawn(move || {
                let _ = tx.send(engine.run(&req));
                ready();
            });
        if let Err(e) = started {
            // spawn そのものが失敗したら、送り手が落ちて rx は Disconnected。
            // 受け側が理由を出せるよう、ここでも記録しておく。
            let (tx2, rx2) = std::sync::mpsc::channel();
            let _ = tx2.send(Err(ContextError::Spawn(e.to_string())));
            return rx2;
        }
        rx
    }
}

/// `-42%` / `±0%` の表記。
fn percent_label(original: usize, optimized: usize) -> String {
    let p = reduction_percent(original, optimized);
    if p <= 0.0 {
        "±0%".to_string()
    } else {
        format!("-{}%", p.round() as i64)
    }
}

/// ヘッダの `strategy=…` を拾って「実際に使われた戦略」にする。
/// 道具が `strategy=` を名乗らない場合は要求された戦略をそのまま返す。
fn applied_label(detail: &str, requested: ContextStrategy) -> String {
    detail
        .split_whitespace()
        .find_map(|w| w.strip_prefix("strategy="))
        .map(str::to_string)
        .unwrap_or_else(|| requested.id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests_support::Lab;

    #[test]
    fn 戦略の往復と一覧() {
        for s in ContextStrategy::ALL {
            assert_eq!(ContextStrategy::parse(s.id()), Some(s));
        }
        // 知らない語は黙って auto へ落ちない
        assert_eq!(ContextStrategy::parse("おまかせ"), None);
        assert_eq!(ContextStrategy::default(), ContextStrategy::Auto);
    }

    #[test]
    fn 結果はヘッダと本文とメトリクスを持つ() {
        let lab = Lab::new("engine-basic");
        lab.write("a.rs", "// コメント\nfn a() {}\n");
        let out = lab
            .engine()
            .run(&ContextRequest::new(ContextSource::File {
                path: "a.rs".into(),
                params: Default::default(),
            }))
            .unwrap();
        assert_eq!(out.content, "fn a() {}");
        assert!(
            out.summary.starts_with("[context] read a.rs "),
            "{}",
            out.summary
        );
        assert!(out.summary.contains("~"), "{}", out.summary);
        assert_eq!(out.applied, "slim(auto)");
        assert!(out.metrics.saved_tokens() > 0);
        assert!(out.metrics.reduction_percent() > 0.0);
        assert_eq!(out.render(), format!("{}\n{}", out.summary, out.content));
        assert_eq!(out.metrics.operation, ContextOperation::Read);
    }

    /// **出自は挙動を変えない。** これが Provider 非依存の実効部。
    #[test]
    fn 出自は挙動を変えない() {
        let lab = Lab::new("engine-origin");
        let mut src = String::new();
        for i in 0..200 {
            src.push_str(&format!("pub fn f{i}() {{\n    work({i});\n}}\n"));
        }
        lab.write("a.rs", &src);
        let engine = lab.engine();
        let mut seen: Vec<(String, String)> = Vec::new();
        for who in ["claude", "codex", "gemini", "hermes", "opencode", ""] {
            let req = ContextRequest::new(ContextSource::File {
                path: "a.rs".into(),
                params: Default::default(),
            })
            .with_origin(if who.is_empty() {
                ContextOrigin::unknown()
            } else {
                ContextOrigin {
                    agent: Some(who.to_string()),
                    ..ContextOrigin::unknown()
                }
            });
            let out = engine.run(&req).unwrap();
            seen.push((out.content.clone(), out.applied.clone()));
        }
        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "出自で結果が変わった (Provider 依存が混ざっている)"
        );
        // 出自は結果のメトリクスには残る (分類はできる)
        let req =
            ContextRequest::new(ContextSource::Count("abc".into())).with_origin(ContextOrigin {
                agent: Some("claude".into()),
                session: Some("s-1".into()),
                task: Some("t-9".into()),
            });
        let out = engine.run(&req).unwrap();
        assert_eq!(out.metrics.origin.agent.as_deref(), Some("claude"));
        assert_eq!(out.metrics.origin.session.as_deref(), Some("s-1"));
        assert_eq!(out.metrics.origin.task.as_deref(), Some("t-9"));
    }

    #[test]
    fn 上限を超えたら畳んでそう名乗る() {
        let lab = Lab::new("engine-cap");
        let body = (0..4000)
            .map(|i| format!("value_{i} = {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        lab.write("d.txt", &body);
        let out = lab
            .engine()
            .run(
                &ContextRequest::new(ContextSource::TextFile {
                    path: "d.txt".into(),
                    level: Default::default(),
                })
                .with_max_tokens(300),
            )
            .unwrap();
        assert!(out.truncated);
        assert!(out.summary.contains("[capped]"), "{}", out.summary);
        assert!(out.content.contains("snipped"));
        assert!(out.metrics.optimized_tokens < 400);
        // 上限 0 は「上限なし」
        let out = lab
            .engine()
            .run(
                &ContextRequest::new(ContextSource::TextFile {
                    path: "d.txt".into(),
                    level: Default::default(),
                })
                .with_max_tokens(0),
            )
            .unwrap();
        assert!(!out.truncated);
    }

    #[test]
    fn 削減が無いときはプラマイゼロと出す() {
        assert_eq!(percent_label(0, 0), "±0%");
        assert_eq!(percent_label(100, 100), "±0%");
        assert_eq!(percent_label(100, 140), "±0%");
        assert_eq!(percent_label(100, 25), "-75%");
        let lab = Lab::new("engine-nochange");
        let out = lab
            .engine()
            .run(&ContextRequest::new(ContextSource::Count("abc".into())))
            .unwrap();
        assert_eq!(out.metrics.saved_tokens(), 0);
        assert!(out.summary.contains("±0%"), "{}", out.summary);
    }

    #[test]
    fn 失敗はどれも理由つきで返る() {
        let lab = Lab::new("engine-errors");
        lab.write("a.rs", "fn a() {}\n");
        let engine = lab.engine();
        let e = engine
            .run(&ContextRequest::new(ContextSource::File {
                path: "../secret.txt".into(),
                params: Default::default(),
            }))
            .unwrap_err();
        assert!(matches!(e, ContextError::OutsideWorkspace { .. }));
        assert!(e.to_string().contains("outside the workspace"), "{e}");

        let e = engine
            .run(&ContextRequest::new(ContextSource::File {
                path: "nope.rs".into(),
                params: Default::default(),
            }))
            .unwrap_err();
        assert!(matches!(e, ContextError::Io(_)), "{e:?}");

        assert_eq!(
            ContextError::NoWorkspace.to_string(),
            "no workspace root given"
        );
        assert!(ContextError::TooLarge {
            path: "x".into(),
            bytes: 9
        }
        .to_string()
        .contains("too large"));
        assert!(ContextError::Binary("x".into())
            .to_string()
            .contains("binary"));
        assert!(ContextError::Spawn("no threads".into())
            .to_string()
            .contains("background"));
    }

    /// 裏のスレッド経路。**描画から呼ぶのはこちら。**
    #[test]
    fn 裏で走らせても同じ結果になる() {
        let lab = Lab::new("engine-spawn");
        lab.write("a.rs", "// c\nfn a() {}\n");
        let engine = lab.engine();
        let req = ContextRequest::new(ContextSource::File {
            path: "a.rs".into(),
            params: Default::default(),
        });
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        let rx = engine.spawn(req, move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst)
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("結果が返る")
            .expect("成功する");
        assert_eq!(out.content, "fn a() {}");
        // 通知は**送信のあと**に来るので、受け取った直後にはまだ立っていない
        // ことがある。決まった時間で寝るのではなく、立つまで待つ。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !done.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "完了通知が来ない");
            std::thread::yield_now();
        }
    }

    /// 台帳へ残す指定があれば、走らせるたびに合計が増える。
    #[test]
    fn 台帳へ残す指定が効く() {
        let lab = Lab::new("engine-ledger");
        lab.write("a.rs", "// コメントだらけの行\n// もう 1 行\nfn a() {}\n");
        let store = lab.root().join("ledger").join("metrics.json");
        let engine = lab.engine().with_metrics_store(Some(store.clone()));
        for _ in 0..3 {
            engine
                .run(&ContextRequest::new(ContextSource::File {
                    path: "a.rs".into(),
                    params: Default::default(),
                }))
                .unwrap();
        }
        let l = crate::context::metrics::Ledger::load(&store);
        assert_eq!(l.total().operations, 3);
        assert!(l.total().saved_tokens() > 0);
        assert_eq!(l.by_operation()[0].0, ContextOperation::Read);
    }
}
