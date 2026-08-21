//! ユニバーサルプレビュー — **どんなファイルでも壊れずに開く**ための下地。
//!
//! エディタは長らく「テキストとして読めなかったもの」を `textenc::decode_bytes`
//! の lossy 変換に落としていた。結果、動画・書庫・実行ファイル・SQLite を開くと
//! **バイナリの文字化けが本文になる**。本文にならないだけならまだしも、巨大な
//! バイナリが丸ごと `String` に化けてメモリと描画の両方を殺す。
//!
//! ここは「読めないもの」を **読める形へ落とす純関数**だけを置く層。
//!
//! * [`looks_binary`] — 中身 (拡張子ではない) でテキストかどうかを決める
//! * [`sniff_kind`] — マジックナンバーから種別名を当てる
//! * [`hex_row`] — 16 進ダンプの 1 行を**その行だけ**組み立てる (全体を展開しない)
//! * [`probe_media`] / [`locate_moov`] / [`probe_mp4_moov`] — 動画・音声のヘッダ解析
//! * [`parse_zip_at`] — ZIP のセントラルディレクトリ解析 (新規依存なし)
//! * [`decode_animation`] / [`frame_at`] — アニメーション画像 (GIF / APNG /
//!   アニメーション WebP) を RGBA のコマ列へ落とし、経過時間からコマ番号を決める
//!
//! ## 設計方針
//!
//! - **IO を持たない。** ファイルを読むのは `editor.rs`。ここはバイト列を受けて
//!   値を返すだけなので、テーブルテストで壊れた入力まで固定できる。
//! - **絶対に panic しない。** 途中で切れたヘッダ・嘘のサイズ・負の入れ子は
//!   すべて「情報なし」へ落とす。プレビューが原因でエディタが落ちてはならない。
//! - **文字列 (i18n) を持たない。** 表示文言は app.rs が `tr` / `trf` で組む。
//!   ここが返す `&'static str` は "PNG" のような**書式名**だけで翻訳不要。

use std::path::Path;

// ---------------------------------------------------------------------------
// テキスト / バイナリの判定
// ---------------------------------------------------------------------------

/// 中身を覗く先頭バイト数。8 KB あれば符号化の判定には十分で、
/// どんなに大きいファイルでもこの分しか読まない。
pub const SNIFF_BYTES: usize = 8 * 1024;

/// 制御文字がこの割合 (%) を超えたらバイナリとみなす。
///
/// 5% は「Latin-1 などの未知の 8bit テキストを誤ってバイナリにしない」ことを
/// 優先した値。ランダムなバイナリでは C0 制御が約 13% 出るので十分に分かれる。
const BINARY_CTRL_PCT: usize = 5;

/// 先頭バイト列からテキストとして開けないかを判定する**純関数**。
///
/// 判定の順序 (先に一致したところで決まる):
/// 1. 空 → テキスト (空ファイルを 16 進ダンプへ送らない)
/// 2. BOM (UTF-8 / UTF-16 LE / UTF-16 BE) → テキスト
/// 3. NUL バイトがある → バイナリ (テキストに NUL は出ない)
/// 4. UTF-8 として妥当 (末尾が途中で切れているのは許す) → テキスト
/// 5. Shift_JIS / CP932 の並びとして妥当 → テキスト
/// 6. 制御文字が [`BINARY_CTRL_PCT`] % を超える → バイナリ
/// 7. それ以外 → テキスト (未知の 8bit 符号化として扱う)
pub fn looks_binary(head: &[u8]) -> bool {
    if head.is_empty() {
        return false;
    }
    if head.starts_with(&[0xEF, 0xBB, 0xBF])
        || head.starts_with(&[0xFF, 0xFE])
        || head.starts_with(&[0xFE, 0xFF])
    {
        return false;
    }
    if head.contains(&0) {
        return true;
    }
    if valid_utf8_prefix(head) {
        return false;
    }
    if valid_sjis_prefix(head) {
        return false;
    }
    let ctrl = head.iter().filter(|b| is_control_byte(**b)).count();
    ctrl * 100 > head.len() * BINARY_CTRL_PCT
}

/// テキストに現れてはいけない制御バイトか。
/// タブ・改行・垂直タブ・改ページ・復帰・ESC は**テキスト側**に数える
/// (ANSI ログや古い帳票が誤ってバイナリ扱いされないため)。
fn is_control_byte(b: u8) -> bool {
    matches!(b, 0x00..=0x08 | 0x0E..=0x1A | 0x1C..=0x1F | 0x7F)
}

/// 末尾が途中で切れているのを許して UTF-8 として妥当か見る。
///
/// 先頭 8 KB を切り出すと多バイト文字の途中で切れるのが普通なので、
/// `error_len() == None` (= 入力が足りないだけ) は妥当として扱う。
fn valid_utf8_prefix(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// Shift_JIS / CP932 の**バイト並び**として妥当か (変換表は使わない)。
///
/// CP932 のデコードは Windows の `WideCharToMultiByte` に頼っており、
/// 他 OS では使えない。判定に OS 依存を持ち込まないよう、ここでは
/// 「先導バイト + 後続バイト」の構造だけを見る。日本語 CP932 のテキストは
/// これを必ず満たし、ランダムなバイナリはまず満たさない。
fn valid_sjis_prefix(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // ASCII (0x5C/0x7E の解釈差はここでは問わない) と半角カナ
            0x00..=0x7F | 0xA1..=0xDF => i += 1,
            // 2 バイト文字の先導バイト
            0x81..=0x9F | 0xE0..=0xFC => {
                let Some(t) = bytes.get(i + 1) else {
                    // 末尾で切れているだけ (先頭 N バイトを切り出した副作用)
                    return true;
                };
                if !matches!(t, 0x40..=0x7E | 0x80..=0xFC) {
                    return false;
                }
                i += 2;
            }
            // 0x80 / 0xA0 / 0xFD..=0xFF は単独で現れない
            _ => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// マジックナンバーによる種別推定
// ---------------------------------------------------------------------------

/// `(オフセット, マジック, 種別名)`。**上から順に**照合し、最初の一致を採る。
///
/// 種別名は "PNG" のような**書式名**なので翻訳しない (どの言語でも同じ綴り)。
const MAGICS: &[(usize, &[u8], &str)] = &[
    (0, b"\x89PNG\r\n\x1a\n", "PNG"),
    (0, b"\xFF\xD8\xFF", "JPEG"),
    (0, b"GIF87a", "GIF"),
    (0, b"GIF89a", "GIF"),
    (0, b"BM", "BMP"),
    (0, b"\x00\x00\x01\x00", "ICO"),
    (0, b"%PDF-", "PDF"),
    (0, b"PK\x03\x04", "ZIP"),
    (0, b"PK\x05\x06", "ZIP"),
    (0, b"PK\x07\x08", "ZIP"),
    (0, b"\x1F\x8B", "GZIP"),
    (0, b"BZh", "BZIP2"),
    (0, b"\xFD7zXZ\x00", "XZ"),
    (0, b"\x28\xB5\x2F\xFD", "Zstandard"),
    (0, b"7z\xBC\xAF\x27\x1C", "7-Zip"),
    (0, b"Rar!\x1A\x07", "RAR"),
    (0, b"\x04\x22\x4D\x18", "LZ4"),
    (0, b"!<arch>", "ar"),
    (0, b"\x7FELF", "ELF"),
    (0, b"MZ", "PE"),
    (0, b"\xFE\xED\xFA\xCE", "Mach-O"),
    (0, b"\xFE\xED\xFA\xCF", "Mach-O"),
    (0, b"\xCE\xFA\xED\xFE", "Mach-O"),
    (0, b"\xCF\xFA\xED\xFE", "Mach-O"),
    (0, b"SQLite format 3\x00", "SQLite"),
    (0, b"\x1A\x45\xDF\xA3", "Matroska"),
    (0, b"OggS", "Ogg"),
    (0, b"fLaC", "FLAC"),
    (0, b"ID3", "MP3"),
    (0, b"\xFF\xFB", "MP3"),
    (0, b"\xFF\xF3", "MP3"),
    (0, b"\xFF\xF2", "MP3"),
    (0, b"\x00\x00\x01\xBA", "MPEG-PS"),
    (0, b"\x00\x00\x01\xB3", "MPEG-ES"),
    (0, b"\x00asm", "WebAssembly"),
    (0, b"dex\n", "DEX"),
    (0, b"OTTO", "OpenType"),
    (0, b"wOFF", "WOFF"),
    (0, b"wOF2", "WOFF2"),
    (0, b"\x00\x01\x00\x00\x00", "TrueType"),
    (0, b"\x25\x21PS", "PostScript"),
    (0, b"\xED\xAB\xEE\xDB", "RPM"),
    (257, b"ustar", "TAR"),
];

/// 先頭バイト列からファイルの種別名を当てる**純関数**。分からなければ `None`。
///
/// 拡張子を見ない。拡張子は嘘をつく (`.log` の中身が gzip、`.txt` が SQLite 等)。
pub fn sniff_kind(head: &[u8]) -> Option<&'static str> {
    // RIFF は 8..12 のフォームで中身が変わるので先に見る
    if head.len() >= 12 && head.starts_with(b"RIFF") {
        return Some(match &head[8..12] {
            b"WAVE" => "WAV",
            b"AVI " => "AVI",
            b"WEBP" => "WebP",
            _ => "RIFF",
        });
    }
    // ISO-BMFF (mp4 / mov / m4a) は 4..8 が "ftyp"
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Some(match &head[8..12] {
            b"qt  " => "QuickTime",
            b"M4A " => "M4A",
            _ => "MP4",
        });
    }
    // CAFEBABE は Mach-O ユニバーサルバイナリと Java class が衝突する。
    // 直後の 4 バイトが Java の class バージョン (>= 45) かで分ける。
    if head.len() >= 8 && head.starts_with(b"\xCA\xFE\xBA\xBE") {
        let v = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
        return Some(if v >= 45 { "Java class" } else { "Mach-O" });
    }
    MAGICS
        .iter()
        .find(|(off, magic, _)| {
            head.len() >= off + magic.len() && &&head[*off..off + magic.len()] == magic
        })
        .map(|(_, _, name)| *name)
}

// ---------------------------------------------------------------------------
// 16 進ダンプ
// ---------------------------------------------------------------------------

/// 1 行に並べるバイト数 (古典的な 16 進ダンプと同じ)。
pub const HEX_BYTES_PER_ROW: usize = 16;

/// 16 進ダンプでタブが抱えるバイト数の上限。
///
/// 4 MB = 262144 行。行は**見えている分だけ**組み立てるので描画は一定コストだが、
/// 元のバイト列はタブが生きている間ずっと持ち続けるためここで頭を打つ。
pub const HEX_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// バイト数から 16 進ダンプの行数を求める。
pub fn hex_row_count(len: usize) -> usize {
    len.div_ceil(HEX_BYTES_PER_ROW)
}

