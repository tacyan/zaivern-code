//! 🔗 **成果物が参照しているのに実在しないファイル**を見つける。
//!
//! ## なぜ要るか (実測)
//!
//! 「かっこいい HP を作る」の Run で、出来上がった `index.html` は
//!
//! ```html
//! <script defer src="./assets/vendor/three/three.min.js"></script>
//! <script defer src="./assets/js/scene.js"></script>
//! <script defer src="./assets/js/main.js"></script>
//! ```
//!
//! と 3 本読み込んでいたが、**実在するのは真ん中の 1 本だけ**だった。
//! `three.min.js` と `main.js` は設計 (`docs/architecture.md`) が後から
//! 決めたファイルで、**どのタスクの `files` にも載っていない** —
//! 持ち主が居ないファイルは誰も作らない。
//!
//! それでも Run は「完了条件: コンソールにエラーが出ない」を掲げたまま
//! 進んだ。静的なサイトには `Cargo.toml` も `package.json` も無いので
//! [`super::validation_defaults::detect`] は `Undetermined` を返し、
//! **関門が 1 つも無かった**からである。ブラウザで 1 回開けば 5 秒で
//! 分かることを、誰も 1 度もしていなかった。
//!
//! ## ここが見るもの / 見ないもの
//!
//! * 見る — HTML の `src` / `href` のうち**ローカルを指すもの**
//! * 見ない — `http:` `https:` `//cdn…` `data:` `mailto:` `#anchor`
//!   (外部と面内アンカーは、この層では確かめようがない)
//!
//! 見た目の良し悪しは判定しない。**「読み込むと言ったものが在るか」**
//! だけを見る。ここを越えても綺麗とは限らないが、ここで落ちるページは
//! **確実に壊れている**。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 参照はしているが実在しないファイル 1 件。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dangling {
    /// 参照している側 (ワークスペース相対)。
    pub from: String,
    /// HTML に書かれていた綴りそのまま。
    pub raw: String,
    /// 解決した先 (ワークスペース相対)。
    pub resolved: String,
}

impl Dangling {
    pub fn detail(&self) -> String {
        format!("{} が読み込む `{}` がありません", self.from, self.raw)
    }
}

/// 走査するファイル数の上限。**黙って一部だけ見ない** —
/// 超えたら [`ScanError::TooManyFiles`] で降りる。
pub const MAX_FILES: usize = 20_000;

/// 1 枚の HTML を読む上限。
pub const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;

/// 走査に入らないディレクトリ名。
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor.bak",
    ".next",
    ".venv",
    "__pycache__",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// ワークスペースが読めない。
    Unreadable { path: String, reason: String },
    /// 大きすぎて全部は見られない。**一部だけ見て「問題なし」と言わない。**
    TooManyFiles { seen: usize },
}

impl ScanError {
    pub fn detail(&self) -> String {
        match self {
            ScanError::Unreadable { path, reason } => {
                format!("{path} を読めません ({reason})")
            }
            ScanError::TooManyFiles { seen } => format!(
                "ファイルが多すぎて確かめられません ({seen} 件を超えました)。\
                 対象を絞ってください"
            ),
        }
    }
}

