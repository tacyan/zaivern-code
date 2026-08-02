use std::path::Path;

use eframe::egui::{text::LayoutJob, Color32, FontId, TextFormat};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Highlighter as SynHighlighter, ThemeSet};
use syntect::parsing::{Scope, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::grammar::{self, FoldKindSpec, Grammar, GrammarSet, ScanState, Span, Tok};

/// Files larger than this are laid out without highlighting to stay snappy.
const MAX_HIGHLIGHT_BYTES: usize = 400_000;

/// Single lines longer than this (e.g. minified JS) are laid out without
/// highlighting so one huge line cannot freeze the UI.
const MAX_HIGHLIGHT_LINE_BYTES: usize = 8_192;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// プロセスで 1 つだけの [`Highlighter`]。
///
/// `SyntaxSet` / `ThemeSet` は数 MB あるので、エディタ本文・差分ビュー・
/// Markdown プレビューがそれぞれ持つとメモリが素直に倍々になる。
/// 内部のキャッシュは `Mutex` 越しなので複数箇所から同時に呼んで安全。
/// **初回呼び出しまでロードしない**ので、起動時間には影響しない。
pub fn shared() -> &'static Highlighter {
    static H: OnceLock<Highlighter> = OnceLock::new();
    H.get_or_init(Highlighter::new)
}

pub struct Highlighter {
    ps: SyntaxSet,
    ts: ThemeSet,
    /// プラグインが持ち込んだ軽量シンタックス定義 (`[[syntax]]`)。
    /// syntect が知らない言語 (TypeScript / Kotlin / Zig …) はこちらで塗る。
    ///
    /// [`shared`] はプロセスで 1 つの `&'static` なので、プラグインの
    /// 有効/無効が切り替わったときに差し替えられるよう内部可変にしてある。
    /// 読み側は `Arc` を 1 回複製するだけで、ロックを跨いで持ち歩かない。
    packs: RwLock<Arc<GrammarSet>>,
    /// テーマ名 → トークン種類ごとの色。テーマは滅多に変わらないので
    /// 一度作ったら使い回す (毎行スコープ解決をやり直さないため)。
    palettes: Mutex<HashMap<String, Arc<Palette>>>,
    /// key → LayoutJob と、その挿入順。キーは本文全体のハッシュを含むため
    /// 1 打鍵ごとに新しいエントリが増える。上限到達で全消しすると次の
    /// フレームに全再ハイライトのスパイクが出るので、古い方から 1 件ずつ
    /// 追い出す。文書まるごとの LayoutJob を抱えるため上限は小さめ。
    cache: Mutex<(HashMap<u64, LayoutJob>, std::collections::VecDeque<u64>)>,
}

const HL_CACHE_CAP: usize = 32;

/// トークン種類ごとの見た目。syntect のテーマスコープから引くので、
/// 追加言語の配色も**カラーテーマを変えれば一緒に変わる**。
#[derive(Clone, Debug)]
struct Palette {
    fg: [Color32; Tok::COUNT],
    italic: [bool; Tok::COUNT],
    underline: [bool; Tok::COUNT],
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            ps: SyntaxSet::load_defaults_newlines(),
            ts: ThemeSet::load_defaults(),
            packs: RwLock::new(Arc::new(GrammarSet::default())),
            palettes: Mutex::new(HashMap::new()),
            cache: Mutex::new((
                HashMap::with_capacity(HL_CACHE_CAP),
                std::collections::VecDeque::with_capacity(HL_CACHE_CAP),
            )),
        }
    }

    /// いま読み込んでいる言語定義 (`Arc` の複製を返すので、呼び出し側は
    /// ロックを握ったまま走査しない)。
    fn packs(&self) -> Arc<GrammarSet> {
        match self.packs.read() {
            Ok(g) => g.clone(),
            // 毒されたロックでも「追加言語が無い」状態で描画は続ける。
            Err(e) => e.into_inner().clone(),
        }
    }

    /// プラグイン由来の言語定義を差し替える。折りたたみ用の [`LangSpec`] も
    /// ここで登録するので、以後 `lang_spec()` が追加言語を知っている状態になる。
    pub fn set_grammars(&self, packs: GrammarSet) {
        register_lang_specs(&packs);
        match self.packs.write() {
            Ok(mut g) => *g = Arc::new(packs),
            Err(e) => *e.into_inner() = Arc::new(packs),
        }
        // 既存のキャッシュは「その言語を知らなかった頃」の結果なので捨てる。
        if let Ok(mut g) = self.cache.lock() {
            g.0.clear();
            g.1.clear();
        }
    }

    /// 追加で認識できる言語の数 (プラグイン画面の表示用)。
    pub fn extra_lang_count(&self) -> usize {
        self.packs().grammars.len()
    }

    /// 追加言語の名前 (プラグイン画面の表示用、名前順)。
    pub fn extra_lang_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.packs().names().iter().map(|s| s.to_string()).collect();
        v.sort_by_key(|s| s.to_lowercase());
        v
    }

    pub fn lang_for(&self, path: Option<&Path>, text: &str) -> String {
        // プラグインのパックを先に見る。syntect が知らない言語を足すのが
        // 主目的だが、`.sass` → Ruby Haml のような既定の取り違えを
        // 利用者が上書きできる余地もここで確保する。
        let packs = self.packs();
        if let Some(p) = path {
            if let Some(name) = packs.detect_path(p) {
                return name;
            }
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if let Some(s) = self.ps.find_syntax_by_extension(ext) {
                    return s.name.clone();
                }
            }
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if let Some(s) = self.ps.find_syntax_by_extension(name) {
                    return s.name.clone();
                }
            }
        }
        if let Some(line) = text.lines().next() {
            if let Some(name) = packs.detect_first_line(line) {
                return name;
            }
            if let Some(s) = self.ps.find_syntax_by_first_line(line) {
                return s.name.clone();
            }
        }
        "Plain Text".into()
    }

    /// フェンスコードの言語トークン ("rust", "py" など) から言語名を引く。
    pub fn lang_for_fence(&self, token: &str) -> String {
        if token.is_empty() {
            return "Plain Text".into();
        }
        if let Some(name) = self.packs().detect_token(token) {
            return name;
        }
        self.ps
            .find_syntax_by_token(token)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Plain Text".into())
    }

    /// テーマ名からパレットを作る (作成済みなら使い回す)。
    fn palette(&self, theme_name: &str) -> Option<Arc<Palette>> {
        if let Ok(g) = self.palettes.lock() {
            if let Some(p) = g.get(theme_name) {
                return Some(p.clone());
            }
        }
        let theme = self.ts.themes.get(theme_name)?;
        let sh = SynHighlighter::new(theme);
        let default = sh.get_default();
        let mut pal = Palette {
            fg: [Color32::WHITE; Tok::COUNT],
            italic: [false; Tok::COUNT],
            underline: [false; Tok::COUNT],
        };
        for i in 0..Tok::COUNT {
            let tok = Tok::from_index(i);
            let style = Scope::new(tok.scope())
                .ok()
                .map(|s| sh.style_for_stack(&[s]))
                .unwrap_or(default);
            pal.fg[i] = Color32::from_rgb(
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
            );
            pal.italic[i] = style.font_style.contains(FontStyle::ITALIC);
            pal.underline[i] = style.font_style.contains(FontStyle::UNDERLINE);
        }
        let arc = Arc::new(pal);
        if let Ok(mut g) = self.palettes.lock() {
            g.insert(theme_name.to_string(), arc.clone());
        }
        Some(arc)
    }

    pub fn layout_job(
        &self,
        text: &str,
        lang: &str,
        theme_name: &str,
        font: FontId,
        fallback: Color32,
    ) -> LayoutJob {
        // キャッシュキーのハッシュ計算
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        lang.hash(&mut hasher);
        theme_name.hash(&mut hasher);
        font.hash(&mut hasher);
        fallback.hash(&mut hasher);
        let key = hasher.finish();

        if let Ok(guard) = self.cache.lock() {
            if let Some(cached_job) = guard.0.get(&key) {
                return cached_job.clone();
            }
        }

        let plain = |job: &mut LayoutJob| {
            job.append(
                text,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: fallback,
                    ..Default::default()
                },
            );
        };

        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;

        if text.len() > MAX_HIGHLIGHT_BYTES {
            plain(&mut job);
            self.cache_put(key, &job);
            return job;
        }

        // syntect が知っている言語はそちらで塗る。知らない言語 (TypeScript /
        // Kotlin / Zig …) はプラグインのパックへ落とし、そこにも無ければ素の文字。
        let Some(syntax) = self
            .ps
            .find_syntax_by_name(lang)
            .filter(|s| s.name != "Plain Text")
        else {
            let packs = self.packs();
            match (packs.by_name(lang), self.palette(theme_name)) {
                (Some(g), Some(pal)) => {
                    append_grammar(&mut job, text, g, &pal, &font, fallback);
                }
                _ => plain(&mut job),
            }
            self.cache_put(key, &job);
            return job;
        };

        let Some(theme) = self.ts.themes.get(theme_name) else {
            plain(&mut job);
            self.cache_put(key, &job);
            return job;
        };

        let mut h = HighlightLines::new(syntax, theme);
        for line in LinesWithEndings::from(text) {
            if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
                job.append(
                    line,
                    0.0,
                    TextFormat {
                        font_id: font.clone(),
                        color: fallback,
                        ..Default::default()
                    },
                );
                continue;
            }
            match h.highlight_line(line, &self.ps) {
                Ok(regions) => {
                    for (style, piece) in regions {
                        let fg = style.foreground;
                        let mut fmt = TextFormat {
                            font_id: font.clone(),
                            color: Color32::from_rgb(fg.r, fg.g, fg.b),
                            ..Default::default()
                        };
                        if style.font_style.contains(FontStyle::ITALIC) {
                            fmt.italics = true;
                        }
                        if style.font_style.contains(FontStyle::UNDERLINE) {
                            fmt.underline = eframe::egui::Stroke::new(1.0_f32, fmt.color);
                        }
                        job.append(piece, 0.0, fmt);
                    }
                }
                Err(_) => {
                    // エラー後の HighlightLines は内部状態が壊れている可能性が
                    // あるので、以降の行のために作り直す。
                    h = HighlightLines::new(syntax, theme);
                    job.append(
                        line,
                        0.0,
                        TextFormat {
                            font_id: font.clone(),
                            color: fallback,
                            ..Default::default()
                        },
                    );
                }
            }
        }

        self.cache_put(key, &job);

        job
    }

    /// 連続した行の並びを 1 パスで色分けし、**行ごとの (開始, 終了, 色)** を返す。
    ///
    /// 差分ビューのように「行を 1 本ずつ別ウィジェットで描く」画面のための API。
    /// 1 行ずつ [`Self::layout_job`] を呼ぶとキャッシュが行数ぶん溢れるうえ、
    /// 複数行にまたがる文字列やブロックコメントの状態が毎行リセットされて
    /// 色が壊れる。ここでは syntect の状態を行を跨いで持ち回る。
    ///
    /// 範囲は**各行の先頭を 0 とするバイトオフセット**の半開区間で、必ず
    /// 昇順・連続・行長に収まる (呼び出し側はそのまま `&line[s..e]` できる)。
    /// 合計が [`MAX_HIGHLIGHT_BYTES`] を超える / 言語が Plain Text /
    /// テーマが無い場合は**空の Vec** を返す (= 呼び出し側は素の色で描く)。
    pub fn line_spans(
        &self,
        lines: &[&str],
        lang: &str,
        theme_name: &str,
    ) -> Vec<Vec<(usize, usize, Color32)>> {
        let total: usize = lines.iter().map(|l| l.len() + 1).sum();
        let syntax = self
            .ps
            .find_syntax_by_name(lang)
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());
        if total > MAX_HIGHLIGHT_BYTES || syntax.name == "Plain Text" {
            return Vec::new();
        }
        let Some(theme) = self.ts.themes.get(theme_name) else {
            return Vec::new();
        };
        let mut h = HighlightLines::new(syntax, theme);
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
                // 極端に長い 1 行 (minify 済み JS など) で UI を止めない。
                out.push(Vec::new());
                continue;
            }
            // syntect は行末の改行込みで状態を進めるので付けて渡し、
            // 範囲は元の行長で丸める。
            let with_nl = format!("{line}\n");
            match h.highlight_line(&with_nl, &self.ps) {
                Ok(regions) => {
                    let mut spans: Vec<(usize, usize, Color32)> = Vec::with_capacity(regions.len());
                    let mut off = 0usize;
                    for (style, piece) in regions {
                        let start = off.min(line.len());
                        off += piece.len();
                        let end = off.min(line.len());
                        if end > start {
                            let fg = style.foreground;
                            spans.push((start, end, Color32::from_rgb(fg.r, fg.g, fg.b)));
                        }
                    }
                    out.push(spans);
                }
                Err(_) => {
                    // エラー後の HighlightLines は内部状態が壊れている可能性が
                    // あるので、以降の行のために作り直す。
                    h = HighlightLines::new(syntax, theme);
                    out.push(Vec::new());
                }
            }
        }
        out
    }

    /// キャッシュへ 1 件入れる。上限は古い方から追い出す (全消しすると
    /// 次のフレームに全再ハイライトのスパイクが出るため)。
    fn cache_put(&self, key: u64, job: &LayoutJob) {
        if let Ok(mut guard) = self.cache.lock() {
            let (map, order) = &mut *guard;
            while map.len() >= HL_CACHE_CAP {
                match order.pop_front() {
                    Some(old) => {
                        map.remove(&old);
                    }
                    None => {
                        map.clear();
                        break;
                    }
                }
            }
            if map.insert(key, job.clone()).is_none() {
                order.push_back(key);
            }
        }
    }
}