/// 16 進ダンプの **1 行だけ** を組み立てる純関数。
///
/// `00000000  89 50 4e 47 0d 0a 1a 0a  00 00 00 0d 49 48 44 52  |.PNG........IHDR|`
///
/// 途中で終わる最終行も 16 進欄を空白で埋めるので ASCII 欄の桁が揃う。
/// 範囲外の行を渡してもオフセットだけの空行を返す (panic しない)。
pub fn hex_row(bytes: &[u8], row: usize) -> String {
    let start = row.saturating_mul(HEX_BYTES_PER_ROW);
    let chunk = if start < bytes.len() {
        &bytes[start..(start + HEX_BYTES_PER_ROW).min(bytes.len())]
    } else {
        &[][..]
    };
    let mut s = String::with_capacity(80);
    s.push_str(&format!("{start:08x}  "));
    for i in 0..HEX_BYTES_PER_ROW {
        if i == HEX_BYTES_PER_ROW / 2 {
            s.push(' ');
        }
        match chunk.get(i) {
            Some(b) => s.push_str(&format!("{b:02x} ")),
            None => s.push_str("   "),
        }
    }
    s.push_str(" |");
    for b in chunk {
        s.push(if (0x20..0x7F).contains(b) {
            *b as char
        } else {
            '.'
        });
    }
    s.push('|');
    s
}

// ---------------------------------------------------------------------------
// 動画 / 音声
// ---------------------------------------------------------------------------

/// メディアカードで開く動画の拡張子 (小文字)。
pub const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "m4v"];

/// メディアカードで開く音声の拡張子 (小文字)。
pub const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "m4a"];

/// 書庫一覧で開く拡張子 (小文字)。いずれも ZIP 形式。
/// ZIP として読めなかったものは 16 進ダンプへ落とすので、嘘の拡張子でも壊れない。
pub const ARCHIVE_EXTS: &[&str] = &["zip", "jar", "whl"];

fn ext_matches(path: &Path, set: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            set.iter().any(|x| *x == e)
        })
        .unwrap_or(false)
}

/// 動画 / 音声としてメディアカードで開くパスか (大文字小文字は無視)。
pub fn is_media_path(path: &Path) -> bool {
    ext_matches(path, VIDEO_EXTS) || ext_matches(path, AUDIO_EXTS)
}

/// 動画か (`is_media_path` が真のときだけ意味を持つ)。
pub fn is_video_path(path: &Path) -> bool {
    ext_matches(path, VIDEO_EXTS)
}

/// 書庫として一覧で開くパスか (大文字小文字は無視)。
pub fn is_archive_path(path: &Path) -> bool {
    ext_matches(path, ARCHIVE_EXTS)
}

/// ヘッダから取れた範囲のメディア情報。取れなかった項目は `None` (UI は「—」)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MediaInfo {
    /// 再生時間 (秒)。
    pub duration_secs: Option<f64>,
    /// 映像の幅 (px)。
    pub width: Option<u32>,
    /// 映像の高さ (px)。
    pub height: Option<u32>,
    /// 標本化周波数 (Hz)。
    pub sample_rate: Option<u32>,
    /// チャンネル数。
    pub channels: Option<u16>,
}

impl MediaInfo {
    /// 何ひとつ取れなかったか (別の経路をもう一度試すかの判断に使う)。
    pub fn is_empty(&self) -> bool {
        self.duration_secs.is_none()
            && self.width.is_none()
            && self.height.is_none()
            && self.sample_rate.is_none()
            && self.channels.is_none()
    }
}

/// 秒を `h:mm:ss` / `m:ss` に整形する純関数。
/// 有限でない値・負の値は `—` (取れなかったのと同じ見た目) にする。
pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || !(0.0..1e9).contains(&secs) {
        return "—".into();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// 与えられた先頭バイト列からメディア情報を読む**純関数**。
///
/// 追加の依存 (ffmpeg / symphonia) を入れず、ヘッダを自前で読める形式だけを扱う:
/// * RIFF/WAVE — `fmt ` と `data` チャンクから標本化周波数と再生時間
/// * FLAC — STREAMINFO から標本化周波数・チャンネル数・総サンプル数
/// * ISO-BMFF (mp4/mov/m4a) — `moov` がこの範囲にあれば [`probe_mp4_moov`] へ
///
/// 読めない形式 (MP3 / Matroska / Ogg) は既定値 (すべて `None`) を返す。
pub fn probe_media(head: &[u8]) -> MediaInfo {
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WAVE" {
        return probe_wav(head);
    }
    if head.starts_with(b"fLaC") {
        return probe_flac(head);
    }
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        if let Some((off, len)) = find_box(head, b"moov") {
            let end = off.saturating_add(len).min(head.len());
            return probe_mp4_moov(&head[off.min(head.len())..end]);
        }
    }
    MediaInfo::default()
}

/// RIFF/WAVE の `fmt ` と `data` から情報を取り出す。
fn probe_wav(b: &[u8]) -> MediaInfo {
    let mut info = MediaInfo::default();
    let mut byte_rate = 0u32;
    let mut pos = 12usize;
    // チャンクは (id 4B, size 4B LE, payload) が偶数境界で並ぶ
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let Some(size) = le_u32(b, pos + 4) else {
            break;
        };
        let body = pos + 8;
        if id == b"fmt " {
            info.channels = le_u16(b, body + 2);
            info.sample_rate = le_u32(b, body + 4);
            byte_rate = le_u32(b, body + 8).unwrap_or(0);
        } else if id == b"data" && byte_rate > 0 {
            info.duration_secs = Some(size as f64 / byte_rate as f64);
            break;
        }
        // 奇数長のチャンクは 1 バイトのパディングが入る
        let step = (size as usize)
            .saturating_add(size as usize % 2)
            .saturating_add(8);
        pos = pos.saturating_add(step.max(8));
    }
    info
}

/// FLAC の STREAMINFO ブロック (先頭固定位置) から情報を取り出す。
fn probe_flac(b: &[u8]) -> MediaInfo {
    let mut info = MediaInfo::default();
    // 4B "fLaC" + 4B ブロックヘッダ の直後が STREAMINFO 本体
    let Some(v) = be_u64(b, 4 + 4 + 10) else {
        return info;
    };
    let rate = (v >> 44) as u32 & 0xF_FFFF;
    let ch = ((v >> 41) & 0x7) as u16 + 1;
    let samples = v & 0xF_FFFF_FFFF;
    if rate > 0 {
        info.sample_rate = Some(rate);
        info.channels = Some(ch);
        if samples > 0 {
            info.duration_secs = Some(samples as f64 / rate as f64);
        }
    }
    info
}

/// ISO-BMFF の box ヘッダを読む。
///
/// 返り値は `(ヘッダ長, box 全体の長さ, 型)`。**長さ 0 は「ファイル末尾まで」**
/// を意味する仕様なので、そのまま 0 で返して呼び出し側に判断させる。
fn box_header(buf: &[u8]) -> Option<(u64, u64, [u8; 4])> {
    let size32 = be_u32(buf, 0)? as u64;
    let typ: [u8; 4] = buf.get(4..8)?.try_into().ok()?;
    match size32 {
        // 1 = 64bit 拡張サイズが直後に続く
        1 => {
            let big = be_u64(buf, 8)?;
            if big < 16 {
                return None;
            }
            Some((16, big, typ))
        }
        // 0 = ファイル末尾まで
        0 => Some((8, 0, typ)),
        n if n < 8 => None,
        n => Some((8, n, typ)),
    }
}

/// バイト列の中からトップレベルの box を探し、`(本体の開始, 本体の長さ)` を返す。
fn find_box(buf: &[u8], want: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = 0usize;
    let mut guard = 0usize;
    while pos + 8 <= buf.len() && guard < 4096 {
        guard += 1;
        let (hdr, total, typ) = box_header(&buf[pos..])?;
        let total = if total == 0 {
            (buf.len() - pos) as u64
        } else {
            total
        };
        if total < hdr {
            return None;
        }
        if &typ == want {
            return Some((pos + hdr as usize, (total - hdr) as usize));
        }
        pos = pos.checked_add(total as usize)?;
    }
    None
}

/// トップレベル box を辿って `moov` の `(本体の開始位置, 本体の長さ)` を探す。
///
/// `read_at` は「指定位置から 16 バイト読む」だけの閉包 (IO は呼び出し側)。
/// **`mdat` を読み飛ばせる**ので、moov が末尾にある数 GB の mp4 でも
/// 16 バイト × box 数しか読まない。末尾で 16 バイト取れないときは 0 埋めしてよい。
pub fn locate_moov(
    file_len: u64,
    mut read_at: impl FnMut(u64) -> Option<[u8; 16]>,
) -> Option<(u64, u64)> {
    let mut pos = 0u64;
    let mut guard = 0usize;
    while pos + 8 <= file_len && guard < 4096 {
        guard += 1;
        let buf = read_at(pos)?;
        let (hdr, total, typ) = box_header(&buf)?;
        let total = if total == 0 {
            file_len.saturating_sub(pos)
        } else {
            total
        };
        if total < hdr {
            return None;
        }
        if &typ == b"moov" {
            return Some((pos + hdr, total - hdr));
        }
        pos = pos.checked_add(total)?;
    }
    None
}

/// 最大の入れ子の深さ (壊れたファイルで再帰が止まらなくならないように)。
const MP4_MAX_DEPTH: usize = 6;

/// `moov` **本体**から再生時間と解像度を読む純関数。
///
/// * `mvhd` — timescale と duration から再生時間
/// * `trak` → `tkhd` — 16.16 固定小数の width / height (映像トラックだけ非零)
pub fn probe_mp4_moov(moov: &[u8]) -> MediaInfo {
    let mut info = MediaInfo::default();
    walk_boxes(moov, 0, &mut info);
    info
}

fn walk_boxes(buf: &[u8], depth: usize, info: &mut MediaInfo) {
    if depth > MP4_MAX_DEPTH {
        return;
    }
    let mut pos = 0usize;
    let mut guard = 0usize;
    while pos + 8 <= buf.len() && guard < 4096 {
        guard += 1;
        let Some((hdr, total, typ)) = box_header(&buf[pos..]) else {
            return;
        };
        let total = if total == 0 {
            (buf.len() - pos) as u64
        } else {
            total
        };
        if total < hdr {
            return;
        }
        let body_start = pos + hdr as usize;
        let body_end = (pos + total as usize).min(buf.len());
        if body_start >= body_end {
            // 嘘のサイズ: 進めるだけ進めて次へ (無限ループにしない)
            pos = pos.saturating_add((total as usize).max(8));
            continue;
        }
        let body = &buf[body_start..body_end];
        match &typ {
            b"mvhd" => read_mvhd(body, info),
            b"tkhd" => read_tkhd(body, info),
            // 中身に目的の box が入っているコンテナだけ降りる
            b"trak" | b"mdia" | b"udta" => walk_boxes(body, depth + 1, info),
            _ => {}
        }
        pos = pos.saturating_add((total as usize).max(8));
    }
}