/// HTML から**ローカルを指す参照**だけを拾う (純関数)。
///
/// 属性の切り出しは素朴な走査で行う。**HTML パーサを持ち込まない** —
/// ここが欲しいのは「読み込むと言ったもの」の一覧であって、DOM ではない。
pub fn local_refs(html: &str) -> Vec<String> {
    // **書いてある順に返す。** 属性ごとにまとめて返すと、報告の並びが
    // ページの見た目と食い違って、どこを直せばよいのか読めなくなる。
    let mut hits: Vec<(usize, String)> = Vec::new();
    for attr in ["src=", "href=", "data-src="] {
        let mut base = 0usize;
        while let Some(i) = html[base..].find(attr) {
            let at = base + i;
            base = at + attr.len();
            let rest = &html[base..];
            let Some(quote) = rest.chars().next() else {
                break;
            };
            if quote != '"' && quote != '\'' {
                continue;
            }
            let after = &rest[quote.len_utf8()..];
            let Some(end) = after.find(quote) else {
                break;
            };
            if let Some(v) = as_local(after[..end].trim()) {
                hits.push((at, v));
            }
            base += quote.len_utf8() + end;
        }
    }
    hits.sort_by_key(|(at, _)| *at);
    let mut out: Vec<String> = Vec::new();
    for (_, v) in hits {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// ローカルを指しているなら、**問い合わせと断片を落とした綴り**を返す。
fn as_local(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.starts_with('#') || v.starts_with("//") {
        return None;
    }
    // `scheme:` が付いているものは外部 (`http:` `data:` `mailto:` `tel:` …)。
    // Windows のドライブ文字 (`C:`) と区別するため、**2 文字以上**の
    // スキームだけを外部とみなす。
    if let Some(colon) = v.find(':') {
        let scheme = &v[..colon];
        if scheme.len() >= 2 && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return None;
        }
    }
    let cut = v.find(['?', '#']).unwrap_or(v.len());
    let path = v[..cut].trim();
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// 参照を、**ワークスペース相対**の綴りへ直す (純関数)。
///
/// `page` はワークスペース相対の HTML の位置。`/` 始まりはワークスペースの
/// 根から。`..` はここで畳む — 畳まないと、同じファイルが別の綴りで
/// 2 度出る。根より上へ出るものは `None` (このワークスペースの外)。
pub fn resolve(page: &str, reference: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if !reference.starts_with('/') {
        let dir = page.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for seg in dir.split('/') {
            if !seg.is_empty() {
                parts.push(seg);
            }
        }
    }
    for seg in reference.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return None; // 根より上 = 外
                }
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// ワークスペースを走査して、宙に浮いた参照を返す。
///
/// **実在の判定は OS に任せる** (`Path::exists`)。ここで綴りの集合と
/// 突き合わせると、大小を区別しないファイルシステム (macOS の既定・
/// Windows) で `docs/PLAN.md` を `docs/plan.md` と書いた参照が
/// **在るのに「無い」**になる。
pub fn scan(workspace: &Path) -> Result<Vec<Dangling>, ScanError> {
    let mut pages: Vec<String> = Vec::new();
    let mut seen = 0usize;
    walk(workspace, workspace, &mut pages, &mut seen)?;
    pages.sort();
    let mut out: BTreeSet<Dangling> = BTreeSet::new();
    for page in &pages {
        let full = workspace.join(page);
        let Ok(meta) = std::fs::metadata(&full) else {
            continue;
        };
        if meta.len() > MAX_PAGE_BYTES {
            continue;
        }
        let Ok(html) = std::fs::read_to_string(&full) else {
            continue;
        };
        for raw in local_refs(&html) {
            let Some(rel) = resolve(page, &raw) else {
                continue;
            };
            if workspace.join(&rel).exists() {
                continue;
            }
            out.insert(Dangling {
                from: page.clone(),
                raw,
                resolved: rel,
            });
        }
    }
    Ok(out.into_iter().collect())
}

fn walk(
    root: &Path,
    dir: &Path,
    pages: &mut Vec<String>,
    seen: &mut usize,
) -> Result<(), ScanError> {
    let rd = std::fs::read_dir(dir).map_err(|e| ScanError::Unreadable {
        path: dir.display().to_string(),
        reason: e.to_string(),
    })?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in rd.flatten() {
        *seen += 1;
        if *seen > MAX_FILES {
            return Err(ScanError::TooManyFiles { seen: MAX_FILES });
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            dirs.push(path);
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".html") || lower.ends_with(".htm") {
            if let Some(rel) = rel_of(root, &path) {
                pages.push(rel);
            }
        }
    }
    for d in dirs {
        walk(root, &d, pages, seen)?;
    }
    Ok(())
}

/// ワークスペース相対の綴り (`/` 区切り) にする。**OS で分岐しない** —
/// `Path::strip_prefix` の結果を綴り直すだけなので Windows でも同じ形になる。
fn rel_of(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **実機で壊れていた `index.html` そのもの。**
    #[test]
    fn 実機の404を拾う() {
        let html = r#"
          <link rel="stylesheet" href="./assets/css/style.css">
          <script defer src="./assets/vendor/three/three.min.js"></script>
          <script defer src="./assets/js/scene.js"></script>
          <script defer src="./assets/js/main.js"></script>
        "#;
        let got = local_refs(html);
        assert_eq!(
            got,
            vec![
                "./assets/css/style.css".to_string(),
                "./assets/vendor/three/three.min.js".to_string(),
                "./assets/js/scene.js".to_string(),
                "./assets/js/main.js".to_string(),
            ],
        );
    }

    /// **外部と面内アンカーは対象外。** ここを拾うと、CDN を使った
    /// ページが毎回「壊れている」ことになる。
    #[test]
    fn 外部参照は見ない() {
        let html = r##"
          <script src="https://cdn.example.com/three.min.js"></script>
          <script src="http://example.com/a.js"></script>
          <script src="//cdn.example.com/b.js"></script>
          <img src="data:image/png;base64,AAAA">
          <a href="#top">上へ</a>
          <a href="mailto:a@example.com">mail</a>
          <a href="tel:0000">tel</a>
        "##;
        assert!(local_refs(html).is_empty(), "外部を拾った: {:?}", local_refs(html));
    }

    /// 問い合わせと断片は落とす (`style.css?v=3` は `style.css`)。
    #[test]
    fn 問い合わせと断片を落とす() {
        let html = r#"<link href="./a.css?v=3"><script src="b.js#x"></script>"#;
        assert_eq!(local_refs(html), vec!["./a.css".to_string(), "b.js".to_string()]);
    }

    /// 相対・絶対・`..` を、根からの 1 つの綴りへ畳む。
    #[test]
    fn 参照を根からの綴りへ畳む() {
        for (page, r, want) in [
            ("index.html", "./assets/js/main.js", Some("assets/js/main.js")),
            ("index.html", "/assets/js/main.js", Some("assets/js/main.js")),
            ("docs/a.html", "../assets/x.css", Some("assets/x.css")),
            ("docs/a.html", "b.css", Some("docs/b.css")),
            ("docs/deep/a.html", "../../top.js", Some("top.js")),
            // 根より上は「このワークスペースの外」。
            ("index.html", "../outside.js", None),
        ] {
            assert_eq!(resolve(page, r).as_deref(), want, "{page} → {r}");
        }
    }

    /// **単引用符でも拾う。** 片方だけ見ていると、そちらで書いた
    /// ページが素通りする。
    #[test]
    fn 単引用符でも拾う() {
        let html = "<script src='./a.js'></script>";
        assert_eq!(local_refs(html), vec!["./a.js".to_string()]);
    }

    /// 実ファイルで往復する。**在るものは挙げない・無いものは挙げる。**
    #[test]
    fn 実ファイルで在る無しを見分ける() {
        let dir = crate::test_util::unique_temp_dir("zaivern", "webcheck");
        std::fs::create_dir_all(dir.join("assets/js")).unwrap();
        std::fs::write(dir.join("assets/js/scene.js"), "// ok\n").unwrap();
        std::fs::write(dir.join("assets/css.skip"), "").unwrap();
        std::fs::write(
            dir.join("index.html"),
            "<script src=\"./assets/js/scene.js\"></script>\
             <script src=\"./assets/js/main.js\"></script>\
             <script src=\"https://cdn.example.com/x.js\"></script>",
        )
        .unwrap();

        let got = scan(&dir).expect("走査できる");
        assert_eq!(got.len(), 1, "拾い方が違う: {got:?}");
        assert_eq!(got[0].resolved, "assets/js/main.js");
        assert_eq!(got[0].from, "index.html");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **HTML が 1 枚も無ければ「問題なし」。** ここで何か言うと、
    /// Web ではないリポジトリで毎回警告が出る。
    #[test]
    fn htmlが無ければ何も言わない() {
        let dir = crate::test_util::unique_temp_dir("zaivern", "webcheck-empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        assert!(scan(&dir).expect("走査できる").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 走査に入らない場所は入らない (`node_modules` の中の HTML で
    /// 埋もれさせない)。
    #[test]
    fn 除外ディレクトリへは入らない() {
        let dir = crate::test_util::unique_temp_dir("zaivern", "webcheck-skip");
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(
            dir.join("node_modules/pkg/demo.html"),
            "<script src=\"./nope.js\"></script>",
        )
        .unwrap();
        assert!(scan(&dir).expect("走査できる").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}

// ── ブラウザで開いて、コンソールを見る ─────────────────────────────────
//
// 「コンソールにエラーが出ない」は完了条件に**書かれるだけ**で、誰も
// 測っていなかった。Chrome があれば headless で開いて `ERROR:CONSOLE` を
// 数える。無ければ**未確認**と言う ([skip] は緑ではない)。

/// コンソール検査の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConsoleVerdict {
    /// 測れなかった (Chrome が無い・起動できない・時間切れ)。理由つき。
    Skipped(String),
    /// エラー 0 件。
    Clean,
    /// エラーの本文 (重複は畳む)。
    Errors(Vec<String>),
}

/// Chrome を待つ上限。**固定の待ちは遅い環境で誤って殺す**が、ここは
/// 1 枚の静的ページを開くだけなので、超えたら固まっている。
const CHROME_WAIT: std::time::Duration = std::time::Duration::from_secs(25);

/// ページが仮想時間で進む量 (ミリ秒)。アニメーションの `requestAnimationFrame`
/// もこの分だけ進んでから DOM を吐く。
const VIRTUAL_TIME_BUDGET_MS: u32 = 4000;

/// 環境変数で実体を指定できる (`ZAIVERN_CHROME=/path/to/chrome`)。
pub const CHROME_ENV: &str = "ZAIVERN_CHROME";

/// Chrome / Chromium の実体を探す。**ハードコードしない** — OS ごとの
/// 標準の置き場と PATH を順に見る。
pub fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(CHROME_ENV) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        for app in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        ] {
            candidates.push(Path::new("/Applications").join(app));
            if let Some(home) = dirs::home_dir() {
                candidates.push(home.join("Applications").join(app));
            }
        }
    }
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                candidates.push(
                    Path::new(&base)
                        .join("Google")
                        .join("Chrome")
                        .join("Application")
                        .join("chrome.exe"),
                );
            }
        }
    }
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    // PATH (Linux の既定・どの OS でも手で通した場合)。
    let names: &[&str] = if cfg!(windows) {
        &["chrome.exe", "chromium.exe"]
    } else {
        &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "chrome",
        ]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Chrome の stderr から**コンソールのエラー**だけを抜く (純関数)。
///
/// 行の形 (Chrome 151 / macOS で実測。版によって `CONSOLE(12)` と
/// `CONSOLE:12` の両方がある):
/// `[1234:5678:0903/120000.000000:INFO:CONSOLE:1] "Uncaught Error: boom", source: file:///…/m.html (1)`
///
/// **水準 (INFO / ERROR) では選べない** — 実測では `console.log` も
/// 捕まえられなかった例外も同じ `INFO:CONSOLE` で出た。だから本文で選ぶ:
/// `Uncaught …` (例外) と `net::ERR_` / `Failed to load resource` (読み込み
/// 失敗)、それに水準が `ERROR` のもの。`console.log` の文言は拾わない
/// (拾うと正常なページが「エラーあり」になる)。同じ本文は 1 つに畳む。
pub fn parse_console_errors(stderr: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in stderr.lines() {
        let Some(tag) = line.find(":CONSOLE") else {
            continue;
        };
        let level_is_error = line[..tag].ends_with(":ERROR");
        let Some(start) = line.find("] \"") else {
            continue;
        };
        let body = &line[start + 3..];
        let end = body.rfind("\", source:").unwrap_or(body.len());
        let msg = body[..end].trim_end_matches('"').trim();
        if msg.is_empty() {
            continue;
        }
        let looks_like_error = level_is_error
            || msg.starts_with("Uncaught ")
            || msg.contains("net::ERR_")
            || msg.contains("Failed to load resource");
        if !looks_like_error {
            continue;
        }
        let msg = msg.to_string();
        if !out.contains(&msg) {
            out.push(msg);
        }
    }
    out
}

/// `page` (ワークスペース相対) を headless Chrome で開き、コンソールの
/// エラーを数える。
pub fn console_errors(workspace: &Path, page: &str) -> ConsoleVerdict {
    let Some(chrome) = find_chrome() else {
        return ConsoleVerdict::Skipped(format!(
            "Chrome / Chromium が見つかりません ({CHROME_ENV} で実体を指定できます)"
        ));
    };
    let full = workspace.join(page);
    if !full.is_file() {
        return ConsoleVerdict::Skipped(format!("{page} がありません"));
    }
    let url = match file_url(&full) {
        Some(u) => u,
        None => {
            return ConsoleVerdict::Skipped(format!("{} を URL にできません", full.display()))
        }
    };
    let profile = scratch_profile("webcheck");
    let out = run_chrome(
        &chrome,
        &[
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--allow-file-access-from-files",
            "--enable-logging=stderr",
            "--v=0",
            &format!("--user-data-dir={}", profile.display()),
            &format!("--virtual-time-budget={VIRTUAL_TIME_BUDGET_MS}"),
            "--dump-dom",
            &url,
        ],
        // **DOM が出たら終わり。** Chrome は dump のあと自分では終わらない
        // (実測: macOS / Chrome 151 で 20 秒待っても生きていた)。
        |stdout, _| stdout.contains("</html>"),
    );
    let _ = std::fs::remove_dir_all(&profile);
    match out {
        Err(why) => ConsoleVerdict::Skipped(why),
        Ok(stderr) => {
            let errs = parse_console_errors(&stderr);
            if errs.is_empty() {
                ConsoleVerdict::Clean
            } else {
                ConsoleVerdict::Errors(errs)
            }
        }
    }
}

/// headless Chrome が `--window-size` で縮められる幅の下限 (実測: macOS /
/// Chrome 151 で 375 を指定しても約 500 で描き、PNG だけが 375 に切られる)。
///
/// これより狭い幅は [`iframe_wrapper`] 越しに描く。**この下限を知らずに
/// 撮った 375px の画像を「右端が切れている」と読み、実際には崩れていない
/// ページを 3 回続けて崩れていると言った** — 道具の嘘は判断を丸ごと
/// 誤らせる。
pub const CHROME_MIN_WINDOW_WIDTH: u32 = 500;

/// 狭い幅で描くための包み紙 (純関数)。`iframe` は指定した幅でレイアウト
/// するので、外の窓が広くても中身は `width` px のページになる。
pub fn iframe_wrapper(page_url: &str, width: u32, height: u32) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head>\
         <body style=\"margin:0;background:#fff\">\
         <iframe src=\"{page_url}\" style=\"border:0;display:block;width:{width}px;height:{height}px\"></iframe>\
         </body></html>"
    )
}

