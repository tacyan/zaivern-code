//! Language Pack — `locales/<id>.json` による UI 多言語化。
//!
//! 辞書は **安定 ID をキーにした平の JSON** (`"app.settings": "Settings"`)。
//! 同梱ぶんは `locales/` をビルド時に埋め込む (`include_str!`) ので、
//! **どの OS のどのインストール先でも追加ファイル無しで全言語が使える**。
//!
//! ```json
//! // locales/en.json
//! { "app.new_session": "New Session", "agent.approve_all": "Approve All" }
//! ```
//!
//! ## 解決の順番
//! 1. 選択中の言語 (`config.toml` の `ui_language`。既定 `"auto"`)
//! 2. その言語の**フォールバック**   (`zh-TW` → `zh-CN`、`pt-PT` → `pt-BR`)
//! 3. 基準言語 `en`
//! 4. それでも無ければ **呼び出し側が渡した文字列そのもの** (= 日本語原文)
//!
//! 訳が 1 つ欠けても UI は必ず何かを表示する。壊れない。
//!
//! ## 既存の日本語原文キーとの橋渡し
//! このリポジトリの `tr()` 呼び出しは **日本語の原文そのもの**を渡している
//! (3000 箇所以上)。`locales/ja.json` は `ID → 日本語原文` なので、その逆引き
//! (`日本語原文 → ID`) を作れば、**呼び出し側を 1 行も書き換えずに**全部の
//! 文字列が 6 言語になる。新しいコードは ID (`tr("agent.approve")`) を使う。
//! 逆引きの構築は [`crate::i18n::set_locale`]。
//!
//! ## ユーザー／コミュニティが言語を足す
//! - `~/.zaivern/locales/fr.json`
//! - `~/.config/zaivern/locales/fr.json` (XDG。macOS/Windows でも `dirs` が返す場所)
//! - プラグイン (`plugin.toml` の `[language] locales = "locales"`)
//!
//! 置くだけで言語選択に並ぶ。同梱言語と同じ ID を書けば**同梱ぶんを上書き**
//! できる (訳の直しを再ビルド無しで試せる)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

/// 基準言語。どの言語でも、訳が欠けたらまずここへ落ちる。
pub const BASE: &str = "en";

/// 同梱言語。**この並びがそのまま UI の並び順**（利用者指定の優先順位）。
pub const BUILTIN: &[(&str, &str, &str)] = &[
    ("en", "English", include_str!("../locales/en.json")),
    ("ja", "日本語", include_str!("../locales/ja.json")),
    ("zh-CN", "简体中文", include_str!("../locales/zh-CN.json")),
    ("ko", "한국어", include_str!("../locales/ko.json")),
    ("pt-BR", "Português (Brasil)", include_str!("../locales/pt-BR.json")),
    ("es", "Español", include_str!("../locales/es.json")),
];

/// 「自動」を表す設定値。
pub const AUTO: &str = "auto";

/// 同梱言語の生 JSON。
pub fn builtin_json(id: &str) -> Option<&'static str> {
    BUILTIN.iter().find(|(i, _, _)| *i == id).map(|(_, _, j)| *j)
}

/// 言語の表示名。同梱に無ければ ID をそのまま返す（コミュニティ言語）。
pub fn display_name(id: &str) -> String {
    if let Some((_, n, _)) = BUILTIN.iter().find(|(i, _, _)| *i == id) {
        return (*n).to_string();
    }
    // 同梱外でも主要言語は母語名を出す（`fr.json` を置いた人が「fr」ではなく
    // 「Français」と見えるように）。ここに無い言語は ID をそのまま出す。
    match id {
        "ar" => "العربية",
        "de" => "Deutsch",
        "fr" => "Français",
        "hi" => "हिन्दी",
        "id" => "Bahasa Indonesia",
        "it" => "Italiano",
        "nl" => "Nederlands",
        "pl" => "Polski",
        "pt-PT" => "Português (Portugal)",
        "ru" => "Русский",
        "th" => "ไทย",
        "tr" => "Türkçe",
        "uk" => "Українська",
        "vi" => "Tiếng Việt",
        "zh-TW" => "繁體中文",
        _ => id,
    }
    .to_string()
}