/// プラグイン定義の言語を 1 文書ぶん塗って `job` へ積む。
/// 走査は 1 行ごと ([`grammar::scan_line`]) で、行を跨ぐコメント・文字列は
/// `ScanState` が引き継ぐ。長すぎる 1 行は syntect 経路と同じく素通しにする。
fn append_grammar(
    job: &mut LayoutJob,
    text: &str,
    g: &Grammar,
    pal: &Palette,
    font: &FontId,
    fallback: Color32,
) {
    let mut st = ScanState::default();
    let mut spans: Vec<Span> = Vec::new();
    for line in LinesWithEndings::from(text) {
        if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
            job.append(
                line,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: fallback,
                    ..Default::default()
                },
            );
            continue;
        }
        spans.clear();
        grammar::scan_line(g, line, &mut st, &mut spans);
        for s in &spans {
            let i = s.tok.index();
            let mut fmt = TextFormat {
                font_id: font.clone(),
                color: pal.fg[i],
                ..Default::default()
            };
            if pal.italic[i] {
                fmt.italics = true;
            }
            if pal.underline[i] {
                fmt.underline = eframe::egui::Stroke::new(1.0_f32, fmt.color);
            }
            job.append(&line[s.start..s.end], 0.0, fmt);
        }
    }
}

// ===========================================================================
// 構造解析レイヤ (折りたたみ / インデントガイド / スティッキースクロール)
//
// ここは **描画を一切しない純関数の層**。app.rs 側の描画コードは
// `fold_ranges` / `indent_guides` / `active_guide` / `sticky_headers` を
// 呼ぶだけでよく、言語ごとの知識 (コメント記号・括弧・見出し) は
// すべて [`LANG_SPECS`] という 1 枚のデータ表に閉じ込めてある。
// 新しい言語を足すときは表に 1 行足すだけで、関数側は触らない。
//
// --- UI (app.rs) 側の配線の手引き -------------------------------------------
//
// 行番号はすべて **0 始まり** (`text.split('\n')` の添字)。`Editor::cursor` は
// 1 始まりなので、渡す前に 1 を引くこと。
//
// * 折りたたみ: 毎フレーム `buf.refresh_folds()` を呼ぶ (本文が変わった
//   ときだけ中で再計算する)。ガターは `buf.folds.marker(line)` で ▼/▶ を
//   描き、クリックで `buf.folds.toggle_fold(line)`。描画時は
//   `buf.folds.hidden_spans()` で隠す行区間をまとめて取る。
//   編集で行が増減したら `buf.folds.shift_lines(at, delta)` を先に呼ぶと
//   畳んだ状態が編集を跨いで生き残る。
// * インデントガイド: `indent_guides(&buf.text, tab_width)[line].1` が
//   その行で縦線を引く桁位置。`active_guide(&buf.text, tab_width, caret)`
//   の桁だけ色を変えると VS Code と同じ見た目になる。
// * スティッキースクロール: `sticky_headers(&buf.text, &buf.lang,
//   最上部の可視行, 最大段数)` を上端に重ねて描く。
// ===========================================================================

/// 折りたたみ範囲の計算方式。言語ごとに [`LANG_SPECS`] で選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldStrategy {
    /// 括弧の対応で畳む (C 系)。文字列・コメントの中の括弧は数えない。
    Brackets,
    /// インデントの深さで畳む (Python / YAML / 既定)。どの言語でも動く。
    Indent,
    /// 見出しの階層で畳む (Markdown)。インデントも併用する。
    Markdown,
}

/// 折りたたみ範囲の種類。UI はこれでガターの記号や色を変えられる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    /// 括弧 / フェンスで囲まれたブロック。
    Block,
    /// インデントで作られたブロック。
    Indent,
    /// 複数行コメント (ブロックコメント、または連続した行コメント)。
    Comment,
    /// Markdown の見出し節。
    Section,
}