/// `page` を headless Chrome で描いて PNG に落とす (幅 × 高さ)。
///
/// 品質の判定そのものはしない — **人 (や別のエージェント) が目で見る**
/// ための材料を作る。375px と 1280px の 2 枚を並べれば、崩れは 5 秒で分かる。
/// [`CHROME_MIN_WINDOW_WIDTH`] より狭い幅は iframe で描いて**その幅に切り出す**。
pub fn screenshot(
    workspace: &Path,
    page: &str,
    width: u32,
    height: u32,
    out: &Path,
) -> Result<(), String> {
    let chrome = find_chrome().ok_or_else(|| {
        format!("Chrome / Chromium が見つかりません ({CHROME_ENV} で実体を指定できます)")
    })?;
    let full = workspace.join(page);
    let url = file_url(&full).ok_or_else(|| format!("{} を URL にできません", full.display()))?;
    let _ = std::fs::remove_file(out);
    let profile = scratch_profile(&format!("webshot-{width}x{height}"));
    let narrow = width < CHROME_MIN_WINDOW_WIDTH;
    // 狭い幅は包み紙を経由する。包み紙はプロファイルの隣に置く
    // (終わったら一緒に消える)。
    let (target_url, window_w) = if narrow {
        let wrap = profile.with_extension("wrap.html");
        std::fs::write(&wrap, iframe_wrapper(&url, width, height))
            .map_err(|e| format!("{} を書けません: {e}", wrap.display()))?;
        let wrap_url = file_url(&wrap).ok_or("包み紙を URL にできません")?;
        (wrap_url, CHROME_MIN_WINDOW_WIDTH.max(width + 16))
    } else {
        (url, width)
    };
    let target = out.to_path_buf();
    let r = run_chrome(
        &chrome,
        &[
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--allow-file-access-from-files",
            "--hide-scrollbars",
            &format!("--user-data-dir={}", profile.display()),
            &format!("--window-size={window_w},{height}"),
            &format!("--virtual-time-budget={VIRTUAL_TIME_BUDGET_MS}"),
            &format!("--screenshot={}", out.display()),
            &target_url,
        ],
        // PNG が書かれたら終わり。
        move |_, _| std::fs::metadata(&target).map(|m| m.len() > 0).unwrap_or(false),
    );
    let _ = std::fs::remove_file(profile.with_extension("wrap.html"));
    let _ = std::fs::remove_dir_all(&profile);
    r.map(|_| ())?;
    if !out.is_file() {
        return Err(format!("{} が書かれませんでした", out.display()));
    }
    if narrow {
        // 包み紙の余白を落として、頼まれた幅×高さだけを残す。
        let img = image::open(out).map_err(|e| format!("{} を読めません: {e}", out.display()))?;
        let w = width.min(img.width());
        let h = height.min(img.height());
        img.crop_imm(0, 0, w, h)
            .save(out)
            .map_err(|e| format!("{} を書けません: {e}", out.display()))?;
    }
    Ok(())
}

