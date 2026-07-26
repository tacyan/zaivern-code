//! 文字コードの入口 — 日本語 Windows で「文字化け」を出さないための境界層。
//!
//! # なぜ必要か
//!
//! Rust の文字列は UTF-8 だが、**Windows から返ってくるバイト列は UTF-8 ではない**。
//!
//! - `powershell` / `netsh` / `cmd` の出力は**コンソールのコードページ**で返る。
//!   日本語 Windows では CP932 (Shift_JIS) なので、`String::from_utf8_lossy` で
//!   受けると「パスが見つかりません」が `\u{fffd}p\u{fffd}X…` になる。
//!   実害は表示だけではない: `err.contains("キャンセル")` のような**文字列照合が
//!   必ず外れる**ので、UAC のキャンセルを「不明なエラー」として扱ってしまう。
//! - 日本語圏のソースコード・ログ・CSV は今も CP932 が現役で、
//!   `std::fs::read_to_string` は不正な UTF-8 でエラーになる
//!   (エディタが「開けませんでした」と言うだけで理由が分からない)。
//!
//! そこで「外から来たバイト列は必ずここを通して `String` にする」という
//! 一枚の層を置く。追加クレートは使わず、変換は Windows の
//! `MultiByteToWideChar` / `WideCharToMultiByte` を直接呼ぶ
//! (OS のコードページ表がそのまま使えるので、変換表を抱え込まなくて済む)。
//!
//! # 使い分け
//!
//! | 用途 | 関数 |
//! |---|---|
//! | 子プロセスの stdout/stderr | [`decode_output`] |
//! | ファイルを読む (BOM/UTF-16 も判定) | [`decode_bytes`] |
//! | ファイルを元の符号化で書き戻す | [`encode_bytes`] |
//!
//! # 方針
//!
//! - **UTF-8 として妥当ならそれを最優先で信じる。** 近年のツールはほぼ UTF-8 で、
//!   CP932 のバイト列が偶然 UTF-8 として妥当になることは稀 (逆は日常的に起きる)。
//! - Windows 以外では OS のコードページ変換を持たないため、
//!   UTF-8 として読めないバイト列は従来どおり lossy で受ける
//!   (mac / Linux で CP932 のファイルを開くのは想定外の使い方なので、
//!   変換表を抱え込むより「化けても落ちない」を選ぶ)。

#![allow(dead_code)]

/// 読み込んだバイト列がどの符号化だったか。保存時に元へ戻すために持ち回る。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Encoding {
    /// UTF-8 (BOM なし) — 既定。
    #[default]
    Utf8,
    /// UTF-8 (BOM 付き)。Excel が吐く CSV などで実際に使われる。
    Utf8Bom,
    /// UTF-16 リトルエンディアン (BOM 付き)。Windows のツールが吐く。
    Utf16Le,
    /// UTF-16 ビッグエンディアン (BOM 付き)。
    Utf16Be,
    /// OS のコードページ (日本語 Windows なら 932 = Shift_JIS)。
    Ansi(u32),
}

impl Encoding {
    /// 画面に出す短い名前。UTF-8 (BOM なし) は既定なので空文字を返し、
    /// 「わざわざ書く価値がある符号化のときだけ」ステータスバーに出せるようにする。
    ///
    /// コードページ番号は OS から受け取った値をそのまま出す。よく知られた
    /// 番号だけは通称を添える (どの言語環境でも同じ扱い — 特定の地域を
    /// 前提にしない)。
    pub fn label(&self) -> String {
        match self {
            Encoding::Utf8 => String::new(),
            Encoding::Utf8Bom => "UTF-8 BOM".to_string(),
            Encoding::Utf16Le => "UTF-16 LE".to_string(),
            Encoding::Utf16Be => "UTF-16 BE".to_string(),
            // コードページが分からない環境 (Windows 以外) で読み替えた場合
            Encoding::Ansi(0) => "不明なコードページ".to_string(),
            Encoding::Ansi(cp) => match common_alias(*cp) {
                Some(name) => format!("CP{cp} ({name})"),
                None => format!("CP{cp}"),
            },
        }
    }

    /// UTF-8 (BOM なし) 以外か = 保存時に変換が要るか。
    pub fn is_legacy(&self) -> bool {
        !matches!(self, Encoding::Utf8)
    }

    /// 省略しない名前。[`label`](Self::label) は既定を空文字にするので、
    /// 「UTF-8 / CRLF」のように**必ず何か書きたい**場所ではこちらを使う。
    pub fn name(&self) -> String {
        match self {
            Encoding::Utf8 => "UTF-8".to_string(),
            other => other.label(),
        }
    }
}

/// Windows の ANSI コードページのうち、番号だけでは伝わりにくいものの通称。
/// 表示専用 — 動作はすべて OS が返した番号で決めるので、ここに無い番号でも
/// そのまま扱える (地域を問わず同じ経路で動く)。
fn common_alias(cp: u32) -> Option<&'static str> {
    Some(match cp {
        932 => "Shift_JIS",
        936 => "GBK",
        949 => "EUC-KR",
        950 => "Big5",
        1250..=1258 => "Windows",
        _ => return None,
    })
}

