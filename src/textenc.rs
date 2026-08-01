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

/// CP932 の符号位置。日本語 Windows の ANSI コードページ。
const CP_932: u32 = 932;

/// JIS X 0208 (素の Shift_JIS) と CP932 (Microsoft 拡張) で**解釈が割れる**文字。
///
/// 同じ Shift_JIS バイト列を、JIS の表と Microsoft の表が別の Unicode へ写す。
/// 有名な事故が波ダッシュ `〜`: バイト 0x8160 を JIS は U+301C WAVE DASH、
/// CP932 は U+FF5E FULLWIDTH TILDE と読む。見た目は同じなので気付かない。
///
/// これが実害になるのは**保存時**。Windows の `WideCharToMultiByte` は
/// `WC_NO_BEST_FIT_CHARS` 付きで呼んでいるので、U+301C を渡すと
/// 「CP932 に無い文字」として失敗する → [`encode_bytes`] がファイル全体を
/// UTF-8 へ格上げしてしまう。本文には `〜` が 1 文字あるだけなのに、
/// **符号化が黙って変わる** (Git の差分が全行になる)。
///
/// 対応表を 1 つ持って CP932 側の字形へ寄せれば、見た目を変えずに
/// 元の符号化のまま保存できる。左が JIS 側、右が CP932 側。
const JIS_TO_CP932: &[(char, char)] = &[
    ('\u{00A2}', '\u{FFE0}'), // ¢ → ￠ (0x8191)
    ('\u{00A3}', '\u{FFE1}'), // £ → ￡ (0x8192)
    ('\u{00AC}', '\u{FFE2}'), // ¬ → ￢ (0x81CA)
    ('\u{2016}', '\u{2225}'), // ‖ DOUBLE VERTICAL LINE → ∥ PARALLEL TO (0x8161)
    ('\u{2212}', '\u{FF0D}'), // − MINUS SIGN → － FULLWIDTH HYPHEN-MINUS (0x817C)
    ('\u{301C}', '\u{FF5E}'), // 〜 WAVE DASH → ～ FULLWIDTH TILDE (0x8160)
];

/// JIS 側の字形を CP932 側へ寄せる。対象が 1 文字も無ければ入力をそのまま返す
/// (借用のまま返るので、ほとんどの本文では確保が起きない)。
pub fn fold_to_cp932(text: &str) -> std::borrow::Cow<'_, str> {
    let hit = |c: char| {
        JIS_TO_CP932
            .iter()
            .find(|(jis, _)| *jis == c)
            .map(|(_, w)| *w)
    };
    if !text.chars().any(|c| hit(c).is_some()) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.chars().map(|c| hit(c).unwrap_or(c)).collect())
}

/// OS のコードページへ変換する。表現できない文字が 1 つでもあれば `None`
/// (呼び出し側は UTF-8 で保存する)。Windows 以外は常に `None`。
///
/// CP932 のときだけ、素の変換が失敗したら [`fold_to_cp932`] で
/// JIS/CP932 の揺れを吸収してもう一度試す (波ダッシュ 1 文字で
/// ファイル全体が UTF-8 に化けるのを防ぐ)。それでも駄目なら `None`。
fn encode_ansi(text: &str, cp: u32) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        if cp == 0 {
            return None;
        }
        if let Some(bytes) = win::encode(text, cp) {
            return Some(bytes);
        }
        if cp == CP_932 {
            if let std::borrow::Cow::Owned(folded) = fold_to_cp932(text) {
                return win::encode(&folded, cp);
            }
        }
        None
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

// ═════════════ 符号化を明示して開き直す / 保存する (エンコーディングピッカー) ═════════════
//
// [`decode_bytes`] は「開いた瞬間に自動判定し、保存で元へ戻す」経路で、
// 判定が当たっている限りこれで足りる。当たらなかったときの逃げ道が無いのが問題だった:
//
//   * 判定が外れた (CP932 のファイルが偶然 UTF-8 として妥当だった等)
//   * わざと別の符号化で保存したい (相手のツールが CP932 しか読めない等)
//
// ここはその 2 つだけを担当する。[`reopen_with`] は**判定を一切見ない**で復号し、
// [`save_with`] は**要求された符号化から絶対に勝手に乗り換えない**
// ([`encode_bytes`] は変換できない文字があると黙って UTF-8 へ格上げする。
// 保存経路としては正しい保険だが、ユーザーが明示的に選んだときにそれをやると
// 「CP932 で保存したはずが UTF-8 になっていた」という別の事故になる)。

/// UTF-8 の BOM。
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// 「この実行環境が本当に読み書きできる」符号化 1 件ぶんの説明。
///
/// 一覧は**実測**で作る ([`supported_encodings`])。表に載っているものは
/// すべて往復 (保存 → 開き直しで完全一致) が確認済み。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodingInfo {
    /// この項目が指す符号化。そのまま [`reopen_with`] / [`save_with`] へ渡せる。
    pub enc: Encoding,
    /// 安定した識別子 (設定ファイルやコマンドの引数に使う)。例: `"utf-8"` `"cp932"`。
    pub id: String,
    /// 画面に出す名前。例: `"CP932 (Shift_JIS)"`。
    pub label: String,
    /// 別名。ユーザーが「sjis」「shift-jis」と打っても引けるようにする。
    pub aliases: Vec<&'static str>,
    /// 日本語 (かな・漢字) を往復できるか。ASCII しか通らない符号化と区別する。
    pub japanese: bool,
}

/// 往復検査に使う見本。ASCII は全候補、日本語は通る候補だけが `japanese: true` になる。
const PROBE_ASCII: &str = "Aa1 -_/ ok\n";
const PROBE_JA: &str = "日本語のテキスト かな カナ 漢字\n";
/// Latin-1 / 西欧コードページ用。ASCII だけだと「日本語が通らないこと」しか分からないので、
/// その符号化らしい文字も 1 度試す (通らなければ ASCII 判定のまま表に載る)。
const PROBE_LATIN: &str = "café naïve ÿ\n";

/// 候補表。**番号と別名だけ**を持ち、使えるかどうかは一切決め打ちしない
/// (実際に往復できたものだけが [`supported_encodings`] に載る)。
/// OS 既定のコードページは実行時に足す (どの言語環境でもその環境の既定が並ぶ)。
fn encoding_candidates() -> Vec<(Encoding, &'static str, &'static [&'static str])> {
    let mut v: Vec<(Encoding, &'static str, &'static [&'static str])> = vec![
        (Encoding::Utf8, "utf-8", &["utf8", "u8"]),
        (
            Encoding::Utf8Bom,
            "utf-8-bom",
            &["utf8bom", "utf-8 bom", "bom"],
        ),
        (
            Encoding::Utf16Le,
            "utf-16le",
            &["utf16le", "utf-16 le", "ucs-2le"],
        ),
        (
            Encoding::Utf16Be,
            "utf-16be",
            &["utf16be", "utf-16 be", "ucs-2be"],
        ),
        (
            Encoding::Ansi(CP_932),
            "cp932",
            &["shift_jis", "shift-jis", "sjis", "ms932", "windows-31j"],
        ),
        (Encoding::Ansi(CP_EUC_JP), "cp51932", &["euc-jp", "eucjp"]),
        (
            Encoding::Ansi(CP_EUC_JP_X0212),
            "cp20932",
            &["euc-jp-x0212"],
        ),
        (
            Encoding::Ansi(CP_ISO2022JP),
            "cp50220",
            &["iso-2022-jp", "jis", "iso2022jp"],
        ),
        (
            Encoding::Ansi(CP_ISO2022JP_ALLOW1B),
            "cp50221",
            &["iso-2022-jp-1"],
        ),
        (
            Encoding::Ansi(CP_LATIN1),
            "cp28591",
            &["latin-1", "latin1", "iso-8859-1"],
        ),
        (Encoding::Ansi(1252), "cp1252", &["windows-1252"]),
    ];
    let os = os_ansi_code_page();
    if os != 0 && !v.iter().any(|(e, _, _)| *e == Encoding::Ansi(os)) {
        // 表に無い言語環境 (例: 中国語 936 / 韓国語 949) でもその環境の既定は必ず出す。
        // `id` は空にしておき、組み立て時に `cp<番号>` を作る。
        v.push((Encoding::Ansi(os), "", &[]));
    }
    v
}

/// EUC-JP (IE 系)。
const CP_EUC_JP: u32 = 51932;
/// EUC-JP (JIS X 0212 込み)。
const CP_EUC_JP_X0212: u32 = 20932;
/// ISO-2022-JP。
const CP_ISO2022JP: u32 = 50220;
/// ISO-2022-JP (半角カナを 1B 経由で通す版)。
const CP_ISO2022JP_ALLOW1B: u32 = 50221;
/// ISO-8859-1 (Latin-1)。
const CP_LATIN1: u32 = 28591;

/// **この実行環境が実際に読み書きできる**符号化の一覧。
///
/// 表は決め打ちではなく、候補ごとに「[`save_with`] して [`reopen_with`] したら
/// 元の文字列に戻るか」を実測して作る。だから:
///
/// * Windows 以外では ANSI コードページ変換 (`WideCharToMultiByte`) が無いので、
///   UTF-8 / UTF-8 BOM / UTF-16 LE / UTF-16 BE だけが並ぶ。
/// * Windows では OS が持っているコードページだけが並ぶ
///   (ISO-2022-JP のように `WC_NO_BEST_FIT_CHARS` を受け付けない符号化は
///   「保存できない」ので**載らない** — 表に出す以上は必ず保存できる)。
/// * 表に無い言語環境でも、その環境の既定コードページは自動で加わる。
///
/// 初回呼び出しで 1 度だけ実測し、以後は使い回す。
pub fn supported_encodings() -> &'static [EncodingInfo] {
    static TABLE: std::sync::OnceLock<Vec<EncodingInfo>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out: Vec<EncodingInfo> = Vec::new();
        for (enc, id, aliases) in encoding_candidates() {
            if !round_trips(enc, PROBE_ASCII) {
                continue;
            }
            if out.iter().any(|i| i.enc == enc) {
                continue;
            }
            let latin = round_trips(enc, PROBE_LATIN);
            let japanese = round_trips(enc, PROBE_JA);
            let _ = latin; // ラテン系の可否は今のところ表に出さない (判定だけ通す)
            let id = if id.is_empty() {
                match enc {
                    Encoding::Ansi(cp) => format!("cp{cp}"),
                    other => other.name().to_lowercase(),
                }
            } else {
                id.to_string()
            };
            out.push(EncodingInfo {
                enc,
                id,
                label: enc.name(),
                aliases: aliases.to_vec(),
                japanese,
            });
        }
        out
    })
}

/// `enc` がこの環境で本当に使えるか ([`supported_encodings`] に載っているか)。
pub fn is_supported(enc: Encoding) -> bool {
    supported_encodings().iter().any(|i| i.enc == enc)
}

/// 名前・別名・識別子から符号化を引く (大文字小文字とハイフン/アンダースコアは無視)。
/// 使えない符号化は引けない (`None`)。
pub fn encoding_by_name(name: &str) -> Option<Encoding> {
    let key = normalize_enc_key(name);
    supported_encodings()
        .iter()
        .find(|i| {
            normalize_enc_key(&i.id) == key
                || normalize_enc_key(&i.label) == key
                || i.aliases.iter().any(|a| normalize_enc_key(a) == key)
        })
        .map(|i| i.enc)
}

