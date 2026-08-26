//! `zai context …` — Context Engine の **CLI アダプタ**。
//!
//! ここは薄い皮で、引数を [`super::ContextRequest`] へ写して結果を印字する
//! だけ。**判断はコアが持つ** (どの戦略へ降りるか・どこまで読んでよいか)。
//!
//! MCP アダプタを足すときも、置き場はこの隣であってコアの中ではない。
//! 同じ [`super::ContextEngine`] を別の形で包むだけで済む。
//!
//! ## 追加インストールを要求しない
//!
//! `cargo install token-slim-mcp` も `claude mcp add …` も要らない。
//! Zaivern を入れた人は `zai context …` がその場で使える。

use std::path::PathBuf;

use super::optimizer::JsonLimits;
use super::tools::{
    directory::MapParams, grep::SearchParams, read::ReadParams, refs::RefsParams, text::TextLevel,
};
use super::walk::Filter;
// **クレート内 API はここ** — `crate::context::…` を通す
// (道具の内部パスを直に指すと、外から使える面と食い違う)。
use crate::context::{
    ContextEngine, ContextError, ContextOrigin, ContextRequest, ContextSource, ContextStrategy,
};

/// 成功。
const EXIT_OK: i32 = 0;
/// 実行時エラー。
const EXIT_ERR: i32 = 1;
/// 使い方の誤り。
const EXIT_USAGE: i32 = 2;

/// `zai context --help` の本文。
pub const HELP: &str = "\
context (コンテキストエンジン — AI へ渡す前に情報量を減らします。どのエージェントでも共通):
  zai context read <パス> [--mode auto|slim|outline|raw] [--offset N] [--limit N]
                 [--max-tokens N] [--keep-comments]
                                        ファイルを畳んで出す (既定 auto = 大きい構造つきは構造だけ)
  zai context grep <正規表現> [--path <場所>] [--ext rs,toml] [--max-results N]
                 [--ignore-case] [--literal] [--include <glob>] [--exclude <glob>]
                 [--exclude-tests]      木を検索して path:line:本文 だけを出す
  zai context refs <記号> [--path <場所>] [--ext ...] [--depth 1|2]
                 [--max-results N] [--include-tests]
                                        参照を定義 / 呼び出し / テスト / import / コメントへ分類
  zai context map [<場所>] [--depth N] [--max-entries N]
                                        ディレクトリの地図 (ls -R / find の代わり)
  zai context json <パス> | --text <文字列> [--max-depth N] [--max-array N] [--max-string N]
                                        JSON / JSONC を最小化して刈る
  zai context text <パス> | --text <文字列> | - [--level normal|aggressive] [--max-tokens N]
                                        ログや出力を畳む (- は標準入力)
  zai context tokens <パス> | --text <文字列> | -
                                        トークン数を見積もる (何も畳みません)
  zai context stats [--json] [--reset]  これまでの削減量

  共通: --root <場所> で読んでよい範囲を指定 (既定はカレントディレクトリ)
        --agent/--session/--task <ID> は集計のラベルで、処理は変わりません
  終了コード: 0 = 成功 / 1 = 実行時エラー / 2 = 使い方の誤り
  ファイルは読むだけで、書き換えも外部通信もしません。範囲外のパスは断ります。
";

/// `zai context <sub>` の実体。argv は `"context"` の**次**から渡される。
pub fn cli_main(argv: &[String]) -> i32 {
    if argv.is_empty() || argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", HELP.trim_end());
        return EXIT_OK;
    }
    let sub = argv[0].as_str();
    let rest = &argv[1..];
    match run(sub, rest) {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            EXIT_OK
        }
        Err(CliFail::Usage(m)) => {
            eprintln!("{m}\n\n{}", HELP.trim_end());
            EXIT_USAGE
        }
        Err(CliFail::Run(m)) => {
            eprintln!("{m}");
            EXIT_ERR
        }
    }
}

/// 失敗の理由。**使い方の誤りと実行時エラーを混ぜない** (終了コードが違う)。
#[derive(Debug)]
enum CliFail {
    Usage(String),
    Run(String),
}