/// 子プロセスの stdout / stderr を文字列にする。
///
/// UTF-8 として妥当ならそのまま。そうでなければ Windows のコンソール
/// コードページ (OEM → ANSI の順) で読み直す。
///
/// ただし「末尾で切れているだけの UTF-8」はコードページで読み直さない。
/// 出力を途中で打ち切ったり、チャンク境界で切れたバイト列は最後の 1 文字だけが
/// 不完全なので、そこで CP932 として読み直すと**全体**が化ける
/// (壊れているのは末尾の数バイトだけなので lossy で受けるのが正しい)。
pub fn decode_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        // error_len() == None = 「入力が途中で終わった」= 不正な符号化ではない
        Err(e) if e.error_len().is_none() => String::from_utf8_lossy(bytes).into_owned(),
        Err(_) => decode_ansi_or_lossy(bytes, console_code_page()),
    }
}

/// ファイルの中身を文字列にする。BOM と UTF-16 も見る。
/// 返り値の [`Encoding`] を保存時に [`encode_bytes`] へ渡すと元の形で書き戻せる。
pub fn decode_bytes(bytes: &[u8]) -> (String, Encoding) {
    // BOM は最優先。UTF-16 のテキストは UTF-8 としては読めないので、
    // ここで拾わないと「化ける」ではなく「開けない」になる。
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (decode_utf8_or_ansi(rest), Encoding::Utf8Bom);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return (decode_utf16(rest, true), Encoding::Utf16Le);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return (decode_utf16(rest, false), Encoding::Utf16Be);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), Encoding::Utf8),
        Err(_) => {
            let cp = ansi_code_page();
            (decode_ansi_or_lossy(bytes, cp), Encoding::Ansi(cp))
        }
    }
}

/// [`decode_bytes`] で判定した符号化のままバイト列へ戻す。
///
/// 変換できない文字 (CP932 に無い絵文字など) があるときは
/// **UTF-8 として書く**: 元の符号化を守るために文字を落とすと、
/// 保存した瞬間に本文が壊れる (それは文字化けより悪い)。
/// 呼び出し側は返り値の [`Encoding`] を見て、変わっていれば
/// 「UTF-8 で保存した」と伝えられる。
pub fn encode_bytes(text: &str, enc: Encoding) -> (Vec<u8>, Encoding) {
    match enc {
        Encoding::Utf8 => (text.as_bytes().to_vec(), Encoding::Utf8),
        Encoding::Utf8Bom => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(text.as_bytes());
            (out, Encoding::Utf8Bom)
        }
        Encoding::Utf16Le => (encode_utf16(text, true), Encoding::Utf16Le),
        Encoding::Utf16Be => (encode_utf16(text, false), Encoding::Utf16Be),
        Encoding::Ansi(cp) => match encode_ansi(text, cp) {
            Some(bytes) => (bytes, Encoding::Ansi(cp)),
            // 表現できない文字が混ざった / Windows 以外 → UTF-8 へ格上げして保存する
            None => (text.as_bytes().to_vec(), Encoding::Utf8),
        },
    }
}

// ───────────────────────── 改行コード ─────────────────────────
//
// # なぜここにあるか
//
// 「そのバイト列をどう文字にしたか」と「その本文の改行がどれか」は、
// どちらもファイルを開いた瞬間に決まり、保存で元へ戻すために持ち回る情報。
// 同じ場所に置いておくと [`TextFormat`] 一つでステータスバーに
// 「UTF-8 / CRLF」と出せる。
//
// # 現状の保持方針 (2026-07 時点) と推奨
//
// **現状**: [`decode_bytes`] は CR を落とさない。src/editor.rs の open / reload は
// その文字列をそのまま `Buffer.text` に入れ、保存 (`Buffer::write_to`) も
// [`encode_bytes`] へそのまま渡す。つまり **CRLF のファイルは `\r` が本文に乗ったまま**
// 編集される (行末に見えない `\r` が 1 文字ぶら下がる)。
// このモジュールからその方針を勝手に変えることはしない。
//
// **推奨** (editor.rs 側を触れる担当への申し送り):
//
// 1. 読み込み: `let le = detect_line_ending(&text);` を `Buffer` に覚え、
//    本文は `normalize_to(&text, LineEnding::Lf)` にして LF だけを持つ。
//    こうすると検索・桁数・折り返し・`trim_end` が `\r` を気にしなくて済む。
// 2. 保存: `normalize_to(&buf.text, buf.line_ending)` を [`encode_bytes`] へ渡す。
//    元が CRLF のファイルは CRLF のまま書き戻る (差分が全行になる事故を防ぐ)。
// 3. 混在 ([`LineEnding::Mixed`]) は保存時に最多の様式へ寄せる
//    ([`LineEnding::as_str`] が最多を返す) か、ユーザーに選ばせる。
//
// どちらの方針でもこのモジュールの関数は安全に使える:
// [`normalize_to`] は冪等で、既に統一済みの本文を渡しても何も増やさない。