fn read_mvhd(b: &[u8], info: &mut MediaInfo) {
    let version = b.first().copied().unwrap_or(0);
    let (ts, dur) = if version == 1 {
        (be_u32(b, 20), be_u64(b, 24))
    } else {
        (be_u32(b, 12), be_u32(b, 16).map(u64::from))
    };
    if let (Some(ts), Some(dur)) = (ts, dur) {
        // 0xFFFF... は「不明」を表す慣用値なので採らない
        if ts > 0 && dur != u64::MAX && dur != u32::MAX as u64 {
            info.duration_secs = Some(dur as f64 / ts as f64);
        }
    }
}

fn read_tkhd(b: &[u8], info: &mut MediaInfo) {
    let version = b.first().copied().unwrap_or(0);
    let at = if version == 1 { 88 } else { 76 };
    let (Some(w), Some(h)) = (be_u32(b, at), be_u32(b, at + 4)) else {
        return;
    };
    // 16.16 固定小数。音声トラックは 0 なので採らない
    let (w, h) = (w >> 16, h >> 16);
    if w > 0 && h > 0 && info.width.is_none() {
        info.width = Some(w);
        info.height = Some(h);
    }
}

// ---------------------------------------------------------------------------
// ZIP (セントラルディレクトリ)
// ---------------------------------------------------------------------------

/// 一覧に載せるエントリ数の上限 (これを超えたら `truncated`)。
pub const ZIP_MAX_ENTRIES: usize = 5000;

/// ZIP の 1 エントリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    /// 書庫内のパス。
    pub name: String,
    /// 展開後のサイズ。
    pub size: u64,
    /// 圧縮後のサイズ。
    pub compressed: u64,
    /// ディレクトリエントリか (名前が `/` で終わる)。
    pub dir: bool,
}

/// ZIP として読めなかった理由。文言は app.rs が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZipError {
    /// 終端レコード (EOCD) が無い = そもそも ZIP ではない。
    NoEndRecord,
    /// 終端レコードはあるが、セントラルディレクトリが途中で壊れている。
    BrokenDirectory,
}

/// ZIP の一覧結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZipListing {
    /// 表示するエントリ (最大 [`ZIP_MAX_ENTRIES`] 件)。
    pub entries: Vec<ZipEntry>,
    /// 読み取れたエントリの総数 (`entries.len()` 以上になりうる)。
    pub total: usize,
    /// 上限で打ち切ったか。
    pub truncated: bool,
    /// 読めなかった理由。
    pub error: Option<ZipError>,
}

/// EOCD を後ろから探す走査幅 (コメント最大 65535 + レコード 22)。
pub const ZIP_TAIL_SCAN: u64 = 66 * 1024;

/// ZIP のセントラルディレクトリを解析する**純関数**。
///
/// `window` はファイルの `base` バイト目以降を写したもの (末尾まで含むこと)。
/// セントラルディレクトリと終端レコードはファイル末尾側にあるので、
/// **末尾の一部だけ**渡せば数 GB の書庫でも丸ごと読まずに一覧が作れる。
///
/// 新規依存は入れず、End of Central Directory を末尾から探す古典的手法で読む。
/// Zip64 (エントリ数 / オフセット / サイズが 32bit に収まらないもの) にも対応する。
/// 壊れた入力では panic せず、読めたところまでを `error` 付きで返す。
pub fn parse_zip_at(window: &[u8], base: u64) -> ZipListing {
    let Some(eocd) = find_eocd(window) else {
        return ZipListing {
            error: Some(ZipError::NoEndRecord),
            ..Default::default()
        };
    };
    let mut declared = be_or(le_u16(window, eocd + 10), 0) as usize;
    let mut cd_off = be_or(le_u32(window, eocd + 16), 0) as u64;
    if declared == 0xFFFF || cd_off == 0xFFFF_FFFF {
        if let Some((n, off)) = zip64_end(window, eocd, base) {
            declared = n;
            cd_off = off;
        }
    }
    // window はファイルの base バイト目から始まる
    let Some(rel) = cd_off
        .checked_sub(base)
        .and_then(|v| usize::try_from(v).ok())
    else {
        return ZipListing {
            error: Some(ZipError::BrokenDirectory),
            ..Default::default()
        };
    };
    let mut entries: Vec<ZipEntry> = Vec::new();
    let mut total = 0usize;
    let mut broken = false;
    let mut pos = rel;
    for _ in 0..declared {
        if pos + 46 > window.len() || &window[pos..pos + 4] != b"PK\x01\x02" {
            broken = true;
            break;
        }
        let mut compressed = be_or(le_u32(window, pos + 20), 0) as u64;
        let mut size = be_or(le_u32(window, pos + 24), 0) as u64;
        let n = be_or(le_u16(window, pos + 28), 0) as usize;
        let ex = be_or(le_u16(window, pos + 30), 0) as usize;
        let cm = be_or(le_u16(window, pos + 32), 0) as usize;
        let name_end = pos + 46 + n;
        if name_end > window.len() {
            broken = true;
            break;
        }
        let raw = &window[pos + 46..name_end];
        let name = match std::str::from_utf8(raw) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(raw).into_owned(),
        };
        if size == 0xFFFF_FFFF || compressed == 0xFFFF_FFFF {
            let extra_end = (name_end + ex).min(window.len());
            let (z_size, z_comp) = zip64_extra(
                &window[name_end.min(window.len())..extra_end],
                size == 0xFFFF_FFFF,
                compressed == 0xFFFF_FFFF,
            );
            if let Some(v) = z_size {
                size = v;
            }
            if let Some(v) = z_comp {
                compressed = v;
            }
        }
        total += 1;
        if entries.len() < ZIP_MAX_ENTRIES {
            entries.push(ZipEntry {
                dir: name.ends_with('/'),
                name,
                size,
                compressed,
            });
        }
        pos = name_end + ex + cm;
    }
    ZipListing {
        truncated: entries.len() < total,
        total,
        entries,
        error: broken.then_some(ZipError::BrokenDirectory),
    }
}

/// `Option` を既定値付きで剥がす小道具 (読み取り失敗を 0 として扱う)。
fn be_or<T>(v: Option<T>, d: T) -> T {
    v.unwrap_or(d)
}

/// 終端レコード (`PK\x05\x06`) を後ろから探す。
fn find_eocd(window: &[u8]) -> Option<usize> {
    if window.len() < 22 {
        return None;
    }
    let scan_from = window.len().saturating_sub(ZIP_TAIL_SCAN as usize);
    (scan_from..=window.len() - 22)
        .rev()
        .find(|&i| &window[i..i + 4] == b"PK\x05\x06")
}

/// Zip64 の終端レコードからエントリ数とセントラルディレクトリ位置を読む。
fn zip64_end(window: &[u8], eocd: usize, base: u64) -> Option<(usize, u64)> {
    // EOCD の 20 バイト手前に Zip64 EOCD ロケータ (PK\x06\x07) がある
    let loc = eocd.checked_sub(20)?;
    if window.get(loc..loc + 4)? != b"PK\x06\x07" {
        return None;
    }
    let abs = le_u64(window, loc + 8)?;
    let rel = usize::try_from(abs.checked_sub(base)?).ok()?;
    if window.get(rel..rel + 4)? != b"PK\x06\x06" {
        return None;
    }
    let n = usize::try_from(le_u64(window, rel + 32)?).ok()?;
    let off = le_u64(window, rel + 48)?;
    Some((n.min(ZIP_MAX_ENTRIES * 64), off))
}

/// Zip64 拡張フィールド (ID 0x0001) から実サイズを読む。
/// **0xFFFFFFFF だった項目だけ**がこの順で並ぶ (展開後 → 圧縮後 → …)。
fn zip64_extra(extra: &[u8], want_size: bool, want_comp: bool) -> (Option<u64>, Option<u64>) {
    let mut p = 0usize;
    while p + 4 <= extra.len() {
        let id = be_or(le_u16(extra, p), 0);
        let len = be_or(le_u16(extra, p + 2), 0) as usize;
        let body_end = (p + 4 + len).min(extra.len());
        if id == 0x0001 {
            let body = &extra[p + 4..body_end];
            let mut at = 0usize;
            let mut size = None;
            let mut comp = None;
            if want_size {
                size = le_u64(body, at);
                at += 8;
            }
            if want_comp {
                comp = le_u64(body, at);
            }
            return (size, comp);
        }
        p = p + 4 + len;
    }
    (None, None)
}

// ---------------------------------------------------------------------------
// タブが持つプレビューの中身
// ---------------------------------------------------------------------------

/// 16 進ダンプタブの中身。
pub struct HexDoc {
    /// 表示するバイト列 (最大 [`HEX_MAX_BYTES`])。
    pub bytes: Vec<u8>,
    /// ディスク上のファイルサイズ。
    pub file_bytes: u64,
    /// マジックナンバーから当てた種別名。
    pub kind: Option<&'static str>,
    /// 上限で打ち切ったか。
    pub truncated: bool,
}

/// メディアカードタブの中身。
pub struct MediaDoc {
    /// ヘッダから取れた情報。
    pub info: MediaInfo,
    /// ディスク上のファイルサイズ。
    pub file_bytes: u64,
    /// マジックナンバーから当てた種別名。
    pub kind: Option<&'static str>,
    /// 映像か (false なら音声)。
    pub video: bool,
}

/// 書庫一覧タブの中身。
pub struct ArchiveDoc {
    /// セントラルディレクトリの解析結果。
    pub listing: ZipListing,
    /// ディスク上のファイルサイズ。
    pub file_bytes: u64,
}

/// タブが抱えるプレビューの中身。`Buffer::preview` に 1 本だけ持つ。
///
/// 種類ごとに `Option` フィールドを増やすと `Buffer` の生成箇所が
/// そのたびに全部壊れるので、1 つの列挙型にまとめてある。
pub enum PreviewDoc {
    /// バイナリの 16 進ダンプ。
    Hex(HexDoc),
    /// 動画・音声の情報カード。
    Media(MediaDoc),
    /// 書庫の中身一覧。
    Archive(ArchiveDoc),
    /// マルチバッファ (複数ファイルの抜粋を 1 本の面に並べたもの)。
    ///
    /// 他の 3 つと違い**ファイル 1 本の中身ではない**が、「専用ビューアで描く
    /// タブの中身」という役割は同じなので同じ列挙型に入れる
    /// (`Buffer` にフィールドを増やすと生成箇所が全部壊れるため)。
    Multi(crate::multibuffer::Multibuffer),
}

/// どのプレビューかの印 (借用を握ったまま `&mut self` を呼べないので、
/// 描画の振り分けはこの Copy な値で行う)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewTag {
    Hex,
    Media,
    Archive,
    Multi,
}

impl PreviewDoc {
    /// 中身の種類を Copy な印で返す。
    pub fn tag(&self) -> PreviewTag {
        match self {
            PreviewDoc::Hex(_) => PreviewTag::Hex,
            PreviewDoc::Media(_) => PreviewTag::Media,
            PreviewDoc::Archive(_) => PreviewTag::Archive,
            PreviewDoc::Multi(_) => PreviewTag::Multi,
        }
    }
}