fn normalize_enc_key(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '-' | '_' | ' ' | '(' | ')'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// 保存 → 開き直しで完全一致するか。[`supported_encodings`] の実測本体。
fn round_trips(enc: Encoding, sample: &str) -> bool {
    match save_with(sample, enc, LineEnding::Lf) {
        Ok(bytes) => {
            let r = reopen_with_report(&bytes, enc);
            r.replacements == 0 && r.text == sample
        }
        Err(_) => false,
    }
}

/// [`reopen_with_report`] の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reopened {
    /// 復号した本文。
    pub text: String,
    /// 指定した符号化 + 本文から数え直した改行コード。
    pub format: TextFormat,
    /// 置換文字 U+FFFD の個数 = **化けた箇所の数**。
    ///
    /// 元のバイト列に本物の U+FFFD が入っていた場合もここに数える
    /// (安全側の誤検知 — 「化けているかも」と余計に警告するだけで、
    /// 化けているのに黙っていることは無い)。
    pub replacements: usize,
}

impl Reopened {
    /// 1 文字でも化けたか。UI はこれを見て「この符号化では読めていません」と出せる。
    pub fn lossy(&self) -> bool {
        self.replacements > 0
    }
}

/// **自動判定を無視して** `enc` で開き直す (「エンコーディングを指定して開き直す」)。
///
/// BOM の扱い:
/// * `Utf8` / `Utf8Bom` — 先頭に UTF-8 BOM があれば剥がす (本文に U+FEFF が
///   見えないようにする)。返す [`TextFormat`] は**要求どおりの符号化**なので、
///   `Utf8` を選べば保存時に BOM が落ち、`Utf8Bom` を選べば付く。
/// * `Utf16Le` / `Utf16Be` — 一致する向きの BOM だけ剥がす。
///   向きを間違えて選ぶと本文が化けるので、[`Reopened::lossy`] で気付ける。
///
/// 化けた箇所の数は [`Reopened::replacements`]。
pub fn reopen_with_report(bytes: &[u8], enc: Encoding) -> Reopened {
    let text = decode_exact(bytes, enc);
    let replacements = text.chars().filter(|c| *c == '\u{FFFD}').count();
    let line_ending = detect_line_ending(&text);
    Reopened {
        text,
        format: TextFormat {
            encoding: enc,
            line_ending,
        },
        replacements,
    }
}

/// [`reopen_with_report`] の簡易版。化け具合を見ないなら (テスト・内部利用) こちら。
pub fn reopen_with(bytes: &[u8], enc: Encoding) -> (String, TextFormat) {
    let r = reopen_with_report(bytes, enc);
    (r.text, r.format)
}

/// 判定を一切せず `enc` として復号する。
fn decode_exact(bytes: &[u8], enc: Encoding) -> String {
    match enc {
        Encoding::Utf8 | Encoding::Utf8Bom => {
            let body = bytes.strip_prefix(&UTF8_BOM).unwrap_or(bytes);
            String::from_utf8_lossy(body).into_owned()
        }
        Encoding::Utf16Le => decode_utf16(bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes), true),
        Encoding::Utf16Be => {
            decode_utf16(bytes.strip_prefix(&[0xFE, 0xFF]).unwrap_or(bytes), false)
        }
        Encoding::Ansi(cp) => decode_ansi_or_lossy(bytes, cp),
    }
}

/// [`save_with`] が保存を**断った**理由。UI はこれをそのまま文にできる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeIssue {
    /// その符号化に無い文字が本文にある。位置は**元の本文**基準
    /// (改行コードを揃える前) なので、そのままキャレットを飛ばせる。
    Unrepresentable {
        enc: Encoding,
        /// 保存できない最初の文字。
        ch: char,
        /// 本文先頭からの文字数 (0 起点)。
        char_index: usize,
        /// 本文先頭からのバイト位置 (0 起点)。
        byte_index: usize,
        /// 行番号 (1 起点)。
        line: usize,
        /// 行内の文字位置 (1 起点)。
        column: usize,
    },
    /// この実行環境にはその符号化への変換表が無い
    /// (Windows 以外での CP932、`WC_NO_BEST_FIT_CHARS` を受け付けない ISO-2022-JP 等)。
    /// 本文の中身とは無関係に保存できない。
    Unsupported { enc: Encoding },
}

impl EncodeIssue {
    pub fn encoding(&self) -> Encoding {
        match self {
            EncodeIssue::Unrepresentable { enc, .. } | EncodeIssue::Unsupported { enc } => *enc,
        }
    }

    /// 保存できない最初の文字 (環境側の理由なら `None`)。
    pub fn ch(&self) -> Option<char> {
        match self {
            EncodeIssue::Unrepresentable { ch, .. } => Some(*ch),
            EncodeIssue::Unsupported { .. } => None,
        }
    }

    /// 保存できない最初の文字の位置 (文字数, 0 起点)。
    pub fn char_index(&self) -> Option<usize> {
        match self {
            EncodeIssue::Unrepresentable { char_index, .. } => Some(*char_index),
            EncodeIssue::Unsupported { .. } => None,
        }
    }

    /// そのまま出せる説明文。
    /// 例: 「この文字は CP932 (Shift_JIS) で保存できません: 「𠮟」(12行目 3文字目)」
    pub fn message(&self) -> String {
        match self {
            EncodeIssue::Unrepresentable {
                enc,
                ch,
                line,
                column,
                ..
            } => format!(
                "この文字は {} で保存できません: 「{ch}」({line}行目 {column}文字目)",
                enc.name()
            ),
            EncodeIssue::Unsupported { enc } => format!(
                "この環境では {} で保存できません (変換表がありません)",
                enc.name()
            ),
        }
    }
}

/// **符号化と改行コードを明示して**バイト列にする (「エンコーディングを指定して保存」)。
///
/// [`encode_bytes`] との決定的な違いは、**要求された符号化から絶対に乗り換えない**こと。
/// 変換できない文字が 1 つでもあれば [`EncodeIssue`] を返して保存自体を断る
/// (`encode_bytes` は黙って UTF-8 へ格上げする。自動保存経路では本文を守るために
/// それが正しいが、ユーザーが符号化を選んだ場面でやると「選んだはずの符号化に
/// なっていない」という別の事故になる)。
///
/// 改行は `ending` へ揃えてから符号化する ([`normalize_to`] は冪等・無損失)。
/// [`LineEnding::Mixed`] を渡すと最多の様式へ寄る。
pub fn save_with(text: &str, enc: Encoding, ending: LineEnding) -> Result<Vec<u8>, EncodeIssue> {
    let body = normalize_to(text, ending);
    match enc {
        Encoding::Utf8 => Ok(body.into_bytes()),
        Encoding::Utf8Bom => {
            let mut out = UTF8_BOM.to_vec();
            out.extend_from_slice(body.as_bytes());
            Ok(out)
        }
        // UTF-16 は Unicode 全体を表現できるので失敗しない
        Encoding::Utf16Le => Ok(encode_utf16(&body, true)),
        Encoding::Utf16Be => Ok(encode_utf16(&body, false)),
        Encoding::Ansi(cp) => match encode_ansi(&body, cp) {
            Some(bytes) => Ok(bytes),
            // 位置は**元の本文**で数える。改行コードの正規化は CR/LF しか触らず、
            // CR/LF はどのコードページでも表現できるので、駄目な文字の集合は変わらない。
            None => Err(first_encode_failure(text, cp)),
        },
    }
}

/// 本文全体の変換が失敗したときに、**最初に**引っかかった文字を特定する。
/// 失敗経路でしか呼ばないので 1 文字ずつ試して構わない。
fn first_encode_failure(text: &str, cp: u32) -> EncodeIssue {
    let enc = Encoding::Ansi(cp);
    // ASCII 1 文字すら通らない = 変換表そのものが無い環境
    if encode_ansi("A", cp).is_none() {
        return EncodeIssue::Unsupported { enc };
    }
    // 1 文字ずつなら全部通るのに全体では失敗する = 文字の問題ではない
    // (状態を持つ符号化など)。中身のせいにしない。
    first_unencodable(text, enc, |c| encode_ansi(&c.to_string(), cp).is_some())
        .unwrap_or(EncodeIssue::Unsupported { enc })
}

