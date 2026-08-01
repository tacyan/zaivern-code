//! Skills / slash command の**発見・解析・一覧**。
//!
//! Claude Code は 2 種類の「持ち込みプロンプト」を同じ `.claude` の下に置く:
//!
//! - **Skill** — `<どこか>/skills/<名前>/SKILL.md`。YAML frontmatter の
//!   `description:` を手掛かりに、エージェント側が必要になった時点で読み込む。
//! - **slash command** — `<どこか>/commands/<名前>.md`。`/<名前>` と打つと
//!   その本文がプロンプトとして展開される。**ファイル名がコマンド名**。
//!
//! どちらも「置いた本人が、どこに何を置いたか忘れる」という 1 つの問題なので、
//! ここで **1 枚の表**に畳む。
//!
//! 設計上の要点:
//!
//! - **frontmatter の解析は自前の純関数** ([`split_front_matter`])。
//!   依存を増やさない。壊れた入力 (閉じない `---` / 値にコロン / 空 / 巨大) で
//!   panic せず、「frontmatter は無かった」という結果へ倒す。
//! - **有効/無効の切り替えは作らない。** Claude Code 側にその概念が無い。
//!   ここが書き換えるファイルは 1 つも無い (読み取りだけ)。
//! - **走査は要求されたときだけ。** プラグインの木は数百ディレクトリになるので、
//!   毎フレーム歩くことは決してしない (設計原則 3: アイドルのコストはゼロ)。
//! - **Skill は「送る」ではなく「パスをコピー」。** slash command は `/名前` が
//!   そのまま呼び出し方だが、Skill の名前は打鍵で呼ぶ形が保証されていない
//!   (プラグイン由来は `プラグイン:名前` になり、説明文の一致で読み込まれる形もある)。
//!   当てずっぽうの `/名前` を送ると黙って空振りするので、曖昧さの無いパスを渡す。

use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};

use crate::fuzzy::PreparedQuery;
use crate::i18n::{tr, trf};
use crate::panels::space;
use crate::theme::Theme;

/// 走査するディレクトリ名。**区切り文字は書かない** (必ず `Path::join` で組む)。
const CLAUDE_DIR: &str = ".claude";
/// Skill を入れる場所の名前。
const SKILLS_DIR: &str = "skills";
/// slash command を入れる場所の名前。
const COMMANDS_DIR: &str = "commands";
/// プラグインを展開する場所の名前。
const PLUGINS_DIR: &str = "plugins";
/// Skill の本体ファイル名。
const SKILL_FILE: &str = "SKILL.md";
/// slash command の拡張子。
const MD_EXT: &str = "md";

/// 1 ファイルから読む上限。これを超える分は読まない
/// (壊れた巨大ファイルで UI を止めないため。frontmatter は先頭にある)。
const MAX_DOC_BYTES: u64 = 1024 * 1024;

/// frontmatter として走査する最大行数。
///
/// 先頭が `---` なのに閉じていないファイル (Markdown の水平線から始まる文書等) で
/// 全文を舐めないための歯止め。超えたら「frontmatter は無かった」へ倒す。
const MAX_FRONT_MATTER_LINES: usize = 200;

/// 詳細に出す本文の行数。
const BODY_HEAD_LINES: usize = 4;

/// プラグインの木を歩く深さの上限 (`plugins/` からの相対)。
///
/// 実際の配置は `<市場>/plugins/<プラグイン>/skills/<名前>/SKILL.md` のように
/// 深いので 5 段は要る。無限に歩かないことの方が大事なので上限を置く。
const PLUGIN_WALK_DEPTH: usize = 6;

/// プラグインの木で見るディレクトリ数の上限 (壊れた木で固まらないため)。
const PLUGIN_WALK_BUDGET: usize = 4000;

// ---------------------------------------------------------------------------
// 種別と出典
// ---------------------------------------------------------------------------

/// 一覧に並ぶものの種別。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum EntryKind {
    /// `skills/<名前>/SKILL.md`
    Skill,
    /// `commands/<名前>.md` — `/<名前>` で呼ぶ
    Command,
}

impl EntryKind {
    /// 行頭のバッジ。**どの言語でも同じ**なので辞書には載せない。
    pub fn badge(self) -> &'static str {
        match self {
            EntryKind::Skill => "Skill",
            EntryKind::Command => "Cmd",
        }
    }

    /// 走査対象のディレクトリ名。
    fn dir_name(self) -> &'static str {
        match self {
            EntryKind::Skill => SKILLS_DIR,
            EntryKind::Command => COMMANDS_DIR,
        }
    }
}

/// どこに置かれていたか。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Origin {
    /// ワークスペース直下の `.claude`
    Project,
    /// ホーム直下の `.claude`
    User,
    /// ホームの `.claude/plugins` 配下
    Plugin,
}

impl Origin {
    /// 出典列のラベル (辞書のキー)。
    pub fn label(self) -> &'static str {
        match self {
            Origin::Project => "プロジェクト",
            Origin::User => "ユーザー",
            // 「プラグイン」単独は 20-app.toml が別の意味 (見出し) で使っているので、
            // 出典としての語をここで分ける (辞書は後勝ちで先の訳が消えるため)。
            Origin::Plugin => "プラグイン由来",
        }
    }
}

// ---------------------------------------------------------------------------
// frontmatter の解析 (純関数)
// ---------------------------------------------------------------------------

/// frontmatter から取り出す値。**`name` と `description` の 2 つで足りる。**
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrontMatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// `off` から 1 行を取り出し、`off` を次の行頭へ進める。行末の改行は含まない。
fn take_line<'a>(s: &'a str, off: &mut usize) -> Option<&'a str> {
    if *off >= s.len() {
        return None;
    }
    let rest = &s[*off..];
    match rest.find('\n') {
        Some(i) => {
            *off += i + 1;
            Some(&rest[..i])
        }
        None => {
            *off = s.len();
            Some(rest)
        }
    }
}

/// 前後の引用符を 1 組だけ外す。
fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.chars().count() >= 2 && v.starts_with(q) && v.ends_with(q) {
            let n = q.len_utf8();
            return v[n..v.len() - n].to_string();
        }
    }
    v.to_string()
}

/// `キー: 値` を 1 行から取り出す。**最初のコロンだけ**で割るので、
/// 値の中のコロン (`Use when …: …` や URL) は壊れない。
///
/// 先頭に空白のある行は入れ子の値なので見ない (最上位のキーだけ拾う)。
fn front_matter_kv(line: &str) -> Option<(String, String)> {
    if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
        return None;
    }
    let (k, v) = line.split_once(':')?;
    let key = k.trim().to_ascii_lowercase();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    Some((key, unquote(v)))
}