// ---------------------------------------------------------------------------
// アニメーション画像 (GIF / APNG / アニメーション WebP)
// ---------------------------------------------------------------------------
//
// `image::load_from_memory` は**先頭の 1 コマしか返さない**ので、これまで
// アニメーション GIF は静止画になっていた。ここはバイト列を受けて
// **RGBA のコマ列**へ落とす純関数だけを置く。テクスチャ化 (GPU への転送) と
// 時計は呼び出し側 (`editor.rs` / `app.rs` / `markdown.rs`) の仕事。

/// 復号したアニメーションの 1 コマ。
///
/// コマは**合成済み** (GIF の部分更新と廃棄方法、APNG のブレンドを適用済み) で、
/// どのコマも `Animation::width` × `Animation::height` の全面ぶんある。
/// 呼び出し側は差分合成を自分でやらなくてよい。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimFrame {
    /// RGBA8 の画素 (幅 × 高さ × 4 バイト)。
    pub rgba: Vec<u8>,
    /// このコマを表示し続ける時間 (ミリ秒)。[`normalize_delay_ms`] 済み。
    pub delay_ms: u32,
}

/// 繰り返し回数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimLoop {
    /// 無限に繰り返す (GIF の NETSCAPE 拡張が 0 / 無指定のとき)。
    Forever,
    /// 有限回。`0` は 1 回として扱う ([`frame_at`] 参照)。
    Times(u32),
}

/// どの上限に当たって復号を打ち切ったか。
///
/// **黙って切らない**ための印。`Animation::truncated` が `Some` なら
/// 「そのファイルの全部ではない」ことを呼び出し側が UI に出せる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimStop {
    /// コマ数の上限 ([`AnimLimits::max_frames`]) に当たった。
    FrameCount,
    /// 復号後の総バイト数の上限 ([`AnimLimits::max_total_bytes`]) に当たった。
    TotalBytes,
    /// ファイルが途中で切れている / 壊れていて、そこから先が読めなかった。
    Decode,
}

/// アニメーションとして読めなかった理由。文言は app.rs が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimError {
    /// アニメーションを持ちうる形式ではない (JPEG / BMP / ICO / 画像ですらない)。
    NotAnimated,
    /// 形式は GIF / PNG / WebP だが、この 1 本はアニメーションを持たない。
    /// 静止画として `image::load_from_memory` で開くのが正しい。
    Still,
    /// 対応形式だが 1 コマも復号できなかった (先頭から壊れている)。
    Broken,
    /// 宣言された寸法が大きすぎて、1 コマぶんの領域すら確保できない。
    /// **復号を始める前に**断るので、嘘の寸法でメモリを焼かれない。
    TooLarge,
}

/// アニメーションを持ちうる形式。中身までは見ない (マジックナンバーだけ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimFormat {
    /// GIF87a / GIF89a。
    Gif,
    /// PNG。アニメーションかどうかは `acTL` チャンクの有無で決まる (APNG)。
    Png,
    /// RIFF/WEBP。アニメーションかどうかは `ANIM` チャンクの有無で決まる。
    WebP,
}

/// 復号したアニメーション 1 本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Animation {
    /// `frames` の幅 (縮小後)。全コマ共通。
    pub width: u32,
    /// `frames` の高さ (縮小後)。全コマ共通。
    pub height: u32,
    /// 元の (縮小前の) 幅。
    pub source_width: u32,
    /// 元の (縮小前の) 高さ。
    pub source_height: u32,
    /// コマ列。**必ず 1 つ以上**入る (0 コマなら [`AnimError::Broken`] で返す)。
    pub frames: Vec<AnimFrame>,
    /// 繰り返し回数。
    pub loops: AnimLoop,
    /// 上限や破損で途中までしか読めなかったときの理由。
    pub truncated: Option<AnimStop>,
}

impl Animation {
    /// コマごとの表示時間だけを取り出す。[`frame_at`] へそのまま渡せる。
    ///
    /// 画素と時計を分けて持てるようにするための橋渡し。再生位置の計算に
    /// 数十 MB の画素を持ち回らせないため、呼び出し側はこれを 1 度だけ作って
    /// 使い回すのがよい。
    pub fn delays_ms(&self) -> Vec<u32> {
        self.frames.iter().map(|f| f.delay_ms).collect()
    }
}

/// この時間 (ms) 未満の遅延は「速すぎる指定」とみなす。
///
/// GIF の遅延は 1/100 秒単位で、`0` は「できるだけ速く」の意味になる。
/// 実際には 0 を指定した広告 GIF が CPU を焼くため、Firefox も Chromium も
/// **10ms 未満は 100ms に読み替える**という同じ挙動を採っている。
/// ここもそれに合わせる (合わせないと、同じ GIF がブラウザの数倍速で回る)。
pub const ANIM_MIN_DELAY_MS: u32 = 10;

/// [`ANIM_MIN_DELAY_MS`] 未満だったときの読み替え先 (ms) = 10fps 相当。
pub const ANIM_DEFAULT_DELAY_MS: u32 = 100;

/// 復号するコマ数の上限。
///
/// 既定遅延 [`ANIM_DEFAULT_DELAY_MS`] (100ms) 換算で 51 秒ぶん。
/// プレビューとしてはこれ以上を一度に抱える理由がない。
pub const ANIM_MAX_FRAMES: usize = 512;

/// 復号後 (RGBA8) の総バイト数の上限。
///
/// `markdown::MAX_IMAGE_BYTES` (24MB) は**符号化された**入力 1 枚の上限で、
/// 展開後の大きさではない。実測: `assets/zaivern-demo.gif` は 765KB しかないが
/// 960×540 × 127 コマ = **251MB** へ展開される (327 倍)。
/// 入力の上限だけでは記憶量を全く縛れないので、展開後にも上限を置く。
/// 96MiB = `MAX_IMAGE_BYTES` の 4 倍。
pub const ANIM_MAX_TOTAL_BYTES: usize = 96 * 1024 * 1024;

/// 1 コマの長辺の上限 (画素)。これを超えるコマは縮小してから積む。
///
/// `markdown::PREVIEW_MAX_SIDE` (1600) は**静止画 1 枚**の上限で、
/// アニメーションではそこにコマ数が掛かる。上の実測 GIF を 512 へ縮めると
/// 512×288 × 127 コマ = 74.9MB となり [`ANIM_MAX_TOTAL_BYTES`] に収まる
/// (縮小しないと 251MB で 96MB を超え、半分以上のコマを捨てることになる)。
pub const ANIM_MAX_SIDE: u32 = 512;

/// 復号の上限。呼び出し側が用途に応じて緩められるよう値で持つ
/// (紙面に小さく載せる Markdown と、全画面のビューアでは適切な値が違う)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimLimits {
    /// 積むコマ数の上限。
    pub max_frames: usize,
    /// 積んだ RGBA の総バイト数の上限。
    pub max_total_bytes: usize,
    /// 1 コマの長辺の上限。`0` は「縮小しない」。
    pub max_side: u32,
}

impl Default for AnimLimits {
    fn default() -> Self {
        Self {
            max_frames: ANIM_MAX_FRAMES,
            max_total_bytes: ANIM_MAX_TOTAL_BYTES,
            max_side: ANIM_MAX_SIDE,
        }
    }
}

/// 再生位置の問い合わせ結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimCursor {
    /// いま表示すべきコマ番号。
    pub frame: usize,
    /// 次にコマが変わるまでの残り (ms)。`None` = **もう変わらない**
    /// (有限ループを再生し切った / コマが 1 枚しかない)。
    ///
    /// 呼び出し側はこれが `Some(ms)` のときだけ `request_repaint_after(ms)` を
    /// 呼ぶ。`None` なら再描画を一切要求しない = **アイドルの費用がゼロになる**。
    pub next_in_ms: Option<u64>,
}

/// 先頭バイトから「アニメーションを持ちうる形式か」を当てる。中身は見ない。
///
/// 本当にアニメーションかどうかは復号しないと分からない (静止 PNG と APNG、
/// 静止 WebP とアニメーション WebP は同じマジックナンバー) が、
/// JPEG や ICO をわざわざ復号器へ通さずに済ませるための門番として使う。
pub fn animation_format(head: &[u8]) -> Option<AnimFormat> {
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some(AnimFormat::Gif);
    }
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(AnimFormat::Png);
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some(AnimFormat::WebP);
    }
    None
}

/// GIF の遅延を実際に使う値へ読み替える。根拠は [`ANIM_MIN_DELAY_MS`]。
pub fn normalize_delay_ms(raw_ms: u32) -> u32 {
    if raw_ms < ANIM_MIN_DELAY_MS {
        ANIM_DEFAULT_DELAY_MS
    } else {
        raw_ms
    }
}

/// コマ列の総再生時間 (ms)。1 周ぶん。
pub fn total_ms(delays_ms: &[u32]) -> u64 {
    delays_ms.iter().map(|d| u64::from(*d)).sum()
}

/// **経過時間 → 表示すべきコマ番号**を返す純関数。
///
/// 呼び出し側は時計 (経過ミリ秒) を持つだけでよく、再生位置の決め方は
/// 全部ここで固定される (= テストで完全に固定できる)。
///
/// 壊れない条件:
/// - `delays_ms` が空、または総再生時間が 0 のときは**割り算をしない**で
///   `frame = 0` / `next_in_ms = None` を返す (ゼロ除算も無限ループも起きない)。
/// - `AnimLoop::Times(0)` は 1 回として扱う (0 回再生という状態を作らない)。
/// - 有限ループを再生し切った後は**最後のコマで止まり** `next_in_ms = None`。
pub fn frame_at(delays_ms: &[u32], loops: AnimLoop, elapsed_ms: u64) -> AnimCursor {
    let total = total_ms(delays_ms);
    if delays_ms.is_empty() || total == 0 {
        return AnimCursor {
            frame: 0,
            next_in_ms: None,
        };
    }
    let last = delays_ms.len() - 1;
    let (pos, final_pass) = match loops {
        AnimLoop::Forever => (elapsed_ms % total, false),
        AnimLoop::Times(n) => {
            let n = u64::from(n.max(1));
            if elapsed_ms >= total.saturating_mul(n) {
                return AnimCursor {
                    frame: last,
                    next_in_ms: None,
                };
            }
            (elapsed_ms % total, elapsed_ms / total + 1 == n)
        }
    };
    let mut acc = 0u64;
    for (i, d) in delays_ms.iter().enumerate() {
        let end = acc + u64::from(*d);
        if pos < end {
            // 最終周の最後のコマは、ここで止まる = 次の再描画が要らない。
            let stop_here = final_pass && i == last;
            return AnimCursor {
                frame: i,
                next_in_ms: if stop_here { None } else { Some(end - pos) },
            };
        }
        acc = end;
    }
    // pos < total なので上の走査で必ず返るが、丸め等で漏れても止めるだけにする。
    AnimCursor {
        frame: last,
        next_in_ms: None,
    }
}