/// OS / ファイル名 / 設定値から来る言語タグを正規化する。
///
/// `ja_JP.UTF-8` → `ja` / `zh_CN` → `zh-CN` / `zh-Hans` → `zh-CN` /
/// `pt` → `pt-BR` / `en-US` → `en`。**未知の言語は主タグをそのまま返す**
/// (`fr_FR` → `fr`)。コミュニティが置いた `fr.json` が拾えるようにするため。
pub fn normalize(tag: &str) -> String {
    // "ja_JP.UTF-8@collation" → "ja_JP"
    let head = tag.split(['.', '@']).next().unwrap_or("").trim();
    let mut parts = head.split(['-', '_']).filter(|s| !s.is_empty());
    let lang = parts.next().unwrap_or("").to_ascii_lowercase();
    let mut script = String::new();
    let mut region = String::new();
    for p in parts {
        if p.len() == 4 && p.chars().all(|c| c.is_ascii_alphabetic()) {
            script = titlecase(p);
        } else if p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()) {
            region = p.to_ascii_uppercase();
        } else if p.len() == 3 && p.chars().all(|c| c.is_ascii_digit()) {
            region = p.to_string();
        }
    }
    match lang.as_str() {
        // 中国語だけは簡体/繁体の区別が本質。script > region の順に見る。
        "zh" => {
            let hant = script == "Hant"
                || (script.is_empty() && matches!(region.as_str(), "TW" | "HK" | "MO"));
            if hant { "zh-TW".into() } else { "zh-CN".into() }
        }
        // ポルトガル語は既定をブラジルにする (同梱が pt-BR のため)。
        "pt" => {
            if region == "PT" { "pt-PT".into() } else { "pt-BR".into() }
        }
        "" => String::new(),
        _ => lang,
    }
}

fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase(),
        None => String::new(),
    }
}

/// `id` の訳が欠けたときに辿る順番 (自分自身を含み、最後は必ず [`BASE`])。
pub fn fallback_chain(id: &str) -> Vec<String> {
    let mut out = vec![id.to_string()];
    // 地域変種 → 同語の同梱変種
    match id {
        "zh-TW" => out.push("zh-CN".into()),
        "pt-PT" => out.push("pt-BR".into()),
        _ => {}
    }
    if !out.iter().any(|x| x == BASE) {
        out.push(BASE.to_string());
    }
    out
}

// ─── 置き場 ──────────────────────────────────────────────────────

/// ユーザーが言語ファイルを置ける場所。**先に来たものが勝つ**
/// (`~/.zaivern/locales` が最優先)。存在しないディレクトリも返す —
/// 「どこへ置けばいいか」を UI から案内するため。
pub fn user_dirs() -> Vec<PathBuf> {
    let mut out = vec![crate::config::zaivern_dir().join("locales")];
    if let Some(c) = dirs::config_dir() {
        let p = c.join("zaivern").join("locales");
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// 選べる言語 1 件。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Info {
    pub id: String,
    pub name: String,
    /// バイナリ同梱か。false = ディスク上のファイルだけで存在する。
    pub builtin: bool,
    /// ディスク上のファイル (上書き or 追加)。同梱のみなら None。
    pub path: Option<PathBuf>,
}

/// ディレクトリ直下の `<id>.json` を集める。読めないディレクトリは黙って飛ばす
/// (置いていない人に毎回エラーを出さない)。
fn scan_dir(dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?;
            let id = normalize(stem);
            if id.is_empty() {
                return None;
            }
            Some((id, p))
        })
        .collect();
    found.sort();
    out.extend(found);
}

/// 選べる言語の一覧。**同梱の並び (優先順位) が先**、その後にコミュニティ言語を
/// ID 順で並べる。同じ ID が複数あっても 1 行にまとめる。
pub fn available(extra: &[PathBuf]) -> Vec<Info> {
    let mut disk: Vec<(String, PathBuf)> = Vec::new();
    for d in user_dirs() {
        scan_dir(&d, &mut disk);
    }
    for d in extra {
        scan_dir(d, &mut disk);
    }

    let mut out: Vec<Info> = BUILTIN
        .iter()
        .map(|(id, name, _)| Info {
            id: (*id).to_string(),
            name: (*name).to_string(),
            builtin: true,
            path: disk.iter().find(|(i, _)| i == id).map(|(_, p)| p.clone()),
        })
        .collect();

    let mut extras: Vec<String> = disk
        .iter()
        .map(|(i, _)| i.clone())
        .filter(|i| !BUILTIN.iter().any(|(b, _, _)| b == i))
        .collect();
    extras.sort();
    extras.dedup();
    for id in extras {
        let path = disk.iter().find(|(i, _)| *i == id).map(|(_, p)| p.clone());
        out.push(Info {
            name: display_name(&id),
            id,
            builtin: false,
            path,
        });
    }
    out
}