impl From<ContextError> for CliFail {
    fn from(e: ContextError) -> Self {
        match e {
            ContextError::BadRequest(_) | ContextError::NoWorkspace => {
                CliFail::Usage(e.to_string())
            }
            other => CliFail::Run(other.to_string()),
        }
    }
}

/// 解析済みの共通オプション。
struct Common {
    root: PathBuf,
    origin: ContextOrigin,
    max_tokens: Option<usize>,
    strategy: Option<ContextStrategy>,
}

/// 引数を 1 度だけ舐めて、共通オプションと残りへ分ける。
fn split_common(args: &[String]) -> Result<(Common, Vec<String>), CliFail> {
    let mut root: Option<String> = None;
    let mut origin = ContextOrigin::unknown();
    let mut max_tokens = None;
    let mut strategy = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let need = |v: Option<&String>| -> Result<String, CliFail> {
            v.cloned()
                .ok_or_else(|| CliFail::Usage(format!("{a} に値がありません")))
        };
        match a.as_str() {
            "--root" => root = Some(need(it.next())?),
            "--agent" => origin.agent = Some(need(it.next())?),
            "--session" => origin.session = Some(need(it.next())?),
            "--task" => origin.task = Some(need(it.next())?),
            "--max-tokens" => max_tokens = Some(parse_usize(&need(it.next())?, "--max-tokens")?),
            "--mode" | "--strategy" => {
                let v = need(it.next())?;
                strategy = Some(ContextStrategy::parse(&v).ok_or_else(|| {
                    CliFail::Usage(format!(
                        "{a} が {v:?} です。auto / slim / outline / raw のいずれかにしてください"
                    ))
                })?);
            }
            _ => rest.push(a.clone()),
        }
    }
    let root = match root {
        Some(r) => PathBuf::from(r),
        None => std::env::current_dir()
            .map_err(|e| CliFail::Run(format!("カレントディレクトリが判りません: {e}")))?,
    };
    Ok((
        Common {
            root,
            origin,
            max_tokens,
            strategy,
        },
        rest,
    ))
}

fn parse_usize(v: &str, name: &str) -> Result<usize, CliFail> {
    v.parse()
        .map_err(|_| CliFail::Usage(format!("{name} は 0 以上の整数にしてください ({v:?})")))
}

/// `--name <値>` を 1 つ取り出す。
fn take_opt(args: &[String], name: &str) -> Result<(Option<String>, Vec<String>), CliFail> {
    let mut value = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            value = Some(
                it.next()
                    .cloned()
                    .ok_or_else(|| CliFail::Usage(format!("{name} に値がありません")))?,
            );
        } else {
            rest.push(a.clone());
        }
    }
    Ok((value, rest))
}

/// `--name` を 1 つ取り出す。
fn take_flag(args: &[String], name: &str) -> (bool, Vec<String>) {
    let mut found = false;
    let mut rest = Vec::new();
    for a in args {
        if a == name {
            found = true;
        } else {
            rest.push(a.clone());
        }
    }
    (found, rest)
}

/// 繰り返せる `--name <値>` を**全部**取り出す (`,` 区切りも展開する)。
///
/// [`take_opt`] を繰り返し呼ぶ書き方にすると、1 回目で同名の指定を全部
/// 取り除いたうえで**最後の 1 つしか返さない**ので、先に書いた値が黙って
/// 消える (実際にそう書いて `--exclude a/** --exclude b/**` の a が消えた)。
fn take_many(args: &[String], name: &str) -> Result<(Vec<String>, Vec<String>), CliFail> {
    let mut out = Vec::new();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a != name {
            rest.push(a.clone());
            continue;
        }
        let v = it
            .next()
            .ok_or_else(|| CliFail::Usage(format!("{name} に値がありません")))?;
        out.extend(
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    Ok((out, rest))
}

/// 先頭の位置引数 1 つ。
fn first_positional(rest: &[String], what: &str) -> Result<String, CliFail> {
    let Some(v) = rest.first() else {
        return Err(CliFail::Usage(format!("{what} を指定してください")));
    };
    if v.starts_with('-') && v != "-" {
        return Err(CliFail::Usage(format!("知らないオプションです: {v}")));
    }
    if rest.len() > 1 {
        return Err(CliFail::Usage(format!("余分な引数です: {}", rest[1])));
    }
    Ok(v.clone())
}

/// 走査の絞り込みを組む。
fn build_filter(rest: &[String]) -> Result<(Filter, Vec<String>), CliFail> {
    let (ext, rest) = take_opt(rest, "--ext")?;
    let (include, rest) = take_many(&rest, "--include")?;
    let (exclude, rest) = take_many(&rest, "--exclude")?;
    let (no_tests, rest) = take_flag(&rest, "--exclude-tests");
    let mut f = Filter {
        include,
        exclude,
        ..Filter::default()
    };
    if let Some(e) = ext {
        f = f.with_exts(&e);
    }
    if no_tests {
        f = f.exclude_tests();
    }
    Ok((f, rest))
}

/// 標準入力を全部読む (`-` を渡されたとき)。
fn read_stdin() -> Result<String, CliFail> {
    use std::io::Read as _;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| CliFail::Run(format!("標準入力を読めません: {e}")))?;
    Ok(buf)
}