/// 縮小が要るなら縮小後の寸法を返す。不要なら `None`。
///
/// `editor::image_downscale` と同じ規則だが、`preview` は IO を持つ層へ
/// 依存しない (ここは純関数だけの層) ので自前に持つ。
fn anim_downscale(w: u32, h: u32, max_side: u32) -> Option<(u32, u32)> {
    let longest = w.max(h);
    if max_side == 0 || longest <= max_side || longest == 0 {
        return None;
    }
    let scale = f64::from(max_side) / f64::from(longest);
    let nw = ((f64::from(w) * scale).round() as u32).clamp(1, max_side);
    let nh = ((f64::from(h) * scale).round() as u32).clamp(1, max_side);
    Some((nw, nh))
}

/// `image` の [`image::Delay`] をミリ秒へ落とし、読み替え規則を当てる。
fn delay_to_ms(delay: image::Delay) -> u32 {
    let (numer, denom) = delay.numer_denom_ms();
    let raw = if denom == 0 {
        0
    } else {
        (u64::from(numer) / u64::from(denom)).min(u64::from(u32::MAX)) as u32
    };
    normalize_delay_ms(raw)
}

/// バイト列をアニメーションとして復号する**純関数** (IO なし)。
///
/// GIF / APNG / アニメーション WebP を同じ形へ落とす。`image` 0.25 が
/// `AnimationDecoder` を実装しているのはこの 3 つだけで、Cargo.toml の
/// feature (`png` / `gif` / `webp`) は既に全部入っている = 依存追加は要らない。
///
/// - どのコマも合成済み・同じ寸法。GIF の部分更新 / 廃棄方法は `image` 側が畳む。
/// - [`AnimLimits`] を必ず超えない。超えたら**そこで復号をやめて**
///   `truncated` に理由を入れて返す (黙って切らない)。
/// - 壊れた入力で panic しない。読めた分が 1 コマもなければ [`AnimError`]。
pub fn decode_animation(bytes: &[u8], limits: &AnimLimits) -> Result<Animation, AnimError> {
    use image::ImageDecoder;
    use std::io::Cursor;

    match animation_format(bytes).ok_or(AnimError::NotAnimated)? {
        AnimFormat::Gif => {
            let dec = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
                .map_err(|_| AnimError::Broken)?;
            let (w, h) = dec.dimensions();
            collect_frames(dec, w, h, limits)
        }
        AnimFormat::Png => {
            let dec = image::codecs::png::PngDecoder::new(Cursor::new(bytes))
                .map_err(|_| AnimError::Broken)?;
            if !dec.is_apng().unwrap_or(false) {
                return Err(AnimError::Still);
            }
            let (w, h) = dec.dimensions();
            let dec = dec.apng().map_err(|_| AnimError::Broken)?;
            collect_frames(dec, w, h, limits)
        }
        AnimFormat::WebP => {
            let dec = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|_| AnimError::Broken)?;
            if !dec.has_animation() {
                return Err(AnimError::Still);
            }
            let (w, h) = dec.dimensions();
            collect_frames(dec, w, h, limits)
        }
    }
}

/// 復号器からコマを**遅延評価のまま**取り出し、上限で止める。
///
/// 上限の検査を `next()` の**前**に置いてあるのが肝で、こうしないと
/// 「上限ちょうどの次の 1 コマ」を復号してから捨てることになる
/// (`tests::上限に当たった後は一切復号していない` が番人)。
fn collect_frames<'a, D: image::AnimationDecoder<'a>>(
    dec: D,
    src_w: u32,
    src_h: u32,
    limits: &AnimLimits,
) -> Result<Animation, AnimError> {
    if src_w == 0 || src_h == 0 {
        return Err(AnimError::Broken);
    }
    // 復号器は**縮小前の全面**を確保するので、宣言寸法の 1 コマぶんが
    // 予算に収まらないなら復号を始めてはいけない (嘘の寸法対策)。
    let src_bytes = u64::from(src_w)
        .saturating_mul(u64::from(src_h))
        .saturating_mul(4);
    if src_bytes > limits.max_total_bytes as u64 {
        return Err(AnimError::TooLarge);
    }

    let loops = match dec.loop_count() {
        image::metadata::LoopCount::Infinite => AnimLoop::Forever,
        image::metadata::LoopCount::Finite(n) => AnimLoop::Times(n.get()),
    };
    let scaled = anim_downscale(src_w, src_h, limits.max_side);
    let (w, h) = scaled.unwrap_or((src_w, src_h));
    let per_frame = (w as usize)
        .saturating_mul(h as usize)
        .saturating_mul(4)
        .max(1);

    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut total = 0usize;
    let mut truncated = None;
    let mut it = dec.into_frames();
    loop {
        if frames.len() >= limits.max_frames {
            truncated = Some(AnimStop::FrameCount);
            break;
        }
        if total.saturating_add(per_frame) > limits.max_total_bytes {
            truncated = Some(AnimStop::TotalBytes);
            break;
        }
        let Some(item) = it.next() else { break };
        let Ok(frame) = item else {
            truncated = Some(AnimStop::Decode);
            break;
        };
        let delay_ms = delay_to_ms(frame.delay());
        let mut buf = frame.into_buffer();
        // 全コマを同じ寸法へ揃える。揃っていないと呼び出し側は 1 枚の
        // テクスチャを使い回せず、描画が破綻する。
        if buf.dimensions() != (w, h) {
            if buf.width() == 0 || buf.height() == 0 {
                truncated = Some(AnimStop::Decode);
                break;
            }
            // `resize(Triangle)` ではなく `thumbnail` を使う。ここは**必ず縮小**
            // なので箱平均 (整数演算) で足り、浮動小数のフィルタより桁違いに
            // 安い。コマ数ぶん掛かるので定数倍がそのまま体感になる。
            //
            // 実測 (release / assets/zaivern-demo.gif 127 コマ / 同じ機械で
            // 連続して 3 回ずつ): Triangle 4.03・4.23・5.57 秒 →
            // thumbnail 2.86・2.79・2.48 秒。**絶対値は当てにしない**
            // (16 コアに対し負荷 35 の状態で測ったので 2 倍ほど膨らんでいる)。
            // 意味があるのは同条件で並べた比 = およそ 1.7 倍速いこと。
            buf = image::imageops::thumbnail(&buf, w, h);
        }
        total = total.saturating_add(buf.as_raw().len());
        frames.push(AnimFrame {
            rgba: buf.into_raw(),
            delay_ms,
        });
    }

    if frames.is_empty() {
        return Err(AnimError::Broken);
    }
    Ok(Animation {
        width: w,
        height: h,
        source_width: src_w,
        source_height: src_h,
        frames,
        loops,
        truncated,
    })
}