/// 本文に現れた改行の内訳。混在ファイルの状況をそのまま持つので、
/// ステータスバーに「LF が 3 行だけ混ざっている」と出せる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineEndingCounts {
    /// 単独の `\n`。
    pub lf: usize,
    /// `\r\n` の組。
    pub crlf: usize,
    /// 単独の `\r` (古い Mac / 壊れたツールの出力)。
    pub cr: usize,
}

impl LineEndingCounts {
    /// 改行の総数 = 行数 - 1 (最終行に改行が無い場合)。
    pub fn total(&self) -> usize {
        self.lf + self.crlf + self.cr
    }

    /// 最も多い様式。同数のときは CRLF → LF → CR の順で決める
    /// (見えない `\r` が本文に残るほうが実害が大きいので、まず CRLF を疑わせる)。
    /// 改行が 1 つも無ければ既定の [`LineEnding::Lf`] — 判定材料が無いときに
    /// OS を見て決めると、同じファイルが環境ごとに違う様式で保存されてしまう。
    pub fn dominant(&self) -> LineEnding {
        if self.total() == 0 {
            return LineEnding::Lf;
        }
        let max = self.crlf.max(self.lf).max(self.cr);
        if self.crlf == max {
            LineEnding::Crlf
        } else if self.lf == max {
            LineEnding::Lf
        } else {
            LineEnding::Cr
        }
    }

    /// 少数派の合計行数 = 統一すれば書き換わる行数。0 なら混在していない。
    pub fn strays(&self) -> usize {
        let dom = match self.dominant() {
            LineEnding::Crlf => self.crlf,
            LineEnding::Cr => self.cr,
            _ => self.lf,
        };
        self.total() - dom
    }
}

/// 改行コード。混在は「内訳ごと」持つ ([`LineEndingCounts`]) ので、
/// UI は最多の様式と少数派の行数の両方を出せる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n` — Unix / 既定。
    #[default]
    Lf,
    /// `\r\n` — Windows。
    Crlf,
    /// `\r` — 古い Mac。今も稀に流れてくる。
    Cr,
    /// 1 つのファイルに複数の様式。中身は内訳。
    Mixed(LineEndingCounts),
}

impl LineEnding {
    /// 書き出すときの実際の文字列。混在は最多の様式へ寄せる。
    pub fn as_str(&self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
            LineEnding::Cr => "\r",
            LineEnding::Mixed(c) => c.dominant().as_str(),
        }
    }

    /// 混在なら最多の様式、そうでなければ自分自身。変換先を決めるときに使う。
    pub fn dominant(&self) -> LineEnding {
        match self {
            LineEnding::Mixed(c) => c.dominant(),
            other => *other,
        }
    }

    /// 混在しているか (UI が注意を促すかどうかの判断に使う)。
    pub fn is_mixed(&self) -> bool {
        matches!(self, LineEnding::Mixed(_))
    }

    /// ステータスバー向けの名前。混在は「CRLF (LF 3行混在)」のように内訳を添える。
    pub fn label(&self) -> String {
        match self {
            LineEnding::Lf => "LF".to_string(),
            LineEnding::Crlf => "CRLF".to_string(),
            LineEnding::Cr => "CR".to_string(),
            LineEnding::Mixed(c) => {
                let dom = c.dominant();
                let strays: Vec<String> = [
                    (LineEnding::Crlf, c.crlf),
                    (LineEnding::Lf, c.lf),
                    (LineEnding::Cr, c.cr),
                ]
                .iter()
                .filter(|(kind, n)| *n > 0 && *kind != dom)
                .map(|(kind, n)| format!("{} {n}行", kind.label()))
                .collect();
                if strays.is_empty() {
                    dom.label()
                } else {
                    format!("{} ({}混在)", dom.label(), strays.join(", "))
                }
            }
        }
    }
}

/// 本文の改行を数える。構文は一切見ない — **実際の CR / LF バイトだけ**を数える。
///
/// つまりソースコードの文字列リテラルの中に本物の CR LF が入っていれば、
/// それも 1 行として数える (エディタから見れば実際にそこで行が変わるため。
/// 逆に `"\\r\\n"` のようなエスケープ表記の 2 文字は改行バイトではないので数えない)。
/// CR / LF のバイト値は UTF-8 の多バイト列の途中には決して現れないので、
/// バイト走査でも日本語のファイルを壊さない。
pub fn count_line_endings(text: &str) -> LineEndingCounts {
    let mut c = LineEndingCounts::default();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\r' => {
                if b.get(i + 1) == Some(&b'\n') {
                    c.crlf += 1;
                    i += 2;
                } else {
                    c.cr += 1;
                    i += 1;
                }
            }
            b'\n' => {
                c.lf += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    c
}