/// 設定を読み、エンジンを組む。**設定で無効なら断る** (勝手に動かない)。
fn engine_for(root: &PathBuf) -> Result<ContextEngine, CliFail> {
    let cfg = crate::config::load(std::slice::from_ref(root), false);
    if !super::enabled(&cfg) {
        return Err(CliFail::Run(format!(
            "コンテキスト最適化は無効です (config.toml の [features] で \"{}\" を true にしてください)",
            super::KEY_ENABLED
        )));
    }
    super::engine_for(std::slice::from_ref(root), &cfg).map_err(CliFail::from)
}

fn run(sub: &str, args: &[String]) -> Result<String, CliFail> {
    if sub == "stats" {
        return stats(args);
    }
    let (common, rest) = split_common(args)?;
    let engine = engine_for(&common.root)?;
    let cfg_strategy = {
        let cfg = crate::config::load(std::slice::from_ref(&common.root), false);
        super::strategy_from_config(&cfg)
    };
    let strategy = common.strategy.unwrap_or(cfg_strategy);

    let source = match sub {
        "read" => {
            let (offset, rest) = take_opt(&rest, "--offset")?;
            let (limit, rest) = take_opt(&rest, "--limit")?;
            let (keep, rest) = take_flag(&rest, "--keep-comments");
            let path = first_positional(&rest, "ファイルのパス")?;
            ContextSource::File {
                path: PathBuf::from(path),
                params: ReadParams {
                    offset: offset.map(|v| parse_usize(&v, "--offset")).transpose()?,
                    limit: limit.map(|v| parse_usize(&v, "--limit")).transpose()?,
                    strip_comments: !keep,
                },
            }
        }
        "grep" | "search" => {
            let (filter, rest) = build_filter(&rest)?;
            let (max_results, rest) = take_opt(&rest, "--max-results")?;
            let (ignore_case, rest) = take_flag(&rest, "--ignore-case");
            let (literal, rest) = take_flag(&rest, "--literal");
            let (path, rest) = take_opt(&rest, "--path")?;
            let pattern = first_positional(&rest, "検索するパターン")?;
            let mut params = SearchParams::new(pattern);
            params.filter = filter;
            params.ignore_case = ignore_case;
            params.literal = literal;
            params.max_results = max_results
                .map(|v| parse_usize(&v, "--max-results"))
                .transpose()?;
            ContextSource::Search {
                root: path
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(".")),
                params,
            }
        }
        "refs" => {
            let (filter, rest) = build_filter(&rest)?;
            let (max_results, rest) = take_opt(&rest, "--max-results")?;
            let (depth, rest) = take_opt(&rest, "--depth")?;
            let (include_tests, rest) = take_flag(&rest, "--include-tests");
            let (path, rest) = take_opt(&rest, "--path")?;
            let symbol = first_positional(&rest, "追いかける記号")?;
            let mut params = RefsParams::new(symbol);
            params.filter = filter;
            params.include_tests = include_tests;
            params.depth = depth
                .map(|v| parse_usize(&v, "--depth"))
                .transpose()?
                .unwrap_or(1);
            params.max_results = max_results
                .map(|v| parse_usize(&v, "--max-results"))
                .transpose()?;
            ContextSource::Symbol {
                root: path
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(".")),
                params,
            }
        }
        "map" | "dir" => {
            let (depth, rest) = take_opt(&rest, "--depth")?;
            let (max_entries, rest) = take_opt(&rest, "--max-entries")?;
            let path = rest.first().cloned().unwrap_or_else(|| ".".to_string());
            if rest.len() > 1 {
                return Err(CliFail::Usage(format!("余分な引数です: {}", rest[1])));
            }
            ContextSource::Directory {
                path: PathBuf::from(path),
                params: MapParams {
                    depth: depth.map(|v| parse_usize(&v, "--depth")).transpose()?,
                    max_entries: max_entries
                        .map(|v| parse_usize(&v, "--max-entries"))
                        .transpose()?,
                },
            }
        }
        "json" => {
            let (text, rest) = take_opt(&rest, "--text")?;
            let (max_depth, rest) = take_opt(&rest, "--max-depth")?;
            let (max_array, rest) = take_opt(&rest, "--max-array")?;
            let (max_string, rest) = take_opt(&rest, "--max-string")?;
            let mut lim = JsonLimits::default();
            let mut touched = false;
            if let Some(v) = max_depth {
                lim.max_depth = parse_usize(&v, "--max-depth")?;
                touched = true;
            }
            if let Some(v) = max_array {
                lim.max_array = parse_usize(&v, "--max-array")?;
                touched = true;
            }
            if let Some(v) = max_string {
                lim.max_string = parse_usize(&v, "--max-string")?;
                touched = true;
            }
            let limits = touched.then_some(lim);
            match text {
                Some(t) => ContextSource::Json { text: t, limits },
                None => {
                    let path = first_positional(&rest, "JSON のパス (または --text)")?;
                    if path == "-" {
                        ContextSource::Json {
                            text: read_stdin()?,
                            limits,
                        }
                    } else {
                        ContextSource::JsonFile {
                            path: PathBuf::from(path),
                            limits,
                        }
                    }
                }
            }
        }
        "text" => {
            let (text, rest) = take_opt(&rest, "--text")?;
            let (level, rest) = take_opt(&rest, "--level")?;
            let level = match level {
                Some(l) => TextLevel::parse(&l).ok_or_else(|| {
                    CliFail::Usage(format!(
                        "--level が {l:?} です。normal か aggressive にしてください"
                    ))
                })?,
                None => TextLevel::default(),
            };
            match text {
                Some(t) => ContextSource::Text { text: t, level },
                None => {
                    let path = first_positional(&rest, "テキストのパス (または --text / -)")?;
                    if path == "-" {
                        ContextSource::Text {
                            text: read_stdin()?,
                            level,
                        }
                    } else {
                        ContextSource::TextFile {
                            path: PathBuf::from(path),
                            level,
                        }
                    }
                }
            }
        }
        "tokens" | "count" => {
            let (text, rest) = take_opt(&rest, "--text")?;
            match text {
                Some(t) => ContextSource::Count(t),
                None => {
                    let path = first_positional(&rest, "パス (または --text / -)")?;
                    if path == "-" {
                        ContextSource::Count(read_stdin()?)
                    } else {
                        ContextSource::CountFile(PathBuf::from(path))
                    }
                }
            }
        }
        other => return Err(CliFail::Usage(format!("知らないサブコマンドです: {other}"))),
    };

    let mut req = ContextRequest::new(source)
        .with_strategy(strategy)
        .with_origin(common.origin);
    if let Some(n) = common.max_tokens {
        req = req.with_max_tokens(n);
    }
    Ok(engine.run(&req)?.render())
}