/// 「その符号化で書ける文字か」を判定する `ok` を使って、最初に書けない文字を探す。
///
/// 位置の数え方 (文字数・バイト位置・行・桁) をここ 1 か所に閉じ込めてあるので、
/// OS の変換表が無い環境でも `ok` を差し替えれば同じ計算を検査できる。
fn first_unencodable<F>(text: &str, enc: Encoding, ok: F) -> Option<EncodeIssue>
where
    F: Fn(char) -> bool,
{
    let mut line = 1usize;
    let mut column = 1usize;
    for (char_index, (byte_index, ch)) in text.char_indices().enumerate() {
        if !ok(ch) {
            return Some(EncodeIssue::Unrepresentable {
                enc,
                ch,
                char_index,
                byte_index,
                line,
                column,
            });
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    None
}

// ───────────────────── 符号化の推定 (候補を confidence 付きで返す) ─────────────────────

/// バイト列がどの符号化かを**候補ごとの確からしさ付き**で返す。降順に整列。
///
/// [`decode_bytes`] は「1 つ選んで開く」ためのもので、外れたときに何も言えない。
/// こちらは「UTF-8 として開いたが CP932 かもしれない」という UI を作るための材料。
///
/// 判定はバイト列の**構造だけ**を見る (OS の変換表を使わないので、どの環境でも
/// 同じ答えになる)。復号できるかどうかは別問題なので、UI は [`is_supported`] で
/// 絞ってから並べること。
///
/// 目安:
/// * `1.0` — BOM がある (疑う余地なし)
/// * `0.9` 以上 — その符号化としてしか読めない並び
/// * `0.5` 前後 — ASCII だけ等、どの符号化でも読めるので決め手が無い
/// * 返らない — その符号化としては壊れている
pub fn detect_all(bytes: &[u8]) -> Vec<(Encoding, f32)> {
    let mut out: Vec<(Encoding, f32)> = Vec::new();
    let mut push = |enc: Encoding, score: f32| {
        if score > 0.0 {
            out.push((enc, score));
        }
    };

    // BOM は決定的。付いていたら本体を剥がして残りの候補を採点する。
    let (body, bom) = if let Some(rest) = bytes.strip_prefix(&UTF8_BOM) {
        (rest, Some(Encoding::Utf8Bom))
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (rest, Some(Encoding::Utf16Le))
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (rest, Some(Encoding::Utf16Be))
    } else {
        (bytes, None)
    };
    if let Some(enc) = bom {
        push(enc, 1.0);
    }
    // BOM が確定しているときは他の候補を「参考」まで下げる (誤って上に来ない)
    let damp = if bom.is_some() { 0.3 } else { 1.0 };
    // NUL バイトはバイト指向の符号化ではまず現れない (UTF-16 の署名のようなもの)。
    // 「ASCII + NUL」の並びは UTF-8 としても妥当なので、これを効かせないと
    // BOM 無し UTF-16 が UTF-8 に負ける。
    let byte_damp = damp * if body.contains(&0) { 0.3 } else { 1.0 };

    if bom != Some(Encoding::Utf8Bom) {
        push(Encoding::Utf8, score_utf8(body) * byte_damp);
    }
    if bom.is_none() {
        push(Encoding::Utf16Le, score_utf16(body, true));
        push(Encoding::Utf16Be, score_utf16(body, false));
    }
    push(Encoding::Ansi(CP_932), score_sjis(body) * byte_damp);
    push(
        Encoding::Ansi(preferred_cp(&[CP_EUC_JP, CP_EUC_JP_X0212])),
        score_euc_jp(body) * byte_damp,
    );
    push(
        Encoding::Ansi(preferred_cp(&[CP_ISO2022JP, CP_ISO2022JP_ALLOW1B])),
        score_iso2022jp(body) * byte_damp,
    );
    // OS 既定のコードページ (単バイト系ならこれが正解のことが多い)
    let os = os_ansi_code_page();
    if os != 0 && os != CP_932 {
        push(Encoding::Ansi(os), score_single_byte(body) * byte_damp);
    }

    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// 同じ符号化に複数のコードページ番号があるとき、**この環境で使える方**を選ぶ。
/// どれも使えなければ先頭 (代表的な番号) を返す — 推定結果としては正しく、
/// 使えるかどうかは [`is_supported`] で分かる。
fn preferred_cp(cands: &[u32]) -> u32 {
    cands
        .iter()
        .copied()
        .find(|cp| is_supported(Encoding::Ansi(*cp)))
        .unwrap_or(cands[0])
}

fn score_utf8(b: &[u8]) -> f32 {
    if std::str::from_utf8(b).is_err() {
        return 0.0;
    }
    // 多バイト列が 1 つでもあれば「UTF-8 としてしか読めない」に近い。
    // ASCII だけなら他の符号化でも読めるので決め手にはならない。
    if b.iter().any(|c| *c >= 0x80) {
        1.0
    } else {
        0.9
    }
}

/// BOM 無し UTF-16 の見当。ASCII 文字が「片方が 0x00」の組で並ぶ性質を使う。
fn score_utf16(b: &[u8], little: bool) -> f32 {
    if b.len() < 4 || !b.len().is_multiple_of(2) {
        return 0.0;
    }
    let pairs = b.len() / 2;
    let hits = b
        .chunks_exact(2)
        .filter(|c| {
            let (zero, other) = if little { (c[1], c[0]) } else { (c[0], c[1]) };
            zero == 0 && (other == b'\n' || other == b'\r' || other == b'\t' || other >= 0x20)
        })
        .count();
    let ratio = hits as f32 / pairs as f32;
    if ratio >= 0.8 {
        0.85
    } else if ratio >= 0.5 {
        0.45
    } else {
        0.0
    }
}

/// Shift_JIS / CP932 の見当。1 バイトでも構造が壊れていたら 0。
fn score_sjis(b: &[u8]) -> f32 {
    let is_lead = |c: u8| (0x81..=0x9F).contains(&c) || (0xE0..=0xFC).contains(&c);
    let is_trail = |c: u8| (0x40..=0x7E).contains(&c) || (0x80..=0xFC).contains(&c);
    let mut i = 0;
    let mut double = 0usize; // 2 バイト文字の数 (強い証拠)
    let mut kana = 0usize; // 半角カナ (弱い証拠 — 実ファイルでは稀)
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            i += 1;
        } else if (0xA1..=0xDF).contains(&c) {
            kana += 1;
            i += 1;
        } else if is_lead(c) && b.get(i + 1).copied().is_some_and(is_trail) {
            double += 1;
            i += 2;
        } else {
            return 0.0;
        }
    }
    non_ascii_score(double, kana)
}

/// EUC-JP の見当。
fn score_euc_jp(b: &[u8]) -> f32 {
    let is_ku = |c: u8| (0xA1..=0xFE).contains(&c);
    let mut i = 0;
    let mut double = 0usize;
    let mut kana = 0usize;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            i += 1;
        } else if c == 0x8E {
            // 半角カナ (SS2)
            match b.get(i + 1) {
                Some(t) if (0xA1..=0xDF).contains(t) => {
                    kana += 1;
                    i += 2;
                }
                _ => return 0.0,
            }
        } else if c == 0x8F {
            // JIS X 0212 (SS3)
            match (b.get(i + 1), b.get(i + 2)) {
                (Some(a), Some(t)) if is_ku(*a) && is_ku(*t) => {
                    double += 1;
                    i += 3;
                }
                _ => return 0.0,
            }
        } else if is_ku(c) && b.get(i + 1).copied().is_some_and(is_ku) {
            double += 1;
            i += 2;
        } else {
            return 0.0;
        }
    }
    non_ascii_score(double, kana)
}

/// ISO-2022-JP の見当。8 ビット目が立っていたら即 0、エスケープがあれば強い証拠。
fn score_iso2022jp(b: &[u8]) -> f32 {
    if b.iter().any(|c| *c >= 0x80) {
        return 0.0;
    }
    let esc = b.windows(3).any(|w| {
        w[0] == 0x1B
            && matches!(
                (w[1], w[2]),
                (b'$', b'B') | (b'$', b'@') | (b'(', b'B') | (b'(', b'J')
            )
    });
    if esc {
        0.95
    } else {
        // ASCII のみ = ISO-2022-JP としても読めるが決め手が無い
        0.4
    }
}

/// 単バイトコードページ (Latin-1 系など) の見当。並びの制約が無いので常に「読める」。
/// ASCII 域外があるほど「UTF-8 ではない」証拠にはなるが、どのコードページかは決められない。
fn score_single_byte(b: &[u8]) -> f32 {
    if b.iter().any(|c| *c >= 0x80) {
        0.45
    } else {
        0.4
    }
}

/// 2 バイト文字と半角カナの内訳から点をつける。
/// 半角カナだけの並びは (実ファイルでは稀なので) 決め手として弱く扱う。
fn non_ascii_score(double: usize, kana: usize) -> f32 {
    if double == 0 && kana == 0 {
        return 0.5; // ASCII のみ — 壊れてはいないが証拠も無い
    }
    if double == 0 {
        return 0.6; // 半角カナだけ — ありうるが弱い
    }
    0.95
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
        let need =
            unsafe { MultiByteToWideChar(cp, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
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

// ─────────────────────── 表示幅 (端末セル幅) ───────────────────────
//
// # なぜここにあるか
//
// 「バイト列 → 文字」と同じく「文字 → 画面で何桁を占めるか」も、外の世界
// (端末・他人の書いたテキスト) と自分の描画のあいだの**境界の決めごと**。
// 桁数の数え方が場所ごとにバラバラだと、CJK では即座にカーソルずれ・選択ずれ・
// 二重描画になる。だから表は 1 つだけ置いて、全員がここを見る。
//
// # 権威 (どれが「本当の桁数」か)
//
// **端末グリッドの真実は vt100 のセル (`Cell::is_wide`)** であって、この表ではない。
// vt100 は `unicode-width` の `UnicodeWidthChar::width()` を使う。したがって
// この表は「vt100 と一致すること」を要件として作られており、
// テスト [`width_matches_the_real_vt100_grid`] が実際の vt100 パーサへ
// 1 文字ずつ書き込んで突き合わせる。ずれたらテストが落ちる。
//
// この表が要るのは**グリッドの外**で桁を数える場面:
//   * IME 未確定文字列 (preedit) のオーバーレイ幅
//   * Cockpit のタイル・通知に出す端末末尾行の切り詰め
//   * 桁揃えが要る一覧表示
// ここで `chars().count()` を使うと、日本語の行だけ 2 倍はみ出す。
//
// # East Asian Ambiguous の方針 (既定 = Narrow)
//
// `─ │ × ± ° ※ ○ ● ■ △ ▽ Α-Ω а-я …` などは East Asian Width が **Ambiguous**
// で、「CJK 環境では 2 桁、それ以外では 1 桁」と規格が両方を認めている。
// 歴史的に日本語ユーザーは 2 桁を期待し (MS ゴシック時代の名残)、欧米ユーザーは
// 1 桁を期待する。
//
// **本プロジェクトの既定は [`AmbiguousWidth::Narrow`] (1 桁)。** 理由:
//
// 1. **グリッドと一致させないと壊れる。** 描画・カーソル・選択は vt100 のセルに
//    従う。vt100 は `width()` (= Ambiguous は 1) を使うので、こちらだけ 2 にすると
//    「見た目 2 桁・グリッド 1 桁」でカーソルが必ずずれる。片方だけ変えられない。
// 2. **相手 (CLI エージェント) も 1 桁で描いている。** Claude Code / Codex /
//    Gemini CLI などの TUI は罫線を Ambiguous 幅 1 前提でレイアウトする。
//    2 桁で数えると枠が破綻する。iTerm2 / Windows Terminal / VS Code の既定も 1。
//
// [`AmbiguousWidth::Wide`] を残してあるのは、将来 (a) 設定として出す
// (b) 全角罫線を好むユーザー向けプロファイルを作る、ときに**この 1 か所を
// 切り替えれば済む**ようにするため。グリッドまで 2 桁に揃えるには
// `vendor/vt100` 側も `width_cjk()` へ切り替える必要がある — 対で変えること。

/// East Asian Ambiguous な文字を何桁として数えるか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmbiguousWidth {
    /// 1 桁。vt100 グリッド・現代の端末エミュレータの既定と一致する。
    Narrow,
    /// 2 桁。CJK 専用端末の伝統的な挙動 (グリッド側も揃えないと使えない)。
    Wide,
}

/// 端末グリッドと一致する方針。ここを変えるときは `vendor/vt100` も対で変える。
pub const GRID_AMBIGUOUS: AmbiguousWidth = AmbiguousWidth::Narrow;

/// 幅 0 の文字 (結合記号・異体字セレクタ・ゼロ幅制御)。
///
/// これらは**直前のセルに乗る**のであって新しいセルを作らない。
/// vt100 も `width() == 0` の文字を直前セルへ `append` する。
/// 濁点 (U+3099/U+309A)・ハングルの中声/終声 (U+1160..=U+11FF)・
/// 異体字セレクタ (U+FE00..=U+FE0F, U+E0100..) と ZWJ (U+200D) がここに入るのが
/// 「ハングルが分裂する」「絵文字が分解する」を防ぐ肝。
const ZERO_WIDTH: &[(u32, u32)] = &[
    (0x00AD, 0x00AD), // SOFT HYPHEN (端末は桁を進めない)
    (0x0300, 0x036F), // 結合ダイアクリティカル
    (0x0483, 0x0489),
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    (0x0610, 0x061A),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DC),
    (0x06DF, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06EA, 0x06ED),
    (0x0711, 0x0711),
    (0x0730, 0x074A),
    (0x07A6, 0x07B0),
    (0x07EB, 0x07F3),
    (0x0816, 0x0819),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082D),
    (0x0859, 0x085B),
    (0x08E3, 0x0902),
    (0x093A, 0x093A),
    (0x093C, 0x093C),
    (0x0941, 0x0948),
    (0x094D, 0x094D),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    (0x0981, 0x0981),
    (0x09BC, 0x09BC),
    (0x09C1, 0x09C4),
    (0x09CD, 0x09CD),
    (0x09E2, 0x09E3),
    (0x0A01, 0x0A02),
    (0x0A3C, 0x0A3C),
    (0x0A41, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D),
    (0x0A70, 0x0A71),
    (0x0A81, 0x0A82),
    (0x0ABC, 0x0ABC),
    (0x0AC1, 0x0AC5),
    (0x0AC7, 0x0AC8),
    (0x0ACD, 0x0ACD),
    (0x0B01, 0x0B01),
    (0x0B3C, 0x0B3C),
    (0x0B3F, 0x0B3F),
    (0x0B41, 0x0B44),
    (0x0B4D, 0x0B4D),
    (0x0B56, 0x0B56),
    (0x0B82, 0x0B82),
    (0x0BC0, 0x0BC0),
    (0x0BCD, 0x0BCD),
    (0x0C00, 0x0C00),
    (0x0C3E, 0x0C40),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D),
    (0x0C55, 0x0C56),
    (0x0CBC, 0x0CBC),
    (0x0CBF, 0x0CBF),
    (0x0CC6, 0x0CC6),
    (0x0CCC, 0x0CCD),
    (0x0D01, 0x0D01),
    (0x0D41, 0x0D44),
    (0x0D4D, 0x0D4D),
    (0x0DCA, 0x0DCA),
    (0x0DD2, 0x0DD4),
    (0x0DD6, 0x0DD6),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E47, 0x0E4E),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EBC),
    (0x0EC8, 0x0ECD),
    (0x0F18, 0x0F19),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F71, 0x0F7E),
    (0x0F80, 0x0F84),
    (0x0F86, 0x0F87),
    (0x0F8D, 0x0F97),
    (0x0F99, 0x0FBC),
    (0x0FC6, 0x0FC6),
    (0x102D, 0x1030),
    (0x1032, 0x1037),
    (0x1039, 0x103A),
    (0x103D, 0x103E),
    (0x1058, 0x1059),
    (0x105E, 0x1060),
    (0x1071, 0x1074),
    (0x1082, 0x1082),
    (0x1085, 0x1086),
    (0x108D, 0x108D),
    (0x109D, 0x109D),
    // ハングル字母の中声・終声。初声 (U+1100..=U+115F) は幅 2 で、
    // ここは幅 0 として初声のセルに積み上がる = 音節が 1 セルに合成される。
    (0x1160, 0x11FF),
    (0x135D, 0x135F),
    (0x1712, 0x1714),
    (0x1732, 0x1734),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B4, 0x17B5),
    (0x17B7, 0x17BD),
    (0x17C6, 0x17C6),
    (0x17C9, 0x17D3),
    (0x17DD, 0x17DD),
    (0x180B, 0x180E),
    (0x18A9, 0x18A9),
    (0x1920, 0x1922),
    (0x1927, 0x1928),
    (0x1932, 0x1932),
    (0x1939, 0x193B),
    (0x1A17, 0x1A18),
    (0x1AB0, 0x1AFF),
    (0x1B00, 0x1B03),
    (0x1B34, 0x1B34),
    (0x1B36, 0x1B3A),
    (0x1B3C, 0x1B3C),
    (0x1B42, 0x1B42),
    (0x1B6B, 0x1B73),
    (0x1DC0, 0x1DFF),
    // ゼロ幅スペース〜ZWJ〜双方向制御。U+200D (ZWJ) がここに入ることで
    // 👨‍👩‍👧 のような ZWJ 連結が「構成要素の幅の和」として数えられる。
    (0x200B, 0x200F),
    (0x202A, 0x202E), // 双方向制御 (U+2028/U+2029 は行区切りで幅 1)
    (0x2060, 0x2069), // WJ・不可視演算子・分離方向制御
    (0x206A, 0x206F),
    (0x20D0, 0x20F0),
    (0x302A, 0x302F), // 表意文字用の声調記号
    // 濁点・半濁点 (か + ゛ = が)。直前の仮名セルに乗る。
    (0x3099, 0x309A),
    (0x3164, 0x3164), // ハングル・フィラー
    (0xA806, 0xA806),
    (0xA80B, 0xA80B),
    (0xA825, 0xA826),
    (0xFB1E, 0xFB1E),
    (0xFE00, 0xFE0F), // 異体字セレクタ VS1..VS16 (VS16 = 絵文字表示指定)
    (0xFE20, 0xFE2F), // 結合半記号
    (0xFEFF, 0xFEFF), // ZWNBSP (BOM)
    (0xFF9E, 0xFFA0), // 半角の濁点・半濁点・ハングルフィラー (直前のセルに乗る)
    (0xFFF9, 0xFFFB),
    (0x101FD, 0x101FD),
    (0x1D167, 0x1D169),
    (0x1D173, 0x1D182),
    (0x1D185, 0x1D18B),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F), // タグ文字 (🏴󠁧󠁢󠁳󠁣󠁴󠁿 のような旗の構成要素)
    (0xE0100, 0xE01EF), // 異体字セレクタ補助 (漢字の字形指定)
];