// ─── 読み込み ────────────────────────────────────────────────────

/// 平の JSON (`{"id": "訳文"}`) を読む。文字列以外の値は**黙って捨てず**
/// エラーにする (書き間違いに気付けるように)。
pub fn parse_json(raw: &str, whence: &str) -> Result<HashMap<String, String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("{whence}: JSON の解析に失敗: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| format!("{whence}: 最上位が {{ }} のオブジェクトではありません"))?;
    let mut out = HashMap::with_capacity(obj.len());
    for (k, val) in obj {
        match val.as_str() {
            Some(s) => {
                out.insert(k.clone(), s.to_string());
            }
            None => {
                // 翻訳者への注記などを `_comment` で書けるようにする。
                if k.starts_with('_') {
                    continue;
                }
                return Err(format!("{whence}: キー {k:?} の値が文字列ではありません"));
            }
        }
    }
    Ok(out)
}

/// 1 言語ぶんの辞書を組み立てる (同梱 → ディスクの順に後勝ちで重ねる)。
/// **フォールバックは含まない** — 重ねるのは [`resolved`] の仕事。
pub fn load_one(id: &str, extra: &[PathBuf], errs: &mut Vec<String>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(raw) = builtin_json(id) {
        match parse_json(raw, &format!("同梱 {id}.json")) {
            Ok(m) => map = m,
            // 同梱が壊れているのはビルドの事故。テストが番人なので実行時は記録だけ。
            Err(e) => errs.push(e),
        }
    }
    let mut dirs = user_dirs();
    dirs.extend_from_slice(extra);
    // 後勝ちにするため優先度の低い方から重ねる
    for d in dirs.iter().rev() {
        let p = d.join(format!("{id}.json"));
        if !p.is_file() {
            continue;
        }
        match std::fs::read_to_string(&p) {
            Ok(raw) => match parse_json(&raw, &p.display().to_string()) {
                Ok(m) => map.extend(m),
                Err(e) => errs.push(e),
            },
            Err(e) => errs.push(format!("{} を読めません: {e}", p.display())),
        }
    }
    map
}

/// 表示に使う最終辞書。`id` → フォールバック → [`BASE`] の順に重ねる
/// (**先に見た方が勝つ**ので、逆順に extend する)。
pub fn resolved(id: &str, extra: &[PathBuf], errs: &mut Vec<String>) -> HashMap<String, String> {
    let chain = fallback_chain(id);
    let mut out: HashMap<String, String> = HashMap::new();
    for step in chain.iter().rev() {
        out.extend(load_one(step, extra, errs));
    }
    out
}

// ─── 翻訳ファイルの検査 ──────────────────────────────────────────

/// 基準辞書と 1 枚の翻訳を突き合わせた結果。
///
/// **「確かめられなかった」を成功にしない**ため、`zai i18n check` は
/// [`Report::is_clean`] が false なら終了コード 1 で降りる。
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Report {
    /// 基準にあるのに訳が無い鍵。
    pub missing: Vec<String>,
    /// 基準に無い鍵 (綴り間違い / 古い鍵)。
    pub extra: Vec<String>,
    /// `{name}` の集合が基準と食い違う鍵。**実行時に穴が開く**ので必ず直す。
    pub placeholder: Vec<String>,
    /// 値が空白だけの鍵。
    pub empty: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty()
            && self.extra.is_empty()
            && self.placeholder.is_empty()
            && self.empty.is_empty()
    }
}

/// `{name}` 形式のプレースホルダ名を集める。名前は ASCII 英数字と `_` のみ
/// ([`crate::i18n::trf`] が置換する形式と同じ)。
pub fn placeholders(s: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for part in s.split('{').skip(1) {
        if let Some((name, _)) = part.split_once('}') {
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.insert(name.to_string());
            }
        }
    }
    out
}