/// 本文の改行コードを判定する。
///
/// - 空文字列 / 改行の無い 1 行だけの本文 → [`LineEnding::Lf`]
///   (材料が無いときは環境に依存しない既定を返す)。
/// - 1 種類しか出てこない → その様式。
/// - 2 種類以上 → [`LineEnding::Mixed`] に**内訳ごと**返す。
///   UI は `label()` で「CRLF (LF 3行混在)」と出せる。
pub fn detect_line_ending(text: &str) -> LineEnding {
    let counts = count_line_endings(text);
    if counts.strays() > 0 {
        LineEnding::Mixed(counts)
    } else {
        counts.dominant()
    }
}

/// 改行を `target` へ揃える。本文の他の文字は一切変えない (無損失)。
///
/// - `\r\n` を二重にしない (`\r` と `\n` を別々に数えない)。
/// - 単独の `\r` も 1 つの改行として扱う。
/// - 既に揃っている本文を渡しても内容は変わらない (冪等)。
/// - `target` が [`LineEnding::Mixed`] のときは最多の様式へ揃える。
pub fn normalize_to(text: &str, target: LineEnding) -> String {
    let eol = target.as_str();
    let b = text.as_bytes();
    // 最悪 (LF → CRLF) でも 1 改行につき 1 バイトしか増えない
    let mut out = String::with_capacity(text.len() + count_line_endings(text).total());
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        let skip = match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => {
                i += 1;
                continue;
            }
        };
        // 切る位置は必ず CR/LF の直前 = 文字境界
        out.push_str(&text[last..i]);
        out.push_str(eol);
        i += skip;
        last = i;
    }
    out.push_str(&text[last..]);
    out
}

/// ファイルを開いたときに決まる「読み方」一式。保存時に元へ戻すために持ち回る。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextFormat {
    pub encoding: Encoding,
    pub line_ending: LineEnding,
}

impl TextFormat {
    /// ステータスバー 1 行ぶん。例: 「UTF-8 / CRLF」「CP932 (Shift_JIS) / LF」。
    pub fn label(&self) -> String {
        format!("{} / {}", self.encoding.name(), self.line_ending.label())
    }
}

/// [`decode_bytes`] + [`detect_line_ending`]。開く経路はこれ 1 本で足りる。
///
/// 返す本文は**変換していない** (CR は落とさない) ので、現状の
/// 「生のまま持つ」方針のまま差し替えられる。LF 正規化へ移るときは
/// 呼び出し側で `normalize_to(&text, LineEnding::Lf)` を挟む。
pub fn decode_with_format(bytes: &[u8]) -> (String, TextFormat) {
    let (text, encoding) = decode_bytes(bytes);
    let line_ending = detect_line_ending(&text);
    (
        text,
        TextFormat {
            encoding,
            line_ending,
        },
    )
}

// ───────────────────────── PowerShell 連携 ─────────────────────────

/// `powershell -Command` へ渡すスクリプトの先頭に足す 1 行。
///
/// PowerShell 5.1 は既定でコンソールのコードページで書き出すため、
/// 英語以外の Windows では出力が UTF-8 にならない。これを UTF-8 に固定すると
/// [`decode_output`] の UTF-8 経路に乗り、コードページ推測に頼らなくなる。
/// コンソールを持たない (リダイレクトされた) 場合は代入が失敗し得るので
/// `try` で囲む — 失敗しても [`decode_output`] のコードページ判定が受け止める。
pub const PS_UTF8_PRELUDE: &str =
    "try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false } catch {}\n";

/// `.ps1` ファイルとして書き出すためのバイト列 (**BOM 付き UTF-8**)。
///
/// Windows PowerShell 5.1 は BOM の無い `.ps1` を ANSI として読む。
/// 日本語を含むパスや文字列をスクリプトに埋め込むと、BOM 無しでは
/// そこで壊れる (実行はできてしまうので、原因が分かりにくい形で失敗する)。
pub fn ps_script_bytes(script: &str) -> Vec<u8> {
    encode_bytes(script, Encoding::Utf8Bom).0
}

// ───────────────────────── 内部 ─────────────────────────

/// BOM を剥がした後の本体を UTF-8 として読む (壊れていれば ANSI として読み直す)。
fn decode_utf8_or_ansi(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_ansi_or_lossy(bytes, ansi_code_page()),
    }
}

fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

fn encode_utf16(text: &str, little: bool) -> Vec<u8> {
    let mut out = if little {
        vec![0xFF, 0xFE]
    } else {
        vec![0xFE, 0xFF]
    };
    for u in text.encode_utf16() {
        out.extend_from_slice(&if little {
            u.to_le_bytes()
        } else {
            u.to_be_bytes()
        });
    }
    out
}

/// OS のコードページで読む。Windows 以外・変換失敗時は lossy で受ける
/// (「読めない」で止めるより、化けても中身を見せたほうが直せる)。
fn decode_ansi_or_lossy(bytes: &[u8], cp: u32) -> String {
    #[cfg(windows)]
    if cp != 0 {
        if let Some(s) = win::decode(bytes, cp) {
            return s;
        }
    }
    #[cfg(not(windows))]
    let _ = cp;
    String::from_utf8_lossy(bytes).into_owned()
}