/// YAML のブロックスカラー標識か (`>` / `|` に `+` `-` 桁数が付く形)。
///
/// 値が空の場合も「続きの字下げ行に本体がある」形なので真にする。
/// **実物の SKILL.md はこの形を普通に使う**ので、対応しないと説明が `>` だけになる。
fn is_block_scalar(v: &str) -> bool {
    let v = v.trim();
    if v.is_empty() {
        return true;
    }
    let mut c = v.chars();
    match c.next() {
        Some('>') | Some('|') => c.all(|x| x == '+' || x == '-' || x.is_ascii_digit()),
        _ => false,
    }
}

/// frontmatter の行から `name` / `description` を取り出す (純関数)。
///
/// ブロックスカラーの続き (字下げ行) は**空白 1 つで畳む**。
/// ここが作る値は 1 行要約として使うので、改行を保つ意味が無い。
pub fn parse_front_matter_lines(lines: &[&str]) -> FrontMatter {
    let mut fm = FrontMatter::default();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let Some((key, value)) = front_matter_kv(line) else {
            continue;
        };
        let wanted = matches!(key.as_str(), "name" | "description");
        let value = if wanted && is_block_scalar(&value) {
            // 続きの字下げ行を集める。字下げの無い行が来たら本体は終わり
            // (入れ子のマップ `allowed-tools:` も同じ形で自然に止まる)。
            let mut parts: Vec<&str> = Vec::new();
            while i < lines.len() {
                let cont = lines[i];
                let indented = cont.starts_with(' ') || cont.starts_with('\t');
                if !indented && !cont.trim().is_empty() {
                    break;
                }
                let t = cont.trim();
                if !t.is_empty() {
                    parts.push(t);
                }
                i += 1;
            }
            parts.join(" ")
        } else {
            value
        };
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "name" => fm.name = Some(value),
            "description" => fm.description = Some(value),
            _ => {}
        }
    }
    fm
}

/// frontmatter を切り出す (純関数)。返り値は `(取れた値, 本文)`。
///
/// frontmatter が無い / 閉じていない / 長すぎる場合は、値は空で
/// **本文は全文**になる (握り潰さず、そのまま本文として扱う)。
pub fn split_front_matter(text: &str) -> (FrontMatter, &str) {
    let src = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut off = 0usize;
    let Some(first) = take_line(src, &mut off) else {
        return (FrontMatter::default(), src);
    };
    if first.trim_end_matches('\r').trim() != "---" {
        return (FrontMatter::default(), src);
    }
    let mut lines: Vec<&str> = Vec::new();
    loop {
        let mut probe = off;
        let Some(line) = take_line(src, &mut probe) else {
            // 閉じないまま終わった → frontmatter ではなかったことにする
            return (FrontMatter::default(), src);
        };
        if lines.len() >= MAX_FRONT_MATTER_LINES {
            return (FrontMatter::default(), src);
        }
        let t = line.trim_end_matches('\r');
        off = probe;
        let closed = t.trim_end();
        if closed == "---" || closed == "..." {
            break;
        }
        lines.push(t);
    }
    (parse_front_matter_lines(&lines), &src[off..])
}

/// 本文の 1 行要約 — 最初の中身のある行 (見出しの `#` は落とす)。
pub fn body_summary(body: &str) -> String {
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let t = t.trim_start_matches('#').trim();
        if t.is_empty() {
            continue;
        }
        return t.to_string();
    }
    String::new()
}

/// 本文の先頭数行 (空行は詰める)。詳細に出す。
pub fn body_head(body: &str, n: usize) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(n)
        .map(str::to_string)
        .collect()
}

/// 1 ファイルの中身から作る表示用の値 (純関数)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doc {
    pub name: String,
    pub description: String,
    pub head: Vec<String>,
}

/// 中身を表示用に畳む (純関数)。
///
/// `fallback_name` は Skill ならディレクトリ名、コマンドならファイル名 (拡張子なし)。
/// **コマンドの名前は必ずファイル名**なので frontmatter の `name:` では上書きしない
/// (`/名前` はファイル名で決まる。ここを frontmatter に任せると打てない名前が出る)。
pub fn parse_doc(kind: EntryKind, fallback_name: &str, text: &str) -> Doc {
    let (fm, body) = split_front_matter(text);
    let name = match kind {
        EntryKind::Command => fallback_name.to_string(),
        EntryKind::Skill => fm
            .name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| fallback_name.to_string()),
    };
    let description = fm
        .description
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| body_summary(body));
    Doc {
        name,
        description,
        head: body_head(body, BODY_HEAD_LINES),
    }
}

// ---------------------------------------------------------------------------
// 一覧の要素
// ---------------------------------------------------------------------------

/// 一覧の 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub origin: Origin,
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// 本文の先頭数行 (詳細に出す)
    pub head: Vec<String>,
}

impl Entry {
    /// エージェントの入力欄へ差し込む文字列。**コマンドだけ**が持つ。
    ///
    /// Skill は打鍵で呼ぶ形が保証されていないので `None` (代わりにパスを渡す)。
    pub fn slash(&self) -> Option<String> {
        match self.kind {
            EntryKind::Command => Some(format!("/{} ", self.name)),
            EntryKind::Skill => None,
        }
    }
}

/// 展開する詳細の行 (純関数)。
pub fn detail_lines(e: &Entry) -> Vec<String> {
    let mut out = Vec::new();
    if !e.description.is_empty() {
        out.push(e.description.clone());
    }
    out.push(e.path.display().to_string());
    for l in &e.head {
        out.push(l.clone());
    }
    out
}

/// 文字数で省略する (**文字境界で切る**。バイトで切ると日本語が壊れる)。
pub fn ellipsize(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// 発見 (I/O)
// ---------------------------------------------------------------------------

/// ホームの `.claude`。取れなければ `None` (どの環境でも `dirs` 由来)。
pub fn user_claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(CLAUDE_DIR))
}

/// ワークスペース直下の `.claude`。
pub fn project_claude_dir(root: &Path) -> PathBuf {
    root.join(CLAUDE_DIR)
}

/// `<claude_dir>/skills` または `<claude_dir>/commands`。
pub fn kind_dir(claude_dir: &Path, kind: EntryKind) -> PathBuf {
    claude_dir.join(kind.dir_name())
}