/// `zai context stats` — これまでの削減量。
fn stats(args: &[String]) -> Result<String, CliFail> {
    let (json, rest) = take_flag(args, "--json");
    let (reset, rest) = take_flag(&rest, "--reset");
    if let Some(x) = rest.first() {
        return Err(CliFail::Usage(format!("知らないオプションです: {x}")));
    }
    let path = super::metrics::store_path(&crate::config::zaivern_dir());
    if reset {
        // **自分が作ったファイルだけを消す。** パターン検索で見つけたものは触らない。
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(format!("削減量の記録を消しました: {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok("残っている記録はありません".to_string())
            }
            Err(e) => return Err(CliFail::Run(format!("{}: {e}", path.display()))),
        }
    }
    let saved = super::metrics::Ledger::load(&path);
    let live = super::metrics::snapshot();
    let today = super::metrics::today();
    if json {
        let v = serde_json::json!({
            "store": path.to_string_lossy(),
            "persisted": saved.to_json(),
            "this_process": live.to_json(),
            "today": {
                "day": today,
                "saved_tokens": saved.day(today).saved_tokens() + live.day(today).saved_tokens(),
            },
            "by_agent": saved
                .by_agent()
                .iter()
                .map(|(a, t)| serde_json::json!({
                    "agent": a,
                    "operations": t.operations,
                    "saved_tokens": t.saved_tokens(),
                }))
                .collect::<Vec<_>>(),
        });
        return serde_json::to_string_pretty(&v)
            .map_err(|e| CliFail::Run(format!("JSON にできません: {e}")));
    }
    let mut out = Vec::new();
    let t = saved.total();
    out.push(format!(
        "保存済み: {} 回 / ~{} → ~{} tok (節約 ~{}, {:.1}%)",
        t.operations,
        t.original_tokens,
        t.optimized_tokens,
        t.saved_tokens(),
        t.reduction_percent()
    ));
    let l = live.total();
    out.push(format!(
        "このプロセス: {} 回 / 節約 ~{} tok ({:.1}%)",
        l.operations,
        l.saved_tokens(),
        l.reduction_percent()
    ));
    if saved.days_recorded() > 0 {
        out.push(format!("記録のある日数: {}", saved.days_recorded()));
    }
    for (op, tot) in saved.by_operation() {
        out.push(format!(
            "  {:<10} {:>5} 回  節約 ~{} tok ({:.1}%)",
            op.id(),
            tot.operations,
            tot.saved_tokens(),
            tot.reduction_percent()
        ));
    }
    // どのエージェントのために減らしたか (出自を渡した呼び出しだけ)
    for (agent, tot) in saved.by_agent() {
        out.push(format!(
            "  agent {:<8} {:>5} 回  節約 ~{} tok",
            agent,
            tot.operations,
            tot.saved_tokens()
        ));
    }
    if t.operations == 0 {
        out.push(format!(
            "(記録は既定で残しません。残すには [features] の \"{}\" を true に)",
            super::KEY_PERSIST
        ));
    }
    Ok(out.join("\n"))
}