/// OS のコードページへ変換する。表現できない文字が 1 つでもあれば `None`
/// (呼び出し側は UTF-8 で保存する)。Windows 以外は常に `None`。
fn encode_ansi(text: &str, cp: u32) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        if cp == 0 {
            return None;
        }
        win::encode(text, cp)
    }
    #[cfg(not(windows))]
    {
        let _ = (text, cp);
        None
    }
}

/// ファイル向けのコードページ。**OS に聞く** (日本語 Windows なら 932、
/// 中国語なら 936…)。番号を決め打ちしないので、どの言語環境でもその環境の
/// 既定で読める。Windows 以外は 0 = 「不明」を返し、変換自体を行わない。
///
/// 公開しているのは、テストの素材を「この環境の既定」で組み立てるため
/// (バイト列を書き下すと特定の言語環境専用のテストになってしまう)。
pub fn os_ansi_code_page() -> u32 {
    ansi_code_page()
}

fn ansi_code_page() -> u32 {
    #[cfg(windows)]
    return unsafe { win::GetACP() };
    #[cfg(not(windows))]
    0
}

/// コンソール出力向けのコードページ。GUI アプリには自分のコンソールが無いため
/// `GetConsoleOutputCP` は当てにならず、子プロセスが使う OEM を先に見て、
/// 取れなければ ANSI に落とす (どちらも OS が返す値)。
fn console_code_page() -> u32 {
    #[cfg(windows)]
    {
        let oem = unsafe { win::GetOEMCP() };
        if oem != 0 {
            return oem;
        }
        unsafe { win::GetACP() }
    }
    #[cfg(not(windows))]
    0
}