/// 先頭 [`MAX_DOC_BYTES`] バイトだけ読む。読めなければ `None`。
fn read_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(MAX_DOC_BYTES).read_to_end(&mut buf).ok()?;
    // 途中で切れた UTF-8 は lossy に倒す (壊れたバイト列で落とさない)
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 1 ファイルを読んで行にする。
fn load_entry(
    kind: EntryKind,
    origin: Origin,
    path: PathBuf,
    fallback_name: &str,
) -> Option<Entry> {
    let text = read_head(&path)?;
    let doc = parse_doc(kind, fallback_name, &text);
    if doc.name.is_empty() {
        return None;
    }
    Some(Entry {
        kind,
        origin,
        name: doc.name,
        description: doc.description,
        path,
        head: doc.head,
    })
}

/// `skills/` を 1 段だけ見る (`<名前>/SKILL.md`)。
fn scan_skills_dir(dir: &Path, origin: Origin, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let sub = e.path();
        if !sub.is_dir() {
            continue;
        }
        let file = sub.join(SKILL_FILE);
        if !file.is_file() {
            continue;
        }
        let fallback = sub
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(entry) = load_entry(EntryKind::Skill, origin, file, &fallback) {
            out.push(entry);
        }
    }
}

/// `commands/` を 1 段だけ見る (`<名前>.md`)。
fn scan_commands_dir(dir: &Path, origin: Origin, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let file = e.path();
        if !file.is_file() {
            continue;
        }
        let is_md = file
            .extension()
            .map(|x| x.to_string_lossy().eq_ignore_ascii_case(MD_EXT))
            .unwrap_or(false);
        if !is_md {
            continue;
        }
        let fallback = file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(entry) = load_entry(EntryKind::Command, origin, file, &fallback) {
            out.push(entry);
        }
    }
}

/// プラグインの木から `skills` / `commands` を探す。
///
/// 配置は市場やキャッシュの版によって段数が変わるので、**深さと件数に上限を置いた
/// 幅優先の探索**で拾う。見つけたディレクトリの中身は 1 段だけ見る。
fn scan_plugins(root: &Path, out: &mut Vec<Entry>) {
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut budget = PLUGIN_WALK_BUDGET;
    while let Some((dir, depth)) = queue.pop() {
        if budget == 0 {
            return;
        }
        budget -= 1;
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == SKILLS_DIR {
                scan_skills_dir(&p, Origin::Plugin, out);
            } else if name == COMMANDS_DIR {
                scan_commands_dir(&p, Origin::Plugin, out);
            } else if depth + 1 < PLUGIN_WALK_DEPTH {
                queue.push((p, depth + 1));
            }
        }
    }
}

/// 並び順を決める鍵。`read_dir` の順は環境依存なので**必ず並べ替える**。
fn sort_key(e: &Entry) -> (EntryKind, Origin, String, PathBuf) {
    (e.kind, e.origin, e.name.to_lowercase(), e.path.clone())
}

/// ワークスペースのルート群とホームを走査する。
///
/// **要求されたときだけ呼ぶこと。** プラグインの木は数百ディレクトリになる。
pub fn scan(roots: &[PathBuf]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut dirs: Vec<(PathBuf, EntryKind, Origin)> = Vec::new();
    for root in roots {
        let cd = project_claude_dir(root);
        dirs.push((
            kind_dir(&cd, EntryKind::Skill),
            EntryKind::Skill,
            Origin::Project,
        ));
        dirs.push((
            kind_dir(&cd, EntryKind::Command),
            EntryKind::Command,
            Origin::Project,
        ));
    }
    if let Some(cd) = user_claude_dir() {
        dirs.push((
            kind_dir(&cd, EntryKind::Skill),
            EntryKind::Skill,
            Origin::User,
        ));
        dirs.push((
            kind_dir(&cd, EntryKind::Command),
            EntryKind::Command,
            Origin::User,
        ));
    }
    let mut seen_dirs: Vec<PathBuf> = Vec::new();
    for (dir, kind, origin) in dirs {
        if seen_dirs.contains(&dir) {
            continue;
        }
        seen_dirs.push(dir.clone());
        match kind {
            EntryKind::Skill => scan_skills_dir(&dir, origin, &mut out),
            EntryKind::Command => scan_commands_dir(&dir, origin, &mut out),
        }
    }
    if let Some(cd) = user_claude_dir() {
        scan_plugins(&cd.join(PLUGINS_DIR), &mut out);
    }
    // 同じファイルを 2 度出さない (ルートが入れ子でも 1 行)
    let mut seen: Vec<PathBuf> = Vec::new();
    out.retain(|e| {
        if seen.contains(&e.path) {
            false
        } else {
            seen.push(e.path.clone());
            true
        }
    });
    out.sort_by_key(sort_key);
    out
}

// ---------------------------------------------------------------------------
// 絞り込み (純関数)
// ---------------------------------------------------------------------------

/// 名前に当たったときの上乗せ。説明に当たっただけの行より上に出す。
const NAME_HIT_BONUS: i32 = 1000;

/// 検索語で絞り込み、良い順に並べた添字を返す (純関数)。
///
/// 空の検索語は**全件をそのままの順で**返す (並べ替えない)。
pub fn filter(entries: &[Entry], query: &str) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..entries.len()).collect();
    }
    let pq = PreparedQuery::new(q);
    let mut hits: Vec<(i32, usize)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let by_name = pq.score(&e.name).map(|s| s + NAME_HIT_BONUS);
        let by_desc = pq.score(&e.description);
        if let Some(s) = by_name.into_iter().chain(by_desc).max() {
            hits.push((s, i));
        }
    }
    // 同点は元の順 (= 種別・出典・名前順) を保つ
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, i)| i).collect()
}

// ---------------------------------------------------------------------------
// レイアウト (純関数)
// ---------------------------------------------------------------------------

/// 列の間隔。
const GAP: f32 = space::SM;
/// 種別バッジの幅 ("Skill" が入る)。
const BADGE_W: f32 = 44.0;
/// 出典列の幅 ("プラグイン" が入る)。
const ORIGIN_W: f32 = 92.0;
/// 名前列の最小幅 (これを割ると名前が読めない)。
const NAME_MIN_W: f32 = 100.0;
/// 名前列の最大幅 (これ以上広げても説明の方が読みたい)。
const NAME_MAX_W: f32 = 220.0;
/// 説明列を出す最小幅 (これを割るなら出さない方がまし)。
const DESC_MIN_W: f32 = 120.0;
/// 操作列 (送る/コピー + 開く) をラベル付きで並べる幅。
const ACTIONS_FULL_W: f32 = 108.0;
/// 操作列をアイコンだけに縮めた幅。
const ACTIONS_ICON_W: f32 = 56.0;
/// 操作列にラベルを付けられる行幅の下限。
const ACTIONS_LABEL_MIN_ROW_W: f32 = 460.0;