/// 幅 2 の文字 (East Asian Wide / Fullwidth と、既定で絵文字表示の記号)。
///
/// `unicode-width` 0.1 の表と一致させてある (vt100 が使う表)。
/// 一致は [`width_matches_the_real_vt100_grid`] が実機の vt100 で検証する。
const WIDE: &[(u32, u32)] = &[
    (0x1100, 0x115F), // ハングル字母 初声
    (0x231A, 0x231B), // ⌚⌛
    (0x2329, 0x232A), // 〈〉
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x2E80, 0x2E99), // CJK 部首補助
    (0x2E9B, 0x2EF3),
    (0x2F00, 0x2FD5), // 康熙部首
    (0x2FF0, 0x2FFB),
    (0x3000, 0x303E), // 全角スペース・CJK 記号と句読点
    (0x3041, 0x3096), // ひらがな
    (0x309B, 0x30FF), // 全角の濁点記号・カタカナ (U+3099/309A は幅 0 側)
    (0x3105, 0x312F), // 注音
    (0x3131, 0x318E), // ハングル互換字母
    (0x3190, 0x31E3),
    (0x31F0, 0x321E),
    (0x3220, 0x3247),
    (0x3250, 0x4DBF), // 囲み CJK + CJK 拡張 A
    (0x4E00, 0xA48C), // CJK 統合漢字 + ハングル/彝
    (0xA490, 0xA4C6),
    (0xA960, 0xA97C), // ハングル字母拡張 A
    (0xAC00, 0xD7A3), // ハングル音節
    (0xF900, 0xFAFF), // CJK 互換漢字
    (0xFE10, 0xFE19), // 縦書き形
    (0xFE30, 0xFE52),
    (0xFE54, 0xFE66),
    (0xFE68, 0xFE6B),
    (0xFF01, 0xFF60), // 全角英数・記号 (U+FF61..=U+FF9F の半角カナは幅 1)
    (0xFFE0, 0xFFE6), // 全角の￠￡￢￣￤￥
    (0x16FE0, 0x16FE4),
    (0x16FF0, 0x16FF1),
    (0x17000, 0x187F7),
    (0x18800, 0x18CD5),
    (0x18D00, 0x18D08),
    (0x1B000, 0x1B152),
    (0x1B164, 0x1B167),
    (0x1B170, 0x1B2FB),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F200, 0x1F202),
    (0x1F210, 0x1F23B),
    (0x1F240, 0x1F248),
    (0x1F250, 0x1F251),
    (0x1F260, 0x1F265),
    (0x1F300, 0x1F320),
    (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C),
    (0x1F37E, 0x1F393),
    (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0),
    (0x1F3F4, 0x1F3F4),
    // U+1F3FB..=U+1F3FF (肌の色モディファイア) はこの範囲に含まれる。
    // 単体では幅 2 だが、基底絵文字の直後に置かれると端末は 1 セルに重ねる —
    // グリッド (vt100) は 2 セル取る側なので、こちらも 2 で数えて一致させる。
    (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC),
    (0x1F4FF, 0x1F53D),
    (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CC, 0x1F6CC),
    (0x1F6D0, 0x1F6D2),
    (0x1F6D5, 0x1F6D7),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F4, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1F978),
    (0x1F97A, 0x1F9CB),
    (0x1F9CD, 0x1F9FF),
    (0x1FA70, 0x1FA74),
    (0x1FA78, 0x1FA7A),
    (0x1FA80, 0x1FA86),
    (0x1FA90, 0x1FAA8),
    (0x1FAB0, 0x1FAB6),
    (0x1FAC0, 0x1FAC2),
    (0x1FAD0, 0x1FAD6),
    (0x20000, 0x2FFFD), // CJK 拡張 B〜F
    (0x30000, 0x3FFFD), // CJK 拡張 G
];

/// East Asian Ambiguous の実用範囲 (罫線・矢印・ギリシャ/キリル・丸数字・数学記号)。
///
/// **既定 ([`AmbiguousWidth::Narrow`]) ではこの表は結果を変えない** —
/// Ambiguous は 1 桁で、表に載っていない文字も 1 桁だから。
/// [`AmbiguousWidth::Wide`] を選んだときにだけ効く。よって「日本語圏の端末で
/// 実際に 2 桁で描かれてきた文字」を実用本位で挙げてあり、Unicode の
/// Ambiguous 全域を網羅してはいない (網羅しても既定の挙動は 1 桁のまま)。
const AMBIGUOUS: &[(u32, u32)] = &[
    (0x00A1, 0x00A1), // ¡
    (0x00A4, 0x00A4), // ¤
    (0x00A7, 0x00A8), // §¨
    (0x00AA, 0x00AA),
    (0x00AE, 0x00AE),
    (0x00B0, 0x00B4), // °±²³´
    (0x00B6, 0x00BA),
    (0x00BC, 0x00BF),
    (0x00C6, 0x00C6),
    (0x00D0, 0x00D0),
    (0x00D7, 0x00D8), // ×Ø
    (0x00DE, 0x00E1),
    (0x00E6, 0x00E6),
    (0x00E8, 0x00EA),
    (0x00EC, 0x00ED),
    (0x00F0, 0x00F0),
    (0x00F2, 0x00F3),
    (0x00F7, 0x00FA), // ÷
    (0x00FC, 0x00FC),
    (0x00FE, 0x00FE),
    (0x0391, 0x03A1), // ギリシャ大文字
    (0x03A3, 0x03A9),
    (0x03B1, 0x03C1), // ギリシャ小文字
    (0x03C3, 0x03C9),
    (0x0401, 0x0401), // Ё
    (0x0410, 0x044F), // キリル
    (0x0451, 0x0451), // ё
    (0x2010, 0x2010),
    (0x2013, 0x2016), // – — ― ‖
    (0x2018, 0x2019), // ‘’
    (0x201C, 0x201D), // “”
    (0x2020, 0x2022), // †‡•
    (0x2024, 0x2027),
    (0x2030, 0x2030), // ‰
    (0x2032, 0x2033), // ′″
    (0x2035, 0x2035),
    (0x203B, 0x203B), // ※
    (0x203E, 0x203E),
    (0x2074, 0x2074),
    (0x207F, 0x207F),
    (0x2081, 0x2084),
    (0x20AC, 0x20AC), // €
    (0x2103, 0x2103), // ℃
    (0x2105, 0x2105),
    (0x2109, 0x2109), // ℉
    (0x2113, 0x2113),
    (0x2116, 0x2116), // №
    (0x2121, 0x2122), // ℡™
    (0x2126, 0x2126),
    (0x212B, 0x212B), // Å
    (0x2153, 0x2154),
    (0x215B, 0x215E),
    (0x2160, 0x216B), // ⅠⅡⅢ… ローマ数字
    (0x2170, 0x2179),
    (0x2189, 0x2189),
    (0x2190, 0x2199), // ←↑→↓ 矢印
    (0x21B8, 0x21B9),
    (0x21D2, 0x21D2), // ⇒
    (0x21D4, 0x21D4), // ⇔
    (0x21E7, 0x21E7),
    (0x2200, 0x2200), // ∀
    (0x2202, 0x2203),
    (0x2207, 0x2208),
    (0x220B, 0x220B),
    (0x220F, 0x220F),
    (0x2211, 0x2211),
    (0x2215, 0x2215),
    (0x221A, 0x221A), // √
    (0x221D, 0x2220),
    (0x2223, 0x2223),
    (0x2225, 0x2225),
    (0x2227, 0x222C),
    (0x222E, 0x222E),
    (0x2234, 0x2237),
    (0x223C, 0x223D),
    (0x2248, 0x2248), // ≈
    (0x224C, 0x224C),
    (0x2252, 0x2252), // ≒
    (0x2260, 0x2261), // ≠≡
    (0x2264, 0x2267), // ≤≥
    (0x226A, 0x226B),
    (0x226E, 0x226F),
    (0x2282, 0x2283),
    (0x2286, 0x2287),
    (0x2295, 0x2295),
    (0x2299, 0x2299),
    (0x22A5, 0x22A5),
    (0x22BF, 0x22BF),
    (0x2312, 0x2312),
    (0x2460, 0x24E9), // ①②③… 丸数字
    (0x24EB, 0x254B), // 囲み文字 + 罫線 ─│┌┐└┘├┤┬┴┼
    (0x2550, 0x2573), // 二重罫線・斜め罫線
    (0x2580, 0x258F), // ブロック要素
    (0x2592, 0x2595),
    (0x25A0, 0x25A1), // ■□
    (0x25A3, 0x25A9),
    (0x25B2, 0x25B3), // ▲△
    (0x25B6, 0x25B7),
    (0x25BC, 0x25BD), // ▼▽
    (0x25C0, 0x25C1),
    (0x25C6, 0x25C8), // ◆◇
    (0x25CB, 0x25CB), // ○
    (0x25CE, 0x25D1), // ◎●
    (0x25E2, 0x25E5),
    (0x25EF, 0x25EF),
    (0x2605, 0x2606), // ★☆
    (0x2609, 0x2609),
    (0x260E, 0x260F),
    (0x261C, 0x261C),
    (0x261E, 0x261E),
    (0x2640, 0x2640), // ♀
    (0x2642, 0x2642), // ♂
    (0x2660, 0x2661), // ♠♡
    (0x2663, 0x2665), // ♣♤♥
    (0x2667, 0x266A), // ♪
    (0x266C, 0x266D),
    (0x266F, 0x266F),
    (0x273D, 0x273D),
    (0x2776, 0x277F), // ❶❷❸…
];