/// 一時プロファイルの置き場 (利用者の Chrome のプロファイルには触らない)。
fn scratch_profile(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "zaivern-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

/// 絶対パスを `file://` URL にする。Windows のドライブ文字は `file:///C:/…`。
fn file_url(p: &Path) -> Option<String> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    let s = abs.to_string_lossy().replace('\\', "/");
    let s = s.trim_start_matches('/');
    Some(format!("file:///{s}"))
}

/// Chrome を起こし、`done(stdout, stderr)` が真になるか、自分で終わるか、
/// 上限に当たるまで待ち、stderr を返す。**終わりは自分で作る** — headless Chrome は
/// `--dump-dom` / `--screenshot` を書いたあと自分では終わらないことがある
/// (実測: macOS / Chrome 151。4 通りの旗で 20 秒待っても生きていた)。
///
/// 落とすときは**プロセス群ごと** ([`crate::procx::kill_tree`] は pgid ==
/// pid を前提にするので、unix では自分の群で起こす)。親だけ殺すと
/// helper が stderr を握ったまま残り、読み取りスレッドが永久に返らない
/// (実際にテストが 10 分固まった)。
fn run_chrome(
    chrome: &Path,
    args: &[&str],
    done: impl Fn(&str, &str) -> bool,
) -> Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    let mut cmd = Command::new(chrome);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{} を起動できません: {e}", chrome.display()))?;
    let out_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let err_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let pump = |pipe: Option<Box<dyn Read + Send>>, buf: Arc<Mutex<String>>| {
        std::thread::spawn(move || {
            let Some(mut p) = pipe else { return };
            let mut chunk = [0u8; 4096];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&chunk[..n]).to_string();
                        if let Ok(mut b) = buf.lock() {
                            b.push_str(&s);
                        }
                    }
                }
            }
        })
    };
    let t_out = pump(
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        out_buf.clone(),
    );
    let t_err = pump(
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        err_buf.clone(),
    );
    let snapshot = |b: &Arc<Mutex<String>>| b.lock().map(|s| s.clone()).unwrap_or_default();
    let started = std::time::Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(format!("Chrome の終了を待てません: {e}")),
        }
        if done(&snapshot(&out_buf), &snapshot(&err_buf)) {
            // 遅れて出るコンソール行を少しだけ待ってから落とす。
            std::thread::sleep(std::time::Duration::from_millis(700));
            crate::procx::kill_tree(child.id());
            let _ = child.wait();
            break;
        }
        if started.elapsed() > CHROME_WAIT {
            timed_out = true;
            crate::procx::kill_tree(child.id());
            let _ = child.wait();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // 群ごと落としたのでパイプは閉じ、読み取りは必ず返る。
    let _ = t_out.join();
    let _ = t_err.join();
    if timed_out {
        return Err(format!(
            "Chrome が {} 秒以内に描き終えませんでした",
            CHROME_WAIT.as_secs()
        ));
    }
    // 返すのは stderr (コンソール行の置き場)。stdout の DOM は `done` の
    // 判定にだけ使う。
    Ok(snapshot(&err_buf))
}