/// 一覧 1 行の列幅。**幅 0 の列は描かない。**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub badge_w: f32,
    pub name_w: f32,
    pub desc_w: f32,
    pub origin_w: f32,
    pub actions_w: f32,
    /// 操作列をアイコンだけに縮退させるか
    pub compact_actions: bool,
}

impl RowLayout {
    /// 描く列の合計幅 (列間の間隔込み)。**必ず可用幅以下**になる。
    pub fn total(&self) -> f32 {
        let cols = [
            self.badge_w,
            self.name_w,
            self.desc_w,
            self.origin_w,
            self.actions_w,
        ];
        let n = cols.iter().filter(|w| **w > 0.0).count();
        if n == 0 {
            return 0.0;
        }
        cols.iter().sum::<f32>() + GAP * (n as f32 - 1.0)
    }
}

/// 行の列幅を決める (純関数)。
///
/// 優先順は **操作 > 名前 > 種別 > 説明 > 出典**。
/// 操作列は「送る / 開く に到達できなくなる」ので最後まで落とさず、
/// 狭いところではアイコンだけに縮退させる。
pub fn row_layout(avail_w: f32) -> RowLayout {
    let avail = if avail_w.is_finite() {
        avail_w.max(0.0)
    } else {
        0.0
    };
    let compact_actions = avail < ACTIONS_LABEL_MIN_ROW_W;
    let actions_w = if compact_actions {
        ACTIONS_ICON_W
    } else {
        ACTIONS_FULL_W
    }
    .min(avail);
    let mut rest = (avail - actions_w - GAP).max(0.0);
    let mut badge_w = 0.0;
    let mut origin_w = 0.0;
    let mut desc_w = 0.0;
    if rest >= NAME_MIN_W + BADGE_W + GAP {
        badge_w = BADGE_W;
        rest -= BADGE_W + GAP;
    }
    if rest >= NAME_MIN_W + DESC_MIN_W + ORIGIN_W + GAP * 2.0 {
        origin_w = ORIGIN_W;
        rest -= ORIGIN_W + GAP;
    }
    let name_w = if rest >= NAME_MIN_W + DESC_MIN_W + GAP {
        let n = (rest * 0.38).clamp(NAME_MIN_W, NAME_MAX_W);
        // 説明が最小幅を割るなら名前を削る (説明は 1 行要約なので価値が高い)
        let n = n.min(rest - DESC_MIN_W - GAP);
        desc_w = rest - n - GAP;
        n
    } else {
        rest
    };
    RowLayout {
        badge_w,
        name_w,
        desc_w,
        origin_w,
        actions_w,
        compact_actions,
    }
}

/// 空状態カードの最大幅。
const EMPTY_CARD_MAX_W: f32 = 460.0;
/// 空状態カードの高さ (アイコン + 見出し + ヒント 2 行)。
const EMPTY_CARD_H: f32 = 168.0;

/// 空状態カードの矩形 (純関数)。**常に `avail` の中央 1 枚**で、必ず収まる。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let aw = avail.width().max(0.0);
    let ah = avail.height().max(0.0);
    let w = (aw - space::LG * 2.0).clamp(0.0, EMPTY_CARD_MAX_W).min(aw);
    let h = EMPTY_CARD_H.min(ah);
    let x = avail.left() + (aw - w) * 0.5;
    let y = avail.top() + (ah - h) * 0.5;
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
}

// ---------------------------------------------------------------------------
// パネル (状態 + 描画)
// ---------------------------------------------------------------------------

/// パネルの表示状態。app が所有する。
#[derive(Default)]
pub struct SkillsPanel {
    /// 走査結果 (種別・出典・名前順に並んでいる)
    pub entries: Vec<Entry>,
    /// 展開中の行 (ファイルのパスで指す)
    pub expanded: Option<PathBuf>,
    /// 絞り込みの検索語
    pub query: String,
    /// 走査済みか。**false の間だけ**走査する (毎フレーム I/O にしない)
    pub scanned: bool,
}

impl SkillsPanel {
    /// タブに添える件数。**0 のときは `None`** (常に 0 のバッジを作らない)。
    pub fn badge(&self) -> Option<usize> {
        match self.entries.len() {
            0 => None,
            n => Some(n),
        }
    }

    /// 次の描画で走査し直す。
    pub fn invalidate(&mut self) {
        self.scanned = false;
    }
}

/// パネルが app へ返す要求。I/O は app 側 (描画の外) で行う。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum SkillAction {
    #[default]
    None,
    /// ファイルをエディタで開く
    Open(PathBuf),
    /// 走査し直す
    Rescan,
    /// エージェントの入力欄へ `/コマンド名 ` を差し込む
    Send(String),
    /// ファイルのパスをクリップボードへ
    CopyPath(PathBuf),
}

/// Skills / コマンド管理パネルを描く。
pub fn ui(ui: &mut egui::Ui, theme: &Theme, panel: &mut SkillsPanel) -> SkillAction {
    let mut action = SkillAction::None;

    // ── 見出し行 ──
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tr("🧩 Skills / コマンド"))
                .size(13.0)
                .color(theme.text),
        );
        let n = panel.entries.len();
        if n > 0 {
            ui.label(
                RichText::new(trf("{n} 件", &[("n", n.to_string())]))
                    .size(11.5)
                    .color(theme.text_dim),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("⟳")
                .on_hover_text(tr("置き場所を読み直す"))
                .clicked()
            {
                action = SkillAction::Rescan;
            }
        });
    });

    // ── 検索 (名前と説明の両方に当てる。絞り込みはここ 1 つだけ) ──
    if !panel.entries.is_empty() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("🔍").size(12.0).color(theme.text_dim));
            let w = (ui.available_width() - space::LG).max(60.0);
            ui.add_sized(
                [w, 22.0],
                egui::TextEdit::singleline(&mut panel.query)
                    .hint_text(tr("名前・説明で絞り込む"))
                    .desired_width(w),
            );
        });
    }

    let order = filter(&panel.entries, &panel.query);
    if order.is_empty() {
        empty_state(ui, theme, panel.entries.is_empty());
        return action;
    }

    ui.add_space(space::XS);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 行の枠には左右 `space::SM` の内側余白があるので、その分を
            // 差し引いた実効幅で列を決める (差し引かないと右端が見切れる)。
            let l = row_layout(ui.available_width() - space::SM * 2.0);
            let mut toggle: Option<PathBuf> = None;
            for i in &order {
                let e = &panel.entries[*i];
                // 可変長リストの中なので、行内の `interact` の ID に
                // 要素固有の値 (パス) を混ぜる。
                ui.push_id(&e.path, |ui| {
                    let open = panel.expanded.as_deref() == Some(e.path.as_path());
                    if entry_row(ui, theme, e, &l, open, &mut action) {
                        toggle = Some(e.path.clone());
                    }
                });
            }
            if let Some(p) = toggle {
                panel.expanded = if panel.expanded.as_deref() == Some(p.as_path()) {
                    None
                } else {
                    Some(p)
                };
            }
        });
    action
}