// **統合担当が `cli.rs` から呼ぶ入口の型を、コンパイル時に固定する。**
// 署名がずれたらここで落ちるので、`"context" => …cli_main(rest)` の 1 行を
// 足すだけで繋がる、という約束が型で担保される。
const _: fn(&[String]) -> i32 = crate::features::context::cli_main;

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn ヘルプは全てのサブコマンドを載せている() {
        for sub in [
            "read", "grep", "refs", "map", "json", "text", "tokens", "stats",
        ] {
            assert!(
                HELP.contains(&format!("zai context {sub}")),
                "{sub} がヘルプに無い"
            );
        }
        // 引数無しと --help は 0 で終わる
        assert_eq!(cli_main(&[]), EXIT_OK);
        assert_eq!(cli_main(&v(&["--help"])), EXIT_OK);
    }

    #[test]
    fn 共通オプションを切り出す() {
        let (c, rest) = split_common(&v(&[
            "a.rs",
            "--root",
            "/tmp/x",
            "--agent",
            "claude",
            "--session",
            "s1",
            "--task",
            "t1",
            "--max-tokens",
            "123",
            "--mode",
            "outline",
        ]))
        .unwrap();
        assert_eq!(rest, v(&["a.rs"]));
        assert_eq!(c.root, PathBuf::from("/tmp/x"));
        assert_eq!(c.origin.agent.as_deref(), Some("claude"));
        assert_eq!(c.origin.session.as_deref(), Some("s1"));
        assert_eq!(c.origin.task.as_deref(), Some("t1"));
        assert_eq!(c.max_tokens, Some(123));
        assert_eq!(c.strategy, Some(ContextStrategy::Outline));
    }

    /// 綴りを間違えたら**使い方の誤り**として断る (黙って既定へ落ちない)。
    #[test]
    fn 知らない値は使い方の誤りとして断る() {
        assert!(matches!(
            split_common(&v(&["--mode", "でたらめ"])),
            Err(CliFail::Usage(_))
        ));
        assert!(matches!(
            split_common(&v(&["--max-tokens", "たくさん"])),
            Err(CliFail::Usage(_))
        ));
        assert!(matches!(
            split_common(&v(&["--root"])),
            Err(CliFail::Usage(_))
        ));
        assert!(matches!(run("しらない", &[]), Err(CliFail::Usage(_))));
    }

    #[test]
    fn 絞り込みを組み立てる() {
        let (f, rest) = build_filter(&v(&[
            "pat",
            "--ext",
            "rs,toml",
            "--include",
            "src/**",
            "--exclude",
            "a/**",
            "--exclude",
            "b/**,c/**",
            "--exclude-tests",
        ]))
        .unwrap();
        assert_eq!(rest, v(&["pat"]));
        assert_eq!(f.exts, v(&["rs", "toml"]));
        assert_eq!(f.include, v(&["src/**"]));
        assert!(f.exclude.contains(&"a/**".to_string()));
        assert!(f.exclude.contains(&"c/**".to_string()));
        assert!(f.exclude.len() > 3, "--exclude-tests が効いていない");
    }

    #[test]
    fn 位置引数の検査() {
        assert_eq!(first_positional(&v(&["a.rs"]), "パス").unwrap(), "a.rs");
        assert!(matches!(
            first_positional(&v(&[]), "パス"),
            Err(CliFail::Usage(_))
        ));
        assert!(matches!(
            first_positional(&v(&["--nope"]), "パス"),
            Err(CliFail::Usage(_))
        ));
        assert!(matches!(
            first_positional(&v(&["a.rs", "b.rs"]), "パス"),
            Err(CliFail::Usage(_))
        ));
        // `-` は標準入力の指定なのでオプション扱いしない
        assert_eq!(first_positional(&v(&["-"]), "パス").unwrap(), "-");
    }

    /// 範囲外・引数の誤りが、それぞれ正しい終了コードへ写ること。
    #[test]
    fn 失敗の種類が終了コードへ写る() {
        assert!(matches!(
            CliFail::from(ContextError::BadRequest("x".into())),
            CliFail::Usage(_)
        ));
        assert!(matches!(
            CliFail::from(ContextError::NoWorkspace),
            CliFail::Usage(_)
        ));
        assert!(matches!(
            CliFail::from(ContextError::OutsideWorkspace {
                path: "x".into(),
                roots: vec![]
            }),
            CliFail::Run(_)
        ));
        assert!(matches!(
            CliFail::from(ContextError::Io("x".into())),
            CliFail::Run(_)
        ));
    }

    /// 端から端まで 1 本通す。**実 `~/.zaivern` には触らない。**
    #[test]
    fn 実際に走らせて出力が出る() {
        let dir = crate::test_util::unique_temp_dir("zaivern-ctx", "cli-e2e");
        let root = crate::pathx::canonical(&dir);
        std::fs::write(root.join("a.rs"), "// コメント\nfn target() {}\n").unwrap();
        let r = |args: &[&str]| -> Result<String, CliFail> {
            let mut full = v(args);
            full.push("--root".into());
            full.push(root.to_string_lossy().into_owned());
            run(full[0].clone().as_str(), &full[1..])
        };
        let out = r(&["read", "a.rs"]).expect("読める");
        assert!(out.starts_with("[context] read a.rs"), "{out}");
        assert!(out.contains("fn target() {}"));
        assert!(!out.contains("コメント"));

        let out = r(&["grep", "target"]).expect("検索できる");
        assert!(out.contains("a.rs:2:"), "{out}");

        let out = r(&["map"]).expect("地図が作れる");
        assert!(out.contains("a.rs"), "{out}");

        let out = r(&["tokens", "--text", "abcd"]).expect("数えられる");
        assert!(out.contains("tokens"), "{out}");

        let out = r(&["json", "--text", "{\"a\":[1,2,3],}"]).expect("JSON が読める");
        assert!(out.contains("jsonc"), "{out}");
        assert!(out.contains("{\"a\":[1,2,3]}"), "{out}");

        // 範囲外は実行時エラー
        assert!(matches!(r(&["read", "../etc/hosts"]), Err(CliFail::Run(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