/// 検査の入口になるページ。根の `index.html` があればそれだけ、無ければ
/// 走査で見つかった HTML 全部 (順序は走査どおり)。
pub fn entry_pages(workspace: &Path) -> Vec<String> {
    if workspace.join("index.html").is_file() {
        return vec!["index.html".to_string()];
    }
    let mut pages = Vec::new();
    let mut seen = 0usize;
    if walk(workspace, workspace, &mut pages, &mut seen).is_ok() {
        pages.sort();
    }
    pages
}

/// Web の成果物に触るタスクか (完了時に読み込み検査を掛ける相手)。
pub fn is_web_path(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.ends_with(".html") || l.ends_with(".htm") || l.ends_with(".css") || l.ends_with(".js")
}

#[cfg(test)]
mod console_tests {
    use super::*;

    /// **Chrome の logging の行そのもの。** INFO は拾わず、ERROR だけ、
    /// 同じ本文は 1 つに畳む。
    #[test]
    fn コンソールのエラーだけを抜く() {
        // 実測の行 (Chrome 151 / macOS): 例外も `INFO:CONSOLE` で出る。
        let stderr = "\
[123:456:0903/120000.000000:INFO:CONSOLE:1] \"fine\", source: file:///a/main.js (1)
[123:456:0903/120000.000001:INFO:CONSOLE:1] \"Uncaught ReferenceError: THREE is not defined\", source: file:///a/scene.js (1)
[123:456:0903/120000.000002:WARNING:CONSOLE(1)] \"deprecated\", source: file:///a/x.js (1)
[123:456:0903/120000.000003:INFO:CONSOLE:1] \"Uncaught ReferenceError: THREE is not defined\", source: file:///a/scene.js (1)
[123:456:0903/120000.000004:ERROR:CONSOLE(0)] \"Failed to load resource: net::ERR_FILE_NOT_FOUND\", source: file:///a/assets/js/main.js (0)
[123:456:0903/120000.000005:ERROR:CONSOLE(7)] \"something the page console.error()ed\", source: file:///a/x.js (7)
[123:456:0903/120000.000006:ERROR:ui/display/mac/cv_display_link_mac.mm:195] CVDisplayLinkCreateWithCGDisplay failed.
";
        assert_eq!(
            parse_console_errors(stderr),
            vec![
                "Uncaught ReferenceError: THREE is not defined".to_string(),
                "Failed to load resource: net::ERR_FILE_NOT_FOUND".to_string(),
                "something the page console.error()ed".to_string(),
            ]
        );
        assert!(parse_console_errors("").is_empty());
    }