/// 1 行を描く。行そのものがクリックされたら `true` (詳細の開閉)。
fn entry_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    e: &Entry,
    l: &RowLayout,
    open: bool,
    action: &mut SkillAction,
) -> bool {
    let mut row_clicked = false;

    let frame = egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(space::SM, 5.0))
        .rounding(6.0)
        .fill(if open { theme.panel_alt } else { theme.bg })
        .show(ui, |ui| {
            // 列の合計は必ず可用幅以下 (`RowLayout::total` の不変条件)。
            // これで行がどの幅でも見切れない。
            ui.set_width(l.total().min(ui.available_width()));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                if l.badge_w > 0.0 {
                    ui.add_sized(
                        [l.badge_w, 18.0],
                        egui::Label::new(
                            RichText::new(e.kind.badge())
                                .size(10.5)
                                .monospace()
                                .color(theme.accent),
                        )
                        .selectable(false),
                    );
                }
                if l.name_w > 0.0 {
                    let label = match e.kind {
                        EntryKind::Command => format!("/{}", e.name),
                        EntryKind::Skill => e.name.clone(),
                    };
                    ui.add_sized(
                        [l.name_w, 18.0],
                        egui::Label::new(
                            RichText::new(ellipsize(&label, name_chars(l.name_w)))
                                .size(12.0)
                                .color(theme.text),
                        )
                        .selectable(false),
                    )
                    .on_hover_text(label);
                }
                if l.desc_w > 0.0 {
                    ui.add_sized(
                        [l.desc_w, 18.0],
                        egui::Label::new(
                            RichText::new(ellipsize(&e.description, name_chars(l.desc_w)))
                                .size(11.0)
                                .color(theme.text_dim),
                        )
                        .selectable(false),
                    )
                    .on_hover_text(e.description.clone());
                }
                if l.origin_w > 0.0 {
                    let text = tr(e.origin.label());
                    ui.add_sized(
                        [l.origin_w, 18.0],
                        egui::Label::new(
                            RichText::new(ellipsize(&text, 12))
                                .size(10.5)
                                .color(theme.text_dim),
                        )
                        .selectable(false),
                    )
                    .on_hover_text(e.path.display().to_string());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let open_label = if l.compact_actions {
                        "📂".to_string()
                    } else {
                        tr("📂 開く")
                    };
                    if ui
                        .button(RichText::new(open_label).size(11.0))
                        .on_hover_text(trf(
                            "ファイルを開く: {p}",
                            &[("p", e.path.display().to_string())],
                        ))
                        .clicked()
                    {
                        *action = SkillAction::Open(e.path.clone());
                    }
                    // slash command は `/名前` がそのまま呼び出し方なので送れる。
                    // Skill は打鍵で呼ぶ形が保証されていないので**パスを渡す**。
                    match e.slash() {
                        Some(text) => {
                            let label = if l.compact_actions {
                                "👾".to_string()
                            } else {
                                tr("👾 送る")
                            };
                            if ui
                                .button(RichText::new(label).size(11.0))
                                .on_hover_text(trf(
                                    "このエージェントへ送る: {c}",
                                    &[("c", text.trim_end().to_string())],
                                ))
                                .clicked()
                            {
                                *action = SkillAction::Send(text);
                            }
                        }
                        None => {
                            let label = if l.compact_actions {
                                "📋".to_string()
                            } else {
                                tr("📋 パス")
                            };
                            if ui
                                .button(RichText::new(label).size(11.0))
                                .on_hover_text(tr("SKILL.md のパスをコピーする\
                                     (Skill は説明文で選ばれるので、打鍵名では呼べません)"))
                                .clicked()
                            {
                                *action = SkillAction::CopyPath(e.path.clone());
                            }
                        }
                    }
                });
            });
            if open {
                ui.add_space(space::XS);
                for line in detail_lines(e) {
                    ui.label(
                        RichText::new(ellipsize(&line, 160))
                            .size(10.5)
                            .color(theme.text_dim),
                    )
                    .on_hover_text(line);
                }
            }
        });

    let hit = ui.interact(
        frame.response.rect,
        ui.id().with("zv-skill-row"),
        egui::Sense::click(),
    );
    if hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if hit.on_hover_text(tr("クリックで詳細を開閉")).clicked() {
        row_clicked = true;
    }
    row_clicked
}

/// 列に入るおおよその文字数 (等幅でない前提で 7px/文字と見積もる)。
fn name_chars(w: f32) -> usize {
    ((w / 7.0).floor() as usize).max(4)
}