/// テスト用の最小サンプル生成 (`preview` と `editor` のテストで共有する)。
///
/// 実ファイルを置くと OS・ロケール・改行の扱いで壊れるので、
/// **バイト列をその場で組み立てる**。どの環境でも同じ入力になる。
#[cfg(test)]
pub mod testdata {
    /// テスト用のアニメーション GIF を組み立てる。
    ///
    /// コマごとに色を変える (同じ絵だと符号化が畳んでコマ数を確かめられない)。
    pub fn make_gif(w: u32, h: u32, frames: usize, delay_ms: u32, repeat: u16) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            let r = if repeat == 0 {
                image::codecs::gif::Repeat::Infinite
            } else {
                image::codecs::gif::Repeat::Finite(repeat)
            };
            let _ = enc.set_repeat(r);
            for i in 0..frames {
                let mut buf = image::RgbaImage::new(w, h);
                let v = ((i * 37) % 256) as u8;
                for px in buf.pixels_mut() {
                    *px = image::Rgba([v, 255u8.wrapping_sub(v), 0x80, 0xFF]);
                }
                let f = image::Frame::from_parts(
                    buf,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(delay_ms, 1),
                );
                let _ = enc.encode_frame(f);
            }
        }
        out
    }

    /// 最小の WAV (44.1kHz / 16bit / ステレオ)。
    pub fn make_wav(seconds: u32) -> Vec<u8> {
        let rate = 44100u32;
        let ch = 2u16;
        let bits = 16u16;
        let byte_rate = rate * ch as u32 * (bits / 8) as u32;
        let data = byte_rate * seconds;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&ch.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&(ch * bits / 8).to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.to_le_bytes());
        // 実データは 0 で埋める (再生時間はヘッダの byte_rate から出る)
        v.resize(v.len() + data.min(4096) as usize, 0);
        v
    }

    fn mp4_box(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    /// timescale / duration / 解像度を指定した最小の mp4。
    /// `moov_last` で「moov が末尾」(ffmpeg の既定) を再現する。
    pub fn make_mp4(timescale: u32, duration: u32, w: u32, h: u32, moov_last: bool) -> Vec<u8> {
        let mut mvhd = vec![0u8; 4]; // version 0 + flags
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification
        mvhd.extend_from_slice(&timescale.to_be_bytes());
        mvhd.extend_from_slice(&duration.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 80]);

        let mut tkhd = vec![0u8; 76]; // version 0 + flags + 各種
        tkhd.extend_from_slice(&(w << 16).to_be_bytes());
        tkhd.extend_from_slice(&(h << 16).to_be_bytes());

        let trak = mp4_box(b"trak", &mp4_box(b"tkhd", &tkhd));
        let mut moov_body = mp4_box(b"mvhd", &mvhd);
        moov_body.extend_from_slice(&trak);
        let moov = mp4_box(b"moov", &moov_body);
        let ftyp = mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso2");
        let mdat = mp4_box(b"mdat", &vec![0u8; 512]);

        let mut v = ftyp;
        if moov_last {
            v.extend_from_slice(&mdat);
            v.extend_from_slice(&moov);
        } else {
            v.extend_from_slice(&moov);
            v.extend_from_slice(&mdat);
        }
        v
    }

    /// 無圧縮 (method 0) の最小 ZIP。
    pub fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for (name, body) in files {
            let local_off = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[0u8; 14]); // version..crc (テストでは中身を見ない)
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(body);

            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&[0u8; 16]); // version..crc
            central.extend_from_slice(&(body.len() as u32).to_le_bytes()); // compressed
            central.extend_from_slice(&(body.len() as u32).to_le_bytes()); // size
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal
            central.extend_from_slice(&0u32.to_le_bytes()); // external
            central.extend_from_slice(&local_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testdata::{make_gif, make_mp4, make_wav, make_zip};
    use super::*;

    // ── テキスト / バイナリ判定 ────────────────────────────────

    #[test]
    fn looks_binary_table() {
        // (入力, バイナリか, 説明)
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        let elf = b"\x7FELF\x02\x01\x01\x00\x00\x00\x00\x00".to_vec();
        let utf16 = {
            let mut v = vec![0xFF, 0xFE];
            v.extend_from_slice(&[0x41, 0x00, 0x42, 0x00]);
            v
        };
        let utf8_bom = {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice("こんにちは".as_bytes());
            v
        };
        // 「日本語」の CP932 (Shift_JIS) 表現
        let cp932 = vec![0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, 0x0A];
        // Latin-1 の "café" (0xE9 は UTF-8 として不正、SJIS としても不正)
        let latin1 = vec![b'c', b'a', b'f', 0xE9, b'\n'];
        let random: Vec<u8> = (0u32..512)
            .map(|i| (i.wrapping_mul(37) % 256) as u8)
            .collect();
        let no_nul_binary: Vec<u8> = (0u32..512)
            .map(|i| {
                let b = (i.wrapping_mul(37) % 255) as u8;
                if b == 0 {
                    1
                } else {
                    b
                }
            })
            .collect();

        let cases: &[(&[u8], bool, &str)] = &[
            (b"", false, "空ファイルはテキスト扱い"),
            (b"hello world\n", false, "純 ASCII"),
            ("日本語のテキスト\n".as_bytes(), false, "UTF-8 日本語"),
            (&utf8_bom, false, "UTF-8 BOM"),
            (&utf16, false, "UTF-16 LE BOM"),
            (&[0xFE, 0xFF, 0x00, 0x41], false, "UTF-16 BE BOM"),
            (&cp932, false, "CP932 日本語"),
            (&latin1, false, "未知の 8bit テキスト"),
            (&png, true, "PNG"),
            (&elf, true, "ELF"),
            (&random, true, "ランダムなバイト列 (NUL あり)"),
            (&no_nul_binary, true, "NUL 無しでも制御文字が多い"),
            (b"\x1b[31mred\x1b[0m\n", false, "ANSI エスケープはテキスト"),
        ];
        for (input, want, why) in cases {
            assert_eq!(looks_binary(input), *want, "{why}");
        }
    }

    #[test]
    fn truncated_multibyte_tail_is_still_text() {
        // 8KB で切り出すと多バイト文字の途中で切れる。それでテキストが
        // バイナリ扱いになってはいけない。
        let s = "あいうえお".as_bytes();
        for cut in 1..s.len() {
            assert!(!looks_binary(&s[..cut]), "{cut} バイト目で切ってもテキスト");
        }
    }

    #[test]
    fn sniff_kind_table() {
        let cases: &[(&[u8], Option<&str>)] = &[
            (b"\x89PNG\r\n\x1a\n", Some("PNG")),
            (b"\xFF\xD8\xFF\xE0", Some("JPEG")),
            (b"GIF89a...", Some("GIF")),
            (b"PK\x03\x04\x14\x00", Some("ZIP")),
            (b"\x7FELF\x02", Some("ELF")),
            (b"MZ\x90\x00", Some("PE")),
            (b"\xCF\xFA\xED\xFE\x0c", Some("Mach-O")),
            (b"SQLite format 3\x00", Some("SQLite")),
            (b"\x1F\x8B\x08\x00", Some("GZIP")),
            (b"%PDF-1.7", Some("PDF")),
            (b"RIFF\x24\x00\x00\x00WAVE", Some("WAV")),
            (b"RIFF\x24\x00\x00\x00WEBP", Some("WebP")),
            (b"RIFF\x24\x00\x00\x00AVI ", Some("AVI")),
            (b"\x00\x00\x00\x18ftypisom", Some("MP4")),
            (b"\x00\x00\x00\x14ftypqt  ", Some("QuickTime")),
            (b"\x1A\x45\xDF\xA3\x01", Some("Matroska")),
            (b"fLaC\x00\x00\x00\x22", Some("FLAC")),
            (b"OggS\x00\x02", Some("Ogg")),
            (b"\x00asm\x01\x00\x00\x00", Some("WebAssembly")),
            (b"hello, this is plain text", None),
            (b"", None),
            (b"\x89", None),
        ];
        for (input, want) in cases {
            assert_eq!(sniff_kind(input), *want, "{input:?}");
        }
    }

    #[test]
    fn cafebabe_splits_mach_o_and_java() {
        // Mach-O ユニバーサルバイナリ: 直後は アーキテクチャ数 (小さい)
        assert_eq!(
            sniff_kind(b"\xCA\xFE\xBA\xBE\x00\x00\x00\x02"),
            Some("Mach-O")
        );
        // Java class: 直後は minor/major バージョン (major >= 45)
        assert_eq!(
            sniff_kind(b"\xCA\xFE\xBA\xBE\x00\x00\x00\x34"),
            Some("Java class")
        );
    }

    #[test]
    fn sniff_never_panics_on_any_prefix() {
        let sample = b"\x00\x00\x00\x18ftypisomRIFF\x24\x00\x00\x00WAVEfmt ";
        for cut in 0..=sample.len() {
            let _ = sniff_kind(&sample[..cut]);
            let _ = looks_binary(&sample[..cut]);
        }
    }

    // ── 16 進ダンプ ────────────────────────────────────────────

    #[test]
    fn hex_row_table() {
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let cases: &[(&[u8], usize, &str)] = &[
            (
                png,
                0,
                "00000000  89 50 4e 47 0d 0a 1a 0a  00 00 00 0d 49 48 44 52  |.PNG........IHDR|",
            ),
            (
                b"abc",
                0,
                "00000000  61 62 63                                          |abc|",
            ),
            (
                b"",
                0,
                "00000000                                                    ||",
            ),
            // 範囲外の行でも panic せず空行を返す
            (
                b"abc",
                9,
                "00000090                                                    ||",
            ),
        ];
        for (bytes, row, want) in cases {
            assert_eq!(hex_row(bytes, *row), *want, "row {row}");
        }
    }

    #[test]
    fn hex_rows_align_and_count() {
        assert_eq!(hex_row_count(0), 0);
        assert_eq!(hex_row_count(1), 1);
        assert_eq!(hex_row_count(16), 1);
        assert_eq!(hex_row_count(17), 2);
        // ASCII 欄の開始桁は行の長短によらず同じ
        let full = hex_row(&[0u8; 16], 0);
        let part = hex_row(&[0u8; 3], 0);
        assert_eq!(
            full.find('|').expect("full の ASCII 欄"),
            part.find('|').expect("part の ASCII 欄"),
            "短い行でも ASCII 欄の桁が揃う"
        );
    }

    // ── メディア ───────────────────────────────────────────────

    #[test]
    fn media_probe_table() {
        let wav = make_wav(3);
        let w = probe_media(&wav);
        assert_eq!(w.sample_rate, Some(44100));
        assert_eq!(w.channels, Some(2));
        assert!(
            (w.duration_secs.expect("WAV の再生時間") - 3.0).abs() < 0.01,
            "{:?}",
            w.duration_secs
        );

        let mp4 = make_mp4(600, 6000, 1920, 1080, false);
        let m = probe_media(&mp4);
        assert_eq!(m.duration_secs, Some(10.0));
        assert_eq!((m.width, m.height), (Some(1920), Some(1080)));

        // 読めない形式は「情報なし」で返る (panic も推測もしない)
        assert!(probe_media(b"ID3\x04\x00\x00\x00\x00\x00\x00").is_empty());
        assert!(probe_media(b"").is_empty());
        assert!(probe_media(b"\x1A\x45\xDF\xA3").is_empty());
    }

    #[test]
    fn flac_streaminfo_is_read() {
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]); // last block, STREAMINFO, 34B
        v.extend_from_slice(&[0u8; 10]); // min/max block, min/max frame
                                         // sample_rate=48000(20b) | channels-1=1(3b) | bits-1=15(5b) | samples=96000(36b)
        let packed: u64 = (48000u64 << 44) | (1u64 << 41) | (15u64 << 36) | 96000u64;
        v.extend_from_slice(&packed.to_be_bytes());
        let info = probe_flac(&v);
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.channels, Some(2));
        assert_eq!(info.duration_secs, Some(2.0));
    }

    #[test]
    fn locate_moov_skips_mdat_at_the_end() {
        let mp4 = make_mp4(1000, 5000, 640, 480, true);
        let len = mp4.len() as u64;
        let mut reads = 0usize;
        let found = locate_moov(len, |pos| {
            reads += 1;
            let mut buf = [0u8; 16];
            let start = usize::try_from(pos).ok()?;
            let end = (start + 16).min(mp4.len());
            if start >= mp4.len() {
                return None;
            }
            buf[..end - start].copy_from_slice(&mp4[start..end]);
            Some(buf)
        });
        let (off, size) = found.expect("末尾の moov を見つける");
        assert!(reads <= 8, "box ヘッダだけを読む: {reads} 回");
        let end = (off as usize + size as usize).min(mp4.len());
        let info = probe_mp4_moov(&mp4[off as usize..end]);
        assert_eq!(info.duration_secs, Some(5.0));
        assert_eq!((info.width, info.height), (Some(640), Some(480)));
    }

    #[test]
    fn media_parsers_never_panic_on_truncated_or_mutated_bytes() {
        let samples = vec![make_wav(1), make_mp4(600, 1200, 320, 240, false)];
        for s in &samples {
            for cut in 0..s.len() {
                let _ = probe_media(&s[..cut]);
                let _ = probe_mp4_moov(&s[..cut]);
            }
            // サイズ欄を嘘に書き換えても止まる (0 / 巨大 / 1 の各パターン)
            for bad in [0u8, 0xFF, 0x01] {
                let mut m = s.clone();
                for i in 0..m.len().min(64) {
                    m[i] = bad;
                }
                let _ = probe_media(&m);
                let _ = probe_mp4_moov(&m);
            }
        }
    }

    #[test]
    fn format_duration_table() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0:00"),
            (7.4, "0:07"),
            (59.6, "1:00"),
            (125.0, "2:05"),
            (3723.0, "1:02:03"),
            (-1.0, "—"),
            (f64::NAN, "—"),
            (f64::INFINITY, "—"),
        ];
        for (secs, want) in cases {
            assert_eq!(format_duration(*secs), *want, "{secs}");
        }
    }

    #[test]
    fn media_and_archive_extensions_are_case_insensitive() {
        for name in ["a.mp4", "a.MOV", "b/c.mkv", "d.WEBM", "e.avi", "f.m4v"] {
            assert!(is_media_path(Path::new(name)), "{name} は動画");
            assert!(is_video_path(Path::new(name)), "{name} は映像");
        }
        for name in ["a.mp3", "a.WAV", "b.flac", "c.AAC", "d.ogg", "e.m4a"] {
            assert!(is_media_path(Path::new(name)), "{name} は音声");
            assert!(!is_video_path(Path::new(name)), "{name} は映像ではない");
        }
        for name in ["a.zip", "b.JAR", "c.whl"] {
            assert!(is_archive_path(Path::new(name)), "{name} は書庫");
        }
        for name in ["a.rs", "b.txt", "noext", "c.mp4x"] {
            assert!(!is_media_path(Path::new(name)), "{name} はメディアでない");
            assert!(!is_archive_path(Path::new(name)), "{name} は書庫でない");
        }
    }

    // ── ZIP ────────────────────────────────────────────────────

    #[test]
    fn zip_central_directory_table() {
        let zip = make_zip(&[
            ("README.md", b"hello"),
            ("src/", b""),
            ("src/main.rs", b"fn main() {}"),
        ]);
        let l = parse_zip_at(&zip, 0);
        assert_eq!(l.error, None);
        assert_eq!(l.total, 3);
        assert!(!l.truncated);
        let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["README.md", "src/", "src/main.rs"]);
        assert_eq!(l.entries[0].size, 5);
        assert_eq!(l.entries[0].compressed, 5);
        assert!(l.entries[1].dir, "末尾が / のエントリはディレクトリ");
        assert!(!l.entries[2].dir);
        assert_eq!(l.entries[2].size, 12);
    }

    #[test]
    fn zip_window_can_start_partway_through_the_file() {
        let zip = make_zip(&[("a.txt", b"12345"), ("b.txt", b"67890")]);
        // セントラルディレクトリの手前で切って「末尾だけ渡す」状況を作る
        let base = 8u64;
        let l = parse_zip_at(&zip[base as usize..], base);
        assert_eq!(l.error, None, "末尾側だけでも読める");
        assert_eq!(l.total, 2);
    }

    #[test]
    fn zip_errors_are_reported_not_panicked() {
        // ZIP ではない
        assert_eq!(
            parse_zip_at(b"not a zip at all", 0).error,
            Some(ZipError::NoEndRecord)
        );
        assert_eq!(parse_zip_at(b"", 0).error, Some(ZipError::NoEndRecord));

        // 終端レコードはあるが、セントラルディレクトリの 2 件目が壊れている
        let mut broken = make_zip(&[("a", b"x"), ("b", b"y")]);
        let second = broken
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == b"PK\x01\x02")
            .map(|(i, _)| i)
            .nth(1)
            .expect("2 件目のセントラルディレクトリ見出し");
        broken[second + 1] = b'X';
        let l = parse_zip_at(&broken, 0);
        assert_eq!(l.error, Some(ZipError::BrokenDirectory));
        assert_eq!(l.total, 1, "読めたところまでは出す");

        // どこで切っても panic しない
        let zip = make_zip(&[("a.txt", b"12345"), ("dir/", b"")]);
        for cut in 0..zip.len() {
            let _ = parse_zip_at(&zip[..cut], 0);
        }
        // バイトを潰しても panic しない
        for i in 0..zip.len() {
            let mut m = zip.clone();
            m[i] = 0xFF;
            let _ = parse_zip_at(&m, 0);
        }
    }

    #[test]
    fn zip_entry_cap_marks_truncated() {
        let names: Vec<String> = (0..ZIP_MAX_ENTRIES + 5).map(|i| format!("f{i}")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        let zip = make_zip(&files);
        let l = parse_zip_at(&zip, 0);
        assert_eq!(l.total, ZIP_MAX_ENTRIES + 5);
        assert_eq!(l.entries.len(), ZIP_MAX_ENTRIES);
        assert!(l.truncated, "上限で打ち切ったことを伝える");
    }

    #[test]
    fn preview_tag_matches_variant() {
        let hex = PreviewDoc::Hex(HexDoc {
            bytes: vec![1, 2, 3],
            file_bytes: 3,
            kind: None,
            truncated: false,
        });
        let media = PreviewDoc::Media(MediaDoc {
            info: MediaInfo::default(),
            file_bytes: 0,
            kind: None,
            video: true,
        });
        let arch = PreviewDoc::Archive(ArchiveDoc {
            listing: ZipListing::default(),
            file_bytes: 0,
        });
        assert_eq!(hex.tag(), PreviewTag::Hex);
        assert_eq!(media.tag(), PreviewTag::Media);
        assert_eq!(arch.tag(), PreviewTag::Archive);
    }

    // ── アニメーション画像 ────────────────────────────────────

    /// リポジトリ同梱のデモ GIF を読む。無ければ `None` (CI のチェックアウトが
    /// 部分的でも落とさない)。パスは `CARGO_MANIFEST_DIR` から組み立てるので
    /// どの環境でも同じ場所を指す (ハードコードしない)。
    fn demo_gif() -> Option<Vec<u8>> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("zaivern-demo.gif");
        std::fs::read(p).ok()
    }

    #[test]
    fn animation_format_table() {
        // (先頭バイト, 期待, 説明)
        let cases: &[(&[u8], Option<AnimFormat>, &str)] = &[
            (b"GIF89a\x01\x00", Some(AnimFormat::Gif), "GIF89a"),
            (b"GIF87a\x01\x00", Some(AnimFormat::Gif), "GIF87a"),
            (
                b"\x89PNG\r\n\x1a\n\x00\x00\x00\r",
                Some(AnimFormat::Png),
                "PNG (APNG かは中身次第)",
            ),
            (
                b"RIFF\x00\x00\x00\x00WEBPVP8 ",
                Some(AnimFormat::WebP),
                "WebP",
            ),
            (b"\xFF\xD8\xFF\xE0", None, "JPEG はアニメーションを持てない"),
            (
                b"RIFF\x00\x00\x00\x00WAVEfmt ",
                None,
                "RIFF でも WAVE は別物",
            ),
            (b"", None, "空"),
            (b"GIF8", None, "GIF の途中で切れた先頭"),
        ];
        for (head, want, why) in cases {
            assert_eq!(animation_format(head), *want, "{why}");
        }
    }

    #[test]
    fn 遅延の読み替え表() {
        // (生の遅延 ms, 期待 ms, 根拠)
        let cases: &[(u32, u32, &str)] = &[
            (0, ANIM_DEFAULT_DELAY_MS, "0 = 「できるだけ速く」→ 100ms"),
            (1, ANIM_DEFAULT_DELAY_MS, "1ms も速すぎる指定として 100ms"),
            (9, ANIM_DEFAULT_DELAY_MS, "10ms 未満は読み替える"),
            (10, 10, "境界ちょうどはそのまま (読み替えない)"),
            (20, 20, "GIF の 2/100 秒"),
            (130, 130, "デモ GIF の 13/100 秒"),
            (u32::MAX, u32::MAX, "上限は置かない (長い静止コマは正当)"),
        ];
        for (raw, want, why) in cases {
            assert_eq!(normalize_delay_ms(*raw), *want, "{why}");
        }
    }

    #[test]
    fn 経過時間からコマ番号を求める表() {
        let d = [100u32, 200, 300]; // 総 600ms
                                    // (遅延列, ループ, 経過 ms, 期待コマ, 次までの残り, 説明)
        type Case<'a> = (&'a [u32], AnimLoop, u64, usize, Option<u64>, &'a str);
        let cases: &[Case] = &[
            (&d, AnimLoop::Forever, 0, 0, Some(100), "先頭"),
            (&d, AnimLoop::Forever, 99, 0, Some(1), "1 コマ目の終わり際"),
            (&d, AnimLoop::Forever, 100, 1, Some(200), "境界は次のコマへ"),
            (&d, AnimLoop::Forever, 299, 1, Some(1), "2 コマ目の終わり際"),
            (&d, AnimLoop::Forever, 300, 2, Some(300), "3 コマ目へ"),
            (
                &d,
                AnimLoop::Forever,
                600,
                0,
                Some(100),
                "1 周して先頭へ戻る",
            ),
            (
                &d,
                AnimLoop::Forever,
                6_000_000,
                0,
                Some(100),
                "何周しても割り算だけ",
            ),
            (&d, AnimLoop::Times(1), 0, 0, Some(100), "1 回再生の先頭"),
            (
                &d,
                AnimLoop::Times(1),
                300,
                2,
                None,
                "最終周の最後のコマは止まる",
            ),
            (
                &d,
                AnimLoop::Times(1),
                600,
                2,
                None,
                "再生し切ったら最後のコマで固定",
            ),
            (
                &d,
                AnimLoop::Times(1),
                u64::MAX,
                2,
                None,
                "経過が飛んでも固定",
            ),
            (
                &d,
                AnimLoop::Times(2),
                300,
                2,
                Some(300),
                "1 周目の最後はまだ続く",
            ),
            (&d, AnimLoop::Times(2), 900, 2, None, "2 周目の最後で止まる"),
            (
                &d,
                AnimLoop::Times(0),
                600,
                2,
                None,
                "0 回は 1 回として扱う",
            ),
            (&[], AnimLoop::Forever, 0, 0, None, "空のコマ列でも割らない"),
            (&[], AnimLoop::Times(3), 999, 0, None, "空 + 有限ループ"),
            (
                &[0, 0, 0],
                AnimLoop::Forever,
                999,
                0,
                None,
                "総再生時間 0 でもゼロ除算しない",
            ),
            (
                &[0, 0, 0],
                AnimLoop::Times(5),
                0,
                0,
                None,
                "総 0 + 有限ループ",
            ),
            (
                &[100],
                AnimLoop::Forever,
                50,
                0,
                Some(50),
                "1 コマだけ + 無限",
            ),
            (
                &[100],
                AnimLoop::Times(1),
                50,
                0,
                None,
                "1 コマだけ = もう変わらない",
            ),
        ];
        for (delays, loops, elapsed, frame, next, why) in cases {
            let got = frame_at(delays, *loops, *elapsed);
            assert_eq!(
                got,
                AnimCursor {
                    frame: *frame,
                    next_in_ms: *next
                },
                "{why} (経過 {elapsed}ms)"
            );
        }
    }

    #[test]
    fn 総再生時間はコマの遅延の合計() {
        assert_eq!(total_ms(&[]), 0);
        assert_eq!(total_ms(&[100, 200, 300]), 600);
        // u32 の最大級を並べても u64 で受けるので溢れない。
        assert_eq!(total_ms(&[u32::MAX, u32::MAX]), 2 * u64::from(u32::MAX));
    }

    #[test]
    fn 実物のデモgifを全コマ復号できる() {
        let Some(bytes) = demo_gif() else {
            eprintln!("[skip] assets/zaivern-demo.gif が無い");
            return;
        };
        let a = decode_animation(&bytes, &AnimLimits::default()).expect("デモ GIF は読める");
        // 実測値 (GIF のヘッダを直接読んで確かめた値)。
        assert_eq!(a.frames.len(), 127, "全 127 コマ");
        assert_eq!((a.source_width, a.source_height), (960, 540), "元の寸法");
        assert_eq!(a.truncated, None, "既定の上限では打ち切られない");
        assert_eq!(a.loops, AnimLoop::Forever, "NETSCAPE 拡張で無限ループ");
        // 既定の上限 (長辺 512) まで縮む。
        assert_eq!((a.width, a.height), (512, 288));
        // 全コマが同じ寸法 = 1 枚のテクスチャを使い回せる。
        let want = (a.width as usize) * (a.height as usize) * 4;
        assert!(
            a.frames.iter().all(|f| f.rgba.len() == want),
            "全コマが {want} バイト"
        );
        // 総再生時間は 12/100 と 13/100 秒の合計 = 15.88 秒。
        let delays = a.delays_ms();
        assert_eq!(delays.len(), a.frames.len());
        assert_eq!(total_ms(&delays), 15_880, "総再生時間 (ms)");
        assert!(
            delays.iter().all(|d| *d >= ANIM_MIN_DELAY_MS),
            "読み替え後は必ず下限以上"
        );
        // 記憶量が上限の内側にあることを、実測として固定する。
        let bytes_used: usize = a.frames.iter().map(|f| f.rgba.len()).sum();
        assert!(
            bytes_used <= ANIM_MAX_TOTAL_BYTES,
            "{bytes_used} <= {ANIM_MAX_TOTAL_BYTES}"
        );
    }

    #[test]
    fn デモgifのコマは合成済みで焼き付きも抜けもない() {
        let Some(bytes) = demo_gif() else {
            eprintln!("[skip] assets/zaivern-demo.gif が無い");
            return;
        };
        let a = decode_animation(&bytes, &AnimLimits::default()).expect("デモ GIF は読める");
        // GIF の部分更新 (差分) が合成されずに返ってくると、更新されなかった
        // 領域が透明 (alpha 0) のまま残る。画面録画なので全面不透明が正しい。
        for (i, f) in a.frames.iter().enumerate() {
            let clear = f
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[3] == 0)
                .count();
            assert_eq!(clear, 0, "{i} コマ目に透明画素が {clear} 個残っている");
        }
        // 全コマが同じ絵 = 前のコマが焼き付いて更新されていない、の検出。
        let first = &a.frames[0].rgba;
        assert!(
            a.frames.iter().any(|f| &f.rgba != first),
            "どのコマも先頭と同じ = 合成が効いていない"
        );
    }

    #[test]
    fn 静止画としてのgifとpngとjpegの断り方() {
        // 1 コマの GIF は「アニメーション 1 コマ」として読める (呼び出し側が
        // frames.len() > 1 で静止画と区別する)。
        let one = make_gif(4, 4, 1, 100, 0);
        let a = decode_animation(&one, &AnimLimits::default()).expect("1 コマでも読める");
        assert_eq!(a.frames.len(), 1);

        // アニメーションでない PNG / WebP は Still で断る (静止画経路へ回す)。
        let png = {
            let mut v = Vec::new();
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png)
                .expect("encode png");
            v
        };
        assert_eq!(
            decode_animation(&png, &AnimLimits::default()),
            Err(AnimError::Still),
            "静止 PNG は APNG ではない"
        );

        // アニメーションを持てない形式は NotAnimated。
        assert_eq!(
            decode_animation(b"\xFF\xD8\xFF\xE0JFIF", &AnimLimits::default()),
            Err(AnimError::NotAnimated),
            "JPEG"
        );
        assert_eq!(
            decode_animation(b"", &AnimLimits::default()),
            Err(AnimError::NotAnimated),
            "空"
        );
    }

    #[test]
    fn 壊れた入力でも復号がpanicしない() {
        let good = make_gif(8, 6, 6, 50, 0);
        let limits = AnimLimits::default();

        // 1) あらゆる位置で切り詰めても panic しない。
        for cut in 0..good.len() {
            let _ = decode_animation(&good[..cut], &limits);
        }
        // 2) 1 バイトずつ壊しても panic しない。
        for at in (0..good.len()).step_by(7) {
            let mut bad = good.clone();
            bad[at] ^= 0xFF;
            let _ = decode_animation(&bad, &limits);
        }
        // 3) GIF ではないバイト列。
        for junk in [
            &b""[..],
            &b"GIF89a"[..],
            &b"GIF89a\x00\x00\x00\x00"[..],
            &[0u8; 64][..],
            &[0xFFu8; 1024][..],
        ] {
            let _ = decode_animation(junk, &limits);
        }
        // 4) 嘘の巨大寸法 (65535×65535 = 17GB) を宣言したヘッダ。ここは
        //    復号器自身も断る (gif クレートが既定で 50MB の上限を持つ) が、
        //    panic せず Err で返ることを固定しておく。
        let mut huge = Vec::from(&b"GIF89a"[..]);
        huge.extend_from_slice(&u16::MAX.to_le_bytes()); // width
        huge.extend_from_slice(&u16::MAX.to_le_bytes()); // height
        huge.extend_from_slice(&[0x00, 0x00, 0x00]); // packed / bg / aspect
        huge.push(0x3B); // trailer
        assert!(decode_animation(&huge, &limits).is_err());
    }

    #[test]
    fn 宣言寸法が予算を超えたら復号を始めない() {
        // 200×100 = 80,000 バイト/コマ。予算をその手前に置くと、コマを
        // 1 つも確保せずに TooLarge で降りる (嘘の寸法でメモリを焼かせない)。
        let bytes = make_gif(200, 100, 4, 60, 0);
        let tight = AnimLimits {
            max_total_bytes: 200 * 100 * 4 - 1,
            max_side: 0, // 縮小に逃がさず、宣言寸法そのもので判定させる
            ..AnimLimits::default()
        };
        assert_eq!(
            decode_animation(&bytes, &tight),
            Err(AnimError::TooLarge),
            "1 コマぶんも入らない予算では復号を始めない"
        );
        // ちょうど 1 コマ入る予算なら、1 コマだけ読んで打ち切りを報告する。
        let just = AnimLimits {
            max_total_bytes: 200 * 100 * 4,
            max_side: 0,
            ..AnimLimits::default()
        };
        let a = decode_animation(&bytes, &just).expect("1 コマは読める");
        assert_eq!(a.frames.len(), 1);
        assert_eq!(a.truncated, Some(AnimStop::TotalBytes));
    }

    #[test]
    fn コマ数の上限で打ち切りが報告される() {
        let bytes = make_gif(8, 6, 20, 50, 0);
        let limits = AnimLimits {
            max_frames: 5,
            ..AnimLimits::default()
        };
        let a = decode_animation(&bytes, &limits).expect("読める");
        assert_eq!(a.frames.len(), 5, "上限ちょうどで止まる");
        assert_eq!(
            a.truncated,
            Some(AnimStop::FrameCount),
            "黙って切らずに理由を返す"
        );
        // 上限に届かなければ打ち切りは報告されない。
        let a = decode_animation(&bytes, &AnimLimits::default()).expect("読める");
        assert_eq!(a.frames.len(), 20);
        assert_eq!(a.truncated, None);
    }

    #[test]
    fn 総バイト数の上限で打ち切りが報告される() {
        let bytes = make_gif(16, 16, 12, 50, 0);
        let per_frame = 16 * 16 * 4;
        let limits = AnimLimits {
            // 3 コマぶんちょうど。4 コマ目は入らない。
            max_total_bytes: per_frame * 3,
            max_side: 0, // 縮小しない (バイト数の上限だけを試す)
            ..AnimLimits::default()
        };
        let a = decode_animation(&bytes, &limits).expect("読める");
        assert_eq!(a.frames.len(), 3, "予算ちょうどまで");
        assert_eq!(a.truncated, Some(AnimStop::TotalBytes));
        let used: usize = a.frames.iter().map(|f| f.rgba.len()).sum();
        assert!(used <= limits.max_total_bytes, "予算を 1 バイトも超えない");
    }

    #[test]
    fn 上限に当たった後は一切復号していない() {
        // 前半 4 コマが健全で、その先が壊れている GIF を作る。
        let mut bytes = make_gif(8, 6, 12, 50, 0);
        let cut = bytes.len() * 2 / 5;
        bytes.truncate(cut);

        // まず「壊れている」ことを確かめる: 上限を外すと Decode で止まる。
        let loose = decode_animation(&bytes, &AnimLimits::default()).expect("前半は読める");
        assert_eq!(
            loose.truncated,
            Some(AnimStop::Decode),
            "上限が緩ければ壊れた場所まで進む"
        );
        let healthy = loose.frames.len();
        assert!(healthy >= 2, "健全なコマが 2 つ以上ある前提 ({healthy})");

        // 上限をその手前に置くと、壊れた場所へ**到達しない** = FrameCount で
        // 止まる。先に全部復号してから切る実装ならここが Decode になる。
        let limits = AnimLimits {
            max_frames: healthy - 1,
            ..AnimLimits::default()
        };
        let tight = decode_animation(&bytes, &limits).expect("読める");
        assert_eq!(tight.frames.len(), healthy - 1);
        assert_eq!(
            tight.truncated,
            Some(AnimStop::FrameCount),
            "上限で止めた後は 1 コマも余分に復号していない"
        );
    }

    #[test]
    fn 復号の費用はコマ数に比例する() {
        // 絶対時間では測らない (負荷で必ず嘘をつく)。守りたい性質は
        // 「コマ数を 2 倍にしたら、積む画素も 2 倍で頭打ちしない」こと。
        let n = 8usize;
        let a = decode_animation(&make_gif(16, 12, n, 50, 0), &AnimLimits::default()).expect("n");
        let b =
            decode_animation(&make_gif(16, 12, n * 2, 50, 0), &AnimLimits::default()).expect("2n");
        let bytes_a: usize = a.frames.iter().map(|f| f.rgba.len()).sum();
        let bytes_b: usize = b.frames.iter().map(|f| f.rgba.len()).sum();
        assert_eq!(a.frames.len(), n);
        assert_eq!(b.frames.len(), n * 2);
        assert_eq!(bytes_b, bytes_a * 2, "画素の総量はコマ数に正比例する");
        assert_eq!(total_ms(&b.delays_ms()), total_ms(&a.delays_ms()) * 2);
    }

    #[test]
    fn ループ回数を読み取る() {
        let inf = make_gif(4, 4, 3, 50, 0);
        assert_eq!(
            decode_animation(&inf, &AnimLimits::default())
                .expect("読める")
                .loops,
            AnimLoop::Forever
        );
        let three = make_gif(4, 4, 3, 50, 3);
        assert_eq!(
            decode_animation(&three, &AnimLimits::default())
                .expect("読める")
                .loops,
            AnimLoop::Times(3)
        );
    }

    #[test]
    fn 上限より大きいコマは縮んで全コマ同じ寸法になる() {
        let bytes = make_gif(200, 100, 4, 60, 0);
        let limits = AnimLimits {
            max_side: 50,
            ..AnimLimits::default()
        };
        let a = decode_animation(&bytes, &limits).expect("読める");
        assert_eq!((a.source_width, a.source_height), (200, 100), "元は保つ");
        assert_eq!((a.width, a.height), (50, 25), "長辺 50 へ縮む (縦横比維持)");
        let want = 50 * 25 * 4;
        assert!(a.frames.iter().all(|f| f.rgba.len() == want));
        // 縮小不要なときは 1 画素も触らない。
        let same = decode_animation(&bytes, &AnimLimits::default()).expect("読める");
        assert_eq!((same.width, same.height), (200, 100));
    }
}