/// 表 (昇順・重なりなし) に符号位置が入っているか。二分探索。
fn in_ranges(table: &[(u32, u32)], c: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// 1 文字が端末で占める桁数 (既定方針 = [`GRID_AMBIGUOUS`])。
///
/// 制御文字は 0 を返す (端末は桁を進めないため)。`\t` の桁送りはタブ位置に
/// 依存するのでここでは扱わない — 呼び出し側で展開してから渡すこと。
pub fn char_width(c: char) -> usize {
    char_width_with(c, GRID_AMBIGUOUS)
}

/// 方針を指定して 1 文字の桁数を得る。
pub fn char_width_with(c: char, amb: AmbiguousWidth) -> usize {
    let u = c as u32;
    // C0 / C1 制御と DEL: 端末は桁を進めない
    if u < 0x20 || (0x7F..0xA0).contains(&u) {
        return 0;
    }
    if in_ranges(ZERO_WIDTH, u) {
        return 0;
    }
    if in_ranges(WIDE, u) {
        return 2;
    }
    if amb == AmbiguousWidth::Wide && in_ranges(AMBIGUOUS, u) {
        return 2;
    }
    1
}

/// 文字列が端末で占める桁数 (既定方針)。
///
/// **1 文字ずつの合計**であって書記素クラスタ単位ではない。これは意図的で、
/// 端末グリッド (vt100) が 1 文字ずつセルを割り当てるから — 見た目の
/// 「合成後の幅」で数えると、グリッドの桁とずれてカーソルが合わなくなる。
/// 結合記号・異体字セレクタ・ZWJ は幅 0 なので、`が` (か+゛) や
/// `👨‍👩‍👧` (人+ZWJ+人+ZWJ+人) は自然に「基底の幅の合計」になる。
pub fn str_width(s: &str) -> usize {
    str_width_with(s, GRID_AMBIGUOUS)
}

/// 方針を指定して文字列の桁数を得る。
pub fn str_width_with(s: &str, amb: AmbiguousWidth) -> usize {
    s.chars().map(|c| char_width_with(c, amb)).sum()
}

/// 表示幅 `max` 桁に収まるよう切り詰め、切ったときだけ末尾に `…` を付ける。
///
/// `chars().count()` で切ると日本語の行が枠から 2 倍はみ出すため、
/// 桁数で数える。全角文字の途中で切れることは無い (1 文字単位で判定する)。
/// `…` 自身も 1 桁ぶん確保する。`max == 0` なら空文字列。
pub fn truncate_to_width(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // 省略記号 1 桁ぶんを残して詰める
    let budget = max - 1;
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// 絵文字の肌色修飾子 (Emoji_Modifier)。前の絵文字にくっついて 1 つの
/// 書記素クラスタを作るが、幅は 2 なので [`char_width`] では 0 にならない。
const EMOJI_MODIFIER: (u32, u32) = (0x1F3FB, 0x1F3FF);
/// 地域表示記号。2 つ並んで 1 つの国旗になる。
const REGIONAL_INDICATOR: (u32, u32) = (0x1F1E6, 0x1F1FF);
/// ZERO WIDTH JOINER。直後の文字まで巻き込んで 1 クラスタにする
/// (`👨‍👩‍👧` = 人 + ZWJ + 人 + ZWJ + 人)。
const ZWJ: char = '\u{200D}';

fn in_range(r: (u32, u32), c: char) -> bool {
    let u = c as u32;
    u >= r.0 && u <= r.1
}

/// この文字は「直前のクラスタにくっつく」か。
///
/// 幅 0 の文字 (結合ダイアクリティカル・異体字セレクタ・ZWJ) と絵文字修飾子が該当する。
/// **ASCII 制御文字は除く** — `\t` や `\n` も [`char_width`] は 0 を返すが、
/// これらは独立したトークンでなければ差分のトークン分割が壊れる。
fn joins_previous(c: char) -> bool {
    let u = c as u32;
    if u < 0x20 || u == 0x7F {
        return false;
    }
    char_width(c) == 0 || in_range(EMOJI_MODIFIER, c)
}

/// `s[at..]` の先頭にある**書記素クラスタ**の終端バイト位置を返す。
///
/// `at` は文字境界であること (そうでなければ `at` をそのまま返す)。
/// 戻り値は必ず文字境界なので、`&s[at..end]` は panic しない。
///
/// 完全な UAX #29 ではなく、差分の語単位ハイライトに必要な範囲だけを見る:
/// 結合記号・異体字セレクタ・ZWJ 連結・絵文字の肌色修飾子・国旗 (地域表示記号 2 つ)。
/// ハングルのジャモ合成は端末グリッドと同じく 1 文字ずつ扱う
/// ([`str_width`] の方針と揃える)。
pub fn grapheme_end(s: &str, at: usize) -> usize {
    if at >= s.len() || !s.is_char_boundary(at) {
        return at.min(s.len());
    }
    let mut it = s[at..].char_indices();
    let Some((_, first)) = it.next() else {
        return s.len();
    };
    let mut end = at + first.len_utf8();
    let regional = in_range(REGIONAL_INDICATOR, first);
    let mut paired_flag = false;
    let mut after_zwj = false;
    for (off, c) in it {
        let abs = at + off;
        if after_zwj {
            // ZWJ の直後は無条件で取り込む (絵文字の連結)。
            end = abs + c.len_utf8();
            after_zwj = false;
            continue;
        }
        if c == ZWJ {
            end = abs + c.len_utf8();
            after_zwj = true;
            continue;
        }
        if joins_previous(c) {
            end = abs + c.len_utf8();
            continue;
        }
        if regional && !paired_flag && in_range(REGIONAL_INDICATOR, c) {
            end = abs + c.len_utf8();
            paired_flag = true;
            continue;
        }
        break;
    }
    end
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
        assert_eq!(
            Encoding::Ansi(28591).label(),
            "CP28591",
            "知らない番号でも出せる"
        );
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
        assert!(
            PS_UTF8_PRELUDE.starts_with("try {"),
            "失敗しても止まらないこと"
        );
    }

    /// 打ち切りでお尻が切れた UTF-8 は、コードページで読み直してはいけない。
    /// (末尾の 1 文字のために全体を CP932 として読むと全部化ける)
    #[test]
    fn truncated_utf8_keeps_the_readable_head() {
        let full = "進捗を報告します".as_bytes();
        let cut = &full[..full.len() - 1]; // 最後の 1 文字が途中で切れている
        let s = decode_output(cut);
        assert!(
            s.starts_with("進捗を報告しま"),
            "頭は読めたままであること: {s}"
        );
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
        assert_eq!(
            normalize_to("末尾に改行なし", LineEnding::Crlf),
            "末尾に改行なし"
        );
        assert_eq!(normalize_to("", LineEnding::Crlf), "");
    }

    #[test]
    fn normalize_keeps_multibyte_text_intact() {
        let src = "日本語\r\n🚀 絵文字\nおわり";
        assert_eq!(
            normalize_to(&normalize_to(src, LineEnding::Lf), LineEnding::Crlf),
            "日本語\r\n🚀 絵文字\r\nおわり"
        );
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

    // ───────── CP932 / Shift_JIS の食い違い ─────────

    /// JIS と CP932 で解釈が割れる文字を、CP932 側の字形へ寄せられること。
    /// 表そのものを検証する (Windows でなくても回る)。
    #[test]
    fn jis_and_cp932_divergent_chars_are_folded() {
        // 有名な波ダッシュ問題を含む既知の食い違い
        let table = [
            ('\u{301C}', '\u{FF5E}', "波ダッシュ 〜 → 全角チルダ"),
            ('\u{2016}', '\u{2225}', "‖ → ∥"),
            ('\u{2212}', '\u{FF0D}', "− → －"),
            ('\u{00A2}', '\u{FFE0}', "¢ → ￠"),
            ('\u{00A3}', '\u{FFE1}', "£ → ￡"),
            ('\u{00AC}', '\u{FFE2}', "¬ → ￢"),
        ];
        for (jis, cp932, what) in table {
            let src = format!("前{jis}後");
            let got = fold_to_cp932(&src);
            assert_eq!(got, format!("前{cp932}後"), "{what}");
            // 寄せた先は冪等 (もう一度通しても変わらない)
            assert_eq!(fold_to_cp932(&got), got, "{what}: 冪等");
        }
    }

    /// 食い違い文字が無い本文は**確保せずそのまま**返る (常用パスを重くしない)。
    #[test]
    fn fold_to_cp932_is_borrowed_when_nothing_diverges() {
        let plain = "日本語のテキスト ASCII 123 ～ ／ ￥";
        assert!(
            matches!(fold_to_cp932(plain), std::borrow::Cow::Borrowed(_)),
            "対象が無ければ借用のまま"
        );
        assert_eq!(fold_to_cp932(plain), plain);
        assert!(matches!(fold_to_cp932(""), std::borrow::Cow::Borrowed(_)));
    }

    /// 表は「JIS 側 → CP932 側」の一方向で、往復ループを作らないこと。
    #[test]
    fn cp932_fold_table_has_no_cycles_or_duplicates() {
        for (i, (jis, cp)) in JIS_TO_CP932.iter().enumerate() {
            assert_ne!(jis, cp, "自分自身への写像は無意味");
            assert!(
                !JIS_TO_CP932.iter().any(|(j, _)| j == cp),
                "写像先が別の写像元になっている (循環): U+{:04X}",
                *cp as u32
            );
            assert!(
                !JIS_TO_CP932[..i].iter().any(|(j, _)| j == jis),
                "写像元の重複: U+{:04X}",
                *jis as u32
            );
        }
    }

    /// 不正なバイト列は**置換文字で受けて、その後ろを捨てない**こと。
    /// (途中で切ると「ファイルの後半が消える」という最悪の壊れ方になる)
    #[test]
    fn invalid_byte_sequences_degrade_without_truncating() {
        // Shift_JIS の「強」= 0x8B 0x5C。第 2 バイトが ASCII の `\` と同じ値で、
        // ここを取りこぼすとテキストに `\` が湧く (SJIS 5C 問題)。
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("SJIS 2 バイト文字", vec![0x82, 0xA0]),
            ("SJIS 5C 問題", vec![0x8B, 0x5C]),
            ("単独の継続バイト", vec![0x80, 0x80]),
            ("半角カナ", vec![0xB1, 0xB2, 0xB3]),
            ("UTF-8 の途中で切れた列", vec![0xE3, 0x81]),
            ("全ビット立て", vec![0xFF, 0xFE, 0xFD]),
        ];
        for (what, mut bytes) in cases {
            let head = b"HEAD-";
            let tail = b"-TAIL";
            let mut input = head.to_vec();
            input.append(&mut bytes);
            input.extend_from_slice(tail);
            for (path, text) in [
                ("decode_bytes", decode_bytes(&input).0),
                ("decode_output", decode_output(&input)),
            ] {
                assert!(text.starts_with("HEAD-"), "{what}/{path}: 前半が残る");
                assert!(
                    text.ends_with("-TAIL"),
                    "{what}/{path}: 後半が切り捨てられない ({text:?})"
                );
            }
        }
    }

    /// 空・1 バイト・BOM だけ、といった端の入力で落ちないこと。
    #[test]
    fn degenerate_inputs_do_not_panic() {
        for bytes in [
            vec![],
            vec![0xEF],
            vec![0xEF, 0xBB],
            vec![0xEF, 0xBB, 0xBF],       // BOM だけ
            vec![0xFF, 0xFE],             // UTF-16LE BOM だけ
            vec![0xFE, 0xFF],             // UTF-16BE BOM だけ
            vec![0xFF, 0xFE, 0x42],       // UTF-16LE で奇数バイト
            vec![0xFF, 0xFE, 0x00, 0xD8], // 対を欠いたサロゲート
        ] {
            let (text, enc) = decode_bytes(&bytes);
            // 復号できた分は必ず書き戻せる (往復で panic しない)
            let (back, _) = encode_bytes(&text, enc);
            assert!(back.len() < 64, "極小入力が膨らまない");
        }
    }

    // ───────── 表示幅 (端末セル幅) ─────────

    /// vt100 グリッドが実際に進めた桁数。**これがこのプロジェクトの正解**。
    /// 1 桁の `x` を置いてから対象を書き、カーソルの移動量を測る。
    fn grid_width(c: char) -> usize {
        let mut p = vt100::Parser::new(1, 40, 0);
        p.process(b"\x1b[H");
        let mut buf = [0u8; 4];
        p.process(b"x");
        p.process(c.encode_utf8(&mut buf).as_bytes());
        let (_, col) = p.screen().cursor_position();
        usize::from(col) - 1
    }

    /// 幅表が**実機の vt100 グリッドと一致**していること。
    /// ここがずれるとカーソル位置・選択範囲・全角の描画幅が全部ずれる。
    /// CJK/IME に効く区画は総当たり、それ以外は間引いて確認する。
    #[test]
    fn width_matches_the_real_vt100_grid() {
        let exhaustive: &[(u32, u32, &str)] = &[
            (0x0020, 0x00FF, "ASCII + Latin-1 (Ambiguous を含む)"),
            (0x1100, 0x11FF, "ハングル字母 (初声=2 / 中声・終声=0)"),
            (0x2000, 0x20FF, "一般句読点 (ZWJ・ゼロ幅)"),
            (0x2500, 0x25FF, "罫線・ブロック (Ambiguous)"),
            (0x3000, 0x30FF, "CJK 記号・かな・カタカナ"),
            (0x3130, 0x318F, "ハングル互換字母"),
            (0xA960, 0xA97F, "ハングル字母拡張 A"),
            (0xFE00, 0xFE0F, "異体字セレクタ"),
            (0xFF00, 0xFFEF, "全角形 + 半角カナ"),
        ];
        let sampled: &[(u32, u32, u32, &str)] = &[
            (0x3400, 0x4DBF, 37, "CJK 拡張 A"),
            (0x4E00, 0x9FFF, 97, "CJK 統合漢字"),
            (0xAC00, 0xD7A3, 101, "ハングル音節"),
            (0xF900, 0xFAFF, 13, "CJK 互換漢字"),
            (0x20000, 0x2A6DF, 997, "CJK 拡張 B"),
        ];
        let mut checked = 0usize;
        // 1 件目で止めず全件集めて出す (表の穴はまとめて直したいため)
        let mut bad: Vec<String> = Vec::new();
        let check = |u: u32, what: &str, bad: &mut Vec<String>| {
            let Some(c) = char::from_u32(u) else { return };
            let (ours, grid) = (char_width(c), grid_width(c));
            if ours != grid {
                bad.push(format!("U+{u:04X} ({what}): 表={ours} グリッド={grid}"));
            }
        };
        for &(lo, hi, what) in exhaustive {
            for u in lo..=hi {
                check(u, what, &mut bad);
                checked += 1;
            }
        }
        for &(lo, hi, step, what) in sampled {
            let mut u = lo;
            while u <= hi {
                check(u, what, &mut bad);
                checked += 1;
                u += step;
            }
        }
        assert!(checked > 1500, "検証した符号位置が少なすぎる: {checked}");
        assert!(
            bad.is_empty(),
            "幅表が vt100 グリッドと食い違う ({} 件):\n{}",
            bad.len(),
            bad.join("\n")
        );
    }

    /// 代表的な文字種の幅を**データ表**で押さえる (方針の明文化)。
    #[test]
    fn width_table_over_a_representative_corpus() {
        // (文字, 期待幅, 説明)
        let corpus: &[(char, usize, &str)] = &[
            // ASCII
            ('A', 1, "ASCII 英字"),
            (' ', 1, "空白"),
            ('\u{0}', 0, "NUL (制御文字は桁を進めない)"),
            ('\u{1B}', 0, "ESC"),
            ('\u{7F}', 0, "DEL"),
            // 日本語
            ('あ', 2, "ひらがな"),
            ('ア', 2, "全角カタカナ"),
            ('漢', 2, "漢字"),
            ('、', 2, "全角読点"),
            ('　', 2, "全角スペース"),
            ('ｱ', 1, "半角カタカナ"),
            ('ﾞ', 0, "半角濁点 (直前の半角カナに乗る = ｶ + ﾞ で 1 セル)"),
            ('｡', 1, "半角句点"),
            ('Ａ', 2, "全角英字 (Fullwidth Forms)"),
            ('￥', 2, "全角円記号"),
            ('¥', 1, "半角円記号 (U+00A5 は Ambiguous でない)"),
            // ハングル
            ('한', 2, "ハングル音節"),
            ('글', 2, "ハングル音節"),
            ('ᄒ', 2, "ハングル初声 (単独では 2 桁)"),
            ('\u{1161}', 0, "ハングル中声 (初声のセルに乗る)"),
            ('\u{11AB}', 0, "ハングル終声 (初声のセルに乗る)"),
            ('ㄱ', 2, "ハングル互換字母"),
            // 中国語
            ('中', 2, "簡体字"),
            ('國', 2, "繁体字"),
            // 結合・ゼロ幅
            ('\u{3099}', 0, "結合濁点 (か + ゛ = が)"),
            ('\u{309A}', 0, "結合半濁点"),
            ('\u{0301}', 0, "結合アキュート"),
            ('\u{200B}', 0, "ゼロ幅スペース"),
            ('\u{200D}', 0, "ZWJ (絵文字連結)"),
            ('\u{FEFF}', 0, "ZWNBSP / BOM"),
            ('\u{FE0F}', 0, "異体字セレクタ 16 (絵文字表示)"),
            ('\u{FE0E}', 0, "異体字セレクタ 15 (文字表示)"),
            ('\u{E0101}', 0, "異体字セレクタ補助 (漢字の字形指定)"),
            // 絵文字
            ('😀', 2, "絵文字"),
            ('👍', 2, "絵文字"),
            ('🧑', 2, "絵文字 (人)"),
            ('\u{1F3FB}', 2, "肌の色モディファイア (グリッドは 2 桁取る)"),
            ('⌚', 2, "既定で絵文字表示の記号"),
            ('★', 1, "Ambiguous な記号は既定 1 桁"),
        ];
        for &(c, want, what) in corpus {
            assert_eq!(char_width(c), want, "U+{:04X} {what}", c as u32);
        }
    }

    /// Ambiguous 幅の方針が**表 1 か所**で切り替わること。
    /// 既定は Narrow (= vt100 グリッドと一致)。
    #[test]
    fn ambiguous_width_policy_is_switchable_from_one_place() {
        assert_eq!(
            GRID_AMBIGUOUS,
            AmbiguousWidth::Narrow,
            "既定はグリッド (vt100 = unicode-width の width()) に合わせる"
        );
        // 日本語ユーザーが 2 桁を期待しがちな代表例
        let ambiguous = [
            ('─', "罫線 横"),
            ('│', "罫線 縦"),
            ('┼', "罫線 十字"),
            ('×', "乗算記号"),
            ('±', "プラスマイナス"),
            ('°', "度"),
            ('※', "米印"),
            ('○', "白丸"),
            ('●', "黒丸"),
            ('■', "黒四角"),
            ('▲', "黒三角"),
            ('①', "丸数字"),
            ('Ⅲ', "ローマ数字"),
            ('α', "ギリシャ小文字"),
            ('Ω', "ギリシャ大文字"),
            ('д', "キリル"),
            ('→', "矢印"),
            ('≒', "ニアリーイコール"),
            ('★', "黒星"),
            ('♪', "音符"),
        ];
        for (c, what) in ambiguous {
            assert_eq!(
                char_width_with(c, AmbiguousWidth::Narrow),
                1,
                "{what}: Narrow"
            );
            assert_eq!(char_width_with(c, AmbiguousWidth::Wide), 2, "{what}: Wide");
            assert_eq!(
                char_width(c),
                grid_width(c),
                "{what}: 既定はグリッドと一致していなければならない"
            );
        }
        // 方針を変えても Wide/ゼロ幅の判定は動かない
        for c in ['あ', '漢', '한', 'Ａ'] {
            assert_eq!(char_width_with(c, AmbiguousWidth::Wide), 2);
        }
        for c in ['\u{200D}', '\u{FE0F}', '\u{3099}'] {
            assert_eq!(char_width_with(c, AmbiguousWidth::Wide), 0);
        }
    }

    /// 文字列の幅は「グリッドが割り当てるセル数」と一致すること。
    /// 濁点・異体字セレクタ・ZWJ 連結・肌の色つき絵文字まで通しで確認する。
    #[test]
    fn str_width_matches_the_grid_for_composed_sequences() {
        let cases: &[(&str, usize, &str)] = &[
            ("hello", 5, "ASCII"),
            ("日本語", 6, "漢字 3 文字"),
            ("あア漢", 6, "かな + 漢字"),
            ("ｱｲｳ", 3, "半角カナ"),
            ("한글", 4, "ハングル音節"),
            (
                "\u{1112}\u{1161}\u{11AB}",
                2,
                "ハングル 초성+중성+종성 = 1 音節",
            ),
            ("か\u{3099}", 2, "か + 結合濁点 = が (1 セル)"),
            ("e\u{0301}", 1, "e + 結合アキュート"),
            ("葛\u{E0100}", 2, "漢字 + 異体字セレクタ"),
            ("😀", 2, "絵文字 1 つ"),
            ("\u{1F44D}\u{1F3FB}", 4, "👍 + 肌の色 (グリッドは 2 セル×2)"),
            (
                "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
                6,
                "ZWJ 連結の家族絵文字 (基底 3 つ ぶんのセル)",
            ),
            ("â\u{FE0F}", 1, "異体字セレクタは幅 0"),
            ("mix 日本 ok", 11, "混在"),
        ];
        for &(s, want, what) in cases {
            assert_eq!(str_width(s), want, "{what}: {s:?}");
            // 実機グリッドでも同じセル数になること
            let mut p = vt100::Parser::new(1, 80, 0);
            p.process(b"\x1b[H");
            p.process(s.as_bytes());
            let (_, col) = p.screen().cursor_position();
            assert_eq!(usize::from(col), want, "{what}: vt100 のカーソル移動量");
        }
    }

    /// 桁数で切り詰めること (文字数で切ると日本語の行が 2 倍はみ出す)。
    #[test]
    fn truncate_to_width_counts_columns_not_chars() {
        assert_eq!(
            truncate_to_width("abcdef", 10),
            "abcdef",
            "収まるなら無加工"
        );
        assert_eq!(
            truncate_to_width("abcdef", 6),
            "abcdef",
            "ちょうどなら無加工"
        );
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        // 「日本語です」= 10 桁。8 桁枠なら 7 桁ぶん + …
        assert_eq!(truncate_to_width("日本語です", 8), "日本語…");
        assert_eq!(truncate_to_width("日本語です", 10), "日本語です");
        // 全角の途中では切らない (奇数枠でも 1 文字単位)
        let cut = truncate_to_width("日本語です", 7);
        assert_eq!(cut, "日本語…");
        assert!(str_width(&cut) <= 7, "枠を超えない");
        assert_eq!(truncate_to_width("日本語", 0), "", "枠 0 は空");
        assert_eq!(truncate_to_width("", 5), "");
        // 結合列の途中で切っても不正な文字列にならない
        for max in 0..8 {
            let s = truncate_to_width("か\u{3099}き\u{3099}く", max);
            assert!(str_width(&s) <= max.max(1), "max={max}");
        }
    }

    /// 幅の表そのものが壊れていない (昇順・重なりなし) こと。
    /// 二分探索は表の整列を前提にしているので、崩れると静かに誤答する。
    #[test]
    fn width_tables_are_sorted_and_disjoint() {
        for (name, table) in [
            ("ZERO_WIDTH", ZERO_WIDTH),
            ("WIDE", WIDE),
            ("AMBIGUOUS", AMBIGUOUS),
        ] {
            for w in table.windows(2) {
                assert!(w[0].0 <= w[0].1, "{name}: 範囲が逆転 {:04X?}", w[0]);
                assert!(
                    w[0].1 < w[1].0,
                    "{name}: 昇順でない/重なっている {:04X?} {:04X?}",
                    w[0],
                    w[1]
                );
            }
        }
        // 幅 0 と 幅 2 が同じ文字を主張していないこと
        for &(lo, hi) in WIDE {
            for u in [lo, hi] {
                assert!(
                    !in_ranges(ZERO_WIDTH, u),
                    "U+{u:04X} が WIDE と ZERO_WIDTH の両方にある"
                );
            }
        }
    }

    // ═══════════ エンコーディングピッカー ═══════════

    /// 「日本語」を各符号化のバイト列で書き下したもの。
    /// 復号表を持たない環境でも判定器 (バイト列の構造だけ見る) を検査できるようにする。
    const NIHONGO_CP932: &[u8] = &[0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA];
    const NIHONGO_EUCJP: &[u8] = &[0xC6, 0xFC, 0xCB, 0xDC, 0xB8, 0xEC];

    /// **表に載っている符号化はすべて本当に往復する** — 表の正直さの検査。
    ///
    /// [`supported_encodings`] 自身も往復で作るが、こちらは**別の見本**で
    /// 確かめる (見本 1 つに合わせた表になっていないこと)。
    #[test]
    fn every_listed_encoding_really_round_trips() {
        let ascii = "line one\nline two\ttabbed\n";
        let ja = "吾輩は猫である。名前はまだ無い。\nカタカナも かなも 漢字も\n";
        for info in supported_encodings() {
            let r = save_with(ascii, info.enc, LineEnding::Lf)
                .unwrap_or_else(|e| panic!("{}: ASCII すら保存できない: {}", info.id, e.message()));
            let back = reopen_with_report(&r, info.enc);
            assert_eq!(back.text, ascii, "{}: ASCII の往復が壊れた", info.id);
            assert_eq!(back.replacements, 0, "{}: ASCII で化けた", info.id);
            assert_eq!(
                back.format.encoding, info.enc,
                "{}: 符号化が変わった",
                info.id
            );

            if info.japanese {
                let bytes = save_with(ja, info.enc, LineEnding::Lf)
                    .unwrap_or_else(|e| panic!("{}: 日本語可のはずが {}", info.id, e.message()));
                let back = reopen_with_report(&bytes, info.enc);
                assert_eq!(back.text, ja, "{}: 日本語の往復が壊れた", info.id);
                assert_eq!(back.replacements, 0, "{}: 日本語で化けた", info.id);
            }
        }
    }

    /// Unicode 系 4 種は変換表を OS に頼らない (純 Rust) ので、どの環境でも必ず載る。
    #[test]
    fn unicode_encodings_are_available_everywhere() {
        for enc in [
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
        ] {
            assert!(is_supported(enc), "{} が表に無い", enc.name());
            let info = supported_encodings().iter().find(|i| i.enc == enc).unwrap();
            assert!(info.japanese, "{} は日本語を通せるはず", enc.name());
        }
    }

    /// ANSI コードページはこの環境で実際に変換できたときだけ載る
    /// (Windows 以外では 1 つも載らないのが正しい)。
    #[test]
    fn ansi_encodings_are_listed_only_when_the_os_can_convert() {
        let ansi: Vec<u32> = supported_encodings()
            .iter()
            .filter_map(|i| match i.enc {
                Encoding::Ansi(cp) => Some(cp),
                _ => None,
            })
            .collect();
        if cfg!(windows) {
            // 何が載るかは OS 次第 (言語環境を決め打ちしない)。載ったものは
            // every_listed_encoding_really_round_trips が往復を保証している。
            return;
        }
        assert!(
            ansi.is_empty(),
            "変換表を持たない環境なのに ANSI が載っている: {ansi:?}"
        );
        assert_eq!(supported_encodings().len(), 4, "Unicode 系 4 種だけのはず");
        assert!(!is_supported(Encoding::Ansi(CP_932)));
    }

    #[test]
    fn encoding_table_ids_are_unique_and_resolvable() {
        let mut ids: Vec<&str> = supported_encodings()
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "識別子が重複している");
        for info in supported_encodings() {
            assert_eq!(encoding_by_name(&info.id), Some(info.enc), "id で引けない");
            assert_eq!(
                encoding_by_name(&info.label),
                Some(info.enc),
                "label で引けない"
            );
            for a in &info.aliases {
                assert_eq!(encoding_by_name(a), Some(info.enc), "別名 {a} で引けない");
            }
        }
    }

    #[test]
    fn encoding_by_name_ignores_case_and_separators() {
        assert_eq!(encoding_by_name("UTF-8"), Some(Encoding::Utf8));
        assert_eq!(encoding_by_name("utf8"), Some(Encoding::Utf8));
        assert_eq!(encoding_by_name("UTF_16 le"), Some(Encoding::Utf16Le));
        assert_eq!(encoding_by_name("存在しない"), None);
        // 使えない符号化は引けない (UI が選べないようにする)
        if !cfg!(windows) {
            assert_eq!(encoding_by_name("sjis"), None);
        }
    }

    // ──────────── 開き直す ────────────

    #[test]
    fn reopen_with_ignores_detection_and_reports_lossy() {
        // CP932 の「日本語」を UTF-8 として開くと 6 バイトすべてが化ける
        let r = reopen_with_report(NIHONGO_CP932, Encoding::Utf8);
        assert!(r.lossy(), "化けたのに lossy でない");
        // 6 バイト中 0x7B だけは ASCII '{' として素通りするので化けるのは 5 バイト
        assert_eq!(r.replacements, 5, "不正バイトの数だけ U+FFFD が出る");
        assert!(
            r.text.contains('{'),
            "たまたま ASCII だったバイトは残る: {:?}",
            r.text
        );
        assert_eq!(
            r.format.encoding,
            Encoding::Utf8,
            "要求した符号化を報告する"
        );
    }

    #[test]
    fn reopen_with_clean_input_reports_no_replacement() {
        let bytes = save_with("日本語\n", Encoding::Utf8, LineEnding::Lf).unwrap();
        let r = reopen_with_report(&bytes, Encoding::Utf8);
        assert!(!r.lossy());
        assert_eq!(r.replacements, 0);
        assert_eq!(r.text, "日本語\n");
    }

    #[test]
    fn reopen_with_strips_the_bom_but_keeps_the_requested_encoding() {
        let bytes = save_with("あ", Encoding::Utf8Bom, LineEnding::Lf).unwrap();
        // BOM 付きのバイト列を「UTF-8 (BOM なし)」で開き直す
        let (text, fmt) = reopen_with(&bytes, Encoding::Utf8);
        assert_eq!(text, "あ", "U+FEFF が本文に残らない");
        assert_eq!(
            fmt.encoding,
            Encoding::Utf8,
            "保存すると BOM が落ちる状態になる"
        );
        // BOM 付きとして開けば当然そのまま
        assert_eq!(reopen_with(&bytes, Encoding::Utf8Bom).0, "あ");
    }

    #[test]
    fn reopen_with_wrong_utf16_endianness_is_visibly_broken() {
        let bytes = save_with("AB\n", Encoding::Utf16Le, LineEnding::Lf).unwrap();
        let (good, _) = reopen_with(&bytes, Encoding::Utf16Le);
        assert_eq!(good, "AB\n");
        let (bad, fmt) = reopen_with(&bytes, Encoding::Utf16Be);
        assert_ne!(bad, "AB\n", "向きを間違えたのに同じ本文が出た");
        assert_eq!(fmt.encoding, Encoding::Utf16Be);
    }

    #[test]
    fn reopen_with_reports_the_line_ending_of_the_decoded_text() {
        let bytes = save_with("a\nb\n", Encoding::Utf8, LineEnding::Crlf).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\n");
        let (_, fmt) = reopen_with(&bytes, Encoding::Utf8);
        assert_eq!(fmt.line_ending, LineEnding::Crlf);
    }

    // ──────────── 指定して保存する ────────────

    #[test]
    fn save_with_applies_the_requested_line_ending() {
        assert_eq!(
            save_with("a\r\nb", Encoding::Utf8, LineEnding::Lf).unwrap(),
            b"a\nb"
        );
        assert_eq!(
            save_with("a\nb", Encoding::Utf8, LineEnding::Crlf).unwrap(),
            b"a\r\nb"
        );
        assert_eq!(
            save_with("a\nb", Encoding::Utf8, LineEnding::Cr).unwrap(),
            b"a\rb"
        );
    }

    /// **回帰テスト**: `encode_bytes` は変換できない文字があると黙って UTF-8 へ
    /// 格上げする。`save_with` は絶対にそれをしない。
    #[test]
    fn save_with_never_silently_changes_the_encoding() {
        let text = "𠮟る 🐧 と\n";
        for info in supported_encodings() {
            match save_with(text, info.enc, LineEnding::Lf) {
                // 書けたなら、**同じ符号化で**開き直して一致しなければならない。
                // 黙って UTF-8 になっていたらここで落ちる。
                Ok(bytes) => {
                    let back = reopen_with_report(&bytes, info.enc);
                    assert_eq!(back.text, text, "{}: 別の符号化で書かれている", info.id);
                    assert_eq!(back.replacements, 0, "{}: 書いた本文が化けている", info.id);
                }
                // 書けないなら断る。UTF-8 へ逃げない。
                Err(e) => assert_eq!(
                    e.encoding(),
                    info.enc,
                    "{}: 別の符号化の話をしている",
                    info.id
                ),
            }
        }
        // CP932 に無い文字は、どの環境でも「保存できた」ことにならない
        let r = save_with(text, Encoding::Ansi(CP_932), LineEnding::Lf);
        assert!(r.is_err(), "CP932 で書けないはずの本文が Ok になった");
        assert_eq!(r.unwrap_err().encoding(), Encoding::Ansi(CP_932));
        // 旧経路 (encode_bytes) は今も UTF-8 へ格上げする — 挙動の違いを明文化する
        assert_eq!(encode_bytes(text, Encoding::Ansi(CP_932)).1, Encoding::Utf8);
    }

    /// 変換できない文字の**特定**は OS の変換表と無関係な計算なので、
    /// 判定関数を差し替えて (= 変換表が無い環境でも) 検査する。
    #[test]
    fn encode_issue_names_the_first_offending_character() {
        let text = "一行目\n二行目\nあいう𠮟えお\n";
        let issue = first_unencodable(text, Encoding::Ansi(CP_932), |c| c != '𠮟')
            .expect("見つからないはずがない");
        match &issue {
            EncodeIssue::Unrepresentable {
                ch,
                char_index,
                byte_index,
                line,
                column,
                enc,
            } => {
                assert_eq!(*ch, '𠮟');
                assert_eq!(*char_index, 11, "「一行目\\n二行目\\nあいう」= 11 文字");
                assert_eq!(
                    *byte_index, 29,
                    "3文字×3バイト + 改行 の 2 行 + 3 文字 = 29 バイト"
                );
                assert_eq!(*line, 3);
                assert_eq!(*column, 4);
                assert_eq!(*enc, Encoding::Ansi(CP_932));
            }
            other => panic!("想定外: {other:?}"),
        }
        assert_eq!(issue.ch(), Some('𠮟'));
        assert_eq!(issue.char_index(), Some(11));
        let msg = issue.message();
        assert!(msg.contains('𠮟'), "文字が入っていない: {msg}");
        assert!(msg.contains("3行目"), "行番号が入っていない: {msg}");
        assert!(
            msg.contains("Shift_JIS"),
            "符号化の名前が入っていない: {msg}"
        );
        // 全部書けるなら None
        assert!(first_unencodable("abc", Encoding::Utf8, |_| true).is_none());
    }

    #[test]
    fn encode_issue_message_is_readable_for_unsupported_environments() {
        let issue = EncodeIssue::Unsupported {
            enc: Encoding::Ansi(CP_932),
        };
        assert_eq!(issue.ch(), None);
        assert_eq!(issue.char_index(), None);
        assert!(issue.message().contains("CP932"), "{}", issue.message());
    }

    /// Windows 以外では ANSI 変換表が無いので、**中身のせいにせず**環境の問題だと言う。
    #[test]
    fn save_with_ansi_without_a_conversion_table_reports_unsupported() {
        if cfg!(windows) {
            return;
        }
        let err = save_with("ASCII only", Encoding::Ansi(CP_932), LineEnding::Lf).unwrap_err();
        assert!(
            matches!(err, EncodeIssue::Unsupported { .. }),
            "ASCII が書けないのは文字のせいではない: {err:?}"
        );
    }

    // ──────────── 推定 ────────────

    #[test]
    fn detect_all_is_sorted_and_scored() {
        for sample in [
            &b"plain ascii\n"[..],
            "日本語\n".as_bytes(),
            NIHONGO_CP932,
            NIHONGO_EUCJP,
            &[],
        ] {
            let got = detect_all(sample);
            for w in got.windows(2) {
                assert!(w[0].1 >= w[1].1, "降順に並んでいない: {got:?}");
            }
            for (enc, score) in &got {
                assert!(
                    *score > 0.0 && *score <= 1.0,
                    "点数の範囲外: {enc:?} {score}"
                );
            }
        }
    }

    #[test]
    fn detect_all_ranks_utf8_cp932_and_eucjp() {
        // UTF-8 の日本語 → UTF-8 が 1 位
        let utf8 = detect_all("日本語です\n".as_bytes());
        assert_eq!(utf8[0].0, Encoding::Utf8);
        assert_eq!(utf8[0].1, 1.0);

        // CP932 の「日本語」→ CP932 が 1 位、UTF-8 は候補にすら出ない
        let sjis = detect_all(NIHONGO_CP932);
        assert_eq!(sjis[0].0, Encoding::Ansi(CP_932), "候補: {sjis:?}");
        assert!(
            !sjis.iter().any(|(e, _)| *e == Encoding::Utf8),
            "UTF-8 として読めないのに候補に出た: {sjis:?}"
        );

        // EUC-JP の「日本語」→ EUC-JP が 1 位 (CP932 としては構造が壊れている)
        let euc = detect_all(NIHONGO_EUCJP);
        assert!(
            matches!(euc[0].0, Encoding::Ansi(cp) if cp == CP_EUC_JP || cp == CP_EUC_JP_X0212),
            "EUC-JP が 1 位でない: {euc:?}"
        );
        assert!(
            !euc.iter().any(|(e, _)| *e == Encoding::Ansi(CP_932)),
            "CP932 として壊れているのに候補に出た: {euc:?}"
        );
    }

    #[test]
    fn detect_all_trusts_the_bom_absolutely() {
        for enc in [Encoding::Utf8Bom, Encoding::Utf16Le, Encoding::Utf16Be] {
            let bytes = save_with("日本語\n", enc, LineEnding::Lf).unwrap();
            let got = detect_all(&bytes);
            assert_eq!(got[0].0, enc, "BOM を見落とした: {got:?}");
            assert_eq!(got[0].1, 1.0);
        }
    }

    #[test]
    fn detect_all_ascii_prefers_utf8_but_keeps_alternatives() {
        let got = detect_all(b"hello world\n");
        assert_eq!(got[0].0, Encoding::Utf8, "{got:?}");
        assert!(got[0].1 < 1.0, "ASCII だけなので断定はしない: {got:?}");
        assert!(got.len() > 1, "「代わりに X で開く」候補が無い: {got:?}");
    }

    #[test]
    fn detect_all_finds_utf16_without_a_bom() {
        let with_bom = save_with("hello world", Encoding::Utf16Le, LineEnding::Lf).unwrap();
        let got = detect_all(&with_bom[2..]); // BOM を剥がして渡す
        assert_eq!(got[0].0, Encoding::Utf16Le, "{got:?}");
    }

    #[test]
    fn detect_all_agrees_with_decode_bytes_on_utf8() {
        let bytes = "日本語\r\nです\r\n".as_bytes();
        assert_eq!(decode_bytes(bytes).1, Encoding::Utf8);
        assert_eq!(detect_all(bytes)[0].0, Encoding::Utf8);
    }

    // ---- grapheme_end (差分の語単位ハイライトの土台) -----------------------

    /// 先頭から順に `grapheme_end` を回して、クラスタの列を作る。
    fn clusters(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.len() {
            let e = grapheme_end(s, i);
            assert!(e > i, "前に進まない: {s:?} at {i}");
            out.push(&s[i..e]);
            i = e;
        }
        out
    }

    #[test]
    fn grapheme_end_table() {
        // (入力, 期待するクラスタ列, 何を見ているか)
        let table: &[(&str, &[&str], &str)] = &[
            ("abc", &["a", "b", "c"], "ASCII は 1 文字 1 クラスタ"),
            ("日本語", &["日", "本", "語"], "日本語のみ"),
            (
                "か\u{3099}き",
                &["か\u{3099}", "き"],
                "結合濁点は基底にくっつく",
            ),
            ("é", &["é"], "合成済み (1 文字)"),
            (
                "e\u{0301}",
                &["e\u{0301}"],
                "分解形 (基底 + 結合アクセント)",
            ),
            ("🚀ok", &["🚀", "o", "k"], "4 バイト絵文字は割らない"),
            (
                "👍\u{1F3FD}!",
                &["👍\u{1F3FD}", "!"],
                "肌色修飾子は前にくっつく",
            ),
            (
                "👨\u{200D}👩\u{200D}👧",
                &["👨\u{200D}👩\u{200D}👧"],
                "ZWJ の家族絵文字は 1 クラスタ",
            ),
            ("🇯🇵🇺🇸", &["🇯🇵", "🇺🇸"], "国旗は地域表示記号 2 つで 1 つ"),
            ("a\tb", &["a", "\t", "b"], "タブは幅 0 でも独立トークン"),
            (
                "あ\u{FE0F}",
                &["あ\u{FE0F}"],
                "異体字セレクタは前にくっつく",
            ),
            ("𝔘𝔫", &["𝔘", "𝔫"], "BMP 外 (4 バイト) を割らない"),
        ];
        for (input, want, why) in table {
            assert_eq!(&clusters(input)[..], *want, "{why}: {input:?}");
        }
    }

    #[test]
    fn grapheme_end_never_splits_a_char() {
        // 4 バイト文字・結合列・絵文字が混ざった文字列でも、返る位置は必ず文字境界。
        let s = "a日🚀\u{1F469}\u{200D}\u{1F4BB}か\u{3099}𝔘";
        let mut i = 0;
        while i < s.len() {
            let e = grapheme_end(s, i);
            assert!(s.is_char_boundary(e), "文字境界でない: {e} in {s:?}");
            assert!(e > i && e <= s.len());
            i = e;
        }
        assert_eq!(i, s.len(), "最後まで走り切る");
    }

    #[test]
    fn grapheme_end_handles_out_of_range_and_non_boundary() {
        let s = "日本";
        assert_eq!(grapheme_end(s, s.len()), s.len(), "終端はそのまま");
        assert_eq!(grapheme_end(s, 999), s.len(), "範囲外は末尾へ丸める");
        // 文字の途中 (境界でない) を渡しても panic しない
        assert_eq!(grapheme_end(s, 1), 1, "境界でなければ動かさない");
        assert_eq!(grapheme_end("", 0), 0, "空文字列");
    }
}