/// 折りたたみ 1 個ぶんの範囲。
///
/// **契約**: `start_line` は畳んでも**表示され続ける**行 (ヘッダ行)。
/// `end_line` は畳んだときに隠れる**最後の行**。よって隠れるのは
/// `start_line+1 ..= end_line` で、`end_line > start_line` が常に成り立つ。
/// 行番号はすべて 0 始まり (`text.split('\n')` の添字)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRange {
    pub start_line: usize,
    pub end_line: usize,
    pub kind: FoldKind,
}

#[allow(dead_code)]
impl FoldRange {
    /// この範囲を畳んだときに `line` が隠れるか。
    pub fn hides(&self, line: usize) -> bool {
        line > self.start_line && line <= self.end_line
    }
    /// 隠れる行数。
    pub fn hidden_len(&self) -> usize {
        self.end_line - self.start_line
    }
}

/// 言語ごとの構文知識。**リテラルを関数側に散らさないための唯一の置き場**。
pub struct LangSpec {
    /// syntect の syntax 名 (大文字小文字は無視して完全一致で引く)。
    pub names: &'static [&'static str],
    /// 折りたたみの計算方式。
    pub strategy: FoldStrategy,
    /// 行コメントの開始トークン。
    pub line_comment: &'static [&'static str],
    /// ブロックコメントの (開始, 終了) トークン。
    pub block_comment: &'static [(&'static str, &'static str)],
    /// 折りたたみに使う括弧の (開き, 閉じ)。
    pub brackets: &'static [(char, char)],
    /// 文字列リテラルの引用符 (この中の括弧・コメントは無視する)。
    pub quotes: &'static [char],
    /// 文字列中のエスケープ文字。
    pub escape: Option<char>,
}

// --- 表の中で使い回す共通の集合 (重複リテラルを 1 か所に) ---
const BR_CURLY: &[(char, char)] = &[('{', '}'), ('[', ']'), ('(', ')')];
const BR_PAREN: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];
const CM_SLASH: &[&str] = &["//"];
const CM_HASH: &[&str] = &["#"];
const CM_DASH: &[&str] = &["--"];
const CM_PERCENT: &[&str] = &["%"];
const CM_SEMI: &[&str] = &[";"];
const CM_HASH_SEMI: &[&str] = &["#", ";"];
const BC_C: &[(&str, &str)] = &[("/*", "*/")];
const BC_XML: &[(&str, &str)] = &[("<!--", "-->")];
const BC_NONE: &[(&str, &str)] = &[];
const CM_NONE: &[&str] = &[];
const Q_D: &[char] = &['"'];
const Q_DS: &[char] = &['"', '\''];
const Q_NONE: &[char] = &[];
const ESC: Option<char> = Some('\\');