    #[test]
    fn webの成果物の見分け() {
        for p in ["index.html", "a/B.HTM", "assets/css/style.css", "assets/js/main.js"] {
            assert!(is_web_path(p), "{p}");
        }
        for p in ["src/main.rs", "docs/PLAN.md", "package.json", ""] {
            assert!(!is_web_path(p), "{p}");
        }
    }

    /// `file://` URL の形。Windows のドライブ文字も 3 本スラッシュ。
    #[test]
    fn ファイルurlの形() {
        let u = file_url(Path::new("/tmp/a b/index.html")).unwrap();
        assert!(u.starts_with("file:///"), "{u}");
        assert!(u.ends_with("index.html"), "{u}");
        assert!(!u.contains("////"), "{u}");
    }

    /// 包み紙は指定した幅の iframe を 1 枚だけ持つ。
    #[test]
    fn 狭い幅の包み紙はその幅のiframeを持つ() {
        let w = iframe_wrapper("file:///a/index.html", 375, 812);
        assert!(w.contains("width:375px"));
        assert!(w.contains("height:812px"));
        assert!(w.contains("src=\"file:///a/index.html\""));
        assert_eq!(w.matches("<iframe").count(), 1);
    }

    /// **狭い幅の PNG は頼んだ幅×高さそのもの。** Chrome が無ければ [skip]。
    #[test]
    fn 狭い幅でも頼んだ大きさのpngになる() {
        if find_chrome().is_none() {
            eprintln!("[skip] Chrome / Chromium が無いので撮れない");
            return;
        }
        let dir = crate::test_util::unique_temp_dir("zaivern", "webshot-narrow");
        std::fs::write(
            dir.join("index.html"),
            "<!doctype html><html><body style=\"margin:0\"><div style=\"width:100vw;height:40px;background:#f00\"></div></body></html>",
        )
        .unwrap();
        let out = dir.join("shot.png");
        screenshot(&dir, "index.html", 375, 812, &out).expect("撮れる");
        let img = image::open(&out).expect("PNG");
        assert_eq!((img.width(), img.height()), (375, 812), "包み紙の余白が残っている");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 実際に Chrome で開く。**Chrome が無い環境では [skip] を出して降りる**
    /// — 緑と言わない。
    #[test]
    fn 実際に開いてエラーを数える() {
        if find_chrome().is_none() {
            eprintln!("[skip] Chrome / Chromium が無いのでコンソール検査を確かめられない");
            return;
        }
        let dir = crate::test_util::unique_temp_dir("zaivern", "webcheck-console");
        std::fs::write(
            dir.join("index.html"),
            "<!doctype html><html><body><h1>ok</h1><script>console.log('fine')</script></body></html>",
        )
        .unwrap();
        assert_eq!(console_errors(&dir, "index.html"), ConsoleVerdict::Clean, "正常なページで赤");

        std::fs::write(
            dir.join("index.html"),
            "<!doctype html><html><body><script src=\"./missing.js\"></script><script>undefinedFn()</script></body></html>",
        )
        .unwrap();
        match console_errors(&dir, "index.html") {
            ConsoleVerdict::Errors(e) => {
                assert!(
                    e.iter().any(|m| m.contains("undefinedFn")),
                    "投げた例外を拾えていない: {e:?}"
                );
            }
            other => panic!("壊れたページで {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