/// 空状態 — 利用可能領域の**中央に 1 枚**のカード。
///
/// `nothing_at_all` が false なら「絞り込みで 0 件」なので、案内を変える。
fn empty_state(ui: &mut egui::Ui, theme: &Theme, nothing_at_all: bool) {
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let card = empty_card(avail);
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::none()
            .fill(theme.panel_alt)
            .stroke(egui::Stroke::new(1.0_f32, theme.border))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width((card.width() - space::MD * 2.0).max(0.0));
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("🧩").size(40.0));
                    if nothing_at_all {
                        ui.label(
                            RichText::new(tr("Skills も slash command もありません"))
                                .size(16.0)
                                .color(theme.text),
                        );
                        ui.label(
                            RichText::new(tr(
                                ".claude/skills/<名前>/SKILL.md を置くとここに並びます。\
                                 .claude/commands/<名前>.md は /<名前> で呼べます \
                                 (ワークスペース直下でも、ホームでも構いません)",
                            ))
                            .size(11.0)
                            .color(theme.text_dim),
                        );
                    } else {
                        ui.label(
                            RichText::new(tr("この検索語に当たるものがありません"))
                                .size(16.0)
                                .color(theme.text),
                        );
                        ui.label(
                            RichText::new(tr("検索欄を消すと全部出ます"))
                                .size(11.0)
                                .color(theme.text_dim),
                        );
                    }
                });
            });
    });
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- frontmatter: テーブルテスト --------------------------------------

    #[test]
    fn frontmatterのテーブル() {
        // (見出し, 入力, 期待する name, 期待する description, 本文の先頭)
        let cases: &[(&str, &str, Option<&str>, Option<&str>, &str)] = &[
            (
                "普通",
                "---\nname: slim\ndescription: Token-slim mode\n---\n# 本文\n",
                Some("slim"),
                Some("Token-slim mode"),
                "# 本文",
            ),
            (
                "frontmatter 無し",
                "# チケット分割タスク\n\n要件定義書を分割する\n",
                None,
                None,
                "# チケット分割タスク",
            ),
            (
                "--- が閉じていない",
                "---\nname: broken\ndescription: never closed\n# 本文\n",
                None,
                None,
                "---",
            ),
            (
                "値にコロンを含む",
                "---\nname: night\ndescription: 夜間モード: 止めるまで回す\n---\n本文\n",
                Some("night"),
                Some("夜間モード: 止めるまで回す"),
                "本文",
            ),
            (
                "引用符つき",
                "---\nname: \"quoted\"\ndescription: 'シングルも外す'\n---\n本文\n",
                Some("quoted"),
                Some("シングルも外す"),
                "本文",
            ),
            ("空", "", None, None, ""),
            ("区切りだけ", "---\n---\n", None, None, ""),
            (
                "日本語のキー値",
                "---\nname: 看板\ndescription: 進捗を1画面で見る\n---\n本文\n",
                Some("看板"),
                Some("進捗を1画面で見る"),
                "本文",
            ),
            (
                "CRLF",
                "---\r\nname: crlf\r\ndescription: 改行が CRLF\r\n---\r\n# 本文\r\n",
                Some("crlf"),
                Some("改行が CRLF"),
                "# 本文",
            ),
            (
                "入れ子の値は拾わない",
                "---\nname: nested\nallowed-tools:\n  name: not-this\n---\n本文\n",
                Some("nested"),
                None,
                "本文",
            ),
            (
                "値が空",
                "---\nname:\ndescription:   \n---\n本文\n",
                None,
                None,
                "本文",
            ),
            (
                "BOM つき",
                "\u{feff}---\nname: bom\ndescription: BOM\n---\n本文\n",
                Some("bom"),
                Some("BOM"),
                "本文",
            ),
            (
                "見出しから始まる水平線",
                "本文だけ\n---\nまだ本文\n",
                None,
                None,
                "本文だけ",
            ),
            // 実物の SKILL.md はブロックスカラーを普通に使う。
            // 対応しないと説明が ">" の 1 文字になる (実マシンで踏んだ)。
            (
                "折り畳みブロック (>)",
                "---\nname: hyperframes\ndescription: >\n  Mandatory entry point: read this first\n  for any request to make a video.\n---\n# 本文\n",
                Some("hyperframes"),
                Some("Mandatory entry point: read this first for any request to make a video."),
                "# 本文",
            ),
            (
                "字面ブロック (|-) と後続キー",
                "---\ndescription: |-\n  一行目\n  二行目\nname: after\n---\n本文\n",
                Some("after"),
                Some("一行目 二行目"),
                "本文",
            ),
            (
                "ブロックの中の空行は詰める",
                "---\ndescription: >-\n  前半\n\n  後半\n---\n本文\n",
                None,
                Some("前半 後半"),
                "本文",
            ),
        ];
        for (title, input, want_name, want_desc, want_body_head) in cases {
            let (fm, body) = split_front_matter(input);
            assert_eq!(fm.name.as_deref(), *want_name, "{title}: name");
            assert_eq!(
                fm.description.as_deref(),
                *want_desc,
                "{title}: description"
            );
            let head = body.lines().map(str::trim).find(|l| !l.is_empty());
            assert_eq!(head.unwrap_or(""), *want_body_head, "{title}: 本文の先頭");
        }
    }

    /// 壊れた入力で panic しない (frontmatter を閉じない巨大ファイルを含む)。
    #[test]
    fn 壊れた入力でも落ちない() {
        let huge = format!("---\nname: huge\n{}", "a: b\n".repeat(200_000));
        let (fm, body) = split_front_matter(&huge);
        // 上限を超えたので「frontmatter は無かった」へ倒れ、本文は全文
        assert_eq!(fm, FrontMatter::default());
        assert_eq!(body.len(), huge.len());

        let big_body = format!("---\nname: ok\n---\n{}", "x".repeat(500_000));
        let (fm, body) = split_front_matter(&big_body);
        assert_eq!(fm.name.as_deref(), Some("ok"));
        assert_eq!(body.len(), 500_000);

        for s in [
            "---",
            "---\n",
            "-",
            ":",
            "---\n:\n---\n",
            "---\n: v\n---\n",
            "---\nname\n---\n",
            "\u{feff}",
            "\u{feff}---",
            "---\n\u{0}\n---\n",
        ] {
            let _ = split_front_matter(s);
            let _ = parse_doc(EntryKind::Skill, "fallback", s);
            let _ = parse_doc(EntryKind::Command, "fallback", s);
        }
    }

    #[test]
    fn 名前の決まり方() {
        // Skill は frontmatter の name を優先し、無ければディレクトリ名
        let d = parse_doc(
            EntryKind::Skill,
            "dir-name",
            "---\nname: from-fm\n---\n本文\n",
        );
        assert_eq!(d.name, "from-fm");
        let d = parse_doc(EntryKind::Skill, "dir-name", "# 説明だけ\n");
        assert_eq!(d.name, "dir-name");
        assert_eq!(d.description, "説明だけ");
        // コマンドは**必ずファイル名** (`/名前` はファイル名で決まる)
        let d = parse_doc(
            EntryKind::Command,
            "ticket-split",
            "---\nname: 別名\ndescription: 分割する\n---\n本文\n",
        );
        assert_eq!(d.name, "ticket-split", "コマンド名がファイル名から外れた");
        assert_eq!(d.description, "分割する");
    }

    #[test]
    fn 説明が無ければ本文の先頭行を使う() {
        let d = parse_doc(
            EntryKind::Command,
            "x",
            "\n\n#  チケット分割タスク  \n\n本文\n",
        );
        assert_eq!(d.description, "チケット分割タスク");
        // 行の中の空白は詰めない (本文をそのまま見せる。前後だけ落とす)
        assert_eq!(
            d.head,
            vec!["#  チケット分割タスク".to_string(), "本文".into()]
        );
    }

    /// Skill には送る文字列が無い (打鍵名で呼べる保証が無いため)。
    #[test]
    fn 送れるのはコマンドだけ() {
        let mk = |kind| Entry {
            kind,
            origin: Origin::User,
            name: "foo".into(),
            description: String::new(),
            path: PathBuf::new(),
            head: Vec::new(),
        };
        assert_eq!(mk(EntryKind::Command).slash().as_deref(), Some("/foo "));
        assert_eq!(mk(EntryKind::Skill).slash(), None);
    }

    // ---- 走査 --------------------------------------------------------------

    fn write(path: &Path, text: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).expect("create dir");
        }
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn 走査はプロジェクトの二か所を見る() {
        let root = crate::test_util::unique_temp_dir("zv-skills", "scan");
        let cd = project_claude_dir(&root);
        write(
            &kind_dir(&cd, EntryKind::Skill)
                .join("alpha")
                .join(SKILL_FILE),
            "---\nname: alpha\ndescription: 最初の Skill\n---\n本文\n",
        );
        write(
            &kind_dir(&cd, EntryKind::Skill)
                .join("bravo")
                .join(SKILL_FILE),
            "# 説明しかない\n",
        );
        write(
            &kind_dir(&cd, EntryKind::Command).join("charlie.md"),
            "---\ndescription: 三番目\n---\n本文\n",
        );
        // SKILL.md の無いディレクトリと .md でないファイルは拾わない
        std::fs::create_dir_all(kind_dir(&cd, EntryKind::Skill).join("empty")).expect("mkdir");
        write(&kind_dir(&cd, EntryKind::Command).join("readme.txt"), "x");

        let all = scan(&[root.clone()]);
        let mine: Vec<&Entry> = all.iter().filter(|e| e.path.starts_with(&root)).collect();
        assert_eq!(mine.len(), 3, "拾った件数が違う: {mine:#?}");
        let names: Vec<&str> = mine.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"bravo"), "{names:?}");
        assert!(names.contains(&"charlie"), "{names:?}");
        let cmd = mine
            .iter()
            .find(|e| e.kind == EntryKind::Command)
            .expect("コマンドがある");
        assert_eq!(cmd.name, "charlie");
        assert_eq!(cmd.description, "三番目");
        assert_eq!(cmd.origin, Origin::Project);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 同じルートを二度渡しても一行() {
        let root = crate::test_util::unique_temp_dir("zv-skills", "dedup");
        let cd = project_claude_dir(&root);
        write(
            &kind_dir(&cd, EntryKind::Command).join("dup.md"),
            "# 重複しない\n",
        );
        let all = scan(&[root.clone(), root.clone()]);
        let n = all.iter().filter(|e| e.path.starts_with(&root)).count();
        assert_eq!(n, 1, "同じファイルが 2 行出た");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 存在しない置き場所はエラーではない() {
        let root = crate::test_util::unique_temp_dir("zv-skills", "missing");
        let sub = root.join("no-such-workspace");
        let all = scan(&[sub.clone()]);
        assert!(all.iter().all(|e| !e.path.starts_with(&sub)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 並び順は環境に依らない() {
        let root = crate::test_util::unique_temp_dir("zv-skills", "order");
        let cd = project_claude_dir(&root);
        for n in ["zulu", "alpha", "Mike"] {
            write(
                &kind_dir(&cd, EntryKind::Skill).join(n).join(SKILL_FILE),
                &format!("---\nname: {n}\n---\n本文\n"),
            );
        }
        let a = scan(&[root.clone()]);
        let b = scan(&[root.clone()]);
        assert_eq!(
            a.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
            b.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
            "走査のたびに並びが変わる"
        );
        let mine: Vec<String> = a
            .iter()
            .filter(|e| e.path.starts_with(&root))
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(mine, vec!["alpha", "Mike", "zulu"], "名前順になっていない");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 巨大ファイルでも読み切らない (先頭だけ読む)。
    #[test]
    fn 巨大ファイルは先頭だけ読む() {
        let root = crate::test_util::unique_temp_dir("zv-skills", "huge");
        let cd = project_claude_dir(&root);
        let body = "x".repeat((MAX_DOC_BYTES as usize) * 2);
        write(
            &kind_dir(&cd, EntryKind::Command).join("huge.md"),
            &format!("---\ndescription: 大きい\n---\n{body}"),
        );
        let all = scan(&[root.clone()]);
        let e = all
            .iter()
            .find(|e| e.name == "huge")
            .expect("巨大ファイルも 1 行になる");
        assert_eq!(e.description, "大きい");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 絞り込み: テーブルテスト ------------------------------------------

    fn sample() -> Vec<Entry> {
        let mk = |kind, name: &str, desc: &str| Entry {
            kind,
            origin: Origin::User,
            name: name.into(),
            description: desc.into(),
            path: PathBuf::from(name),
            head: Vec::new(),
        };
        vec![
            mk(EntryKind::Skill, "slim", "トークン節約モード"),
            mk(EntryKind::Skill, "night", "夜間自動改善モード"),
            mk(EntryKind::Command, "ticket-split", "チケット分割タスク"),
        ]
    }

    #[test]
    fn 絞り込みのテーブル() {
        let e = sample();
        // (検索語, 期待する名前の並び)
        let cases: &[(&str, &[&str])] = &[
            ("", &["slim", "night", "ticket-split"]),
            ("   ", &["slim", "night", "ticket-split"]),
            ("slim", &["slim"]),
            ("ticket", &["ticket-split"]),
            ("モード", &["slim", "night"]),
            ("節約", &["slim"]),
            ("分割", &["ticket-split"]),
            ("zzzz", &[]),
        ];
        for (q, want) in cases {
            let got: Vec<&str> = filter(&e, q)
                .into_iter()
                .map(|i| e[i].name.as_str())
                .collect();
            assert_eq!(&got[..], *want, "検索語 {q:?}");
        }
    }

    /// 名前に当たった行は、説明に当たっただけの行より上に出る。
    #[test]
    fn 名前の一致が説明より上に来る() {
        let mut e = sample();
        e.push(Entry {
            kind: EntryKind::Skill,
            origin: Origin::User,
            name: "other".into(),
            description: "night のことを説明する".into(),
            path: PathBuf::from("other"),
            head: Vec::new(),
        });
        let got: Vec<&str> = filter(&e, "night")
            .into_iter()
            .map(|i| e[i].name.as_str())
            .collect();
        assert_eq!(got, vec!["night", "other"]);
    }

    // ---- レイアウト: テーブルテスト ----------------------------------------

    #[test]
    fn 行レイアウトのテーブル() {
        // (可用幅, バッジ, 説明, 出典, 操作を縮退させるか)
        //
        // 境界は 2 段ある: 操作列がラベル付きへ広がる 460 と、
        // その分だけ出典列が一度落ちて 496 で戻る所。両方を釘で留める。
        let cases: [(f32, bool, bool, bool, bool); 12] = [
            (0.0, false, false, false, true),
            (60.0, false, false, false, true),
            (200.0, false, false, false, true),
            (300.0, true, false, false, true),
            (443.0, true, true, false, true),
            (444.0, true, true, true, true),
            (459.0, true, true, true, true),
            // 460 でラベル付きへ広がる分、出典が一度落ちる
            (460.0, true, true, false, false),
            (495.0, true, true, false, false),
            (496.0, true, true, true, false),
            (900.0, true, true, true, false),
            (2000.0, true, true, true, false),
        ];
        for (w, badge, desc, origin, compact) in cases {
            let l = row_layout(w);
            assert_eq!(l.badge_w > 0.0, badge, "幅 {w}: バッジ {l:?}");
            assert_eq!(l.desc_w > 0.0, desc, "幅 {w}: 説明 {l:?}");
            assert_eq!(l.origin_w > 0.0, origin, "幅 {w}: 出典 {l:?}");
            assert_eq!(l.compact_actions, compact, "幅 {w}: 操作 {l:?}");
        }
    }

    #[test]
    fn 行はどの幅でも見切れない() {
        let mut w = -50.0_f32;
        while w <= 2400.0 {
            let l = row_layout(w);
            let avail = w.max(0.0);
            assert!(
                l.total() <= avail + 0.001,
                "幅 {w}: 合計 {} が可用幅を超えた {l:?}",
                l.total()
            );
            for v in [l.badge_w, l.name_w, l.desc_w, l.origin_w, l.actions_w] {
                assert!(v >= 0.0 && v.is_finite(), "幅 {w}: 負/非有限の列 {l:?}");
            }
            // 操作列は最後まで落とさない (送る / 開く に到達できなくなる)
            if avail >= 1.0 {
                assert!(l.actions_w > 0.0, "幅 {w}: 操作列が消えた {l:?}");
            }
            // 説明を出すなら最小幅は割らない
            if l.desc_w > 0.0 {
                assert!(l.desc_w >= DESC_MIN_W - 0.001, "幅 {w}: 説明が細すぎ {l:?}");
            }
            w += 7.0;
        }
        // 非有限入力でも壊れない
        let l = row_layout(f32::NAN);
        assert!(l.total() <= 0.001, "{l:?}");
    }

    #[test]
    fn 空状態カードは常に可用領域の中に収まる() {
        let sizes = [
            (900.0_f32, 700.0_f32),
            (1200.0, 300.0),
            (320.0, 120.0),
            (200.0, 40.0),
            (0.0, 0.0),
        ];
        for (w, h) in sizes {
            let avail = egui::Rect::from_min_size(egui::pos2(11.0, 23.0), egui::vec2(w, h));
            let card = empty_card(avail);
            assert!(
                avail.contains_rect(card),
                "{w}x{h}: カードがはみ出した {card:?} / {avail:?}"
            );
            assert!(card.width() >= 0.0 && card.height() >= 0.0, "{card:?}");
            assert!(
                ((card.left() - avail.left()) - (avail.right() - card.right())).abs() < 0.01,
                "{w}x{h}: 水平中央でない"
            );
            assert!(
                ((card.top() - avail.top()) - (avail.bottom() - card.bottom())).abs() < 0.01,
                "{w}x{h}: 垂直中央でない"
            );
        }
    }

    #[test]
    fn 省略は文字境界で切る() {
        assert_eq!(ellipsize("abc", 5), "abc");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("日本語テスト", 3), "日本…");
        assert_eq!(ellipsize("abc", 0), "");
        assert_eq!(ellipsize("", 5), "");
    }

    #[test]
    fn 件数バッジは0のとき出さない() {
        let mut p = SkillsPanel::default();
        assert_eq!(p.badge(), None, "0 件でバッジを出している");
        p.entries.push(Entry {
            kind: EntryKind::Skill,
            origin: Origin::User,
            name: "a".into(),
            description: String::new(),
            path: PathBuf::new(),
            head: Vec::new(),
        });
        assert_eq!(p.badge(), Some(1));
    }

    #[test]
    fn 詳細にはパスと本文が出る() {
        let e = Entry {
            kind: EntryKind::Skill,
            origin: Origin::User,
            name: "a".into(),
            description: "説明".into(),
            path: PathBuf::from("a").join(SKILL_FILE),
            head: vec!["1 行目".into(), "2 行目".into()],
        };
        let lines = detail_lines(&e);
        assert_eq!(lines.first().map(String::as_str), Some("説明"));
        assert!(lines.iter().any(|l| l.contains(SKILL_FILE)), "{lines:?}");
        assert!(lines.iter().any(|l| l == "1 行目"), "{lines:?}");
    }

    // ---- ハードコーディングの番人 ------------------------------------------

    #[test]
    fn ソースに絶対パスを直書きしていない() {
        let src = include_str!("skills.rs").replace("\r\n", "\n");
        // 検出語をそのまま書くと**この番人自身**が引っかかるので、組み立てる。
        let q = "\u{22}"; // "
        let sl = "\u{2f}"; // /
        let bs = "\u{5c}"; // \
        let bad = [
            format!("{q}{sl}tmp"),
            format!("{sl}Users{sl}"),
            format!("{sl}home{sl}"),
            format!("C:{bs}"),
        ];
        for b in &bad {
            assert!(!src.contains(b.as_str()), "絶対パスの直書きがある: {b}");
        }
        // 区切り文字入りのパス文字列も**組み立てには**使わない
        // (必ず `Path::join`。画面へ出す案内文は経路ではないので対象外)。
        for name in ["skills", "commands", "plugins", "SKILL.md"] {
            let joined = format!("{sl}{name}{q}");
            assert!(
                !src.contains(joined.as_str()),
                "区切り文字入りのパス文字列がある: {joined}"
            );
        }
    }
}