/// 言語データ表。**上から順に最初に名前が一致したものを使う**。
///
/// 名前は syntect の `SyntaxSet::load_defaults_newlines()` が返す syntax 名。
/// 表にない言語は [`DEFAULT_LANG_SPEC`] (インデント方式) にフォールバックするので、
/// 未知の言語でも折りたたみは必ず動く。
pub static LANG_SPECS: &[LangSpec] = &[
    // Rust は `'a` がライフタイムなので、シングルクォートを文字列扱いしない。
    LangSpec {
        names: &["Rust"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    // C 系 (シングルクォートは文字リテラル)
    LangSpec {
        names: &[
            "C",
            "C++",
            "C#",
            "D",
            "Go",
            "Java",
            "Javascript",
            "JavaScript",
            "TypeScript",
            "TypeScriptReact",
            "JavaScript (Babel)",
            "JavaScript (Rails)",
            "Java Server Page (JSP)",
            "Objective-C",
            "Objective-C++",
            "PHP",
            "Scala",
            "Groovy",
            "Swift",
            "Kotlin",
            "Dart",
            "Pascal",
            "Vala",
            "Zig",
        ],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    // JSON / JSONC (この製品は jsonc.rs でコメント付き JSON を読む)
    LangSpec {
        names: &["JSON", "JSONC", "JSON5"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    LangSpec {
        names: &["CSS", "SCSS", "Sass", "LESS", "Stylus"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Perl"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["R", "Rd (R Documentation)"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Lisp", "Clojure", "Scheme", "Emacs Lisp"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SEMI,
        block_comment: BC_NONE,
        brackets: BR_PAREN,
        quotes: Q_D,
        escape: ESC,
    },
    // Python: 三重引用符は「複数行コメント」として畳む (docstring)
    LangSpec {
        names: &["Python"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: &[("\"\"\"", "\"\"\""), ("'''", "'''")],
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["YAML", "YAML Front Matter"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["TOML", "INI", "Java Properties", "Git Config"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH_SEMI,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &[
            "Shell-Unix-Generic",
            "Bourne Again Shell (bash)",
            "Batch File",
            "Makefile",
            "Dockerfile",
            "CMake",
        ],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Ruby", "Ruby on Rails", "Ruby Haml"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: &[("=begin", "=end")],
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Lua"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_DASH,
        block_comment: &[("--[[", "]]")],
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["SQL", "SQL (Rails)"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_DASH,
        block_comment: BC_C,
        brackets: BR_PAREN,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Haskell", "Literate Haskell"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_DASH,
        block_comment: &[("{-", "-}")],
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    LangSpec {
        names: &["Erlang", "MATLAB", "LaTeX", "TeX", "BibTeX"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_PERCENT,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    LangSpec {
        names: &[
            "HTML",
            "XML",
            "HTML (Rails)",
            "HTML (ASP)",
            "HTML (Tcl)",
            "SVG",
            "XSL",
        ],
        strategy: FoldStrategy::Indent,
        line_comment: CM_NONE,
        block_comment: BC_XML,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Markdown", "MultiMarkdown", "Markdown (GFM)"],
        strategy: FoldStrategy::Markdown,
        line_comment: CM_NONE,
        block_comment: BC_XML,
        brackets: BR_CURLY,
        quotes: Q_NONE,
        escape: None,
    },
    // 差分ビューはコメントも括弧も無い (`-` で始まる行を行コメント扱いしない)
    LangSpec {
        names: &["Diff", "Cargo Build Results"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_NONE,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_NONE,
        escape: None,
    },
];

/// 表に無い言語のフォールバック。インデント方式なのでどの言語でも壊れない。
pub static DEFAULT_LANG_SPEC: LangSpec = LangSpec {
    names: &["Plain Text"],
    strategy: FoldStrategy::Indent,
    line_comment: CM_NONE,
    block_comment: BC_NONE,
    brackets: BR_CURLY,
    quotes: Q_NONE,
    escape: None,
};

/// プラグイン定義の言語ぶんの [`LangSpec`]。文法データ ([`Grammar`]) から作り、
/// **言語名につき 1 回だけ**確保して `&'static` に固定する。プラグインの
/// 読み直しで何度 `set_grammars` が呼ばれても、同じ名前なら作り直さない。
static DYNAMIC_SPECS: std::sync::OnceLock<Mutex<HashMap<String, &'static LangSpec>>> =
    std::sync::OnceLock::new();

fn dynamic_specs() -> &'static Mutex<HashMap<String, &'static LangSpec>> {
    DYNAMIC_SPECS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// 文法データから折りたたみ用の言語仕様を作る。
fn spec_from_grammar(g: &Grammar) -> LangSpec {
    let names: &'static [&'static str] = Box::leak(vec![leak_str(&g.name)].into_boxed_slice());
    let line_comment: &'static [&'static str] = Box::leak(
        g.line_comment
            .iter()
            .map(|s| leak_str(s))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let block_comment: &'static [(&'static str, &'static str)] = Box::leak(
        g.block_comment
            .iter()
            .chain(g.doc_block.iter())
            .map(|(a, b)| (leak_str(a), leak_str(b)))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    // 折りたたみは「文字列の中の括弧を数えない」ためだけに引用符を見る。
    // 1 文字の開き記号だけを渡せば足りる。
    let mut qs: Vec<char> = g
        .strings
        .iter()
        .filter_map(|r| {
            let mut it = r.open.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        })
        .collect();
    qs.sort_unstable();
    qs.dedup();
    let quotes: &'static [char] = Box::leak(qs.into_boxed_slice());
    LangSpec {
        names,
        strategy: match g.fold {
            FoldKindSpec::Brackets => FoldStrategy::Brackets,
            FoldKindSpec::Indent => FoldStrategy::Indent,
            FoldKindSpec::Markdown => FoldStrategy::Markdown,
        },
        line_comment,
        block_comment,
        brackets: BR_CURLY,
        quotes,
        escape: g.escape,
    }
}

/// プラグインの言語定義を折りたたみ側へ登録する ([`Highlighter::set_grammars`] から)。
pub fn register_lang_specs(set: &GrammarSet) {
    let Ok(mut map) = dynamic_specs().lock() else {
        return;
    };
    for g in &set.grammars {
        let key = g.name.to_lowercase();
        if map.contains_key(&key) {
            continue;
        }
        let spec: &'static LangSpec = Box::leak(Box::new(spec_from_grammar(g)));
        map.insert(key, spec);
    }
}

/// syntax 名から言語仕様を引く。**組み込みの表 → プラグイン定義**の順に見る
/// (組み込みを先に見るので、既存言語の挙動はプラグインで変わらない)。
/// どちらにも無ければ [`DEFAULT_LANG_SPEC`]。
pub fn lang_spec(lang: &str) -> &'static LangSpec {
    if let Some(s) = LANG_SPECS
        .iter()
        .find(|s| s.names.iter().any(|n| n.eq_ignore_ascii_case(lang)))
    {
        return s;
    }
    if let Ok(map) = dynamic_specs().lock() {
        if let Some(s) = map.get(&lang.to_lowercase()) {
            return s;
        }
    }
    &DEFAULT_LANG_SPEC
}

/// インデント幅の既定値 (タブ 1 個ぶんの桁数)。UI が設定値を持つなら
/// `fold_ranges_with` / `indent_guides` にそれを渡す。
pub const DEFAULT_TAB_WIDTH: usize = 4;

/// 構造解析を諦める行数。これを超えるファイルは巨大ファイルモード
/// (editor.rs) で読み取り専用になるため、折りたたみ計算は省く。
pub const FOLD_MAX_LINES: usize = 200_000;

/// 1 行の見た目のインデント桁数。空白だけの行は `None` (空行)。
/// タブは次の `tab_width` の倍数まで進む (エディタの表示と同じ勘定)。
fn visual_indent(line: &str, tab_width: usize) -> Option<usize> {
    let tw = tab_width.max(1);
    let mut col = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => col += 1,
            '\t' => col = (col / tw + 1) * tw,
            '\r' => {}
            _ => return Some(col),
        }
    }
    None
}

/// 字句スキャンの結果。コメント・括弧の位置だけを持つ最小限の情報。
#[derive(Default)]
struct SourceScan {
    /// 行コメントだけの行か (行ごと)。
    comment_only: Vec<bool>,
    /// 複数行にまたがるブロックコメントの (開始行, 終了行)。
    block_comments: Vec<(usize, usize)>,
    /// 対応の取れた括弧の (開き行, 閉じ行)。同一行のものは含まない。
    brackets: Vec<(usize, usize)>,
}

/// 文字列・コメントを飛ばしながら 1 パスで走査する。
///
/// 単一行文字列を前提にしている (行末で強制的に閉じる)。Rust の生文字列
/// `r#".."#` や JS のテンプレートリテラルの複数行は正確に追えないが、
/// 折りたたみが少しずれるだけで壊れはしない。
fn scan_source(text: &str, spec: &LangSpec) -> SourceScan {
    let mut out = SourceScan::default();
    // (終了トークン, 開始行)
    let mut in_block: Option<(&'static str, usize)> = None;
    // (期待する閉じ括弧, 開いた行)
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut last_line = 0usize;
    for (ln, raw) in text.split('\n').enumerate() {
        last_line = ln;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let mut saw_code = false;
        let mut saw_line_comment = false;
        let mut i = 0usize;
        while i < line.len() {
            let rest = &line[i..];
            if let Some((close, start)) = in_block {
                match rest.find(close) {
                    Some(p) => {
                        if ln > start {
                            out.block_comments.push((start, ln));
                        }
                        in_block = None;
                        i += p + close.len();
                        continue;
                    }
                    None => break,
                }
            }
            let ch = match rest.chars().next() {
                Some(c) => c,
                None => break,
            };
            if ch.is_whitespace() {
                i += ch.len_utf8();
                continue;
            }
            if spec.line_comment.iter().any(|t| rest.starts_with(*t)) {
                saw_line_comment = true;
                break;
            }
            if let Some(bc) = spec.block_comment.iter().find(|p| rest.starts_with(p.0)) {
                in_block = Some((bc.1, ln));
                i += bc.0.len();
                continue;
            }
            if spec.quotes.contains(&ch) {
                saw_code = true;
                i += ch.len_utf8();
                while i < line.len() {
                    let c = match line[i..].chars().next() {
                        Some(c) => c,
                        None => break,
                    };
                    i += c.len_utf8();
                    if Some(c) == spec.escape {
                        if let Some(n) = line[i..].chars().next() {
                            i += n.len_utf8();
                        }
                        continue;
                    }
                    if c == ch {
                        break;
                    }
                }
                continue;
            }
            if let Some(b) = spec.brackets.iter().find(|p| p.0 == ch) {
                saw_code = true;
                stack.push((b.1, ln));
                i += ch.len_utf8();
                continue;
            }
            if spec.brackets.iter().any(|p| p.1 == ch) {
                saw_code = true;
                // 対応する開きを探す。見つからない/入れ違いは黙って捨てる
                // (壊れたソースでも panic しないことを最優先)。
                if let Some(pos) = stack.iter().rposition(|p| p.0 == ch) {
                    let open_ln = stack[pos].1;
                    stack.truncate(pos);
                    if ln > open_ln {
                        out.brackets.push((open_ln, ln));
                    }
                }
                i += ch.len_utf8();
                continue;
            }
            saw_code = true;
            i += ch.len_utf8();
        }
        out.comment_only.push(saw_line_comment && !saw_code);
    }
    // 閉じられていないブロックコメントは末尾まで畳めるようにする
    if let Some((_, start)) = in_block {
        if last_line > start {
            out.block_comments.push((start, last_line));
        }
    }
    out
}

/// インデント方式の折りたたみ範囲を積む。O(行数)。
///
/// 「自分より深い行が続く行」がヘッダになり、末尾の空行は範囲に含めない
/// (空行まで畳むと、次のブロックとの間の余白まで消えて読みにくいため)。
fn indent_folds(lines: &[&str], tab_width: usize, out: &mut Vec<FoldRange>) {
    let ind: Vec<Option<usize>> = lines.iter().map(|l| visual_indent(l, tab_width)).collect();
    // (インデント, 行)
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut prev_nonblank: Option<usize> = None;
    let mut close = |stack: &mut Vec<(usize, usize)>, upto: usize, prev: Option<usize>| {
        while let Some(&(ti, tl)) = stack.last() {
            if ti >= upto {
                stack.pop();
                if let Some(p) = prev {
                    if p > tl {
                        out.push(FoldRange {
                            start_line: tl,
                            end_line: p,
                            kind: FoldKind::Indent,
                        });
                    }
                }
            } else {
                break;
            }
        }
    };
    for (ln, cur) in ind.iter().enumerate() {
        let Some(a) = *cur else { continue };
        close(&mut stack, a, prev_nonblank);
        stack.push((a, ln));
        prev_nonblank = Some(ln);
    }
    // 末尾まで来たら全部閉じる (深さ 0 以上は必ず条件を満たす)。
    close(&mut stack, 0, prev_nonblank);
}

/// ATX 見出しの階層 (`#` の数)。見出しでなければ `None`。
fn heading_level(line: &str) -> Option<usize> {
    let t = line.strip_suffix('\r').unwrap_or(line);
    let lead = t.len() - t.trim_start_matches(' ').len();
    if lead > 3 {
        return None;
    }
    let t = &t[lead..];
    let hashes = t.len() - t.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    match t[hashes..].chars().next() {
        None | Some(' ') | Some('\t') => Some(hashes),
        _ => None,
    }
}

/// コードフェンスの開始トークン (``` または ~~~)。
fn fence_token(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Markdown の見出し節とコードフェンスを積む。
fn markdown_folds(lines: &[&str], out: &mut Vec<FoldRange>) {
    let mut fence: Option<(usize, &'static str)> = None;
    // (見出しレベル, 行)
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut last_content: Option<usize> = None;
    for (ln, raw) in lines.iter().enumerate() {
        if let Some((fl, tok)) = fence {
            last_content = Some(ln);
            if raw.trim_start().starts_with(tok) {
                if ln > fl {
                    out.push(FoldRange {
                        start_line: fl,
                        end_line: ln,
                        kind: FoldKind::Block,
                    });
                }
                fence = None;
            }
            continue;
        }
        if let Some(tok) = fence_token(raw) {
            fence = Some((ln, tok));
            last_content = Some(ln);
            continue;
        }
        if let Some(level) = heading_level(raw) {
            while let Some(&(l, sl)) = stack.last() {
                if l >= level {
                    stack.pop();
                    if let Some(c) = last_content {
                        if c > sl {
                            out.push(FoldRange {
                                start_line: sl,
                                end_line: c,
                                kind: FoldKind::Section,
                            });
                        }
                    }
                } else {
                    break;
                }
            }
            stack.push((level, ln));
            last_content = Some(ln);
            continue;
        }
        if !raw.trim().is_empty() {
            last_content = Some(ln);
        }
    }
    if let Some((fl, _)) = fence {
        // 閉じ忘れフェンスは末尾まで
        if lines.len() > fl + 1 {
            out.push(FoldRange {
                start_line: fl,
                end_line: lines.len() - 1,
                kind: FoldKind::Block,
            });
        }
    }
    while let Some((_, sl)) = stack.pop() {
        if let Some(c) = last_content {
            if c > sl {
                out.push(FoldRange {
                    start_line: sl,
                    end_line: c,
                    kind: FoldKind::Section,
                });
            }
        }
    }
}

/// 同じ開始行の範囲は**いちばん広いものだけ**残し、開始行の昇順に並べる。
fn normalize_folds(mut v: Vec<FoldRange>, line_count: usize) -> Vec<FoldRange> {
    v.retain(|r| r.end_line > r.start_line && r.start_line + 1 < line_count);
    for r in v.iter_mut() {
        r.end_line = r.end_line.min(line_count.saturating_sub(1));
    }
    v.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then(b.end_line.cmp(&a.end_line))
    });
    v.dedup_by_key(|r| r.start_line);
    v
}

/// バッファ全体の折りたたみ範囲を計算する (既定のタブ幅)。
///
/// - 行番号は 0 始まり。
/// - 同じ開始行の範囲は 1 個だけ (いちばん広いもの)。
/// - 開始行の昇順。入れ子は「外側が先」に並ぶ。
pub fn fold_ranges(text: &str, lang: &str) -> Vec<FoldRange> {
    fold_ranges_with(text, lang, DEFAULT_TAB_WIDTH)
}

/// タブ幅を指定して折りたたみ範囲を計算する。
pub fn fold_ranges_with(text: &str, lang: &str, tab_width: usize) -> Vec<FoldRange> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() > FOLD_MAX_LINES {
        return Vec::new();
    }
    let spec = lang_spec(lang);
    let scan = scan_source(text, spec);
    let mut out: Vec<FoldRange> = Vec::new();
    for (s, e) in scan.block_comments.iter().copied() {
        out.push(FoldRange {
            start_line: s,
            end_line: e,
            kind: FoldKind::Comment,
        });
    }
    // 連続した行コメントの塊もひとまとめに畳めるようにする (VS Code と同じ)
    let n = scan.comment_only.len();
    let mut i = 0usize;
    while i < n {
        if scan.comment_only[i] {
            let mut j = i;
            while j + 1 < n && scan.comment_only[j + 1] {
                j += 1;
            }
            if j > i {
                out.push(FoldRange {
                    start_line: i,
                    end_line: j,
                    kind: FoldKind::Comment,
                });
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    match spec.strategy {
        FoldStrategy::Brackets => {
            for (s, e) in scan.brackets.iter().copied() {
                out.push(FoldRange {
                    start_line: s,
                    end_line: e,
                    kind: FoldKind::Block,
                });
            }
        }
        FoldStrategy::Indent => indent_folds(&lines, tab_width, &mut out),
        FoldStrategy::Markdown => {
            markdown_folds(&lines, &mut out);
            indent_folds(&lines, tab_width, &mut out);
        }
    }
    normalize_folds(out, lines.len())
}

/// 各行の「実効インデント」。空行は前後の非空行の**浅い方**を継ぐ
/// (ブロックの切れ目でガイドが宙に浮かないようにするため)。
fn effective_indents(lines: &[&str], tab_width: usize) -> Vec<usize> {
    let raw: Vec<Option<usize>> = lines.iter().map(|l| visual_indent(l, tab_width)).collect();
    let n = raw.len();
    let mut before = vec![0usize; n];
    let mut cur = 0usize;
    let mut seen = false;
    for i in 0..n {
        match raw[i] {
            Some(v) => {
                cur = v;
                seen = true;
                before[i] = v;
            }
            None => before[i] = if seen { cur } else { 0 },
        }
    }
    let mut after = vec![0usize; n];
    cur = 0;
    seen = false;
    for i in (0..n).rev() {
        match raw[i] {
            Some(v) => {
                cur = v;
                seen = true;
                after[i] = v;
            }
            None => after[i] = if seen { cur } else { 0 },
        }
    }
    (0..n)
        .map(|i| match raw[i] {
            Some(v) => v,
            None => before[i].min(after[i]),
        })
        .collect()
}

/// インデントガイドを引く桁を行ごとに返す。
///
/// **契約**: 戻り値は行数ぶんの要素を持ち、`v[i].0 == i` (0 始まりの行番号)。
/// `v[i].1` はその行で縦線を引く**桁位置**の昇順リスト (0, tab_width, ...)。
/// 空行は前後の文脈から桁を引き継ぐので、ブロックの途中の空行でも線が途切れない。
#[allow(dead_code)]
pub fn indent_guides(text: &str, tab_width: usize) -> Vec<(usize, Vec<usize>)> {
    let tw = tab_width.max(1);
    let lines: Vec<&str> = text.split('\n').collect();
    effective_indents(&lines, tw)
        .into_iter()
        .enumerate()
        .map(|(i, ind)| {
            let mut cols = Vec::new();
            let mut c = 0usize;
            while c < ind {
                cols.push(c);
                c += tw;
            }
            (i, cols)
        })
        .collect()
}

/// 強調表示するガイド (キャレットを含むブロック)。VS Code の
/// `editor.guides.highlightActiveIndentation` 相当。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ActiveGuide {
    /// 強調する縦線の桁位置。
    pub column: usize,
    /// 強調する範囲の最初の行 (ブロックの中身の先頭)。
    pub start_line: usize,
    /// 強調する範囲の最後の行。
    pub end_line: usize,
}

/// キャレット行から見た「今いるブロック」のガイドを求める。
///
/// キャレットがブロックを**開く行**にいるとき (次の行がより深いとき) は、
/// その開いたブロックのガイドを返す。深さ 0 で何も囲んでいなければ `None`。
#[allow(dead_code)]
pub fn active_guide(text: &str, tab_width: usize, caret_line: usize) -> Option<ActiveGuide> {
    let tw = tab_width.max(1);
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    if caret_line >= n {
        return None;
    }
    let eff = effective_indents(&lines, tw);
    let raw: Vec<Option<usize>> = lines.iter().map(|l| visual_indent(l, tw)).collect();
    let own = eff[caret_line];
    let opens = raw[(caret_line + 1).min(n)..]
        .iter()
        .flatten()
        .next()
        .is_some_and(|&v| v > own);
    let column = if opens {
        (own / tw) * tw
    } else if own == 0 {
        return None;
    } else {
        ((own - 1) / tw) * tw
    };
    let anchor = if eff[caret_line] > column {
        caret_line
    } else {
        caret_line + 1
    };
    if anchor >= n || eff[anchor] <= column {
        return None;
    }
    let mut s = anchor;
    while s > 0 && eff[s - 1] > column {
        s -= 1;
    }
    let mut e = anchor;
    while e + 1 < n && eff[e + 1] > column {
        e += 1;
    }
    Some(ActiveGuide {
        column,
        start_line: s,
        end_line: e,
    })
}

/// 画面上端に貼り付けておく「今いる文脈」の行 (VS Code のスティッキースクロール)。
///
/// **契約**: `top_visible_line` (0 始まり) より上にあって、その行をまだ
/// 囲んでいる範囲のヘッダ行を、**外側から順に**返す。`max_rows` を超える
/// ぶんは内側 (深い方) から落とす。戻り値は `(行番号, 行の本文)` で、
/// 行末の空白は落としてある。コメントの塊はヘッダにしない。
#[allow(dead_code)]
pub fn sticky_headers(
    text: &str,
    lang: &str,
    top_visible_line: usize,
    max_rows: usize,
) -> Vec<(usize, String)> {
    if max_rows == 0 || top_visible_line == 0 {
        return Vec::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let spec = lang_spec(lang);
    let mut heads: Vec<usize> = fold_ranges(text, lang)
        .into_iter()
        .filter(|r| r.kind != FoldKind::Comment)
        .filter(|r| r.start_line < top_visible_line && top_visible_line <= r.end_line)
        .map(|r| header_line(&lines, r.start_line, spec))
        .collect();
    heads.sort_unstable();
    heads.dedup();
    heads.retain(|&l| lines.get(l).is_some_and(|s| !s.trim().is_empty()));
    heads.truncate(max_rows);
    heads
        .into_iter()
        .map(|l| (l, lines[l].trim_end().to_string()))
        .collect()
}

/// 開き括弧だけの行 (Allman スタイル) は、その 1 行上を見出しとして使う。
fn header_line(lines: &[&str], start: usize, spec: &LangSpec) -> usize {
    let Some(cur) = lines.get(start) else {
        return start;
    };
    let t = cur.trim();
    let only_brackets = !t.is_empty() && t.chars().all(|c| spec.brackets.iter().any(|p| p.0 == c));
    if !only_brackets {
        return start;
    }
    let mut i = start;
    while i > 0 {
        i -= 1;
        if lines.get(i).is_some_and(|s| !s.trim().is_empty()) {
            return i;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// SyntaxSet / ThemeSet のロードは重いので、テスト全体で 1 個だけ作って共有する。
    /// (syntect の SyntaxSet は Send + Sync なので static に置ける)
    fn hl() -> &'static Highlighter {
        static HL: OnceLock<Highlighter> = OnceLock::new();
        HL.get_or_init(Highlighter::new)
    }

    /// 実プロダクトでも使われているテーマ名 (theme.rs のダークテーマ既定値)。
    const THEME: &str = "base16-ocean.dark";

    fn font() -> FontId {
        FontId::monospace(12.0)
    }

    /// どのテーマ色とも被りにくい番兵色。これが出たら「ハイライトせず素通し」の証拠。
    fn fallback() -> Color32 {
        Color32::from_rgb(1, 2, 3)
    }

    fn job_of(text: &str, lang: &str) -> LayoutJob {
        hl().layout_job(text, lang, THEME, font(), fallback())
    }

    /// スパン列の健全性: 入力文字列を完全に復元し、単調・非重複・境界内・
    /// かつ char 境界を割っていないこと。マルチバイト崩れの検出器。
    fn assert_spans_ok(job: &LayoutJob, text: &str) {
        assert_eq!(job.text, text, "layout job must reproduce the input text");

        let mut prev_end = 0usize;
        for (i, s) in job.sections.iter().enumerate() {
            assert!(
                s.byte_range.start <= s.byte_range.end,
                "section {i} has an inverted range {:?}",
                s.byte_range
            );
            assert_eq!(
                s.byte_range.start, prev_end,
                "section {i} must start where the previous one ended (no gap / no overlap)"
            );
            assert!(
                s.byte_range.end <= job.text.len(),
                "section {i} range {:?} exceeds text length {}",
                s.byte_range,
                job.text.len()
            );
            assert!(
                job.text.is_char_boundary(s.byte_range.start)
                    && job.text.is_char_boundary(s.byte_range.end),
                "section {i} range {:?} splits a UTF-8 char boundary",
                s.byte_range
            );
            prev_end = s.byte_range.end;
        }

        if !text.is_empty() {
            assert_eq!(
                prev_end,
                text.len(),
                "sections must cover the whole text, ending at its length"
            );
        }
    }

    fn color_at(job: &LayoutJob, byte_idx: usize) -> Color32 {
        job.sections
            .iter()
            .find(|s| s.byte_range.start <= byte_idx && byte_idx < s.byte_range.end)
            .map(|s| s.format.color)
            .unwrap_or_else(|| panic!("no section covers byte {byte_idx}"))
    }

    /// `needle` の最初の出現位置の色を返す。
    fn color_of(job: &LayoutJob, needle: &str) -> Color32 {
        let i = job
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in laid out text"));
        color_at(job, i)
    }

    // ---- lang_for -------------------------------------------------------

    #[test]
    fn lang_for_resolves_known_extension() {
        assert_eq!(
            hl().lang_for(Some(Path::new("a.rs")), "fn main() {}"),
            "Rust"
        );
    }

    #[test]
    fn lang_for_unknown_extension_falls_back_to_plain_text() {
        assert_eq!(
            hl().lang_for(Some(Path::new("notes.zzqqxx")), "hello world"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_uses_whole_file_name_when_there_is_no_extension() {
        // 拡張子なしファイル (Makefile 等) は file_name 経由で解決される分岐。
        assert_ne!(
            hl().lang_for(Some(Path::new("/proj/Makefile")), "all:\n\techo hi\n"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_falls_back_to_first_line_when_path_is_none() {
        // シェバンによる判定 (path が無いケース)。
        assert_ne!(
            hl().lang_for(None, "#!/usr/bin/env python3\nprint(1)\n"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_prefers_extension_over_first_line() {
        // 拡張子が勝つこと。シェバンに引きずられて Python にならない。
        assert_eq!(
            hl().lang_for(Some(Path::new("a.rs")), "#!/usr/bin/env python3\n"),
            "Rust"
        );
    }

    #[test]
    fn lang_for_handles_empty_text_without_path() {
        assert_eq!(hl().lang_for(None, ""), "Plain Text");
    }

    #[test]
    fn lang_for_handles_multibyte_first_line() {
        // 日本語だけの 1 行目で first_line 判定に入っても panic しない。
        let lang = hl().lang_for(None, "日本語のテキストです\n2行目\n");
        assert!(!lang.is_empty());
    }

    // ---- lang_for_fence -------------------------------------------------

    #[test]
    fn lang_for_fence_resolves_name_token() {
        assert_eq!(hl().lang_for_fence("rust"), "Rust");
    }

    #[test]
    fn lang_for_fence_resolves_extension_token() {
        assert_eq!(hl().lang_for_fence("py"), "Python");
    }

    #[test]
    fn lang_for_fence_empty_token_is_plain_text() {
        assert_eq!(hl().lang_for_fence(""), "Plain Text");
    }

    #[test]
    fn lang_for_fence_unknown_token_is_plain_text() {
        assert_eq!(hl().lang_for_fence("no-such-language-xyz"), "Plain Text");
    }

    // ---- トークン分類 ---------------------------------------------------

    #[test]
    fn keyword_and_function_name_get_different_colors() {
        let job = job_of("fn main() {}\n", "Rust");
        assert_ne!(
            color_of(&job, "fn"),
            color_of(&job, "main"),
            "keyword and function name must not share a color"
        );
    }

    #[test]
    fn number_literal_differs_from_identifier() {
        let job = job_of("let n = 42;\n", "Rust");
        assert_ne!(color_of(&job, "42"), color_of(&job, "n ="));
    }

    #[test]
    fn comment_and_string_get_different_colors() {
        let comment = job_of("// alpha\n", "Rust");
        let string = job_of("let s = \"alpha\";\n", "Rust");
        assert_ne!(
            color_of(&comment, "alpha"),
            color_of(&string, "alpha"),
            "a comment and a string literal must not share a color"
        );
    }

    // ---- 文字列内の誤分類 -----------------------------------------------

    #[test]
    fn keyword_inside_string_literal_is_not_colored_as_keyword() {
        let text = "let s = \"fn abc\";\n";
        let job = job_of(text, "Rust");
        let inside_fn = job.text.find("\"fn").expect("quote") + 1;
        let inside_abc = job.text.find("abc").expect("abc");

        assert_eq!(
            color_at(&job, inside_fn),
            color_at(&job, inside_abc),
            "`fn` inside a string must be colored like the rest of the string"
        );
        assert_ne!(
            color_at(&job, inside_fn),
            color_of(&job, "let"),
            "`fn` inside a string must not be colored as a keyword"
        );
    }

    #[test]
    fn comment_marker_inside_string_does_not_start_a_comment() {
        let with_marker = job_of("let s = \"// not a comment\";\nlet n = 1;\n", "Rust");
        let baseline = job_of("let n = 1;\n", "Rust");
        assert_eq!(
            color_of(&with_marker, "1"),
            color_of(&baseline, "1"),
            "code after a string containing `//` must still be highlighted as code"
        );
    }

    #[test]
    fn block_comment_marker_inside_string_does_not_open_a_comment() {
        let with_marker = job_of("let s = \"/* open\";\nlet n = 7;\n", "Rust");
        let baseline = job_of("let n = 7;\n", "Rust");
        assert_eq!(color_of(&with_marker, "7"), color_of(&baseline, "7"));
    }

    // ---- 未終端トークン -------------------------------------------------

    #[test]
    fn unterminated_string_is_laid_out_without_panicking() {
        let text = "let s = \"never closed\nlet t = 2;\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_block_comment_is_laid_out_without_panicking() {
        let text = "/* open block\nstill inside\nand still\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_string_at_eof_without_newline_is_laid_out() {
        let text = "let s = \"dangling";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    // ---- マルチバイト ---------------------------------------------------

    #[test]
    fn japanese_comment_does_not_panic_and_preserves_text() {
        let text = "// 日本語のコメント\nfn main() {}\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn japanese_string_literal_does_not_panic_and_preserves_text() {
        let text = "let s = \"日本語\";\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_japanese_string_does_not_panic() {
        // 未終端 × マルチバイトの合わせ技。byte index スライスが char 境界を
        // 割るなら、ここが最初に落ちる。
        let text = "let s = \"日本語のまま閉じない\nlet t = 3;\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn emoji_and_combining_characters_are_preserved() {
        let text = "let s = \"🎌 とれ́ま\"; // 絵文字\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn japanese_survives_the_plain_text_fallback_path() {
        let text = "日本語のプレーンテキスト\n2行目\n";
        let job = job_of(text, "Plain Text");
        assert_spans_ok(&job, text);
    }

    // ---- 空・境界・フォールバック ---------------------------------------

    #[test]
    fn empty_text_produces_no_visible_content() {
        let job = job_of("", "Rust");
        assert_eq!(job.text, "");
    }

    #[test]
    fn empty_text_in_plain_mode_produces_no_visible_content() {
        let job = job_of("", "Plain Text");
        assert_eq!(job.text, "");
    }

    #[test]
    fn blank_and_whitespace_only_lines_are_preserved() {
        let text = "fn a() {}\n\n   \n\nfn b() {}\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn text_without_trailing_newline_is_fully_covered() {
        let text = "fn main() { let x = 1; }";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn crlf_line_endings_are_preserved_verbatim() {
        let text = "fn a() {}\r\n// コメント\r\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unknown_language_uses_the_fallback_color_in_one_section() {
        let text = "fn main() {}\n";
        let job = job_of(text, "No Such Language 12345");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
        assert_eq!(job.text, text);
    }

    #[test]
    fn unknown_theme_falls_back_to_unhighlighted_text() {
        let text = "fn main() {}\n";
        let job = hl().layout_job(text, "Rust", "no-such-theme-xyz", font(), fallback());
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
        assert_eq!(job.text, text);
    }

    #[test]
    fn plain_text_language_skips_highlighting() {
        let text = "fn main() {}\n";
        let job = job_of(text, "Plain Text");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
    }

    #[test]
    fn oversized_text_skips_highlighting() {
        let unit = "fn main() { let x = 1; }\n";
        let text = unit.repeat(MAX_HIGHLIGHT_BYTES / unit.len() + 2);
        assert!(text.len() > MAX_HIGHLIGHT_BYTES);

        let job = job_of(&text, "Rust");
        assert_eq!(
            job.sections.len(),
            1,
            "large files must be laid out in one plain span"
        );
        assert_eq!(job.sections[0].format.color, fallback());
    }

    #[test]
    fn oversized_single_line_is_passed_through_without_highlighting() {
        // minify された JS のような 1 行だけ巨大なテキストでも、その行は
        // 素通し (フォールバック色) にして残りの行はハイライトを続ける。
        let long_line = format!(
            "let s = \"{}\";\n",
            "x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1)
        );
        let text = format!("fn main() {{}}\n{long_line}// tail\n");
        let job = job_of(&text, "Rust");
        assert_spans_ok(&job, &text);

        // 巨大行はフォールバック色で 1 スパンとして追加される
        let long_start = text.find("let s").expect("long line present");
        assert_eq!(color_at(&job, long_start), fallback());
        // 前後の行は通常どおりハイライトされる
        assert_ne!(color_of(&job, "fn"), fallback());
        assert_ne!(color_of(&job, "// tail"), fallback());
    }

    #[test]
    fn requested_font_is_applied_to_every_section() {
        let job = job_of("// コメント\nfn main() { let x = \"s\"; }\n", "Rust");
        assert!(job.sections.iter().all(|s| s.format.font_id == font()));
    }

    #[test]
    fn wrapping_is_disabled_so_the_editor_can_scroll_horizontally() {
        let job = job_of("fn main() {}\n", "Rust");
        assert_eq!(job.wrap.max_width, f32::INFINITY);
    }

    #[test]
    fn mixed_token_document_keeps_spans_well_formed() {
        let text = concat!(
            "// 日本語のコメント: \"fn\" や // を含む\n",
            "/* block\n",
            "   still block */\n",
            "fn main() {\n",
            "    let s = \"fn // /* 日本語 \\\" escaped\";\n",
            "    let n = 0x1F + 42.5;\n",
            "\n",
            "    println!(\"{}\", s);\n",
            "}\n",
        );
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    // =======================================================================
    // 構造解析 (折りたたみ / インデントガイド / スティッキー)
    // =======================================================================

    /// 折りたたみ範囲を `(開始, 終了, 種類)` の並びにして比べやすくする。
    fn folds(text: &str, lang: &str) -> Vec<(usize, usize, FoldKind)> {
        fold_ranges(text, lang)
            .into_iter()
            .map(|r| (r.start_line, r.end_line, r.kind))
            .collect()
    }

    fn starts(text: &str, lang: &str) -> Vec<usize> {
        fold_ranges(text, lang)
            .into_iter()
            .map(|r| r.start_line)
            .collect()
    }

    #[test]
    fn lang_spec_table_picks_strategy() {
        // 表引きが効いているか (名前の大文字小文字は無視)
        let cases = [
            ("Rust", FoldStrategy::Brackets),
            ("rust", FoldStrategy::Brackets),
            ("C++", FoldStrategy::Brackets),
            ("Javascript", FoldStrategy::Brackets),
            ("JSON", FoldStrategy::Brackets),
            ("Python", FoldStrategy::Indent),
            ("YAML", FoldStrategy::Indent),
            ("Markdown", FoldStrategy::Markdown),
            ("Plain Text", FoldStrategy::Indent),
            ("なにか未知の言語", FoldStrategy::Indent),
        ];
        for (lang, want) in cases {
            assert_eq!(lang_spec(lang).strategy, want, "lang={lang}");
        }
        assert_eq!(lang_spec("Python").line_comment, &["#"]);
        assert_eq!(lang_spec("Rust").line_comment, &["//"]);
        // Rust はライフタイム `'a` があるのでシングルクォートを文字列にしない
        assert!(!lang_spec("Rust").quotes.contains(&'\''));
        assert!(lang_spec("C").quotes.contains(&'\''));
    }

    #[test]
    fn fold_rust_nested_braces() {
        let text = "\
fn a() {
    if x {
        y();
    }
}
fn b() {}
";
        let f = folds(text, "Rust");
        assert!(
            f.contains(&(0, 4, FoldKind::Block)),
            "外側の fn が 0..4 で畳める: {f:?}"
        );
        assert!(
            f.contains(&(1, 3, FoldKind::Block)),
            "内側の if が 1..3 で畳める: {f:?}"
        );
        assert!(
            !starts(text, "Rust").contains(&5),
            "1 行で閉じる fn b は畳めない: {f:?}"
        );
    }

    #[test]
    fn fold_rust_block_comment_and_strings() {
        let text = "\
/* これは
   複数行の
   コメント */
fn a() {
    let s = \"} 文字列の中の括弧は数えない {\";
    // 行コメント 1
    // 行コメント 2
    s
}
";
        let f = folds(text, "Rust");
        assert!(
            f.contains(&(0, 2, FoldKind::Comment)),
            "ブロックコメントが畳める: {f:?}"
        );
        assert!(
            f.contains(&(3, 8, FoldKind::Block)),
            "文字列中の括弧に釣られず fn が 3..8: {f:?}"
        );
        assert!(
            f.contains(&(5, 6, FoldKind::Comment)),
            "連続した行コメントも畳める: {f:?}"
        );
    }

    #[test]
    fn fold_rust_broken_source_never_panics() {
        // 閉じ忘れ・入れ違い・未終端文字列でも落ちない
        for text in [
            "fn a() {\n  {\n",
            "}\n}\n{\n",
            "let s = \"未終端\nfn b() {\n}\n",
            "/* 閉じ忘れコメント\nfn c() {\n}\n",
            "",
            "\n\n\n",
        ] {
            let _ = fold_ranges(text, "Rust");
        }
    }

    #[test]
    fn fold_python_indent_and_blank_lines() {
        let text = "\
def outer():
    def inner():
        pass

    return inner

def other():
    pass
";
        let f = folds(text, "Python");
        assert!(
            f.contains(&(0, 4, FoldKind::Indent)),
            "outer は空行を跨いで 0..4 (末尾の空行は含めない): {f:?}"
        );
        assert!(
            f.contains(&(1, 2, FoldKind::Indent)),
            "inner は 1..2: {f:?}"
        );
        assert!(
            f.contains(&(6, 7, FoldKind::Indent)),
            "other は 6..7: {f:?}"
        );
    }

    #[test]
    fn fold_python_docstring_is_comment() {
        let text = "\
def f():
    \"\"\"これは
    docstring
    \"\"\"
    return 1
";
        let f = folds(text, "Python");
        assert!(
            f.contains(&(1, 3, FoldKind::Comment)),
            "docstring が畳める: {f:?}"
        );
        assert!(f.iter().any(|r| r.0 == 0), "def 自体も畳める: {f:?}");
    }

    #[test]
    fn fold_yaml_indent() {
        let text = "\
top:
  a: 1
  b:
    - x
    - y
other: 2
";
        let f = folds(text, "YAML");
        assert!(f.contains(&(0, 4, FoldKind::Indent)), "top は 0..4: {f:?}");
        assert!(f.contains(&(2, 4, FoldKind::Indent)), "b は 2..4: {f:?}");
        assert!(
            !starts(text, "YAML").contains(&5),
            "最終行は畳めない: {f:?}"
        );
    }

    #[test]
    fn fold_markdown_heading_hierarchy() {
        let text = "\
# 見出し1
本文A

## 見出し2
本文B

### 見出し3
本文C

## 見出し2b
本文D
";
        let f = folds(text, "Markdown");
        assert!(
            f.contains(&(0, 10, FoldKind::Section)),
            "# は最後まで: {f:?}"
        );
        assert!(
            f.contains(&(3, 7, FoldKind::Section)),
            "最初の ## は次の ## の直前まで: {f:?}"
        );
        assert!(f.contains(&(6, 7, FoldKind::Section)), "### は 6..7: {f:?}");
        assert!(
            f.contains(&(9, 10, FoldKind::Section)),
            "最後の ## は末尾まで: {f:?}"
        );
    }

    #[test]
    fn fold_markdown_fence_and_heading_in_fence() {
        let text = "\
# 題
```rust
# これは見出しではない
fn a() {}
```
おわり
";
        let f = folds(text, "Markdown");
        assert!(
            f.contains(&(1, 4, FoldKind::Block)),
            "コードフェンスが畳める: {f:?}"
        );
        assert_eq!(
            f.iter().filter(|r| r.2 == FoldKind::Section).count(),
            1,
            "フェンス内の # は見出しにしない: {f:?}"
        );
    }

    #[test]
    fn fold_mixed_tabs_and_spaces() {
        // タブ 1 個 = 4 桁として、空白 4 個と同じ深さに見えること
        let text = "def f():\n\tfirst\n    second\n\t\tdeep\nafter\n";
        let f = folds(text, "Python");
        assert!(
            f.contains(&(0, 3, FoldKind::Indent)),
            "タブと空白を同じ深さとして 0..3: {f:?}"
        );
        assert!(
            f.contains(&(2, 3, FoldKind::Indent)),
            "4 空白の行がタブ 2 個の行を抱える: {f:?}"
        );
        assert!(
            !f.iter().any(|r| r.0 == 1),
            "タブ 1 個と空白 4 個は同じ深さなので親子にならない: {f:?}"
        );
        // タブ幅を変えると深さの比較そのものが変わる
        let t2 = "a:\n\tb\n     c\n";
        let g = |tw: usize| -> Vec<(usize, usize)> {
            fold_ranges_with(t2, "YAML", tw)
                .into_iter()
                .map(|r| (r.start_line, r.end_line))
                .collect()
        };
        assert!(
            g(4).contains(&(1, 2)),
            "タブ幅 4 ならタブ行 (4 桁) より 5 空白の行が深い: {:?}",
            g(4)
        );
        assert!(
            !g(8).contains(&(1, 2)),
            "タブ幅 8 ならタブ行 (8 桁) の方が深い: {:?}",
            g(8)
        );
    }

    #[test]
    fn fold_huge_file_is_skipped() {
        let text = "a\n".repeat(FOLD_MAX_LINES + 1);
        assert!(
            fold_ranges(&text, "Plain Text").is_empty(),
            "行数上限を超えたら計算しない"
        );
    }

    #[test]
    fn fold_ranges_are_sorted_and_unique_by_start() {
        let text = "\
fn a() {
    /* c
       c */
    if x {
    }
}
";
        let r = fold_ranges(text, "Rust");
        for w in r.windows(2) {
            assert!(
                w[0].start_line < w[1].start_line,
                "開始行が昇順かつ一意: {r:?}"
            );
        }
        for x in &r {
            assert!(x.end_line > x.start_line, "空の範囲は無い: {x:?}");
        }
    }

    // ---------------- インデントガイド ----------------

    fn guide_cols(text: &str, tw: usize) -> Vec<Vec<usize>> {
        indent_guides(text, tw)
            .into_iter()
            .enumerate()
            .map(|(i, (l, c))| {
                assert_eq!(i, l, "行番号は添字と一致する");
                c
            })
            .collect()
    }

    #[test]
    fn indent_guides_table() {
        // (説明, 本文, タブ幅, 各行の期待ガイド桁)
        let cases: &[(&str, &str, usize, &[&[usize]])] = &[
            (
                "空白 4 のネスト",
                "a\n    b\n        c\n",
                4,
                &[&[], &[0], &[0, 4], &[]],
            ),
            (
                "タブのネスト",
                "a\n\tb\n\t\tc\n",
                4,
                &[&[], &[0], &[0, 4], &[]],
            ),
            (
                "タブと空白の混在 (同じ深さに見える)",
                "a\n\tb\n    c\n",
                4,
                &[&[], &[0], &[0], &[]],
            ),
            ("タブ幅 2", "a\n  b\n    c\n", 2, &[&[], &[0], &[0, 2], &[]]),
            (
                "深いネスト",
                "a\n    b\n        c\n            d\n",
                4,
                &[&[], &[0], &[0, 4], &[0, 4, 8], &[]],
            ),
        ];
        for (name, text, tw, want) in cases {
            let got = guide_cols(text, *tw);
            let want: Vec<Vec<usize>> = want.iter().map(|v| v.to_vec()).collect();
            assert_eq!(&got, &want, "{name}");
        }
    }

    #[test]
    fn indent_guides_blank_line_inherits_context() {
        // ブロックの途中の空行は線が途切れず、ブロックの切れ目では浅い方を継ぐ
        let text = "def f():\n    a\n\n    b\ndef g():\n    c\n";
        let g = guide_cols(text, 4);
        assert_eq!(g[2], vec![0], "ブロック中の空行はガイドを保つ");
        let text2 = "def f():\n    if x:\n        a\n\n    b\n";
        let g2 = guide_cols(text2, 4);
        assert_eq!(
            g2[3],
            vec![0],
            "ブロックの切れ目の空行は浅い方 (min) を継ぐ"
        );
        let text3 = "\n\n    a\n";
        let g3 = guide_cols(text3, 4);
        assert_eq!(g3[0], Vec::<usize>::new(), "先頭の空行はガイド無し");
    }

    #[test]
    fn active_guide_selection() {
        let text = "\
def f():
    if x:
        a
        b
    c
d
";
        // 中身の行にいるときは、その行を囲むいちばん内側のブロック
        let g = active_guide(text, 4, 2).expect("2 行目は if の中");
        assert_eq!((g.column, g.start_line, g.end_line), (4, 2, 3));
        // ブロックを開く行にいるときは、その開いたブロック
        let g = active_guide(text, 4, 1).expect("1 行目は if 自身");
        assert_eq!((g.column, g.start_line, g.end_line), (4, 2, 3));
        let g = active_guide(text, 4, 0).expect("0 行目は def 自身");
        assert_eq!((g.column, g.start_line, g.end_line), (0, 1, 4));
        // def の中の浅い行
        let g = active_guide(text, 4, 4).expect("4 行目は def の直下");
        assert_eq!((g.column, g.start_line, g.end_line), (0, 1, 4));
        // トップレベルで何も囲んでいない行
        assert!(active_guide(text, 4, 5).is_none(), "深さ 0 は None");
        assert!(active_guide(text, 4, 999).is_none(), "範囲外は None");
    }

    // ---------------- スティッキースクロール ----------------

    const NESTED: &str = "\
mod m {
    fn outer() {
        fn inner() {
            let a = 1;
            let b = 2;
        }
    }
}
fn tail() {
    let c = 3;
}
";

    #[test]
    fn sticky_headers_at_scroll_positions() {
        let heads = |top: usize, max: usize| -> Vec<(usize, String)> {
            sticky_headers(NESTED, "Rust", top, max)
                .into_iter()
                .map(|(l, s)| (l, s.trim().to_string()))
                .collect()
        };
        assert!(heads(0, 5).is_empty(), "先頭では貼るものが無い");
        assert_eq!(
            heads(3, 5),
            vec![
                (0, "mod m {".to_string()),
                (1, "fn outer() {".to_string()),
                (2, "fn inner() {".to_string()),
            ],
            "入れ子の中では外側から順に 3 段"
        );
        assert_eq!(
            heads(3, 2),
            vec![(0, "mod m {".to_string()), (1, "fn outer() {".to_string())],
            "max_rows を超えたら内側から落とす"
        );
        assert!(heads(3, 0).is_empty(), "max_rows=0 は空");
        assert_eq!(
            heads(9, 5),
            vec![(8, "fn tail() {".to_string())],
            "別の関数に入れば貼るものも入れ替わる"
        );
        assert!(
            heads(11, 5).is_empty(),
            "EOF より後ろでは貼るものが無い: {:?}",
            heads(11, 5)
        );
    }

    #[test]
    fn sticky_headers_markdown_uses_headings() {
        let text = "\
# 章
## 節
本文1
本文2
## 節2
本文3
";
        let got: Vec<_> = sticky_headers(text, "Markdown", 3, 5)
            .into_iter()
            .map(|(l, s)| (l, s))
            .collect();
        assert_eq!(
            got,
            vec![(0, "# 章".to_string()), (1, "## 節".to_string())],
            "見出しの階層がそのまま文脈になる"
        );
    }

    #[test]
    fn sticky_headers_skip_comment_blocks_and_allman_brace() {
        // Allman スタイル (開き括弧だけの行) は 1 行上を見出しにする
        let text = "\
/* 長い
   コメント */
int main(int argc)
{
    int a = 1;
    int b = 2;
}
";
        let got: Vec<_> = sticky_headers(text, "C", 5, 5)
            .into_iter()
            .map(|(l, s)| (l, s))
            .collect();
        assert_eq!(
            got,
            vec![(2, "int main(int argc)".to_string())],
            "コメントは貼らず、`{{` の行ではなく宣言行を貼る"
        );
    }
}