#[cfg(windows)]
mod win {
    /// 変換できない文字を見つけたら失敗させるフラグ (`WC_ERR_INVALID_CHARS` 相当)。
    /// 保存時に「?」へ化けさせないために使う。
    const WC_NO_BEST_FIT_CHARS: u32 = 0x0000_0400;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetACP() -> u32;
        pub fn GetOEMCP() -> u32;
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            mb: *const u8,
            mb_len: i32,
            wide: *mut u16,
            wide_len: i32,
        ) -> i32;
        fn WideCharToMultiByte(
            code_page: u32,
            flags: u32,
            wide: *const u16,
            wide_len: i32,
            mb: *mut u8,
            mb_len: i32,
            default_char: *const u8,
            used_default: *mut i32,
        ) -> i32;
    }

    /// コードページ `cp` のバイト列を `String` にする。
    pub fn decode(bytes: &[u8], cp: u32) -> Option<String> {
        if bytes.is_empty() {
            return Some(String::new());
        }
        let len = i32::try_from(bytes.len()).ok()?;
        // 1 回目は必要な UTF-16 長を問い合わせるだけ (出力バッファは渡さない)
        let need = unsafe { MultiByteToWideChar(cp, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return None;
        }
        let mut buf = vec![0u16; need as usize];
        let got =
            unsafe { MultiByteToWideChar(cp, 0, bytes.as_ptr(), len, buf.as_mut_ptr(), need) };
        if got <= 0 {
            return None;
        }
        buf.truncate(got as usize);
        Some(String::from_utf16_lossy(&buf))
    }

    /// `String` をコードページ `cp` のバイト列にする。
    /// 表現できない文字があれば `None` (呼び出し側が UTF-8 へ切り替える)。
    pub fn encode(text: &str, cp: u32) -> Option<Vec<u8>> {
        if text.is_empty() {
            return Some(Vec::new());
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        let wlen = i32::try_from(wide.len()).ok()?;
        let mut used_default: i32 = 0;
        let need = unsafe {
            WideCharToMultiByte(
                cp,
                WC_NO_BEST_FIT_CHARS,
                wide.as_ptr(),
                wlen,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                &mut used_default,
            )
        };
        if need <= 0 {
            return None;
        }
        let mut buf = vec![0u8; need as usize];
        used_default = 0;
        let got = unsafe {
            WideCharToMultiByte(
                cp,
                WC_NO_BEST_FIT_CHARS,
                wide.as_ptr(),
                wlen,
                buf.as_mut_ptr(),
                need,
                std::ptr::null(),
                &mut used_default,
            )
        };
        // used_default が立ったら「?」等へ落とされた = 元の符号化では表せない
        if got <= 0 || used_default != 0 {
            return None;
        }
        buf.truncate(got as usize);
        Some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through_unchanged() {
        let (s, enc) = decode_bytes("日本語 🚀 ok".as_bytes());
        assert_eq!(s, "日本語 🚀 ok");
        assert_eq!(enc, Encoding::Utf8);
        assert!(!enc.is_legacy(), "既定の符号化は「変換が要る」に数えない");
    }

    #[test]
    fn utf8_bom_is_stripped_and_restored() {
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice("あいう".as_bytes());
        let (s, enc) = decode_bytes(&raw);
        assert_eq!(s, "あいう", "BOM は本文に混ぜない");
        assert_eq!(enc, Encoding::Utf8Bom);
        // 保存すると BOM が戻る (Excel 等が読めるままになる)
        let (back, used) = encode_bytes(&s, enc);
        assert_eq!(back, raw);
        assert_eq!(used, Encoding::Utf8Bom);
    }

    #[test]
    fn utf16_le_with_bom_is_read_and_written_back() {
        let raw = encode_utf16("hello 世界", true);
        assert_eq!(&raw[..2], &[0xFF, 0xFE]);
        let (s, enc) = decode_bytes(&raw);
        assert_eq!(s, "hello 世界");
        assert_eq!(enc, Encoding::Utf16Le);
        assert_eq!(encode_bytes(&s, enc).0, raw);
    }

    #[test]
    fn utf16_be_with_bom_is_read_and_written_back() {
        let raw = encode_utf16("hello 世界", false);
        let (s, enc) = decode_bytes(&raw);
        assert_eq!(s, "hello 世界");
        assert_eq!(enc, Encoding::Utf16Be);
        assert_eq!(encode_bytes(&s, enc).0, raw);
    }

    #[test]
    fn odd_length_utf16_does_not_panic() {
        // 途切れた UTF-16 (奇数バイト) を渡しても落ちないこと
        let raw = vec![0xFF, 0xFE, 0x42, 0x00, 0x41];
        let (s, _) = decode_bytes(&raw);
        assert_eq!(s, "B", "最後の余りは捨てる");
    }

    #[test]
    fn empty_input_is_empty_utf8() {
        let (s, enc) = decode_bytes(b"");
        assert!(s.is_empty());
        assert_eq!(enc, Encoding::Utf8);
        assert!(decode_output(b"").is_empty());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(Encoding::Utf8.label(), "", "既定は表示しない");
        assert_eq!(Encoding::Utf16Le.label(), "UTF-16 LE");
        // 番号は OS から来た値をそのまま出す。通称は分かるものだけ添える。
        assert_eq!(Encoding::Ansi(932).label(), "CP932 (Shift_JIS)");
        assert_eq!(Encoding::Ansi(1252).label(), "CP1252 (Windows)");
        assert_eq!(Encoding::Ansi(28591).label(), "CP28591", "知らない番号でも出せる");
        assert!(Encoding::Ansi(932).is_legacy());
    }

    /// この環境の ANSI コードページで表せるが UTF-8 とはバイト列が違う文字列を探す。
    /// テストの素材を**OS から作る**ので、日本語 Windows でも他の言語環境でも
    /// 同じテストがそのまま意味を持つ (バイト列を書き下すと日本語専用になる)。
    #[cfg(windows)]
    fn legacy_fixture() -> Option<(&'static str, Vec<u8>, u32)> {
        legacy_fixture_for(super::ansi_code_page())
    }

    /// 指定コードページで「ASCII でない文字を含む素材」を組み立てる。
    /// decode_output は console (OEM) を、decode_bytes は ANSI を使うので、
    /// テストごとに検証対象と同じコードページで素材を作ること
    /// (西欧 Windows では ANSI=1252 / OEM=437 と食い違う)。
    #[cfg(windows)]
    fn legacy_fixture_for(cp: u32) -> Option<(&'static str, Vec<u8>, u32)> {
        // 各言語環境で「そのコードページにあり ASCII でない」候補を順に試す
        for probe in ["日本語", "中文字", "한국어", "Grüße", "Ünicode"] {
            if let Some(bytes) = super::win::encode(probe, cp) {
                if bytes != probe.as_bytes() {
                    return Some((probe, bytes, cp));
                }
            }
        }
        None
    }

    /// UTF-8 として妥当なバイト列は、たとえ ANSI にも読めても UTF-8 として扱う。
    /// (この優先順を逆にすると、いま正しく出ている出力が壊れる)
    #[test]
    fn valid_utf8_wins_over_the_code_page() {
        let bytes = "これは UTF-8 です".as_bytes();
        assert_eq!(decode_output(bytes), "これは UTF-8 です");
    }

    /// `.ps1` は BOM 付きで書く。これを外すと PowerShell 5.1 が ANSI として読み、
    /// 日本語を含むパスが壊れる (ファイアウォール規則が別のパスを指してしまう)。
    #[test]
    fn ps_scripts_are_written_with_a_bom() {
        let bytes = ps_script_bytes("$exe = 'C:\\Users\\たろう\\zai.exe'\n");
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "BOM が先頭に必要");
        let (back, enc) = decode_bytes(&bytes);
        assert_eq!(back, "$exe = 'C:\\Users\\たろう\\zai.exe'\n");
        assert_eq!(enc, Encoding::Utf8Bom);
    }

    /// PowerShell へ渡す前置きは 1 行で終わり、後続のスクリプトを壊さないこと。
    #[test]
    fn ps_prelude_is_one_self_contained_line() {
        assert!(PS_UTF8_PRELUDE.ends_with('\n'));
        assert_eq!(PS_UTF8_PRELUDE.lines().count(), 1);
        assert!(PS_UTF8_PRELUDE.starts_with("try {"), "失敗しても止まらないこと");
    }

    /// 打ち切りでお尻が切れた UTF-8 は、コードページで読み直してはいけない。
    /// (末尾の 1 文字のために全体を CP932 として読むと全部化ける)
    #[test]
    fn truncated_utf8_keeps_the_readable_head() {
        let full = "進捗を報告します".as_bytes();
        let cut = &full[..full.len() - 1]; // 最後の 1 文字が途中で切れている
        let s = decode_output(cut);
        assert!(s.starts_with("進捗を報告しま"), "頭は読めたままであること: {s}");
    }

    // ── 以下は Windows のコードページ変換を実際に叩く ──

    /// Windows の `powershell` はエラーをコンソールのコードページで返す
    /// (日本語 Windows なら CP932)。ここが lossy 任せだと画面が化けるだけでなく、
    /// 「キャンセル」の照合が外れて原因が分からなくなる。
    #[cfg(windows)]
    #[test]
    fn legacy_output_is_decoded_not_mangled() {
        // decode_output は console (OEM) コードページで読み直すので素材も同じで作る
        let Some((text, bytes, _cp)) = legacy_fixture_for(super::console_code_page()) else {
            return; // この環境のコードページでは試せる文字が無い (US-ASCII 環境など)
        };
        let s = decode_output(&bytes);
        assert_eq!(s, text, "OS のコードページで返る出力は読めなければならない");
        assert!(!s.contains('\u{fffd}'), "置換文字が残ってはいけない: {s}");
    }

    #[cfg(windows)]
    #[test]
    fn legacy_file_round_trips_through_save() {
        let Some((text, bytes, cp)) = legacy_fixture() else {
            return;
        };
        let (s, enc) = decode_bytes(&bytes);
        assert_eq!(s, text, "UTF-8 でないファイルも開けること");
        assert_eq!(enc, Encoding::Ansi(cp));
        // 保存し直しても元のバイト列に戻る (勝手に UTF-8 化して他ツールを壊さない)
        let (back, used) = encode_bytes(&s, enc);
        assert_eq!(back, bytes);
        assert_eq!(used, Encoding::Ansi(cp));
    }

    /// 元の符号化に無い文字 (絵文字など) を混ぜて保存したら、文字を落とすのではなく
    /// UTF-8 へ切り替える。落とすと保存の瞬間に本文が壊れるため。
    #[cfg(windows)]
    #[test]
    fn unrepresentable_chars_force_utf8_on_save() {
        let cp = super::ansi_code_page();
        let text = "text 🚀"; // 絵文字はどの ANSI コードページにも無い
        let (bytes, used) = encode_bytes(text, Encoding::Ansi(cp));
        assert_eq!(used, Encoding::Utf8, "表せないなら UTF-8 へ格上げする");
        assert_eq!(String::from_utf8(bytes).unwrap(), text);
    }

    /// コードページが分からない環境 (Windows 以外) では変換を試みず、
    /// 読めた分をそのまま見せる — 落ちないこと・空にならないことだけを守る。
    #[test]
    fn unknown_code_page_falls_back_without_panicking() {
        let broken = [0x93u8, 0xFA, 0x96, 0x7B]; // どの UTF-8 文字にもならない列
        let (s, enc) = decode_bytes(&broken);
        assert!(!s.is_empty(), "中身を見せずに諦めないこと");
        assert!(matches!(enc, Encoding::Ansi(_)));
        // 保存経路も落ちないこと
        let _ = encode_bytes(&s, enc);
    }

    // ───────────────────────── 改行コード ─────────────────────────

    #[test]
    fn detect_lf_only() {
        assert_eq!(detect_line_ending("a\nb\nc\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("\n"), LineEnding::Lf);
        assert_eq!(LineEnding::Lf.label(), "LF");
        assert_eq!(LineEnding::Lf.as_str(), "\n");
    }

    #[test]
    fn detect_crlf_only() {
        assert_eq!(detect_line_ending("a\r\nb\r\n"), LineEnding::Crlf);
        // \r と \n を別々に数えない = 混在扱いにしない
        assert!(!detect_line_ending("a\r\nb\r\n").is_mixed());
        assert_eq!(LineEnding::Crlf.as_str(), "\r\n");
    }

    #[test]
    fn detect_cr_only() {
        assert_eq!(detect_line_ending("a\rb\rc"), LineEnding::Cr);
        assert_eq!(LineEnding::Cr.label(), "CR");
    }

    #[test]
    fn detect_empty_and_single_line_default_to_lf() {
        // 材料が無いときは OS を見ずに既定を返す (環境で結果が変わらないこと)
        assert_eq!(detect_line_ending(""), LineEnding::Lf);
        assert_eq!(detect_line_ending("改行の無い一行"), LineEnding::Lf);
        assert_eq!(count_line_endings("").total(), 0);
    }

    #[test]
    fn detect_mixed_reports_the_dominant_style_and_strays() {
        // CRLF が 5 行、LF が 3 行 → 「CRLF (LF 3行混在)」
        let text = "a\r\nb\r\nc\r\nd\r\ne\r\nf\ng\nh\n";
        let le = detect_line_ending(text);
        let LineEnding::Mixed(c) = le else {
            panic!("混在と判定されるべき: {le:?}");
        };
        assert_eq!((c.crlf, c.lf, c.cr), (5, 3, 0));
        assert_eq!(c.dominant(), LineEnding::Crlf);
        assert_eq!(c.strays(), 3, "統一すれば書き換わる行数");
        assert_eq!(le.label(), "CRLF (LF 3行混在)");
        assert_eq!(le.as_str(), "\r\n", "保存は最多の様式へ寄せる");
    }

    #[test]
    fn mixed_label_lists_every_stray_kind() {
        let le = detect_line_ending("a\nb\nc\nd\r\ne\rf");
        assert_eq!(le.label(), "LF (CRLF 1行, CR 1行混在)");
        assert_eq!(le.dominant(), LineEnding::Lf);
    }

    /// 文字列リテラルの中に**本物の** CR LF が入っていても 1 行として数える。
    /// エディタから見れば実際にそこで行が変わるため (構文は見ない)。
    #[test]
    fn real_crlf_inside_a_string_literal_still_counts() {
        let text = "let s = \"head\r\ntail\";\n";
        let c = count_line_endings(text);
        assert_eq!((c.crlf, c.lf), (1, 1), "リテラル内の実 CRLF も 1 行");
        assert!(detect_line_ending(text).is_mixed());
        // エスケープ表記 (バックスラッシュ + r) は改行バイトではないので数えない
        let escaped = "let s = \"head\\r\\ntail\";\n";
        assert_eq!(count_line_endings(escaped).crlf, 0);
        assert_eq!(detect_line_ending(escaped), LineEnding::Lf);
    }

    #[test]
    fn normalize_covers_every_pair_of_conversions() {
        let cases = [
            ("a\nb\nc", "a\r\nb\r\nc", "a\rb\rc"), // (LF, CRLF, CR) の同じ本文
        ];
        for (lf, crlf, cr) in cases {
            for src in [lf, crlf, cr] {
                assert_eq!(normalize_to(src, LineEnding::Lf), lf, "{src:?} → LF");
                assert_eq!(normalize_to(src, LineEnding::Crlf), crlf, "{src:?} → CRLF");
                assert_eq!(normalize_to(src, LineEnding::Cr), cr, "{src:?} → CR");
            }
        }
    }

    #[test]
    fn normalize_is_idempotent_and_never_doubles_crlf() {
        let once = normalize_to("a\r\nb\n", LineEnding::Crlf);
        assert_eq!(once, "a\r\nb\r\n");
        assert_eq!(normalize_to(&once, LineEnding::Crlf), once, "冪等");
        assert!(!once.contains("\r\r"), "\\r を増やしてはいけない");
        assert!(!once.contains("\n\n"));
    }

    #[test]
    fn normalize_handles_lone_cr_and_missing_final_newline() {
        assert_eq!(normalize_to("a\rb", LineEnding::Lf), "a\nb");
        assert_eq!(normalize_to("a\r", LineEnding::Crlf), "a\r\n");
        // 末尾に改行が無い本文は末尾に何も足さない
        assert_eq!(normalize_to("末尾に改行なし", LineEnding::Crlf), "末尾に改行なし");
        assert_eq!(normalize_to("", LineEnding::Crlf), "");
    }

    #[test]
    fn normalize_keeps_multibyte_text_intact() {
        let src = "日本語\r\n🚀 絵文字\nおわり";
        assert_eq!(normalize_to(&normalize_to(src, LineEnding::Lf), LineEnding::Crlf), "日本語\r\n🚀 絵文字\r\nおわり");
    }

    #[test]
    fn normalize_to_mixed_uses_the_dominant_style() {
        let mixed = detect_line_ending("a\r\nb\r\nc\n");
        assert_eq!(normalize_to("x\ny\n", mixed), "x\r\ny\r\n");
    }

    /// 符号化と改行を 1 つにして「UTF-8 / CRLF」と出せること。
    #[test]
    fn decode_with_format_reports_encoding_and_line_ending() {
        let (text, fmt) = decode_with_format(b"a\r\nb\r\n");
        assert_eq!(text, "a\r\nb\r\n", "CR は落とさない (現状の方針を変えない)");
        assert_eq!(fmt.encoding, Encoding::Utf8);
        assert_eq!(fmt.line_ending, LineEnding::Crlf);
        assert_eq!(fmt.label(), "UTF-8 / CRLF");

        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice("あ\n".as_bytes());
        let (_, fmt) = decode_with_format(&bom);
        assert_eq!(fmt.label(), "UTF-8 BOM / LF");
        assert_eq!(Encoding::Ansi(932).name(), "CP932 (Shift_JIS)");
    }
}