/// 基準 `base` に対して `other` を検査する**純関数** (I/O 無し = 表で固定できる)。
pub fn compare(base: &HashMap<String, String>, other: &HashMap<String, String>) -> Report {
    let mut r = Report::default();
    for (k, v) in base {
        match other.get(k) {
            None => r.missing.push(k.clone()),
            Some(t) => {
                if t.trim().is_empty() {
                    r.empty.push(k.clone());
                } else if placeholders(v) != placeholders(t) {
                    r.placeholder.push(k.clone());
                }
            }
        }
    }
    for k in other.keys() {
        if !base.contains_key(k) {
            r.extra.push(k.clone());
        }
    }
    r.missing.sort();
    r.extra.sort();
    r.placeholder.sort();
    r.empty.sort();
    r
}

// ─── ソースの走査 (保守用) ───────────────────────────────────────
//
// `zai i18n missing` / 番人テストの土台。**同じ実装を 2 つ持たない**ため、
// 走査とエスケープ解除はここ 1 か所に置く (道具と番人がずれると、番人が
// 通っているのに画面に日本語が残る、という嘘が出る)。

/// Rust の文字列リテラル表記を実行時の値へ戻す。
///
/// 辞書は**実行時の値**で引くので、`\n` や `\u{1F310}` を戻さないと
/// 一生一致しない。未知のエスケープは次の 1 文字をそのまま採る (落とさない)。
pub fn unescape_rust(raw: &str) -> String {
    let mut out = String::new();
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('u') => {
                if it.peek() == Some(&'{') {
                    it.next();
                    let mut hex = String::new();
                    for h in it.by_ref() {
                        if h == '}' {
                            break;
                        }
                        hex.push(h);
                    }
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
            }
            Some('x') => {
                let hex: String = it.by_ref().take(2).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// ファイルパスから ID の名前空間 (`<module>.<action>` の左側) を決める。
pub fn module_of(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let p = path.to_string_lossy().replace('\\', "/");
    if (p.contains("/src/app/") || p.contains("/src/features/")) && stem == "mod" {
        return "app".into();
    }
    stem
}

/// `dir` 以下の `*.rs` から `tr("…")` / `trf("…", …)` の**直書き**文字列を集める。
///
/// 戻り値は (module, 実行時の文字列)。`tr(&x)` のように変数を渡している所は
/// 静的には辿れない — そちらは `ZAIVERN_I18N_TRACE=1` の実行時収集で拾う。
pub fn scan_source_literals(dir: &Path) -> Vec<(String, String)> {
    // 文字列リテラル 1 個。`\"` を含む本文を正しく food する
    let Ok(re) = regex::Regex::new(r#"\b(?:tr|trf)\(\s*"((?:[^"\\]|\\.)*)""#) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut items: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        items.sort();
        for p in items {
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&p) else {
                continue;
            };
            // Windows のチェックアウトは CRLF。正規化してから拾う
            let src = src.replace("\r\n", "\n");
            let m = module_of(&p);
            for c in re.captures_iter(&src) {
                out.push((m.clone(), unescape_rust(&c[1])));
            }
        }
    }
    out
}

/// 辞書に載らなくてよい文字列。
///
/// **テストが「訳されないこと」を確かめるための番兵だけ**を置く。
/// 画面に出る文字列をここへ足して逃げてはいけない (逃がした瞬間、その行は
/// 永久に日本語のまま残る)。
pub const NOT_TRANSLATED: &[&str] = &[
    "__zaivern_i18n_absent_probe__",
    "zz.only.here",
    "zzz.not.an.id",
];

// ─── 自動判定 ────────────────────────────────────────────────────

/// 環境変数から UI 言語を推定する純関数。テストで表に固定できるよう、
/// 環境変数の読み取りそのものを閉包で受け取る (環境変数を書き換えるテストは
/// 並列に走る他のテストへ漏れる)。
pub fn detect_from_env(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    // gettext と同じ優先順位。LANGUAGE は `ja:en` のようにコロン区切り。
    for key in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        let Some(v) = get(key) else { continue };
        let first = v.split(':').next().unwrap_or("").trim();
        if first.is_empty() || first == "C" || first == "POSIX" {
            continue;
        }
        let id = normalize(first);
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

/// OS から UI 言語を推定する。取れなければ None。
pub fn detect() -> Option<String> {
    if let Some(id) = detect_from_env(|k| std::env::var(k).ok()) {
        return Some(id);
    }
    #[cfg(windows)]
    {
        if let Some(id) = detect_windows() {
            return Some(id);
        }
    }
    None
}

/// Windows は環境変数に言語を置かない。OS の API から取る。
#[cfg(windows)]
fn detect_windows() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
    // LOCALE_NAME_MAX_LENGTH = 85 (wchar)
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 1 {
        return None;
    }
    // 戻り値は終端 NUL を含む長さ
    let s = String::from_utf16_lossy(&buf[..(n as usize - 1)]);
    let id = normalize(&s);
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// macOS は環境変数にも言語を置かない (Finder から起動すると `LANG` が無い)。
/// `defaults read -g AppleLocale` が唯一の素直な入口だが、**起動時に同期で
/// 待つわけにはいかない** ので裏のスレッドで 1 度だけ引く。
#[cfg(target_os = "macos")]
fn detect_macos() -> Option<String> {
    let out = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = normalize(String::from_utf8_lossy(&out.stdout).trim());
    (!id.is_empty()).then_some(id)
}

// ─── 判定結果の置き場 ────────────────────────────────────────────
//
// 判定は起動時に 1 度だけ。macOS だけは裏のスレッドで少し遅れて届くので、
// 届いたことを 1 bit のフラグで知らせる (毎フレームの費用は atomic 1 回)。

static DETECTED: RwLock<Option<String>> = RwLock::new(None);
static DETECT_STARTED: AtomicBool = AtomicBool::new(false);
static DETECT_UPDATED: AtomicBool = AtomicBool::new(false);

fn store_detected(id: String) {
    if let Ok(mut g) = DETECTED.write() {
        *g = Some(id);
    }
}

/// OS からの言語判定を始める。**起動時に 1 度だけ**呼ぶ。
///
/// 環境変数 (と Windows なら OS API) はその場で引く。macOS はそこで取れない
/// ことが多く、`defaults` を起こす必要があるので裏のスレッドへ回す
/// (UI スレッドで数十 ms 待たない)。
pub fn begin_detection() {
    if DETECT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(id) = detect() {
        store_detected(id);
        return;
    }
    #[cfg(target_os = "macos")]
    std::thread::spawn(|| {
        if let Some(id) = detect_macos() {
            store_detected(id);
            DETECT_UPDATED.store(true, Ordering::SeqCst);
        }
    });
}

/// いまの判定結果。まだ届いていなければ None。
pub fn detected() -> Option<String> {
    DETECTED.read().ok().and_then(|g| g.clone())
}

/// 遅れて届いた判定を 1 度だけ受け取る (受け取ったら false へ戻る)。
/// 毎フレーム呼んでよい — atomic の swap 1 回しかしない。
pub fn take_detection_update() -> bool {
    DETECT_UPDATED.swap(false, Ordering::SeqCst)
}

/// 設定値 (`auto` または言語 ID) を実際に使う言語 ID へ落とす。
///
/// * `auto` … OS の言語。**同梱にも user dir にも無ければ [`BASE`]**。
///   OS から何も取れないときだけ `fallback`（このアプリの原文言語 = `ja`）。
/// * それ以外 … 正規化してそのまま (無い言語はフォールバック連鎖が拾う)
pub fn resolve(choice: &str, detected: Option<&str>, known: &[String], fallback: &str) -> String {
    let c = choice.trim();
    if !c.is_empty() && !c.eq_ignore_ascii_case(AUTO) {
        return normalize(c);
    }
    match detected {
        None => fallback.to_string(),
        Some(d) => {
            let id = normalize(d);
            for step in fallback_chain(&id) {
                if known.iter().any(|k| *k == step) {
                    return step;
                }
            }
            BASE.to_string()
        }
    }
}

/// このアプリの**原文**の言語。`tr()` に渡っている文字列そのものの言語。
pub const SOURCE_LANG: &str = "ja";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同梱は6言語で優先順位どおりに並ぶ() {
        let ids: Vec<&str> = BUILTIN.iter().map(|(i, _, _)| *i).collect();
        assert_eq!(ids, ["en", "ja", "zh-CN", "ko", "pt-BR", "es"]);
        // 表示名は母語表記 (英語表記に戻さない)
        assert_eq!(display_name("ja"), "日本語");
        assert_eq!(display_name("zh-CN"), "简体中文");
        assert_eq!(display_name("ko"), "한국어");
    }

    #[test]
    fn 言語タグの正規化() {
        let table = [
            ("ja_JP.UTF-8", "ja"),
            ("ja", "ja"),
            ("en_US.UTF-8", "en"),
            ("en-GB", "en"),
            ("zh_CN.UTF-8", "zh-CN"),
            ("zh-Hans", "zh-CN"),
            ("zh-Hans-CN", "zh-CN"),
            ("zh_TW", "zh-TW"),
            ("zh-Hant-HK", "zh-TW"),
            ("ko_KR", "ko"),
            ("pt", "pt-BR"),
            ("pt_BR", "pt-BR"),
            ("pt-PT", "pt-PT"),
            ("es_ES@euro", "es"),
            ("es-419", "es"),
            ("fr_FR.UTF-8", "fr"),
            ("", ""),
            ("C", "c"),
        ];
        for (raw, want) in table {
            assert_eq!(normalize(raw), want, "normalize({raw:?})");
        }
    }

    #[test]
    fn フォールバック連鎖は必ず基準言語で終わる() {
        assert_eq!(fallback_chain("ja"), ["ja", "en"]);
        assert_eq!(fallback_chain("zh-TW"), ["zh-TW", "zh-CN", "en"]);
        assert_eq!(fallback_chain("pt-PT"), ["pt-PT", "pt-BR", "en"]);
        assert_eq!(fallback_chain("fr"), ["fr", "en"]);
        // 基準言語そのものは 1 段だけ (自分自身を 2 回辿らない)
        assert_eq!(fallback_chain("en"), ["en"]);
    }

    #[test]
    fn 環境変数からの判定は優先順位どおり() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(a, _)| *a == k)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        assert_eq!(
            detect_from_env(env(&[("LANG", "ja_JP.UTF-8")])),
            Some("ja".into())
        );
        // LC_ALL が LANG に勝つ
        assert_eq!(
            detect_from_env(env(&[("LANG", "ja_JP.UTF-8"), ("LC_ALL", "ko_KR.UTF-8")])),
            Some("ko".into())
        );
        // LANGUAGE はコロン区切りの先頭
        assert_eq!(
            detect_from_env(env(&[("LANGUAGE", "pt_BR:en")])),
            Some("pt-BR".into())
        );
        // C / POSIX は「言語の指定なし」として飛ばす
        assert_eq!(
            detect_from_env(env(&[("LC_ALL", "C"), ("LANG", "es_ES.UTF-8")])),
            Some("es".into())
        );
        assert_eq!(detect_from_env(env(&[("LC_ALL", "POSIX")])), None);
        assert_eq!(detect_from_env(env(&[])), None);
    }

    #[test]
    fn 自動解決は未対応言語を基準言語へ落とす() {
        let known: Vec<String> = BUILTIN.iter().map(|(i, _, _)| i.to_string()).collect();
        // 明示指定は素通し (正規化はする)
        assert_eq!(resolve("zh_CN", None, &known, "ja"), "zh-CN");
        assert_eq!(resolve("ko", Some("en"), &known, "ja"), "ko");
        // auto: 対応言語ならそれ
        assert_eq!(resolve("auto", Some("ja_JP.UTF-8"), &known, "ja"), "ja");
        assert_eq!(resolve("auto", Some("pt_BR"), &known, "ja"), "pt-BR");
        // auto: 繁体は簡体へ寄る (同梱に zh-TW が無い間)
        assert_eq!(resolve("auto", Some("zh_TW"), &known, "ja"), "zh-CN");
        // auto: 未対応言語 (ドイツ語) は英語
        assert_eq!(resolve("auto", Some("de_DE"), &known, "ja"), "en");
        // auto: OS から何も取れないときだけ原文言語のまま
        assert_eq!(resolve("auto", None, &known, "ja"), "ja");
        assert_eq!(resolve("", None, &known, "ja"), "ja");
        // ユーザーが fr.json を置いていれば auto でも拾う
        let with_fr: Vec<String> = known.iter().cloned().chain(["fr".to_string()]).collect();
        assert_eq!(resolve("auto", Some("fr_FR"), &with_fr, "ja"), "fr");
        assert_eq!(resolve("auto", Some("fr_FR"), &known, "ja"), "en");
    }

    #[test]
    fn 同梱jsonは全部読めて空でない() {
        for (id, _, raw) in BUILTIN {
            let m = parse_json(raw, id).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert!(!m.is_empty(), "{id}.json が空");
        }
    }

    // ── 同梱辞書の不変条件 (番人) ───────────────────────────────────
    //
    // ここが落ちたら直すのは実装ではなく **locales/*.json** である。
    // 生成と検査は `zai i18n missing` / `zai i18n apply` / `zai i18n check`。

    fn builtin_maps() -> Vec<(&'static str, HashMap<String, String>)> {
        BUILTIN
            .iter()
            .map(|(id, _, raw)| {
                (
                    *id,
                    parse_json(raw, id).unwrap_or_else(|e| panic!("{id}: {e}")),
                )
            })
            .collect()
    }

    /// `s` の `{name}` 形式プレースホルダ名の集合。
    fn placeholders(s: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for part in s.split('{').skip(1) {
            if let Some((name, _)) = part.split_once('}') {
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    out.insert(name.to_string());
                }
            }
        }
        out
    }

    #[test]
    fn 全同梱言語のキー集合が一致する() {
        let maps = builtin_maps();
        let base: std::collections::BTreeSet<&String> = maps[0].1.keys().collect();
        for (id, m) in &maps[1..] {
            let mine: std::collections::BTreeSet<&String> = m.keys().collect();
            let missing: Vec<&&String> = base.difference(&mine).take(10).collect();
            let extra: Vec<&&String> = mine.difference(&base).take(10).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{id}.json のキーが en.json と違う — 足りない: {missing:?} / 余分: {extra:?}"
            );
        }
    }

    #[test]
    fn 訳は空でなくプレースホルダも一致する() {
        let maps = builtin_maps();
        let ja: &HashMap<String, String> = &maps
            .iter()
            .find(|(i, _)| *i == SOURCE_LANG)
            .expect("ja がある")
            .1;
        let mut bad = Vec::new();
        let mut with_ph = 0usize;
        for (id, m) in &maps {
            for (k, v) in m {
                if v.trim().is_empty() {
                    bad.push(format!("{id}: {k:?} の訳が空"));
                    continue;
                }
                let Some(src) = ja.get(k) else { continue };
                let (ps, pv) = (placeholders(src), placeholders(v));
                if *id == SOURCE_LANG && !ps.is_empty() {
                    with_ph += 1;
                }
                if ps != pv {
                    bad.push(format!("{id}: {k:?} のプレースホルダ {ps:?} != {pv:?}"));
                }
            }
        }
        bad.sort();
        bad.truncate(20);
        assert!(bad.is_empty(), "同梱辞書の不整合:\n{}", bad.join("\n"));
        // 検査が空振りしていないこと
        assert!(with_ph > 100, "プレースホルダ付きが {with_ph} 件しかない");
    }

    #[test]
    fn ソースのtrリテラルはすべて同梱辞書から引ける() {
        let maps = builtin_maps();
        let ja = &maps
            .iter()
            .find(|(i, _)| *i == SOURCE_LANG)
            .expect("ja がある")
            .1;
        // ID で書いた呼び出しは**鍵**、日本語で書いた呼び出しは**値**で引ける
        let keys: std::collections::HashSet<&str> = ja.keys().map(|s| s.as_str()).collect();
        let values: std::collections::HashSet<&str> = ja.values().map(|s| s.as_str()).collect();

        let lits: Vec<(String, String)> =
            scan_source_literals(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
        assert!(lits.len() > 2000, "抽出が退化している ({} 件)", lits.len());

        let mut miss: Vec<String> = Vec::new();
        for (file, lit) in &lits {
            if lit.trim().is_empty() || NOT_TRANSLATED.contains(&lit.as_str()) {
                continue;
            }
            if keys.contains(lit.as_str()) || values.contains(lit.as_str()) {
                continue;
            }
            miss.push(format!("{file}: {lit:?}"));
        }
        miss.sort();
        miss.dedup();
        let shown: Vec<&String> = miss.iter().take(20).collect();
        assert!(
            miss.is_empty(),
            "{} 件の tr() が辞書に無い (画面に日本語が残る):\n{}",
            miss.len(),
            shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn 翻訳ファイルの検査は4種類の食い違いを見分ける() {
        let base: HashMap<String, String> = [
            ("a.b", "Save {n} files"),
            ("a.c", "Open"),
            ("a.d", "Close"),
            ("a.e", "Run"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let other: HashMap<String, String> = [
            ("a.b", "{m} 件を保存"), // プレースホルダ違い
            ("a.c", "開く"),
            ("a.d", "   "),  // 空
            ("a.zz", "余分"), // 基準に無い
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let r = compare(&base, &other);
        assert_eq!(r.missing, ["a.e"]);
        assert_eq!(r.extra, ["a.zz"]);
        assert_eq!(r.placeholder, ["a.b"]);
        assert_eq!(r.empty, ["a.d"]);
        assert!(!r.is_clean());
        // 完全一致なら綺麗
        assert!(compare(&base, &base).is_clean());
    }

    #[test]
    fn 同梱の全言語は基準と過不足なく噛み合う() {
        let mut errs = Vec::new();
        let base = load_one(BASE, &[], &mut errs);
        assert!(errs.is_empty(), "{errs:?}");
        for (id, _, _) in BUILTIN {
            if *id == BASE {
                continue;
            }
            let m = load_one(id, &[], &mut errs);
            let r = compare(&base, &m);
            assert!(
                r.is_clean(),
                "{id}.json: 不足 {:?} / 余分 {:?} / PH {:?} / 空 {:?}",
                &r.missing[..r.missing.len().min(5)],
                &r.extra[..r.extra.len().min(5)],
                &r.placeholder[..r.placeholder.len().min(5)],
                &r.empty[..r.empty.len().min(5)],
            );
        }
    }

    #[test]
    fn 同じ原文を持つidは訳も一致する() {
        // 呼び出し側が同じ文字列 (`tr("送信")`) を渡す以上、**実行時に区別できない**。
        // なのに辞書が別々の訳を持っていると、逆引きでどちらが勝つかで
        // 表示が変わる (実際に `agent.send`="Send" と `acp.sent`="sent" が
        // ぶつかっていた)。区別したいなら呼び出し側を ID にするのが筋。
        let maps = builtin_maps();
        let ja = &maps
            .iter()
            .find(|(i, _)| *i == SOURCE_LANG)
            .expect("ja がある")
            .1;
        let mut by_text: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (k, v) in ja {
            by_text.entry(v.as_str()).or_default().push(k.as_str());
        }
        let mut bad = Vec::new();
        for (text, ids) in by_text.iter().filter(|(_, v)| v.len() > 1) {
            for (lang, m) in &maps {
                let vals: std::collections::BTreeSet<&str> =
                    ids.iter().filter_map(|k| m.get(*k)).map(|s| s.as_str()).collect();
                if vals.len() > 1 {
                    bad.push(format!("{text:?} {ids:?} [{lang}] -> {vals:?}"));
                }
            }
        }
        bad.sort();
        bad.truncate(15);
        assert!(bad.is_empty(), "同じ原文なのに訳が違う:\n{}", bad.join("\n"));
    }

    #[test]
    fn 利用者が示した正準idは必ずある() {
        // README / 設計の説明に出す ID。名前を変えると外部の説明が嘘になる。
        const CANON: &[&str] = &[
            "app.new_session",
            "app.settings",
            "agent.send",
            "agent.stop",
            "agent.approve",
            "agent.approve_all",
            "agent.broadcast",
        ];
        for (id, m) in builtin_maps() {
            for k in CANON {
                assert!(m.contains_key(*k), "{id}.json に {k} が無い");
            }
        }
    }

    #[test]
    fn 数値やアンダースコア始まりの扱い() {
        // `_` 始まりは翻訳者向けメモとして無視する
        let m = parse_json(r#"{"_note": {"a": 1}, "x.y": "v"}"#, "t").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m["x.y"], "v");
        // それ以外の非文字列はエラー (書き間違いを黙って捨てない)
        assert!(parse_json(r#"{"x.y": 42}"#, "t").is_err());
        assert!(parse_json("[]", "t").is_err());
        assert!(parse_json("{", "t").is_err());
    }
}
