use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use std::sync::Arc;

use eframe::egui::Galley;

use crate::highlight::{fold_ranges, FoldRange, Highlighter};
use crate::preview::{ArchiveDoc, HexDoc, MediaDoc, PreviewDoc};

pub fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// キャッシュキー合成用。XOR と違い非可換なので、値の入れ替わりで
/// 同じキーに衝突しない (FNV 風の乗算 + 加算)。
pub fn combine_hash(acc: u64, v: u64) -> u64 {
    acc.wrapping_mul(0x100000001b3).wrapping_add(v)
}

/// ディスク上の最終更新時刻(外部変更検知用)。
pub fn disk_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// 外部(エージェント・他ツール)によるファイル変更の検知結果。
pub enum ExternalEvent {
    /// 未保存の編集が無かったのでディスクの内容へ読み直した
    Reloaded { index: usize, title: String },
    /// 未保存の編集があるため読み直さなかった(上書き注意)
    Conflict { title: String },
}

/// タブの種類。
///
/// ファイル以外の中身 (PR 差分など) をタブとして開けるようにするための印。
/// `Buffer` に持たせることで、タブの切り替え・クローズ・アクティブ管理は
/// 既存の仕組みをそのまま使い回せる。
///
/// **`File` 以外は読み取り専用。** 保存 / LSP / git ガターは対象外
/// (これらは `path` が `Some` であることを前提に動くため、`path: None` と
/// `read_only()` の二重の防御で守る)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BufferKind {
    /// 通常のファイル (または未保存の untitled)。
    #[default]
    File,
    /// GitHub の Pull Request 差分ビュー。
    PrDiff { number: u64 },
    /// プロンプトレースの racer 差分ビュー (slot = racer の添字)。
    RaceDiff { slot: usize },
    /// 1 コミットの差分ビュー (ガターの git blame をクリックして開く)。
    /// 本文は `git show` の unified diff。
    CommitDiff,
    /// 画像ビューア (png/jpg 等)。ピクセルは `Buffer::image` に持つ。
    /// `path` は `Some` (外部変更の mtime 監視で再デコードするため) だが、
    /// `read_only()` が真なので保存・編集の経路には乗らない。
    Image,
    /// PDF ビューア。抽出したテキストを `text` に持つ**普通の本文タブ**なので、
    /// 検索・折り返し・コピーがそのまま効く。`path` は `Some` (mtime 監視で
    /// 再抽出するため) だが、`read_only()` が真なので抽出結果が元の PDF へ
    /// 書き戻されることはない。
    Pdf,
    /// 16 進ダンプ。**テキストとして読めなかったものが必ずここへ落ちる**
    /// (拡張子ではなく中身で決める。`preview::looks_binary` を参照)。
    /// 中身は `Buffer::preview` の [`PreviewDoc::Hex`]。
    Hex,
    /// 動画・音声の情報カード。中身は [`PreviewDoc::Media`]。
    Media,
    /// 書庫 (ZIP 形式) の中身一覧。中身は [`PreviewDoc::Archive`]。
    Archive,
}

impl BufferKind {
    /// このタブが読み取り専用か。
    pub fn read_only(&self) -> bool {
        !matches!(self, BufferKind::File)
    }

    /// 本文の `TextEdit` ではなく**専用ビューア**で描くタブか。
    ///
    /// 差分タブ (`PrDiff` / `RaceDiff`) はここに含めない。あちらは
    /// 本文 (`text`) を持つ読み取り専用タブで、描画も別経路にある。
    pub fn preview_only(&self) -> bool {
        matches!(
            self,
            BufferKind::Image | BufferKind::Hex | BufferKind::Media | BufferKind::Archive
        )
    }
}

pub struct Buffer {
    pub id: u64,
    pub path: Option<PathBuf>,
    /// タブの種類 (既定は通常ファイル)。
    pub kind: BufferKind,
    pub title: String,
    pub text: String,
    pub saved_hash: u64,
    pub lang: String,
    /// 読み込んだときの文字コード。保存で元の形へ戻すために持つ。
    ///
    /// 日本語圏のソース・ログ・CSV は今も CP932 (Shift_JIS) が現役で、
    /// UTF-8 決め打ちだと**開くことすらできない** (`read_to_string` が失敗する)。
    /// 開けるようにするだけでは足りない: 保存で勝手に UTF-8 へ変えると、
    /// そのファイルを読む他のツール (Excel・既存のバッチ) が壊れる。
    pub encoding: crate::textenc::Encoding,
    /// (cache key, 本文 galley) — recomputed only when text/theme/font change.
    /// キーには折り返し設定と折り返し幅・空白可視化の有無も含まれるため、
    /// それらが変わらない限りフレーム跨ぎで使い回せる
    /// (折り返し無効時は wrap.max_width = INFINITY で幅に依存しない)。
    pub cache: Option<(u64, Arc<Galley>)>,
    /// (cache key, gutter galley) — 行番号 + git 差分マーク色。
    /// galley 化まで済ませて持つので、毎フレームの LayoutJob コピーが要らない。
    /// キーには font size と pixels_per_point が入っており、
    /// フォント/DPI が変われば作り直される。
    pub gutter: Option<(u64, Arc<Galley>)>,
    /// 読み込み/保存時点のディスク上の mtime。外部変更はこれとの差分で検知する。
    pub disk_mtime: Option<SystemTime>,
    /// 警告済みの外部変更 mtime(同じ競合を連続通知しないため)。
    pub conflict_notified: Option<SystemTime>,
    /// 画像タブ (`BufferKind::Image`) のデコード済みピクセル。それ以外は None。
    pub image: Option<ImageDoc>,
    /// PDF タブ (`BufferKind::Pdf`) の抽出待ち。Some の間は本文が
    /// 「読み込み中…」で、`Editor::poll_pdf_jobs` が完成本文へ差し替える。
    pub pdf_job: Option<PdfJob>,
    /// 折りたたみ状態。本文を書き換えたら `refresh_folds()` を呼ぶ。
    pub folds: FoldState,
    /// 行ブックマーク。行の増減時は `bookmarks.shift_lines()` を呼ぶ。
    #[allow(dead_code)]
    pub bookmarks: Bookmarks,
    /// 巨大ファイルモードの制限 (通常のファイルでは既定値 = 無制限)。
    pub large: LargeFileMode,
    /// CSV/TSV のテーブル表示。`None` の間は普通のテキストとして描く。
    pub table: Option<TableView>,
    /// 専用ビューア (16 進 / メディア / 書庫) の中身。
    ///
    /// 種類ごとにフィールドを増やすと `Buffer` の生成箇所が毎回全部壊れるので、
    /// 1 本の列挙型にまとめてある。`kind.preview_only()` が真でも、読めなかった
    /// ときは `None` になりうる (app.rs は「表示できません」を出す)。
    pub preview: Option<PreviewDoc>,
    /// ミニマップの行データ: `(本文 galley のキャッシュキー, 行データ)`。
    ///
    /// **キーが変わったときだけ**組み直す (設計原則 3: アイドル時のコストはゼロ)。
    /// キーは `cache` のキーと同じもの — つまり本文・言語・テーマ・フォント・
    /// 折り返し・空白可視化のどれかが変わったときにだけ再構築が走り、
    /// 何も変わらないフレームでは Vec を読むだけになる。
    pub minimap: Option<(u64, crate::minimap::MinimapRows)>,
}

/// 画像タブのデコード結果。
///
/// デコードは `Editor::open` 時 (egui の ctx が不要)、GPU テクスチャ化は
/// 初回描画時 (ctx が必要) の二段構え。markdown.rs のインライン画像
/// (`load_image_texture`) と同じ流儀。
pub struct ImageDoc {
    /// RGBA8 ピクセル列 (縮小適用済み)。デコード失敗時は空。
    pub rgba: Vec<u8>,
    /// `rgba` の実サイズ [幅, 高さ] (縮小後)。
    pub size: [usize; 2],
    /// 元画像のピクセルサイズ (ステータス行の表示用)。
    pub orig_size: (u32, u32),
    /// ディスク上のファイルサイズ (バイト)。
    pub file_bytes: u64,
    /// デコード失敗時の説明。Some のときビューアはエラー表示になる
    /// (バイナリをテキストとして文字化け表示するより「読めない」と明示する)。
    pub error: Option<String>,
    /// 遅延生成の GPU テクスチャ (初回描画でアップロードし、以後使い回す)。
    pub texture: Option<eframe::egui::TextureHandle>,
}

/// 画像ビューアで開く拡張子 (小文字)。Cargo.toml の image クレートの
/// feature (png/jpeg/gif/webp/ico) に bmp を足した集合。bmp は feature 無効で
/// デコードに失敗するが、テキスト経路でバイナリの文字化けを見せるより
/// 画像タブで「表示できません」と伝える方が親切なのでここへ回す。
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "ico", "bmp"];

/// 拡張子から画像ビューアで開くべきパスか判定する (大文字小文字は無視)。
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            IMAGE_EXTS.iter().any(|x| *x == e)
        })
        .unwrap_or(false)
}

/// GPU テクスチャに安全な最大辺長。egui/wgpu はバックエンドの上限
/// (8192 が下限のことが多い) を超えるテクスチャでエラーになるため、
/// 超える画像は縮小してから載せる。
pub const MAX_TEXTURE_SIDE: u32 = 8192;

/// 縮小が必要なら縮小後サイズ (アスペクト比維持) を返す。不要なら None。
pub fn image_downscale(w: u32, h: u32, max_side: u32) -> Option<(u32, u32)> {
    let longest = w.max(h);
    if longest <= max_side || longest == 0 {
        return None;
    }
    let scale = max_side as f64 / longest as f64;
    let nw = ((w as f64 * scale).round() as u32).clamp(1, max_side);
    let nh = ((h as f64 * scale).round() as u32).clamp(1, max_side);
    Some((nw, nh))
}

/// バイト列を画像としてデコードする。失敗しても panic せず `error` 入りで返す。
/// アニメーション GIF は最初のフレームのみの静止表示 (今夜はこれで十分)。
pub fn decode_image_doc(raw: &[u8], file_bytes: u64) -> ImageDoc {
    match image::load_from_memory(raw) {
        Ok(img) => {
            let mut rgba = img.to_rgba8();
            let orig = rgba.dimensions();
            if let Some((nw, nh)) = image_downscale(orig.0, orig.1, MAX_TEXTURE_SIDE) {
                // 巨大画像は GPU 上限超えの描画エラーを避けるため縮小して載せる
                rgba = image::imageops::resize(
                    &rgba,
                    nw,
                    nh,
                    image::imageops::FilterType::Triangle,
                );
            }
            let (w, h) = rgba.dimensions();
            ImageDoc {
                rgba: rgba.into_raw(),
                size: [w as usize, h as usize],
                orig_size: orig,
                file_bytes,
                error: None,
                texture: None,
            }
        }
        Err(e) => ImageDoc {
            rgba: Vec::new(),
            size: [0, 0],
            orig_size: (0, 0),
            file_bytes,
            error: Some(e.to_string()),
            texture: None,
        },
    }
}

/// 画像ビューア: 表示領域へ収まる「フィット」倍率。等倍を上限にする
/// (小さい画像を無理に引き伸ばさない)。
pub fn image_fit_scale(img_w: f32, img_h: f32, avail_w: f32, avail_h: f32) -> f32 {
    if img_w <= 0.0 || img_h <= 0.0 || avail_w <= 0.0 || avail_h <= 0.0 {
        return 1.0;
    }
    (avail_w / img_w).min(avail_h / img_h).min(1.0)
}

/// 画像ビューアのズーム下限/上限。
pub const IMAGE_ZOOM_MIN: f32 = 0.05;
pub const IMAGE_ZOOM_MAX: f32 = 32.0;

/// 画像ビューア: ズームの段階変更 (dir=+1 拡大 / -1 縮小)。1.25 倍刻み。
pub fn image_zoom_step(cur: f32, dir: i32) -> f32 {
    (cur * 1.25f32.powi(dir)).clamp(IMAGE_ZOOM_MIN, IMAGE_ZOOM_MAX)
}

// ─── PDF ビューア (テキスト抽出) ──────────────────────────────────
//
// PDF は「読み取り専用のテキストタブ」として開く。専用のレンダラを足さない
// 代わりに、検索・折り返し・コピー・テーマといった本文タブの機能を丸ごと
// そのまま使える (`BufferKind::Pdf` は `read_only()` が真なので、保存・
// 編集・置換の経路には乗らない)。
//
// 抽出は pdf-extract (MIT / 純 Rust / lopdf ベース) で行う。ネイティブ
// ライブラリも実行時ダウンロードも要らないので、素の `cargo build` だけで
// macOS / Windows / Linux のどれでも同じように動く。

/// PDF ビューアで開く拡張子 (小文字)。
pub const PDF_EXTS: &[&str] = &["pdf"];

/// 拡張子から PDF ビューアで開くべきパスか判定する (大文字小文字は無視)。
pub fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            PDF_EXTS.iter().any(|x| *x == e)
        })
        .unwrap_or(false)
}

/// テキスト抽出を試みる上限バイト数。
///
/// pdf-extract の抽出コストはページ数とフォント数に比例し、数十 MB 級の
/// スキャン PDF では秒単位になりうる。UI スレッドを止めないための防壁。
/// `MAX_OPEN_BYTES` より小さくしておくことで、「開けないファイル」
/// ではなく「開けるが抽出だけ諦めるタブ」として出せる。
pub const PDF_MAX_BYTES: u64 = 32 * 1024 * 1024;

// ─── ユニバーサルプレビュー (IO 側) ────────────────────────────────
//
// 判定と解析そのものは `preview.rs` の純関数が持つ。ここはファイルから
// **必要な範囲だけを読む**役目に徹する。どれも「丸ごと読まない」のが要点で、
// 数 GB の動画や書庫を開いてもメモリは一定に収まる。

/// メディアのヘッダとして読む先頭バイト数。
/// WAV の `fmt`/`data`、FLAC の STREAMINFO、先頭に `moov` を置いた mp4 は
/// この範囲に収まる。収まらない mp4 は [`crate::preview::locate_moov`] で辿る。
const MEDIA_HEAD_BYTES: u64 = 1024 * 1024;

/// `moov` box をまるごと読む上限。ここを超える moov は実在しない
/// (超えるとしたらチャプタ情報で膨らんだ壊れたファイル)。
const MOOV_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// 書庫の末尾から読む幅。セントラルディレクトリと終端レコードは必ず末尾側に
/// あるので、ここだけ読めば数 GB の zip でも一覧が作れる。
/// 64 MB のディレクトリは約 70 万エントリぶんで、現実の書庫は必ず収まる。
const ARCHIVE_TAIL_BYTES: u64 = 64 * 1024 * 1024;

/// 中身判定のために先頭 [`crate::preview::SNIFF_BYTES`] だけ読む。
fn read_head(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| format!("開けませんでした: {e}"))?;
    let mut head = Vec::with_capacity(crate::preview::SNIFF_BYTES);
    f.take(crate::preview::SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|e| format!("開けませんでした: {e}"))?;
    Ok(head)
}

/// 動画・音声のヘッダを読む。**中身 (mdat) は読まない**。
fn read_media_doc(path: &Path, file_bytes: u64) -> MediaDoc {
    use std::io::{Read, Seek, SeekFrom};
    let mut info = crate::preview::MediaInfo::default();
    let mut kind = None;
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut head = Vec::new();
        let _ = (&mut f).take(MEDIA_HEAD_BYTES).read_to_end(&mut head);
        kind = crate::preview::sniff_kind(&head);
        info = crate::preview::probe_media(&head);
        // ffmpeg 等は既定で `moov` を**末尾**に置く。先頭だけ見て諦めると
        // ほとんどの mp4 が「情報なし」になるので、box を辿って探しに行く
        // (mdat は seek で飛ばすので読むのは 16 バイト × box 数だけ)。
        if info.is_empty() && head.len() >= 8 && &head[4..8] == b"ftyp" {
            let found = crate::preview::locate_moov(file_bytes, |pos| {
                let mut buf = [0u8; 16];
                f.seek(SeekFrom::Start(pos)).ok()?;
                let mut got = 0usize;
                while got < buf.len() {
                    match f.read(&mut buf[got..]) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(_) => return None,
                    }
                }
                // 末尾で 16 バイト取れない分は 0 のまま (locate_moov が許容する)
                Some(buf)
            });
            if let Some((off, len)) = found {
                let mut moov = Vec::new();
                if f.seek(SeekFrom::Start(off)).is_ok() {
                    let _ = (&mut f).take(len.min(MOOV_MAX_BYTES)).read_to_end(&mut moov);
                    info = crate::preview::probe_mp4_moov(&moov);
                }
            }
        }
    }
    MediaDoc {
        info,
        file_bytes,
        kind,
        video: crate::preview::is_video_path(path),
    }
}

/// 書庫の末尾を読んで中身を一覧にする。ZIP でなければ `None`
/// (呼び出し側が 16 進ダンプへ落とす — 拡張子が嘘でも壊れない)。
fn read_archive_doc(path: &Path, file_bytes: u64) -> Option<ArchiveDoc> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let window = file_bytes.min(ARCHIVE_TAIL_BYTES);
    let base = file_bytes - window;
    f.seek(SeekFrom::Start(base)).ok()?;
    let mut buf = Vec::new();
    f.take(window).read_to_end(&mut buf).ok()?;
    let listing = crate::preview::parse_zip_at(&buf, base);
    if listing.error == Some(crate::preview::ZipError::NoEndRecord) {
        return None;
    }
    Some(ArchiveDoc {
        listing,
        file_bytes,
    })
}

/// 16 進ダンプ用に先頭 [`crate::preview::HEX_MAX_BYTES`] だけ読む。
fn read_hex_doc(path: &Path, file_bytes: u64) -> Option<HexDoc> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    f.take(crate::preview::HEX_MAX_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    // 上限まで読むと Vec は倍々に伸びて実際の 2 倍近く確保していることがある
    bytes.shrink_to_fit();
    Some(HexDoc {
        kind: crate::preview::sniff_kind(&bytes),
        truncated: file_bytes > bytes.len() as u64,
        file_bytes,
        bytes,
    })
}

/// パスと種類から専用ビューアの中身を作る。
/// `Editor::open` と `Editor::reload_from_disk` の**唯一の入口**
/// (二か所で組み立てると外部変更のときだけ挙動が違う、が必ず起きる)。
fn build_preview(kind: BufferKind, path: &Path) -> Option<PreviewDoc> {
    let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match kind {
        BufferKind::Media => Some(PreviewDoc::Media(read_media_doc(path, file_bytes))),
        BufferKind::Archive => read_archive_doc(path, file_bytes).map(PreviewDoc::Archive),
        BufferKind::Hex => read_hex_doc(path, file_bytes).map(PreviewDoc::Hex),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 巨大ファイルモード
//
// 以前は 50 MB を超えるファイルを**開くこと自体を断って**いた。しかし
// 「大きいログを見たい」は正当な用途で、断られると外部エディタを開く羽目になる。
// そこで段階を付け、大きいファイルは**読み取り専用 + ハイライト無効**で開く。
// 本当に開けない (メモリが持たない) 大きさだけを [`MAX_OPEN_BYTES`] で断る。
// ---------------------------------------------------------------------------

/// この大きさ以上はシンタックスハイライトを止める (編集は可能)。
///
/// syntect は 1 文書ぶんの `LayoutJob` を作るため、数 MB を超えると
/// 1 打鍵ごとの再ハイライトが目に見えて重くなる。
pub const HEAVY_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// この大きさ以上は読み取り専用で開く (巨大ファイルモード)。
///
/// `TextEdit::multiline` は編集のたびに文字列全体を作り直すので、
/// この規模で編集を許すと 1 打鍵が数百 ms になる。閲覧はできる。
pub const LARGE_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// `Editor::open` が読み込みを拒否する上限。ここを超えるとメモリ上に
/// 載せた時点で破綻するため、素直に断る。
pub const MAX_OPEN_BYTES: u64 = 512 * 1024 * 1024;

/// 大きさの段。`(この大きさ以上, ハイライトする, 編集できる)` を
/// **大きい順**に並べる。最初に一致した段が採用される。
///
/// 閾値を増やしたいときはこの表に 1 行足すだけでよい (関数側は触らない)。
const LARGE_FILE_TIERS: &[(u64, bool, bool)] = &[
    (LARGE_FILE_BYTES, false, false),
    (HEAVY_FILE_BYTES, false, true),
    (0, true, true),
];

/// 巨大ファイルモードの状態。UI (app.rs) はこれを見てバナーを出す。
///
/// 文言は i18n を持つ app.rs 側で `trf!` を使って組み立てる。ここでは
/// 「何がどう制限されているか」だけを渡す (この層は文字列を持たない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LargeFileMode {
    /// 何らかの制限がかかっているか (バナーを出すべきか)。
    pub active: bool,
    /// 編集を禁止するか。
    pub read_only: bool,
    /// シンタックスハイライトを行うか (false なら素のテキストで描く)。
    pub highlight: bool,
    /// 判定に使ったファイルサイズ (バナーに出す)。
    pub bytes: u64,
}

impl Default for LargeFileMode {
    fn default() -> Self {
        Self {
            active: false,
            read_only: false,
            highlight: true,
            bytes: 0,
        }
    }
}

/// サイズから決まる開き方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenDecision {
    /// 開ける (制限の有無は [`LargeFileMode`] を見る)。
    Open(LargeFileMode),
    /// 大きすぎて開けない。
    Refuse { bytes: u64, limit: u64 },
}

/// ファイルサイズから開き方を決める純関数。
pub fn open_decision(bytes: u64) -> OpenDecision {
    if bytes > MAX_OPEN_BYTES {
        return OpenDecision::Refuse {
            bytes,
            limit: MAX_OPEN_BYTES,
        };
    }
    let tier = LARGE_FILE_TIERS
        .iter()
        .find(|(min, _, _)| bytes >= *min)
        .copied()
        .unwrap_or((0, true, true));
    let (_, highlight, editable) = tier;
    OpenDecision::Open(LargeFileMode {
        active: !highlight || !editable,
        read_only: !editable,
        highlight,
        bytes,
    })
}

/// PDF からページ単位のテキストを取り出す。
///
/// pdf-extract は壊れた / 暗号化された PDF に対して `panic!` することが
/// あるため (フォント解析まわり)、`catch_unwind` で必ず握り潰す。
/// app.rs のフレームガードと同じ流儀で、panic は落ちずにメッセージへ落とす。
pub fn extract_pdf_pages(raw: &[u8]) -> Result<Vec<String>, String> {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem_by_pages(raw)
    }));
    match caught {
        Ok(Ok(pages)) => Ok(pages),
        Ok(Err(e)) => Err(e.to_string()),
        // panic の詳細は panic フック (main.rs) が ~/.zaivern/panic.log へ残す
        Err(_) => Err("内部エラー (詳細: ~/.zaivern/panic.log)".into()),
    }
}

/// 抽出したページ群を、ヘッダ + ページ区切り付きの本文へ組み立てる。
fn pdf_render_pages(header: &str, pages: &[String]) -> String {
    let total = pages.len();
    let mut out = String::with_capacity(header.len() + pages.iter().map(|p| p.len() + 32).sum::<usize>());
    out.push_str(header);
    for (i, page) in pages.iter().enumerate() {
        out.push_str(&format!("\n── ページ {} / {} ──\n\n", i + 1, total));
        let body = page.trim_matches(|c: char| c == '\n' || c == '\r');
        if body.trim().is_empty() {
            out.push_str("(このページにテキストはありません)\n");
        } else {
            out.push_str(body);
            out.push('\n');
        }
    }
    out
}

/// PDF タブの本文を組み立てる。**絶対に panic しない**: 抽出に失敗しても
/// 壊れていても、読める日本語のメッセージが入ったテキストを返す。
///
/// `file_bytes` はディスク上のサイズ (ヘッダ表示と上限判定に使う)。
pub fn pdf_buffer_text(name: &str, raw: &[u8], file_bytes: u64) -> String {
    let size = human_bytes(file_bytes);
    if file_bytes > PDF_MAX_BYTES {
        return format!(
            "📄 {name}\n{size} · 読み取り専用\n\n\
             ⚠ PDF が大きすぎるためテキスト抽出を省略しました \
             ({size} > {})。\n外部のビューアで開いてください。\n",
            human_bytes(PDF_MAX_BYTES)
        );
    }
    match extract_pdf_pages(raw) {
        Ok(pages) if !pages.is_empty() && pages.iter().any(|p| !p.trim().is_empty()) => {
            let header = format!(
                "📄 {name}\n{} ページ · {size} · 読み取り専用\n",
                pages.len()
            );
            pdf_render_pages(&header, &pages)
        }
        // ページはあるが全ページ空 = スキャン画像だけの PDF
        Ok(pages) => format!(
            "📄 {name}\n{} ページ · {size} · 読み取り専用\n\n\
             ⚠ この PDF はテキストを抽出できません（画像PDFの可能性があります）\n",
            pages.len()
        ),
        Err(e) => format!(
            "📄 {name}\n{size} · 読み取り専用\n\n\
             ⚠ この PDF はテキストを抽出できません（画像PDFの可能性があります）\n\
             詳細: {e}\n"
        ),
    }
}

/// 抽出完了を待つ本文 (ワーカーへ預けたときのプレースホルダ)。
pub fn pdf_loading_text(name: &str, file_bytes: u64) -> String {
    format!(
        "📄 {name}\n{} · 読み取り専用\n\n⏳ 読み込み中… (テキストを抽出しています)\n",
        human_bytes(file_bytes)
    )
}

/// 同期で抽出完了を待つ上限。
///
/// 実測 (macOS / release / 実ファイル 22 本): 中央値 ≈ 33 ms、
/// 8 割は 250 ms 未満で終わる。一方 139 ページ・11 MB のテキスト主体 PDF は
/// **6.2 秒**かかった。全部同期にすると後者でウィンドウが数秒固まるので、
/// この予算内に終わらなければワーカーへ預けて「読み込み中…」を出す。
pub const PDF_SYNC_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// バックグラウンドで走らせている PDF 抽出の受け口。
///
/// ワーカー側は必ず 1 回だけ本文を送る (panic も `pdf_buffer_text` の内側で
/// 握り潰されてメッセージになる)。タブを閉じて受け口ごと落ちても、
/// 送信が失敗するだけでスレッドは静かに終わる。
pub struct PdfJob {
    rx: std::sync::mpsc::Receiver<String>,
    /// 表示用のファイル名 (スレッドが消えたときのエラー本文に使う)。
    name: String,
    file_bytes: u64,
}

impl PdfJob {
    /// テスト専用: 任意のチャネルから待ち状態を作る (遅い PDF を用意せずに
    /// 「読み込み中 → 完成」の差し替えを検証するため)。
    #[cfg(test)]
    pub fn for_test(rx: std::sync::mpsc::Receiver<String>, name: &str, file_bytes: u64) -> Self {
        Self {
            rx,
            name: name.to_string(),
            file_bytes,
        }
    }

    /// 完了していれば本文を取り出す。まだなら None (UI は待たない)。
    pub fn take(&self) -> Option<String> {
        match self.rx.try_recv() {
            Ok(text) => Some(text),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            // 送信側が結果を送らずに消えた (通常は起こらない)。
            // 永久に「読み込み中…」で固まらないよう、必ず終わらせる。
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(format!(
                "📄 {}\n{} · 読み取り専用\n\n\
                 ⚠ この PDF はテキストを抽出できません（画像PDFの可能性があります）\n\
                 詳細: 抽出処理が中断されました\n",
                self.name,
                human_bytes(self.file_bytes)
            )),
        }
    }
}

/// PDF タブの本文を用意する。`PDF_SYNC_BUDGET` 内に終われば完成した本文を、
/// 間に合わなければ「読み込み中…」と、後で差し替えるための `PdfJob` を返す。
pub fn start_pdf_extraction(name: &str, raw: Vec<u8>, file_bytes: u64) -> (String, Option<PdfJob>) {
    // 上限超えはスレッドを起こす前に打ち切る (数十 MB を move しない)
    if file_bytes > PDF_MAX_BYTES {
        return (pdf_buffer_text(name, &[], file_bytes), None);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let name_owned = name.to_string();
    let worker_name = name_owned.clone();
    std::thread::spawn(move || {
        let text = pdf_buffer_text(&worker_name, &raw, file_bytes);
        // 受け口が落ちていれば送信は失敗する。それで正しい (タブを閉じた後)
        let _ = tx.send(text);
    });
    match rx.recv_timeout(PDF_SYNC_BUDGET) {
        Ok(text) => (text, None),
        Err(_) => (
            pdf_loading_text(name, file_bytes),
            Some(PdfJob {
                rx,
                name: name_owned,
                file_bytes,
            }),
        ),
    }
}

/// エディタ本文の折り返し幅: ON なら利用可能幅、OFF なら無限 (横スクロール)。
pub fn wrap_max_width(word_wrap: bool, avail: f32) -> f32 {
    if word_wrap {
        avail
    } else {
        f32::INFINITY
    }
}

/// バイト数の人向け表示 (画像ビューアのステータス行用)。
pub fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KB", f / K)
    } else {
        format!("{:.1} MB", f / (K * K))
    }
}

/// 空白文字の可視化: スペースを「·」、タブを「→」へ置き換えた LayoutJob を返す。
///
/// TextEdit のカーソルは char 単位で galley と対応付くため、**1 文字は必ず
/// 1 文字へ**置き換える (バイト数は変わってよいが char 数は変えてはいけない)。
/// 置き換えた文字は dim 色の専用セクションに割り、非空白部分は元の
/// ハイライト色のまま残す。
/// 注: タブは通常タブストップ幅へ展開されるが、置換後は「→」1 グリフ幅に
/// なるため、表示切替でタブ由来の桁位置は変わり得る (既知のトレードオフ)。
pub fn whitespace_layout_job(
    job: eframe::egui::text::LayoutJob,
    dim: eframe::egui::Color32,
) -> eframe::egui::text::LayoutJob {
    use eframe::egui::text::LayoutSection;
    let mut text = String::with_capacity(job.text.len() + 16);
    let mut sections: Vec<LayoutSection> = Vec::with_capacity(job.sections.len() * 2);
    for sec in &job.sections {
        let src = &job.text[sec.byte_range.clone()];
        // leading_space は最初のサブセクションだけが引き継ぐ
        let mut leading = sec.leading_space;
        let mut run_start = text.len();
        let mut run_ws: Option<bool> = None;
        let flush = |sections: &mut Vec<LayoutSection>,
                         start: usize,
                         end: usize,
                         ws: bool,
                         leading: &mut f32| {
            if end > start {
                let mut format = sec.format.clone();
                if ws {
                    format.color = dim;
                }
                sections.push(LayoutSection {
                    leading_space: std::mem::take(leading),
                    byte_range: start..end,
                    format,
                });
            }
        };
        for ch in src.chars() {
            let is_ws = ch == ' ' || ch == '\t';
            if run_ws != Some(is_ws) {
                flush(
                    &mut sections,
                    run_start,
                    text.len(),
                    run_ws == Some(true),
                    &mut leading,
                );
                run_start = text.len();
                run_ws = Some(is_ws);
            }
            text.push(match ch {
                ' ' => '·',
                '\t' => '→',
                _ => ch,
            });
        }
        flush(
            &mut sections,
            run_start,
            text.len(),
            run_ws == Some(true),
            &mut leading,
        );
    }
    let mut out = job;
    out.text = text;
    out.sections = sections;
    out
}

// ===========================================================================
// 行番号の付け替え (折りたたみ・ブックマークの編集耐性)
// ===========================================================================

/// 行の挿入 / 削除に追随して行番号を付け替える。
///
/// `at` 行目に `delta` 行が挿入された (`delta > 0`) / 削除された
/// (`delta < 0`) ときの、`line` の新しい位置を返す。削除された行そのものは
/// `None` (印が消える)。`at` より前の行は動かない。
pub fn remap_line(line: usize, at: usize, delta: isize) -> Option<usize> {
    if line < at {
        return Some(line);
    }
    match delta.cmp(&0) {
        std::cmp::Ordering::Equal => Some(line),
        std::cmp::Ordering::Greater => Some(line + delta as usize),
        std::cmp::Ordering::Less => {
            let removed = delta.unsigned_abs();
            if line < at + removed {
                None
            } else {
                Some(line - removed)
            }
        }
    }
}

// ===========================================================================
// 折りたたみ状態 (バッファごと)
// ===========================================================================

/// ガターに出す折りたたみの印。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldMarker {
    /// 畳める範囲の先頭で、いまは開いている (▼)。
    Open,
    /// 畳んである (▶)。
    Closed,
}

/// バッファ 1 個ぶんの折りたたみ状態。
///
/// **契約**: 本文を書き換えたら必ず [`FoldState::refresh`] を呼ぶこと
/// (中で本文ハッシュを見て、変わったときだけ再計算する)。再計算のたびに、
/// 「もう範囲の先頭ではなくなった」畳みは自動的に捨てられる。
/// 行の挿入 / 削除が分かっているときは、先に [`FoldState::shift_lines`] を
/// 呼ぶと畳んだ状態が編集を跨いで生き残る。
#[derive(Default)]
pub struct FoldState {
    /// 畳んである範囲の開始行 (0 始まり)。
    folded: HashSet<usize>,
    /// 直近に計算した範囲 (開始行の昇順)。
    ranges: Vec<FoldRange>,
    /// `ranges` を計算した本文 + 言語のハッシュ。
    hash: Option<u64>,
}

#[allow(dead_code)]
impl FoldState {
    /// 本文が変わっていれば範囲を計算し直す。再計算したら `true`。
    ///
    /// 再計算後、「開始行がもう範囲の先頭ではない」畳みは捨てる
    /// (行がずれて別のコードを隠してしまうより、開いた方が安全)。
    pub fn refresh(&mut self, text: &str, lang: &str) -> bool {
        let key = combine_hash(hash_str(text), hash_str(lang));
        if self.hash == Some(key) {
            return false;
        }
        self.hash = Some(key);
        self.ranges = fold_ranges(text, lang);
        if !self.folded.is_empty() {
            let starts: HashSet<usize> = self.ranges.iter().map(|r| r.start_line).collect();
            self.folded.retain(|l| starts.contains(l));
        }
        true
    }

    /// 直近に計算した範囲 (開始行の昇順)。
    pub fn ranges(&self) -> &[FoldRange] {
        &self.ranges
    }

    /// 畳んである範囲の開始行の集合。
    pub fn folded(&self) -> &HashSet<usize> {
        &self.folded
    }

    /// この行から始まる範囲。
    pub fn range_at(&self, line: usize) -> Option<FoldRange> {
        self.ranges.iter().copied().find(|r| r.start_line == line)
    }

    /// この行が畳める行か (ガターに ▼ を出すか)。
    pub fn is_foldable(&self, line: usize) -> bool {
        self.ranges.iter().any(|r| r.start_line == line)
    }

    /// この行が畳んであるか。
    pub fn is_folded(&self, line: usize) -> bool {
        self.folded.contains(&line)
    }

    /// ガターの印。畳めない行は `None`。
    pub fn marker(&self, line: usize) -> Option<FoldMarker> {
        if !self.is_foldable(line) {
            return None;
        }
        Some(if self.is_folded(line) {
            FoldMarker::Closed
        } else {
            FoldMarker::Open
        })
    }

    /// 折りたたみを切り替える。切り替わったら `true`
    /// (畳めない行を渡したときは何もせず `false`)。
    pub fn toggle_fold(&mut self, line: usize) -> bool {
        if !self.is_foldable(line) {
            return false;
        }
        if !self.folded.remove(&line) {
            self.folded.insert(line);
        }
        true
    }

    /// 畳む (すでに畳んであれば何もしない)。
    pub fn fold(&mut self, line: usize) -> bool {
        self.is_foldable(line) && self.folded.insert(line)
    }

    /// 開く。
    pub fn unfold(&mut self, line: usize) -> bool {
        self.folded.remove(&line)
    }

    /// すべて畳む。
    pub fn fold_all(&mut self) {
        self.folded = self.ranges.iter().map(|r| r.start_line).collect();
    }

    /// すべて開く。
    pub fn unfold_all(&mut self) {
        self.folded.clear();
    }

    /// 入れ子の深さ `level` (1 始まり = いちばん外側) の範囲だけを畳んだ
    /// 状態にする。VS Code の「レベル N まで折りたたむ」相当。
    pub fn fold_level(&mut self, level: usize) {
        self.folded.clear();
        if level == 0 {
            return;
        }
        for (r, d) in self.ranges_with_depth() {
            if d == level {
                self.folded.insert(r.start_line);
            }
        }
    }

    /// 各範囲の入れ子の深さ (1 始まり)。範囲は開始行の昇順。
    pub fn ranges_with_depth(&self) -> Vec<(FoldRange, usize)> {
        let mut stack: Vec<FoldRange> = Vec::new();
        let mut out = Vec::with_capacity(self.ranges.len());
        for r in self.ranges.iter().copied() {
            while stack.last().is_some_and(|t| t.end_line < r.start_line) {
                stack.pop();
            }
            out.push((r, stack.len() + 1));
            stack.push(r);
        }
        out
    }

    /// 畳んだ結果この行が隠れるか。
    pub fn is_line_hidden(&self, line: usize) -> bool {
        self.folded
            .iter()
            .filter_map(|s| self.range_at(*s))
            .any(|r| r.hides(line))
    }

    /// 隠れている行の区間 `(先頭, 末尾)` を、重なりをまとめて昇順で返す。
    /// 行を舐めて描く側はこれを使うと O(行数) で済む。
    pub fn hidden_spans(&self) -> Vec<(usize, usize)> {
        let mut v: Vec<(usize, usize)> = self
            .folded
            .iter()
            .filter_map(|s| self.range_at(*s))
            .map(|r| (r.start_line + 1, r.end_line))
            .collect();
        v.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(v.len());
        for (s, e) in v {
            match out.last_mut() {
                Some(last) if s <= last.1 + 1 => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        out
    }

    /// `line` 以降で最初に表示される行。全部隠れていれば行数を返す。
    pub fn first_visible_from(&self, line: usize, line_count: usize) -> usize {
        let mut l = line;
        while l < line_count && self.is_line_hidden(l) {
            l += 1;
        }
        l
    }

    /// 行の挿入 / 削除に合わせて畳んだ位置をずらす
    /// ([`remap_line`] と同じ規約)。範囲キャッシュは無効化する。
    pub fn shift_lines(&mut self, at: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        self.folded = self
            .folded
            .iter()
            .filter_map(|l| remap_line(*l, at, delta))
            .collect();
        self.hash = None;
    }
}

// ===========================================================================
// ブックマーク (バッファごと)
// ===========================================================================

/// 行ブックマーク。`BTreeSet` なので next / prev が順序どおりに取れる。
#[derive(Default)]
pub struct Bookmarks {
    lines: BTreeSet<usize>,
}

#[allow(dead_code)]
impl Bookmarks {
    /// 印を付け外しする。付いたら `true`。
    pub fn toggle(&mut self, line: usize) -> bool {
        if self.lines.remove(&line) {
            false
        } else {
            self.lines.insert(line);
            true
        }
    }

    pub fn is_marked(&self, line: usize) -> bool {
        self.lines.contains(&line)
    }

    pub fn clear_all(&mut self) {
        self.lines.clear();
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 昇順のイテレータ。
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.lines.iter().copied()
    }

    /// `line` より後の最初の印。無ければ先頭へ回り込む。
    pub fn next_after(&self, line: usize) -> Option<usize> {
        self.lines
            .range(line + 1..)
            .next()
            .or_else(|| self.lines.iter().next())
            .copied()
    }

    /// `line` より前の最後の印。無ければ末尾へ回り込む。
    pub fn prev_before(&self, line: usize) -> Option<usize> {
        self.lines
            .range(..line)
            .next_back()
            .or_else(|| self.lines.iter().next_back())
            .copied()
    }

    /// 行の挿入 / 削除に合わせて印をずらす ([`remap_line`] と同じ規約)。
    /// 削除された行の印は消える。
    pub fn shift_lines(&mut self, at: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        self.lines = self
            .lines
            .iter()
            .filter_map(|l| remap_line(*l, at, delta))
            .collect();
    }
}

// ===========================================================================
// 閉じたタブを開き直す
// ===========================================================================

/// 閉じたタブの記録 (Ctrl+Shift+T で開き直すのに要るものだけ)。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct ClosedTab {
    pub path: PathBuf,
    pub title: String,
    /// 閉じた時点のキャレット位置 (行, 桁) — 1 始まり。
    pub cursor: (usize, usize),
    /// 閉じた時点のスクロール量 (px)。分からなければ 0.0。
    pub scroll: f32,
}

/// 閉じたタブの履歴上限。
pub const CLOSED_TABS_CAP: usize = 20;

/// 直近に閉じたタブの LRU。新しいものが先頭。
///
/// 同じパスを二度閉じたときは古い記録を捨てて 1 件にまとめる
/// (履歴が同じファイルで埋まらないように)。
pub struct ClosedTabs {
    items: VecDeque<ClosedTab>,
    cap: usize,
}

impl Default for ClosedTabs {
    fn default() -> Self {
        Self::with_capacity(CLOSED_TABS_CAP)
    }
}

#[allow(dead_code)]
impl ClosedTabs {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            items: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// 閉じたタブを積む。上限を超えたら**いちばん古いもの**を捨てる。
    pub fn push_closed(&mut self, tab: ClosedTab) {
        self.items.retain(|t| t.path != tab.path);
        self.items.push_front(tab);
        while self.items.len() > self.cap {
            self.items.pop_back();
        }
    }

    /// いちばん最近閉じたタブを取り出す (履歴からは消える)。
    pub fn pop_closed(&mut self) -> Option<ClosedTab> {
        self.items.pop_front()
    }

    /// 取り出さずに覗く。
    pub fn peek(&self) -> Option<&ClosedTab> {
        self.items.front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

// ===========================================================================
// CSV / TSV のテーブル表示
// ===========================================================================

/// テーブル表示の対象にする拡張子 (小文字)。
pub const TABLE_EXTS: &[&str] = &["csv", "tsv", "tab"];

/// 区切り文字の候補。**上にあるものほど優先** (同点のときの決着用)。
pub const TABLE_DELIMITERS: &[char] = &[',', '\t', ';'];

/// テーブル表示で読む行数の上限。数十万行の CSV でも UI を止めない。
pub const TABLE_MAX_ROWS: usize = 50_000;

/// 区切り文字の推定で覗くバイト数。
const TABLE_SNIFF_BYTES: usize = 64 * 1024;

/// 表として解釈した結果。UI はこれをそのままグリッドに流し込める。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct TableView {
    /// 実際に使った区切り文字。
    pub delimiter: char,
    /// 先頭行 (見出し)。空ファイルなら空。
    pub headers: Vec<String>,
    /// 見出しを除いたデータ行。**行ごとに列数が違いうる** (ragged)。
    pub rows: Vec<Vec<String>>,
    /// 全行を通じた最大列数。UI はこの数だけ列を用意し、足りない
    /// セルは空として描く (添字アクセスで panic させないため)。
    pub columns: usize,
    /// 行数上限で打ち切ったか。
    pub truncated: bool,
}

/// 先頭の BOM (U+FEFF) を落とす。Excel が書く CSV には必ず付いてくる。
pub fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// 拡張子がテーブル表示の対象か。
#[allow(dead_code)]
pub fn is_table_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            TABLE_EXTS.iter().any(|x| *x == e)
        })
        .unwrap_or(false)
}

/// 区切り文字を推定する。
///
/// 候補ごとに先頭を実際に解析して、「見出しと同じ列数の行がどれだけ続くか」
/// で採点する。引用の中の区切り文字は解析器が食べるので数に入らない。
/// どれも表に見えなければ `,` を返す。
pub fn detect_delimiter(text: &str) -> char {
    let s = strip_bom(text);
    let head = s
        .char_indices()
        .find(|(i, _)| *i >= TABLE_SNIFF_BYTES)
        .map(|(i, _)| &s[..i])
        .unwrap_or(s);
    let mut best = (0usize, TABLE_DELIMITERS[0]);
    for d in TABLE_DELIMITERS.iter().copied() {
        let t = parse_table_with(head, d, 20);
        if t.headers.len() < 2 {
            continue;
        }
        let consistent = t
            .rows
            .iter()
            .filter(|r| r.len() == t.headers.len())
            .count();
        let score = consistent * 1000 + t.headers.len();
        if score > best.0 {
            best = (score, d);
        }
    }
    best.1
}

/// 区切り文字を推定してから表として解析する。
pub fn parse_table(text: &str, max_rows: usize) -> TableView {
    parse_table_with(text, detect_delimiter(text), max_rows)
}

/// 区切り文字を指定して表として解析する (RFC 4180 準拠 + 寛容)。
///
/// - 引用フィールド `"..."` の中の区切り文字・改行はそのまま中身になる。
/// - 引用の中の `""` は `"` 1 文字。
/// - 行末は LF / CRLF のどちらでもよい。
/// - 列数が揃っていなくてもそのまま返す (**絶対に panic しない**)。
/// - `max_rows` はデータ行 (見出しを除く) の上限。
pub fn parse_table_with(text: &str, delimiter: char, max_rows: usize) -> TableView {
    let s = strip_bom(text);
    let cap = max_rows.saturating_add(1);
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut rec: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut truncated = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if in_quotes {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    it.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        if c == '"' {
            in_quotes = true;
            continue;
        }
        if c == delimiter {
            rec.push(std::mem::take(&mut field));
            continue;
        }
        if c == '\n' || c == '\r' {
            if c == '\r' && it.peek() == Some(&'\n') {
                it.next();
            }
            rec.push(std::mem::take(&mut field));
            // 空行 (フィールド 1 個で中身なし) は行として数えない
            if !(rec.len() == 1 && rec[0].is_empty()) {
                records.push(std::mem::take(&mut rec));
            } else {
                rec.clear();
            }
            if records.len() >= cap {
                truncated = it.peek().is_some();
                break;
            }
            continue;
        }
        field.push(c);
    }
    if !field.is_empty() || !rec.is_empty() {
        rec.push(field);
        if !(rec.len() == 1 && rec[0].is_empty()) {
            records.push(rec);
        }
    }
    let mut iter = records.into_iter();
    let headers = iter.next().unwrap_or_default();
    let rows: Vec<Vec<String>> = iter.collect();
    let columns = rows
        .iter()
        .map(|r| r.len())
        .chain(std::iter::once(headers.len()))
        .max()
        .unwrap_or(0);
    TableView {
        delimiter,
        headers,
        rows,
        columns,
        truncated,
    }
}

impl Buffer {
    pub fn dirty(&self) -> bool {
        hash_str(&self.text) != self.saved_hash
    }

    /// このタブが読み取り専用か。種類 (画像 / PDF / 差分) と
#[allow(dead_code)]
    /// 巨大ファイルモードの**どちらか**が読み取り専用ならそう扱う。
    pub fn read_only(&self) -> bool {
        self.kind.read_only() || self.large.read_only
    }

    /// シンタックスハイライトを行ってよいか (巨大ファイルでは false)。
#[allow(dead_code)]
    pub fn highlight_enabled(&self) -> bool {
        self.large.highlight
    }

    /// 巨大ファイルのバナーを出すべきならそのサイズ。
#[allow(dead_code)]
    pub fn large_file_banner(&self) -> Option<u64> {
        self.large.active.then_some(self.large.bytes)
    }

    /// 折りたたみ範囲を本文に追随させる。再計算したら `true`。
#[allow(dead_code)]
    pub fn refresh_folds(&mut self) -> bool {
        let (text, lang) = (&self.text, &self.lang);
        self.folds.refresh(text, lang)
    }

    /// 本文を表として解析して `table` に載せる (テーブル表示の ON)。
#[allow(dead_code)]
    pub fn build_table(&mut self) -> &TableView {
        let t = parse_table(&self.text, TABLE_MAX_ROWS);
        self.table.insert(t)
    }

    /// テーブル表示を降ろす。
#[allow(dead_code)]
    pub fn drop_table(&mut self) {
        self.table = None;
    }

    /// 本文を**読み込んだときと同じ文字コードで**ディスクへ書く。
    ///
    /// 元の符号化で表せない文字 (CP932 のファイルに絵文字を足した等) があるときは
    /// 文字を落とさず UTF-8 で書き、`Ok(true)` を返す (呼び出し側が知らせる)。
    /// バッファの `encoding` もそのとき UTF-8 へ更新するので、
    /// 次の保存からは変換を試みない。
    pub fn write_to(&mut self, path: &Path) -> std::io::Result<bool> {
        let (bytes, used) = crate::textenc::encode_bytes(&self.text, self.encoding);
        std::fs::write(path, bytes)?;
        let promoted = used != self.encoding;
        self.encoding = used;
        Ok(promoted)
    }
}

pub struct Editor {
    pub buffers: Vec<Buffer>,
    pub active: Option<usize>,
    next_id: u64,
    /// (line, col) of the active buffer's cursor, 1-based.
    pub cursor: (usize, usize),
    untitled_count: u64,
    /// 直近に閉じたタブ (Ctrl+Shift+T で開き直す)。
    pub closed_tabs: ClosedTabs,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            buffers: Vec::new(),
            active: None,
            next_id: 1,
            cursor: (1, 1),
            untitled_count: 0,
            closed_tabs: ClosedTabs::default(),
        }
    }

    pub fn new_untitled(&mut self) {
        self.untitled_count += 1;
        let id = self.next_id;
        self.next_id += 1;
        self.buffers.push(Buffer {
            id,
            path: None,
            kind: BufferKind::File,
            title: format!("untitled-{}", self.untitled_count),
            text: String::new(),
            saved_hash: hash_str(""),
            lang: "Plain Text".into(),
            // 新規ファイルは UTF-8 で作る (既定)
            encoding: crate::textenc::Encoding::Utf8,
            cache: None,
            gutter: None,
            disk_mtime: None,
            conflict_notified: None,
            image: None,
            pdf_job: None,
            folds: FoldState::default(),
            bookmarks: Bookmarks::default(),
            large: LargeFileMode::default(),
            table: None,
            preview: None,
            minimap: None,
        });
        self.active = Some(self.buffers.len() - 1);
    }

    /// 専用ビューアのタブ (16 進 / メディア / 書庫) を 1 枚積んでアクティブにする。
    ///
    /// 本文は**必ず空**にする。`dirty()` が常に false になるので、保存・
    /// 自動保存・検索・置換・差分のどの経路もこのタブを素通りする
    /// (`kind.read_only()` との二重の防御)。`path` は `Some` のままにして
    /// 外部変更の mtime 監視だけは効かせる (画像・PDF タブと同じ流儀)。
    fn push_preview_tab(&mut self, canon: &Path, kind: BufferKind, preview: Option<PreviewDoc>) {
        let title = canon
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "???".into());
        let id = self.next_id;
        self.next_id += 1;
        let mtime = disk_mtime(canon);
        self.buffers.push(Buffer {
            id,
            path: Some(canon.to_path_buf()),
            kind,
            title,
            text: String::new(),
            saved_hash: hash_str(""),
            lang: "Plain Text".into(),
            encoding: crate::textenc::Encoding::Utf8,
            cache: None,
            gutter: None,
            disk_mtime: mtime,
            conflict_notified: None,
            image: None,
            pdf_job: None,
            folds: FoldState::default(),
            bookmarks: Bookmarks::default(),
            large: LargeFileMode::default(),
            table: None,
            preview,
            minimap: None,
        });
        self.active = Some(self.buffers.len() - 1);
    }

    /// Open a file (or focus it if already open).
    /// 既に開いていたタブをディスクから読み直したときだけ Ok(true)。
    pub fn open(&mut self, path: &Path, hl: &Highlighter) -> Result<bool, String> {
        // ルート (file_tree::normalize_roots) と同じ形に揃える。素のパスに
        // しておかないと Windows で「どのルートのファイルか」の前方一致が外れる。
        let canon = crate::pathx::canonical(path);
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| b.path.as_deref() == Some(canon.as_path()))
        {
            self.active = Some(i);
            // 外部(エージェント等)がファイルを書き換えていたら、
            // 未保存の編集が無い場合に限りディスクの内容へ読み直す
            return Ok(self.reload_from_disk(i));
        }

        let file_bytes = std::fs::metadata(&canon).map(|m| m.len()).unwrap_or(0);

        // ── 中身を丸ごとメモリへ載せずに済む種類を、サイズ上限より先に振り分ける ──
        //
        // 動画・音声はヘッダ (と moov box) だけ、書庫は末尾のセントラル
        // ディレクトリだけを読む。だから `MAX_OPEN_BYTES` の対象外にできる
        // (数 GB の mp4 を「大きすぎます」で断らない)。
        if crate::preview::is_media_path(&canon) {
            let doc = build_preview(BufferKind::Media, &canon);
            self.push_preview_tab(&canon, BufferKind::Media, doc);
            return Ok(false);
        }
        if crate::preview::is_archive_path(&canon) {
            if let Some(doc) = build_preview(BufferKind::Archive, &canon) {
                self.push_preview_tab(&canon, BufferKind::Archive, Some(doc));
                return Ok(false);
            }
            // 拡張子が嘘で ZIP ではなかった → 下の共通経路で 16 進ダンプへ落ちる
        }

        // ── 先頭だけ読んで「テキストか」を**中身で**決める ──
        //
        // 拡張子の一覧で網を張っても抜けは必ず出る (sqlite / 実行ファイル /
        // 未知の独自形式)。抜けたものが `textenc::decode_bytes` に落ちると
        // バイナリの文字化けが本文になるので、ここで最後の受け皿を張る。
        let head = read_head(&canon)?;
        // 画像・PDF は専用の抽出を持つので中身判定から外す (どちらもバイナリ)
        let has_viewer = is_image_path(&canon) || is_pdf_path(&canon);
        if !has_viewer && crate::preview::looks_binary(&head) {
            let doc = build_preview(BufferKind::Hex, &canon);
            self.push_preview_tab(&canon, BufferKind::Hex, doc);
            return Ok(false);
        }

        // 巨大ファイルの扱い: 読み込みは UI スレッドの同期 IO なので、
        // 大きいものは「読み取り専用 + ハイライト無効」に落として開き、
        // メモリに載らない規模だけを断る (`open_decision` が決める)。
        let large = match open_decision(file_bytes) {
            OpenDecision::Refuse { bytes, limit } => {
                return Err(format!(
                    "ファイルが大きすぎます ({} > {})",
                    human_bytes(bytes),
                    human_bytes(limit)
                ));
            }
            OpenDecision::Open(mode) => mode,
        };
        // UTF-8 決め打ちで読むと CP932 (Shift_JIS) のファイルが開けないので、
        // バイト列で読んで textenc に判定させる (BOM / UTF-16 もここで拾う)。
        let raw = std::fs::read(&canon).map_err(|e| format!("開けませんでした: {e}"))?;
        // 画像は拡張子で振り分けてビューアタブにする (テキストとしてデコード
        // するとバイナリの文字化けが表示されてしまう)。壊れた画像でも
        // panic せず、error 入りの ImageDoc としてタブに出す。
        if is_image_path(&canon) {
            let title = canon
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "???".into());
            let id = self.next_id;
            self.next_id += 1;
            let mtime = disk_mtime(&canon);
            let doc = decode_image_doc(&raw, raw.len() as u64);
            self.buffers.push(Buffer {
                id,
                path: Some(canon),
                kind: BufferKind::Image,
                title,
                // 本文は空にする: dirty() が常に false になり、保存・自動保存・
                // 検索のどの経路でも画像タブは素通りされる
                text: String::new(),
                saved_hash: hash_str(""),
                lang: "Plain Text".into(),
                encoding: crate::textenc::Encoding::Utf8,
                cache: None,
                gutter: None,
                disk_mtime: mtime,
                conflict_notified: None,
                image: Some(doc),
                pdf_job: None,
                folds: FoldState::default(),
                bookmarks: Bookmarks::default(),
                large: LargeFileMode::default(),
                table: None,
                preview: None,
                minimap: None,
            });
            self.active = Some(self.buffers.len() - 1);
            return Ok(false);
        }
        // PDF は抽出したテキストを読み取り専用タブに載せる。バイナリを
        // textenc に流すと文字化けが本文になってしまうため、画像と同じく
        // 拡張子で先に振り分ける。抽出失敗・暗号化・破損でも panic せず、
        // 「読めない理由」を本文にしてタブは必ず開く。
        if is_pdf_path(&canon) {
            let title = canon
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "???".into());
            let id = self.next_id;
            self.next_id += 1;
            let mtime = disk_mtime(&canon);
            // 小さい PDF は 250ms 以内に終わるのでそのまま本文が入る。
            // 間に合わない大物はワーカーへ預け、「読み込み中…」を出しておく
            // (`poll_pdf_jobs` が完成本文へ差し替える)。
            let file_bytes = raw.len() as u64;
            let (text, job) = start_pdf_extraction(&title, raw, file_bytes);
            self.buffers.push(Buffer {
                id,
                path: Some(canon),
                kind: BufferKind::Pdf,
                title,
                // saved_hash を本文と一致させて dirty() を常に false にする。
                // read_only() との二重の防御で、抽出テキストが元の PDF へ
                // 書き戻されることはない。
                saved_hash: hash_str(&text),
                text,
                lang: "Plain Text".into(),
                encoding: crate::textenc::Encoding::Utf8,
                cache: None,
                gutter: None,
                disk_mtime: mtime,
                conflict_notified: None,
                image: None,
                pdf_job: job,
                folds: FoldState::default(),
                bookmarks: Bookmarks::default(),
                large: LargeFileMode::default(),
                table: None,
                preview: None,
                minimap: None,
            });
            self.active = Some(self.buffers.len() - 1);
            return Ok(false);
        }
        let (text, encoding) = crate::textenc::decode_bytes(&raw);
        let lang = hl.lang_for(Some(&canon), &text);
        let title = canon
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "???".into());

        let id = self.next_id;
        self.next_id += 1;
        let mtime = disk_mtime(&canon);
        self.buffers.push(Buffer {
            id,
            path: Some(canon),
            kind: BufferKind::File,
            title,
            saved_hash: hash_str(&text),
            text,
            lang,
            encoding,
            cache: None,
            gutter: None,
            disk_mtime: mtime,
            conflict_notified: None,
            image: None,
            pdf_job: None,
            folds: FoldState::default(),
            bookmarks: Bookmarks::default(),
            large,
            table: None,
            preview: None,
            minimap: None,
        });
        self.active = Some(self.buffers.len() - 1);
        Ok(false)
    }

    /// バッファをディスクの内容で読み直す。読み直したときだけ true。
    /// 未保存の編集があるバッファには触らない。読めない場合(削除等)も何もしない。
    /// ファイルに紐づかないタブを開き、そのバッファ id を返す。
    ///
    /// 同じ `kind` のタブが既にあれば内容を差し替えて使い回す
    /// (同じ PR を二度開いてもタブが増えない)。`path` は必ず `None` なので、
    /// 保存 / LSP / git ガター / セッション復元はいずれもこのタブを素通りする。
    pub fn open_virtual(&mut self, title: String, text: String, kind: BufferKind) -> u64 {
        if let Some(i) = self.buffers.iter().position(|b| b.kind == kind) {
            let b = &mut self.buffers[i];
            b.title = title;
            b.saved_hash = hash_str(&text);
            b.text = text;
            b.cache = None;
            b.gutter = None;
            self.active = Some(i);
            return b.id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.buffers.push(Buffer {
            id,
            path: None,
            kind,
            title,
            saved_hash: hash_str(&text),
            text,
            lang: "Diff".into(),
            // 読み取り専用タブ (PR 差分など) は保存経路を通らない
            encoding: crate::textenc::Encoding::Utf8,
            cache: None,
            gutter: None,
            disk_mtime: None,
            conflict_notified: None,
            image: None,
            pdf_job: None,
            folds: FoldState::default(),
            bookmarks: Bookmarks::default(),
            large: LargeFileMode::default(),
            table: None,
            preview: None,
            minimap: None,
        });
        self.active = Some(self.buffers.len() - 1);
        id
    }

    pub fn reload_from_disk(&mut self, i: usize) -> bool {
        let Some(b) = self.buffers.get_mut(i) else {
            return false;
        };
        let Some(path) = b.path.clone() else {
            return false;
        };
        let m = disk_mtime(&path);
        // 16 進 / メディア / 書庫タブは**丸ごと読まずに**作り直す
        // (`std::fs::read` より先に返す — 数 GB の動画を再読込で吸い込まない)。
        if matches!(
            b.kind,
            BufferKind::Hex | BufferKind::Media | BufferKind::Archive
        ) {
            if m == b.disk_mtime {
                return false;
            }
            b.preview = build_preview(b.kind, &path);
            b.disk_mtime = m;
            b.conflict_notified = None;
            return true;
        }
        let Ok(raw) = std::fs::read(&path) else {
            b.disk_mtime = m;
            return false;
        };
        // 画像タブはピクセルを再デコードする (テキスト経路に流すと文字化けする)。
        // mtime が同じなら再デコードもしない (ツリーで再クリックしただけ等)。
        if b.kind == BufferKind::Image {
            if m == b.disk_mtime {
                return false;
            }
            b.image = Some(decode_image_doc(&raw, raw.len() as u64));
            b.disk_mtime = m;
            b.conflict_notified = None;
            return true;
        }
        // PDF タブも同じく再抽出する。dirty() にならないよう saved_hash を
        // 本文へ合わせ直すのを忘れないこと (合わせないと「未保存の変更あり」
        // 扱いになり、終了時に保存を促されてしまう)。
        if b.kind == BufferKind::Pdf {
            if m == b.disk_mtime {
                return false;
            }
            let file_bytes = raw.len() as u64;
            let (text, job) = start_pdf_extraction(&b.title, raw, file_bytes);
            b.saved_hash = hash_str(&text);
            b.text = text;
            // 走っていた古い抽出は捨てる (受け口を落とせばワーカーの送信は
            // 失敗するだけ)。差し替え後の本文を古い結果で上書きしない
            b.pdf_job = job;
            b.cache = None;
            b.gutter = None;
            b.disk_mtime = m;
            b.conflict_notified = None;
            return true;
        }
        // エージェントが書き換えた結果で符号化が変わることもあるので、毎回判定する
        let (text, encoding) = crate::textenc::decode_bytes(&raw);
        if text == b.text {
            // 内容は同じ(自前の保存・touch 等)。保存済み扱いに同期するだけ
            b.encoding = encoding;
            b.disk_mtime = m;
            b.conflict_notified = None;
            b.saved_hash = hash_str(&text);
            return false;
        }
        if b.dirty() {
            // 未保存の編集は守る。mtime も据え置き、ポーリング側が競合を警告できる
            // ようにする。encoding も据え置く — 再読込を拒否したのに符号化だけ
            // ディスク側へ合わせると、次の保存で本文が意図しない符号に落ちる
            return false;
        }
        b.encoding = encoding;
        b.disk_mtime = m;
        b.conflict_notified = None;
        b.saved_hash = hash_str(&text);
        b.text = text;
        b.cache = None;
        b.gutter = None;
        true
    }

    /// 終わったバックグラウンド PDF 抽出の結果を本文へ差し替える。
    /// 差し替えたら true (呼び出し側の再描画判断用)。待ちはしない。
    ///
    /// 呼び口は `check_external` (app.rs が約 1 秒ごとに叩く)。egui は
    /// 250ms ごとの再描画予約が入っているので、抽出完了から遅くとも
    /// 1 秒強で「読み込み中…」が本文へ変わる。
    pub fn poll_pdf_jobs(&mut self) -> bool {
        let mut changed = false;
        for b in &mut self.buffers {
            if b.kind != BufferKind::Pdf {
                continue;
            }
            let Some(text) = b.pdf_job.as_ref().and_then(|j| j.take()) else {
                continue;
            };
            // 読み取り専用タブなので dirty にしない (saved_hash も合わせる)
            b.saved_hash = hash_str(&text);
            b.text = text;
            b.pdf_job = None;
            b.cache = None;
            b.gutter = None;
            changed = true;
        }
        changed
    }

    /// 全バッファの外部変更を確認する。クリーンなバッファは自動で読み直し、
    /// 未保存の編集と競合したバッファは一度だけ Conflict を報告する。
    pub fn check_external(&mut self) -> Vec<ExternalEvent> {
        // 走り終わった PDF 抽出をここで拾う (専用のポーリングを app.rs へ
        // 足さずに済むよう、既存の 1 秒ポーリングへ相乗りする)
        self.poll_pdf_jobs();
        let mut events = Vec::new();
        for i in 0..self.buffers.len() {
            let Some(path) = self.buffers[i].path.clone() else {
                continue;
            };
            let m = disk_mtime(&path);
            if m == self.buffers[i].disk_mtime {
                continue;
            }
            if self.buffers[i].dirty() {
                let b = &mut self.buffers[i];
                if b.conflict_notified != m {
                    b.conflict_notified = m;
                    events.push(ExternalEvent::Conflict {
                        title: b.title.clone(),
                    });
                }
                continue;
            }
            if self.reload_from_disk(i) {
                events.push(ExternalEvent::Reloaded {
                    index: i,
                    title: self.buffers[i].title.clone(),
                });
            }
        }
        events
    }

    pub fn close(&mut self, i: usize) {
        self.close_with(i, 0.0);
    }

    /// スクロール位置も覚えてタブを閉じる (開き直したときに元の場所へ戻る)。
    ///
    /// ファイルに紐づかないタブ (untitled / PR 差分など) は本文を保持できない
    /// ため履歴に積まない。閉じたタブは `closed_tabs.pop_closed()` で取り出す。
    pub fn close_with(&mut self, i: usize, scroll: f32) {
        if i >= self.buffers.len() {
            return;
        }
        let cursor = if self.active == Some(i) {
            self.cursor
        } else {
            (1, 1)
        };
        if let Some(b) = self.buffers.get(i) {
            if let Some(path) = b.path.clone() {
                let tab = ClosedTab {
                    path,
                    title: b.title.clone(),
                    cursor,
                    scroll,
                };
                self.closed_tabs.push_closed(tab);
            }
        }
        self.buffers.remove(i);
        self.active = if self.buffers.is_empty() {
            None
        } else {
            Some(match self.active {
                Some(a) if a > i => a - 1,
                Some(a) => a.min(self.buffers.len() - 1),
                None => 0,
            })
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;
    use crate::test_util::unique_temp_dir;

    /// 外部変更を mtime 差として確実に検知させる（同一秒内の書き換え対策）。
    fn bump_mtime(path: &Path) {
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let f = std::fs::File::options()
            .append(true)
            .open(path)
            .expect("open for mtime bump");
        f.set_modified(future).expect("set mtime");
    }

    fn open_one(dir: &Path, name: &str, content: &str) -> (Editor, PathBuf, Highlighter) {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write initial file");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        (ed, path, hl)
    }

    #[test]
    fn combine_hash_is_order_sensitive() {
        // XOR と違い、値が入れ替わっただけの組はキャッシュキーが衝突しない
        let samples = [
            (hash_str("rust"), hash_str("python")),
            (hash_str("theme-a"), hash_str("theme-b")),
            (1u64, 2u64),
            (0u64, u64::MAX),
        ];
        for (a, b) in samples {
            assert_ne!(
                combine_hash(a, b),
                combine_hash(b, a),
                "combine_hash({a:#x}, {b:#x}) must depend on argument order"
            );
        }
    }

    #[test]
    fn external_change_reloads_clean_buffer() {
        let dir = unique_temp_dir("zaivern-editor-test", "reload");
        let (mut ed, path, _hl) = open_one(&dir, "a.md", "old");

        std::fs::write(&path, "new").expect("external write");
        bump_mtime(&path);

        let events = ed.check_external();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ExternalEvent::Reloaded { .. }));
        assert_eq!(ed.buffers[0].text, "new");
        assert!(!ed.buffers[0].dirty());

        // 変化が無ければ以後イベントは出ない
        assert!(ed.check_external().is_empty());
    }

    #[test]
    fn external_change_keeps_dirty_buffer_and_warns_once() {
        let dir = unique_temp_dir("zaivern-editor-test", "conflict");
        let (mut ed, path, _hl) = open_one(&dir, "a.md", "old");
        ed.buffers[0].text = "my unsaved edit".into();

        std::fs::write(&path, "agent wrote this").expect("external write");
        bump_mtime(&path);

        let events = ed.check_external();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ExternalEvent::Conflict { .. }));
        assert_eq!(ed.buffers[0].text, "my unsaved edit");

        // 同じ外部変更で二度は警告しない
        assert!(ed.check_external().is_empty());
    }

    #[test]
    fn reopen_reloads_from_disk() {
        let dir = unique_temp_dir("zaivern-editor-test", "reopen");
        let (mut ed, path, hl) = open_one(&dir, "a.md", "old");

        std::fs::write(&path, "new").expect("external write");
        bump_mtime(&path);

        // 既に開いているファイルを開き直す → ディスクの内容へ読み直される
        assert_eq!(ed.open(&path, &hl), Ok(true));
        assert_eq!(ed.buffers.len(), 1);
        assert_eq!(ed.buffers[0].text, "new");
    }

    /// UTF-8 のファイルは今までどおり (符号化の判定が既定を変えていないこと)。
    #[test]
    fn utf8_file_stays_utf8_on_save() {
        let dir = unique_temp_dir("zaivern-editor-test", "utf8");
        let (mut ed, path, _hl) = open_one(&dir, "a.md", "日本語の本文");
        assert_eq!(ed.buffers[0].encoding, crate::textenc::Encoding::Utf8);
        assert_eq!(ed.buffers[0].text, "日本語の本文");

        ed.buffers[0].text.push_str("と追記");
        assert!(!ed.buffers[0].write_to(&path).expect("save"), "格上げは起きない");
        assert_eq!(
            std::fs::read(&path).expect("read back"),
            "日本語の本文と追記".as_bytes(),
            "UTF-8 のまま書かれること"
        );
    }

    /// BOM 付き UTF-8 (Excel の CSV など) は BOM を保ったまま保存する。
    /// BOM を落とすと、そのファイルを読む他のツールが文字化けする側になる。
    #[test]
    fn bom_is_preserved_across_open_and_save() {
        let dir = unique_temp_dir("zaivern-editor-test", "bom");
        let path = dir.join("data.csv");
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice("列,値\n名前,太郎\n".as_bytes());
        std::fs::write(&path, &raw).expect("write bom file");

        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        assert_eq!(ed.buffers[0].encoding, crate::textenc::Encoding::Utf8Bom);
        assert!(!ed.buffers[0].text.starts_with('\u{feff}'), "BOM は本文に混ぜない");

        assert!(!ed.buffers[0].write_to(&path).expect("save"));
        assert_eq!(std::fs::read(&path).expect("read back"), raw);
    }

    /// **この環境の ANSI コードページ**で書かれたファイル (日本語 Windows なら
    /// Shift_JIS) を開いて保存しても、バイト列が変わらないこと。
    /// UTF-8 決め打ちの頃は、そもそも開けずに「開けませんでした」で終わっていた。
    #[cfg(windows)]
    #[test]
    fn legacy_encoded_file_opens_and_saves_unchanged() {
        let dir = unique_temp_dir("zaivern-editor-test", "legacy");
        let path = dir.join("legacy.txt");
        let body = "日本語のログ";
        // 素材は OS のコードページ変換で作る (バイト列を書き下さない)
        let (raw, enc) = crate::textenc::encode_bytes(body, crate::textenc::Encoding::Ansi(
            crate::textenc::os_ansi_code_page(),
        ));
        if !enc.is_legacy() {
            return; // この環境の ANSI では表せない = 試験対象外
        }
        std::fs::write(&path, &raw).expect("write legacy file");

        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false), "UTF-8 でなくても開けること");
        assert_eq!(ed.buffers[0].text, body, "文字化けせず読めること");
        assert_eq!(ed.buffers[0].encoding, enc);

        assert!(!ed.buffers[0].write_to(&path).expect("save"));
        assert_eq!(
            std::fs::read(&path).expect("read back"),
            raw,
            "保存で勝手に UTF-8 へ変えない (他ツールが読めなくなる)"
        );
    }

    /// 元の符号化で表せない文字を足したら、文字を落とさず UTF-8 で保存する。
    #[cfg(windows)]
    #[test]
    fn adding_unrepresentable_text_promotes_to_utf8() {
        let dir = unique_temp_dir("zaivern-editor-test", "promote");
        let path = dir.join("legacy.txt");
        let (raw, enc) = crate::textenc::encode_bytes(
            "本文",
            crate::textenc::Encoding::Ansi(crate::textenc::os_ansi_code_page()),
        );
        if !enc.is_legacy() {
            return;
        }
        std::fs::write(&path, &raw).expect("write legacy file");

        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        ed.buffers[0].text.push_str(" 🚀");

        assert!(ed.buffers[0].write_to(&path).expect("save"), "格上げを知らせる");
        assert_eq!(ed.buffers[0].encoding, crate::textenc::Encoding::Utf8);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "本文 🚀",
            "絵文字を落として保存してはいけない"
        );
    }

    #[test]
    fn identical_disk_content_syncs_without_event() {
        let dir = unique_temp_dir("zaivern-editor-test", "touch");
        let (mut ed, path, _hl) = open_one(&dir, "a.md", "same");

        // 内容は同じで mtime だけ変わった（touch 相当）→ イベント無し
        bump_mtime(&path);
        assert!(ed.check_external().is_empty());
        assert_eq!(ed.buffers[0].text, "same");
    }

    // ─── 画像ビューア ───────────────────────────────────────────

    /// 単色の小さな PNG をディスクへ書く (image クレート同梱の png エンコーダ)。
    fn write_png(path: &Path, w: u32, h: u32) {
        image::RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]))
            .save(path)
            .expect("write png");
    }

    #[test]
    fn image_extension_routing_table() {
        // 画像として開く拡張子 (大文字小文字は問わない)
        for name in [
            "a.png", "a.PNG", "a.jpg", "a.JPEG", "a.jpeg", "a.gif", "a.webp", "a.ico",
            "a.bmp", "dir.d/photo.Png",
        ] {
            assert!(is_image_path(Path::new(name)), "{name} は画像として開く");
        }
        // テキストとして開く拡張子・拡張子なし・隠しファイル
        for name in ["a.rs", "a.txt", "a.md", "a.svg", "Makefile", ".png", "png"] {
            assert!(!is_image_path(Path::new(name)), "{name} は画像扱いしない");
        }
    }

    #[test]
    fn open_png_becomes_image_buffer() {
        let dir = unique_temp_dir("zaivern-editor-test", "img-open");
        let path = dir.join("pic.png");
        write_png(&path, 3, 2);
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        let b = &ed.buffers[0];
        assert_eq!(b.kind, BufferKind::Image);
        assert!(b.kind.read_only(), "画像タブは読み取り専用");
        assert!(b.text.is_empty() && !b.dirty(), "本文は空で dirty にならない");
        let doc = b.image.as_ref().expect("decoded image");
        assert_eq!(doc.error, None);
        assert_eq!(doc.orig_size, (3, 2));
        assert_eq!(doc.size, [3, 2]);
        assert_eq!(doc.rgba.len(), 3 * 2 * 4);
    }

    #[test]
    fn corrupt_image_opens_with_error_not_garbage() {
        let dir = unique_temp_dir("zaivern-editor-test", "img-corrupt");
        let path = dir.join("broken.png");
        std::fs::write(&path, b"\x89PNG not really a png\x00\x01\x02").expect("write");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false), "壊れた画像でも開ける (panic しない)");
        let b = &ed.buffers[0];
        assert_eq!(b.kind, BufferKind::Image);
        assert!(b.image.as_ref().expect("doc").error.is_some(), "読めない旨を持つ");
        assert!(b.text.is_empty(), "文字化けテキストを本文に入れない");
    }

    #[test]
    fn image_external_change_redecodes_pixels() {
        let dir = unique_temp_dir("zaivern-editor-test", "img-reload");
        let path = dir.join("pic.png");
        write_png(&path, 2, 2);
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));

        write_png(&path, 5, 4);
        bump_mtime(&path);
        let events = ed.check_external();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ExternalEvent::Reloaded { .. }));
        let doc = ed.buffers[0].image.as_ref().expect("redecoded");
        assert_eq!(doc.orig_size, (5, 4), "外部変更でピクセルを再デコードする");
    }

    #[test]
    fn image_downscale_cap_decision() {
        // 上限以内はそのまま (縮小しない)
        assert_eq!(image_downscale(8192, 8192, 8192), None);
        assert_eq!(image_downscale(100, 50, 8192), None);
        assert_eq!(image_downscale(0, 0, 8192), None);
        // 上限超えはアスペクト比を保って縮小
        assert_eq!(image_downscale(10000, 5000, 8192), Some((8192, 4096)));
        assert_eq!(image_downscale(5000, 10000, 8192), Some((4096, 8192)));
        // 極端な縦横比でも 1px 未満にならない
        let (nw, nh) = image_downscale(100_000, 2, 8192).expect("resize");
        assert_eq!((nw, nh), (8192, 1));
    }

    #[test]
    fn image_fit_and_zoom_math() {
        // 大きい画像は収まる倍率へ縮小
        assert_eq!(image_fit_scale(400.0, 100.0, 200.0, 200.0), 0.5);
        assert_eq!(image_fit_scale(100.0, 400.0, 200.0, 200.0), 0.5);
        // 小さい画像は引き伸ばさない (等倍が上限)
        assert_eq!(image_fit_scale(100.0, 50.0, 200.0, 200.0), 1.0);
        // 不正入力でも 0 やNaN を返さない
        assert_eq!(image_fit_scale(0.0, 0.0, 200.0, 200.0), 1.0);

        // 段階ズームは 1.25 倍刻みで、上下限にクランプされる
        assert!((image_zoom_step(1.0, 1) - 1.25).abs() < 1e-6);
        assert!((image_zoom_step(1.25, -1) - 1.0).abs() < 1e-6);
        assert_eq!(image_zoom_step(IMAGE_ZOOM_MAX, 1), IMAGE_ZOOM_MAX);
        assert_eq!(image_zoom_step(IMAGE_ZOOM_MIN, -1), IMAGE_ZOOM_MIN);
    }

    // ─── PDF ビューア ────────────────────────────────────────────

    /// 依存なしで最小の有効な PDF を組み立てる (1 ページ 1 行の Helvetica)。
    /// xref のオフセットを実バイト位置から作るので、ページ数を変えても壊れない。
    fn make_pdf(pages: &[&str]) -> Vec<u8> {
        let n = pages.len();
        let font_id = 3 + 2 * n;
        let mut objs: Vec<String> = Vec::new();
        objs.push("<< /Type /Catalog /Pages 2 0 R >>".into());
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
        objs.push(format!(
            "<< /Type /Pages /Kids [{}] /Count {n} >>",
            kids.join(" ")
        ));
        for (i, body) in pages.iter().enumerate() {
            let content_id = 4 + 2 * i;
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {content_id} 0 R >>"
            ));
            let stream = format!("BT /F1 24 Tf 72 700 Td ({body}) Tj ET\n");
            objs.push(format!(
                "<< /Length {} >>\nstream\n{stream}endstream",
                stream.len()
            ));
        }
        objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());

        let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
        let mut offsets: Vec<usize> = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref_off = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn write_pdf(path: &Path, pages: &[&str]) {
        std::fs::write(path, make_pdf(pages)).expect("write pdf");
    }

    #[test]
    fn pdf_extension_routing_table() {
        for name in ["a.pdf", "a.PDF", "a.Pdf", "dir.d/報告書.pDf"] {
            assert!(is_pdf_path(Path::new(name)), "{name} は PDF として開く");
        }
        // 拡張子が違う・無い・紛らわしいものはテキスト/画像のまま
        for name in ["a.pd", "a.pdfx", "a.png", "a.txt", "pdf", ".pdf.txt", "Makefile"] {
            assert!(!is_pdf_path(Path::new(name)), "{name} は PDF 扱いしない");
        }
        // 画像経路と食い合わない
        assert!(!is_image_path(Path::new("a.pdf")));
    }

    #[test]
    fn open_pdf_becomes_readonly_text_buffer() {
        let dir = unique_temp_dir("zaivern-editor-test", "pdf-open");
        let path = dir.join("hello.pdf");
        write_pdf(&path, &["Hello Zaivern"]);
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        let b = &ed.buffers[0];
        assert_eq!(b.kind, BufferKind::Pdf);
        assert!(b.kind.read_only(), "PDF タブは読み取り専用");
        assert!(!b.dirty(), "開いた直後に dirty にならない");
        assert!(b.text.contains("Hello Zaivern"), "本文: {}", b.text);
        assert!(b.text.contains("hello.pdf"), "ヘッダにファイル名");
        assert!(b.text.contains("1 ページ"), "ヘッダにページ数");
        assert!(b.text.contains("── ページ 1 / 1 ──"), "ページ区切り");
    }

    #[test]
    fn open_multipage_pdf_numbers_every_page() {
        let dir = unique_temp_dir("zaivern-editor-test", "pdf-pages");
        let path = dir.join("multi.pdf");
        write_pdf(&path, &["Page One", "Page Two", "Page Three"]);
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        let t = &ed.buffers[0].text;
        assert!(t.contains("3 ページ"), "ページ総数: {t}");
        for i in 1..=3 {
            assert!(t.contains(&format!("── ページ {i} / 3 ──")), "区切り {i}: {t}");
        }
        for body in ["Page One", "Page Two", "Page Three"] {
            assert!(t.contains(body), "{body} が本文にある");
        }
        // ページ順が保たれている
        let (a, b) = (t.find("Page One").unwrap(), t.find("Page Three").unwrap());
        assert!(a < b, "ページ順");
    }

    #[test]
    fn corrupt_pdf_opens_with_readable_message() {
        let dir = unique_temp_dir("zaivern-editor-test", "pdf-corrupt");
        let path = dir.join("broken.pdf");
        // %PDF ヘッダだけ本物でオブジェクトはでたらめ (暗号化/破損の代表)
        let mut junk = b"%PDF-1.7\n".to_vec();
        junk.extend((0u16..4096).map(|i| (i.wrapping_mul(7) ^ 0x5a) as u8));
        std::fs::write(&path, &junk).expect("write");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false), "壊れた PDF でも panic せず開ける");
        let b = &ed.buffers[0];
        assert_eq!(b.kind, BufferKind::Pdf);
        assert!(!b.dirty(), "壊れた PDF でも dirty にならない");
        assert!(
            b.text.contains("テキストを抽出できません"),
            "読める説明が入る: {}",
            b.text
        );
        // バイナリの文字化けを本文に流し込んでいない
        assert!(!b.text.contains('\u{fffd}'), "置換文字を含まない");
    }

    #[test]
    fn empty_and_garbage_bytes_never_panic() {
        // 空・テキスト・NUL 混じり — どれもメッセージ入りの本文になるだけ
        for raw in [&b""[..], b"not a pdf at all", b"\x00\x01\x02\xff\xfe"] {
            let t = pdf_buffer_text("x.pdf", raw, raw.len() as u64);
            assert!(t.contains("x.pdf"), "ヘッダは必ず付く");
            assert!(t.contains("テキストを抽出できません"), "説明が入る: {t}");
        }
    }

    #[test]
    fn pdf_size_cap_skips_extraction() {
        // 上限超えは抽出せず、理由を本文にする (中身は読まないので raw は空でよい)
        let t = pdf_buffer_text("huge.pdf", b"", PDF_MAX_BYTES + 1);
        assert!(t.contains("大きすぎる"), "上限超えの説明: {t}");
        assert!(t.contains("huge.pdf"));
        assert!(!t.contains("── ページ"), "ページ本文は組み立てない");
        // 上限ちょうどは通常経路 (抽出を試みる)
        let t = pdf_buffer_text("edge.pdf", b"", PDF_MAX_BYTES);
        assert!(!t.contains("大きすぎる"), "境界値は抽出を試みる: {t}");
        // open() の 50 MB 制限より小さくないと、この分岐へ到達できない
        assert!(PDF_MAX_BYTES < MAX_OPEN_BYTES, "抽出上限は読み込み上限より小さい");
    }

    #[test]
    fn pdf_external_change_reextracts_and_stays_clean() {
        let dir = unique_temp_dir("zaivern-editor-test", "pdf-reload");
        let path = dir.join("doc.pdf");
        write_pdf(&path, &["Before Edit"]);
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        assert!(ed.buffers[0].text.contains("Before Edit"));

        write_pdf(&path, &["After Edit", "Second Page"]);
        bump_mtime(&path);
        let events = ed.check_external();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ExternalEvent::Reloaded { .. }));
        let b = &ed.buffers[0];
        assert!(b.text.contains("After Edit"), "再抽出される: {}", b.text);
        assert!(b.text.contains("2 ページ"), "ページ数も更新される");
        assert!(!b.dirty(), "再抽出しても dirty にならない");
    }

    // ── ユニバーサルプレビュー (16 進 / メディア / 書庫) ─────────────
    //
    // どれも「開いて壊れないこと」と「テキスト経路へ落ちないこと」を見る。
    // サンプルは `preview::testdata` がバイト列で組むので環境に依存しない。

    /// `read_only` / `preview_only` の表。**新しい Kind を足したらここへ 1 行**
    /// 増やすこと。preview_only が漏れると `code_editor_ui` の二重防御を
    /// すり抜けて、TextEdit にバイナリが流れ込む。
    #[test]
    fn buffer_kind_capability_table() {
        let cases: &[(BufferKind, bool, bool)] = &[
            // (種類, 読み取り専用, 専用ビューアで描く)
            (BufferKind::File, false, false),
            (BufferKind::PrDiff { number: 1 }, true, false),
            (BufferKind::RaceDiff { slot: 0 }, true, false),
            (BufferKind::Pdf, true, false),
            (BufferKind::Image, true, true),
            (BufferKind::Hex, true, true),
            (BufferKind::Media, true, true),
            (BufferKind::Archive, true, true),
        ];
        for (kind, ro, preview) in cases {
            assert_eq!(kind.read_only(), *ro, "{kind:?} の read_only");
            assert_eq!(kind.preview_only(), *preview, "{kind:?} の preview_only");
        }
    }

    /// 専用ビューアのタブが満たすべき不変条件をまとめて確かめる。
    fn assert_preview_tab(b: &Buffer, kind: BufferKind) {
        assert_eq!(b.kind, kind);
        assert!(b.kind.read_only(), "{kind:?} タブは読み取り専用");
        assert!(b.kind.preview_only(), "{kind:?} は専用ビューアで描く");
        assert!(b.text.is_empty(), "本文は空 (検索・保存の経路に乗らない)");
        assert!(!b.dirty(), "開いただけで dirty にならない");
        assert!(b.preview.is_some(), "中身が入っている");
        assert!(b.path.is_some(), "mtime 監視のためパスは持つ");
    }

    #[test]
    fn binary_file_falls_back_to_hex_dump() {
        let dir = unique_temp_dir("zaivern-editor-test", "hex-fallback");
        // 拡張子は嘘 (.log) — 判定は**中身**で行われる
        let path = dir.join("mystery.log");
        let mut raw = b"SQLite format 3\x00".to_vec();
        raw.extend_from_slice(&[0u8; 512]);
        std::fs::write(&path, &raw).expect("write binary");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        assert_preview_tab(&ed.buffers[0], BufferKind::Hex);
        let Some(PreviewDoc::Hex(doc)) = ed.buffers[0].preview.as_ref() else {
            panic!("16 進ダンプになっていない");
        };
        assert_eq!(doc.kind, Some("SQLite"), "マジックナンバーで種別を当てる");
        assert_eq!(doc.file_bytes, raw.len() as u64);
        assert!(!doc.truncated);
    }

    #[test]
    fn text_files_never_fall_into_the_hex_dump() {
        let dir = unique_temp_dir("zaivern-editor-test", "hex-regression");
        let hl = Highlighter::new();
        // UTF-8 / CP932 / 空 / BOM 付き — どれも今までどおりテキストで開く
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("utf8.txt", "日本語のテキスト\n".as_bytes().to_vec()),
            ("cp932.txt", vec![0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, 0x0A]),
            ("empty.txt", Vec::new()),
            ("bom.txt", {
                let mut v = vec![0xEF, 0xBB, 0xBF];
                v.extend_from_slice("hello".as_bytes());
                v
            }),
            ("ansi.log", b"\x1b[31mred\x1b[0m\n".to_vec()),
        ];
        for (name, bytes) in cases {
            let path = dir.join(name);
            std::fs::write(&path, &bytes).expect("write text");
            let mut ed = Editor::new();
            assert_eq!(ed.open(&path, &hl), Ok(false));
            assert_eq!(ed.buffers[0].kind, BufferKind::File, "{name} はテキスト");
            assert!(ed.buffers[0].preview.is_none(), "{name} にプレビューは付かない");
        }
    }

    #[test]
    fn hex_dump_caps_what_it_holds_in_memory() {
        let dir = unique_temp_dir("zaivern-editor-test", "hex-cap");
        let path = dir.join("big.bin");
        // 上限より 1 MB 大きいバイナリ
        let n = (crate::preview::HEX_MAX_BYTES + 1024 * 1024) as usize;
        std::fs::write(&path, vec![0u8; n]).expect("write big binary");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        let Some(PreviewDoc::Hex(doc)) = ed.buffers[0].preview.as_ref() else {
            panic!("16 進ダンプになっていない");
        };
        assert_eq!(
            doc.bytes.len() as u64,
            crate::preview::HEX_MAX_BYTES,
            "上限までしか抱えない"
        );
        assert!(doc.truncated, "打ち切ったことを伝える");
        assert_eq!(doc.file_bytes, n as u64, "元のサイズは正しく出す");
    }

    #[test]
    fn video_and_audio_open_as_media_cards() {
        let dir = unique_temp_dir("zaivern-editor-test", "media");
        let hl = Highlighter::new();

        // moov が末尾にある mp4 (ffmpeg の既定) でも解析できる
        let mp4 = dir.join("clip.mp4");
        std::fs::write(&mp4, crate::preview::testdata::make_mp4(600, 6000, 1920, 1080, true))
            .expect("write mp4");
        let mut ed = Editor::new();
        assert_eq!(ed.open(&mp4, &hl), Ok(false));
        assert_preview_tab(&ed.buffers[0], BufferKind::Media);
        let Some(PreviewDoc::Media(doc)) = ed.buffers[0].preview.as_ref() else {
            panic!("メディアカードになっていない");
        };
        assert!(doc.video, "mp4 は映像");
        assert_eq!(doc.kind, Some("MP4"));
        assert_eq!(doc.info.duration_secs, Some(10.0));
        assert_eq!((doc.info.width, doc.info.height), (Some(1920), Some(1080)));

        let wav = dir.join("beep.wav");
        std::fs::write(&wav, crate::preview::testdata::make_wav(2)).expect("write wav");
        let mut ed = Editor::new();
        assert_eq!(ed.open(&wav, &hl), Ok(false));
        assert_preview_tab(&ed.buffers[0], BufferKind::Media);
        let Some(PreviewDoc::Media(doc)) = ed.buffers[0].preview.as_ref() else {
            panic!("メディアカードになっていない");
        };
        assert!(!doc.video, "wav は音声");
        assert_eq!(doc.info.sample_rate, Some(44100));
        assert_eq!(doc.info.duration_secs, Some(2.0));

        // 中身が壊れていても「開けない」にはしない (情報が空になるだけ)
        let broken = dir.join("broken.mp3");
        std::fs::write(&broken, b"not really an mp3").expect("write broken");
        let mut ed = Editor::new();
        assert_eq!(ed.open(&broken, &hl), Ok(false));
        assert_preview_tab(&ed.buffers[0], BufferKind::Media);
    }

    #[test]
    fn zip_opens_as_an_entry_list() {
        let dir = unique_temp_dir("zaivern-editor-test", "archive");
        let path = dir.join("lib.jar");
        std::fs::write(
            &path,
            crate::preview::testdata::make_zip(&[
                ("META-INF/", b""),
                ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
                ("Main.class", b"\xCA\xFE\xBA\xBE\x00\x00\x00\x34"),
            ]),
        )
        .expect("write jar");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        assert_preview_tab(&ed.buffers[0], BufferKind::Archive);
        let Some(PreviewDoc::Archive(doc)) = ed.buffers[0].preview.as_ref() else {
            panic!("書庫一覧になっていない");
        };
        assert_eq!(doc.listing.total, 3);
        assert_eq!(doc.listing.error, None);
        assert!(doc.listing.entries[0].dir, "ディレクトリを見分ける");
        assert_eq!(doc.listing.entries[1].size, 22);
    }

    #[test]
    fn a_zip_extension_that_lies_falls_back_to_hex() {
        let dir = unique_temp_dir("zaivern-editor-test", "fake-zip");
        let path = dir.join("fake.zip");
        std::fs::write(&path, b"\x1F\x8B\x08\x00\x00\x00\x00\x00\x00\x03junk").expect("write gzip");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));
        assert_preview_tab(&ed.buffers[0], BufferKind::Hex);
        let Some(PreviewDoc::Hex(doc)) = ed.buffers[0].preview.as_ref() else {
            panic!("16 進ダンプへ落ちていない");
        };
        assert_eq!(doc.kind, Some("GZIP"), "本当の種別を出す");
    }

    #[test]
    fn preview_tabs_rebuild_on_external_change_and_stay_clean() {
        let dir = unique_temp_dir("zaivern-editor-test", "preview-reload");
        let path = dir.join("box.zip");
        std::fs::write(&path, crate::preview::testdata::make_zip(&[("a.txt", b"1")]))
            .expect("write zip");
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        assert_eq!(ed.open(&path, &hl), Ok(false));

        std::fs::write(
            &path,
            crate::preview::testdata::make_zip(&[("a.txt", b"1"), ("b.txt", b"22")]),
        )
        .expect("rewrite zip");
        bump_mtime(&path);
        let events = ed.check_external();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ExternalEvent::Reloaded { .. }));
        let Some(PreviewDoc::Archive(doc)) = ed.buffers[0].preview.as_ref() else {
            panic!("書庫一覧になっていない");
        };
        assert_eq!(doc.listing.total, 2, "作り直される");
        assert!(!ed.buffers[0].dirty(), "作り直しても dirty にならない");
    }

    #[test]
    fn image_and_pdf_still_win_over_the_hex_fallback() {
        // 画像も PDF もバイナリだが、専用ビューアを持つので 16 進へ落とさない
        let dir = unique_temp_dir("zaivern-editor-test", "viewer-priority");
        let hl = Highlighter::new();
        let png = dir.join("a.png");
        write_png(&png, 4, 4);
        let mut ed = Editor::new();
        assert_eq!(ed.open(&png, &hl), Ok(false));
        assert_eq!(ed.buffers[0].kind, BufferKind::Image);

        let pdf = dir.join("a.pdf");
        write_pdf(&pdf, &["Hello"]);
        let mut ed = Editor::new();
        assert_eq!(ed.open(&pdf, &hl), Ok(false));
        assert_eq!(ed.buffers[0].kind, BufferKind::Pdf);
    }

    #[test]
    fn small_pdf_finishes_inside_sync_budget() {
        // 実測: 実ファイル 22 本の中央値 ≈ 33 ms。小さい PDF は同期で
        // 終わるので「読み込み中…」を経由しない (ジョブは残らない)
        let raw = make_pdf(&["Fast Path"]);
        let n = raw.len() as u64;
        let t = std::time::Instant::now();
        let (text, job) = start_pdf_extraction("fast.pdf", raw, n);
        assert!(t.elapsed() < PDF_SYNC_BUDGET * 2, "同期予算の範囲で戻る");
        assert!(job.is_none(), "小さい PDF は待ちにならない");
        assert!(text.contains("Fast Path"), "本文が入っている: {text}");
        assert!(!text.contains("読み込み中"), "プレースホルダのままにしない");
    }

    #[test]
    fn pending_pdf_shows_placeholder_then_fills_in() {
        // 遅い PDF の代わりにチャネルを直接握って「読み込み中 → 完成」を再現
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let mut ed = Editor::new();
        ed.open_virtual("slow.pdf".into(), String::new(), BufferKind::Pdf);
        let placeholder = pdf_loading_text("slow.pdf", 1234);
        {
            let b = &mut ed.buffers[0];
            b.saved_hash = hash_str(&placeholder);
            b.text = placeholder;
            b.pdf_job = Some(PdfJob::for_test(rx, "slow.pdf", 1234));
        }
        assert!(ed.buffers[0].text.contains("読み込み中"), "まずは待ち表示");
        assert!(!ed.poll_pdf_jobs(), "未完了なら本文を触らない");
        assert!(!ed.buffers[0].dirty(), "待っている間も dirty にならない");

        tx.send("📄 slow.pdf\n1 ページ · 1.2 KB · 読み取り専用\n\n本文だよ\n".into())
            .expect("send");
        assert!(ed.poll_pdf_jobs(), "完了したら差し替える");
        let b = &ed.buffers[0];
        assert!(b.text.contains("本文だよ"), "完成本文へ差し替わる: {}", b.text);
        assert!(!b.text.contains("読み込み中"));
        assert!(!b.dirty(), "差し替え後も dirty にならない");
        assert!(b.pdf_job.is_none(), "ジョブは畳まれる");
        assert!(!ed.poll_pdf_jobs(), "二度目は何もしない");
    }

    #[test]
    fn dropped_pdf_worker_never_hangs_on_placeholder() {
        // ワーカーが結果を送らずに消えても「読み込み中…」で固まらない
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        drop(tx);
        let job = PdfJob::for_test(rx, "gone.pdf", 4096);
        let text = job.take().expect("必ず終わらせる");
        assert!(text.contains("gone.pdf"));
        assert!(text.contains("テキストを抽出できません"), "{text}");
    }

    #[test]
    fn pdf_page_rendering_marks_empty_pages() {
        // 抽出できたページが空でも「無い」ことが分かるようにする
        let out = pdf_render_pages("H\n", &["a".into(), "   ".into()]);
        assert!(out.starts_with("H\n"));
        assert!(out.contains("── ページ 1 / 2 ──"));
        assert!(out.contains("── ページ 2 / 2 ──"));
        assert!(out.contains("(このページにテキストはありません)"));
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
    }

    // ─── 折り返し・空白可視化 ──────────────────────────────────

    #[test]
    fn wrap_flag_selects_max_width() {
        assert_eq!(wrap_max_width(true, 640.0), 640.0);
        assert!(wrap_max_width(false, 640.0).is_infinite());
    }

    #[test]
    fn whitespace_transform_replaces_spaces_and_tabs() {
        use eframe::egui::text::{LayoutJob, LayoutSection, TextFormat};
        use eframe::egui::Color32;
        let src = "ab cd\te\n  f";
        let mut job = LayoutJob::default();
        // syntect layouter と同じく、連続する複数セクションで全文を覆う
        let fmt_a = TextFormat {
            color: Color32::RED,
            ..Default::default()
        };
        let fmt_b = TextFormat {
            color: Color32::GREEN,
            ..Default::default()
        };
        job.text = src.into();
        job.sections = vec![
            LayoutSection { leading_space: 0.0, byte_range: 0..5, format: fmt_a.clone() },
            LayoutSection { leading_space: 0.0, byte_range: 5..src.len(), format: fmt_b.clone() },
        ];

        let dim = Color32::GRAY;
        let out = whitespace_layout_job(job, dim);
        // スペース→「·」、タブ→「→」。改行はそのまま
        assert_eq!(out.text, "ab·cd→e\n··f");
        // char 数は変えない (カーソル位置が galley とずれる)
        assert_eq!(out.text.chars().count(), src.chars().count());
        // セクションは全文を隙間なく覆い、空白 run だけが dim 色になる
        let mut covered = 0usize;
        for sec in &out.sections {
            assert_eq!(sec.byte_range.start, covered, "隙間なく連続");
            covered = sec.byte_range.end;
            let s = &out.text[sec.byte_range.clone()];
            if s.chars().all(|c| c == '·' || c == '→') {
                assert_eq!(sec.format.color, dim, "空白 run は dim 色: {s:?}");
            } else {
                assert_ne!(sec.format.color, dim, "非空白 run は元の色: {s:?}");
            }
        }
        assert_eq!(covered, out.text.len(), "全文を覆う");
    }

    #[test]
    fn whitespace_transform_plain_text_unchanged() {
        use eframe::egui::text::{LayoutJob, LayoutSection, TextFormat};
        use eframe::egui::Color32;
        let mut job = LayoutJob::default();
        job.text = "abc\ndef".into();
        job.sections = vec![LayoutSection {
            leading_space: 0.0,
            byte_range: 0..7,
            format: TextFormat::default(),
        }];
        let out = whitespace_layout_job(job, Color32::GRAY);
        assert_eq!(out.text, "abc\ndef", "空白が無ければ本文はそのまま");
        assert_eq!(out.sections.len(), 1);
    }

    // =======================================================================
    // 行番号の付け替え
    // =======================================================================

    #[test]
    fn remap_line_table() {
        // (行, 挿入/削除位置, 増減, 期待)
        let cases: &[(usize, usize, isize, Option<usize>)] = &[
            (5, 3, 0, Some(5)),
            (2, 3, 2, Some(2)),
            (3, 3, 2, Some(5)),
            (5, 3, 2, Some(7)),
            (2, 3, -2, Some(2)),
            (3, 3, -2, None),
            (4, 3, -2, None),
            (5, 3, -2, Some(3)),
            (0, 0, 1, Some(1)),
            (0, 0, -1, None),
        ];
        for (line, at, delta, want) in cases.iter().copied() {
            assert_eq!(
                remap_line(line, at, delta),
                want,
                "line={line} at={at} delta={delta}"
            );
        }
    }

    // =======================================================================
    // 折りたたみ状態
    // =======================================================================

    const RS_NESTED: &str = "\
mod m {
    fn f() {
        1;
    }
}
fn g() {
    2;
}
";

    fn folded_sorted(fs: &FoldState) -> Vec<usize> {
        let mut v: Vec<usize> = fs.folded().iter().copied().collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn fold_state_toggle_and_hidden_lines() {
        let mut fs = FoldState::default();
        assert!(fs.refresh(RS_NESTED, "Rust"), "初回は計算する");
        assert!(!fs.refresh(RS_NESTED, "Rust"), "本文が同じなら再計算しない");
        assert!(fs.is_foldable(0) && fs.is_foldable(1) && fs.is_foldable(5));
        assert!(!fs.is_foldable(2), "中身の行は畳めない");
        assert_eq!(fs.marker(2), None);
        assert_eq!(fs.marker(0), Some(FoldMarker::Open));
        assert!(fs.toggle_fold(0), "畳める行は切り替わる");
        assert_eq!(fs.marker(0), Some(FoldMarker::Closed));
        assert!(!fs.toggle_fold(2), "畳めない行では何も起きない");
        for (line, hidden) in [(0, false), (1, true), (3, true), (4, true), (5, false)] {
            assert_eq!(fs.is_line_hidden(line), hidden, "line={line}");
        }
        assert!(fs.toggle_fold(0), "もう一度で開く");
        assert!(!fs.is_line_hidden(1));
    }

    #[test]
    fn fold_state_all_level_and_spans() {
        let mut fs = FoldState::default();
        fs.refresh(RS_NESTED, "Rust");
        fs.fold_all();
        assert_eq!(folded_sorted(&fs), vec![0, 1, 5], "畳める範囲すべて");
        fs.unfold_all();
        assert!(fs.folded().is_empty());
        fs.fold_level(1);
        assert_eq!(folded_sorted(&fs), vec![0, 5], "レベル 1 は外側だけ");
        fs.fold_level(2);
        assert_eq!(folded_sorted(&fs), vec![1], "レベル 2 は内側だけ");
        fs.fold_level(9);
        assert!(fs.folded().is_empty(), "存在しない深さなら空");
        // 入れ子を両方畳んだときの隠れ行はひと続きにまとまる
        fs.fold_all();
        assert_eq!(fs.hidden_spans(), vec![(1, 4), (6, 7)]);
        assert_eq!(fs.first_visible_from(1, 8), 5, "隠れた先の最初の可視行");
        assert_eq!(fs.first_visible_from(0, 8), 0);
    }

    #[test]
    fn fold_state_survives_an_edit() {
        let mut fs = FoldState::default();
        let text = "fn a() {\n    1;\n}\nfn b() {\n    2;\n}\n";
        fs.refresh(text, "Rust");
        assert!(fs.fold(3), "fn b を畳む");
        // 先頭に 1 行挿入 → 畳んだ位置もひとつ下へずれる
        let edited = format!("use std::io;\n{text}");
        fs.shift_lines(0, 1);
        assert!(fs.refresh(&edited, "Rust"), "本文が変われば再計算する");
        assert!(fs.is_folded(4), "ずれた先でも畳んだまま: {:?}", folded_sorted(&fs));
        assert_eq!(fs.range_at(4).map(|r| r.end_line), Some(6));
        // 挿入した行を消して元に戻す
        fs.shift_lines(0, -1);
        fs.refresh(text, "Rust");
        assert!(fs.is_folded(3), "戻したら元の行に畳みが残る");
    }

    #[test]
    fn fold_state_drops_folds_that_no_longer_open_a_range() {
        let mut fs = FoldState::default();
        let text = "fn a() {\n    1;\n}\nfn b() {\n    2;\n}\n";
        fs.refresh(text, "Rust");
        fs.fold(0);
        fs.fold(3);
        // fn b を 1 行にまとめた → fn b の範囲だけが消える
        let edited = "fn a() {\n    1;\n}\nfn b() { 2; }\n";
        fs.refresh(edited, "Rust");
        assert_eq!(
            folded_sorted(&fs),
            vec![0],
            "範囲の先頭のままの畳みだけ生き残る"
        );
        // 行がずれたのに shift_lines を呼ばなければ、畳みは残らず開く
        fs.fold_all();
        let shifted = format!("use std::io;\n{text}");
        fs.refresh(&shifted, "Rust");
        assert!(
            fs.folded().is_empty(),
            "行がずれたら安全側 (開く) に倒す: {:?}",
            folded_sorted(&fs)
        );
        // 関数ごと消えたら畳みも消える
        fs.refresh("fn a() {}\n", "Rust");
        assert!(fs.folded().is_empty(), "範囲が無くなれば畳みも消える");
    }

    // =======================================================================
    // ブックマーク
    // =======================================================================

    #[test]
    fn bookmarks_toggle_and_navigation_wraps() {
        let mut b = Bookmarks::default();
        assert!(b.is_empty());
        assert_eq!(b.next_after(0), None, "空なら移動先は無い");
        assert!(b.toggle(10));
        assert!(b.toggle(3));
        assert!(b.toggle(7));
        assert!(!b.toggle(3), "二度目は外れる");
        assert_eq!(b.len(), 2);
        assert!(b.is_marked(7) && !b.is_marked(3));
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![7, 10]);
        assert_eq!(b.next_after(0), Some(7));
        assert_eq!(b.next_after(7), Some(10));
        assert_eq!(b.next_after(10), Some(7), "末尾の次は先頭へ回り込む");
        assert_eq!(b.prev_before(10), Some(7));
        assert_eq!(b.prev_before(7), Some(10), "先頭の前は末尾へ回り込む");
        b.clear_all();
        assert!(b.is_empty());
    }

    #[test]
    fn bookmarks_remap_across_inserted_and_deleted_lines() {
        let marks = |b: &Bookmarks| b.iter().collect::<Vec<_>>();
        let mut b = Bookmarks::default();
        for l in [2, 5, 9] {
            b.toggle(l);
        }
        // 3 行目に 2 行挿入
        b.shift_lines(3, 2);
        assert_eq!(marks(&b), vec![2, 7, 11], "挿入位置より後だけ下がる");
        // 6..8 の 2 行を削除 (印の載った 7 行目が消える)
        b.shift_lines(6, -2);
        assert_eq!(marks(&b), vec![2, 9], "消えた行の印は落ちる");
        // 先頭に 1 行足す
        b.shift_lines(0, 1);
        assert_eq!(marks(&b), vec![3, 10]);
        // 増減 0 なら何も起きない
        b.shift_lines(0, 0);
        assert_eq!(marks(&b), vec![3, 10]);
    }

    // =======================================================================
    // 閉じたタブ
    // =======================================================================

    fn closed(name: &str) -> ClosedTab {
        ClosedTab {
            path: PathBuf::from(format!("/tmp/{name}.rs")),
            title: name.into(),
            cursor: (1, 1),
            scroll: 0.0,
        }
    }

    #[test]
    fn closed_tabs_lru_is_bounded_and_ordered() {
        let mut c = ClosedTabs::with_capacity(3);
        assert!(c.pop_closed().is_none(), "空なら取り出せない");
        for n in ["a", "b", "c", "d"] {
            c.push_closed(closed(n));
        }
        assert_eq!(c.len(), 3, "上限を超えたら古いものを捨てる");
        assert_eq!(c.peek().map(|t| t.title.clone()), Some("d".to_string()));
        let order: Vec<String> = std::iter::from_fn(|| c.pop_closed())
            .map(|t| t.title)
            .collect();
        assert_eq!(order, vec!["d", "c", "b"], "新しいものから出てくる");
        assert!(c.is_empty());
    }

    #[test]
    fn closed_tabs_dedupe_same_path() {
        let mut c = ClosedTabs::with_capacity(5);
        c.push_closed(closed("a"));
        c.push_closed(closed("b"));
        c.push_closed(closed("a"));
        assert_eq!(c.len(), 2, "同じパスは 1 件にまとまる");
        assert_eq!(c.pop_closed().map(|t| t.title), Some("a".to_string()));
        assert_eq!(c.pop_closed().map(|t| t.title), Some("b".to_string()));
        c.push_closed(closed("z"));
        c.clear();
        assert!(c.is_empty());
    }

    #[test]
    fn closing_a_file_tab_records_it_for_reopen() {
        let dir = unique_temp_dir("zaivern-editor-test", "closed-tab");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, "hello\n").unwrap();
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        ed.open(&f, &hl).unwrap();
        ed.cursor = (3, 4);
        ed.close_with(0, 120.0);
        let t = ed.closed_tabs.pop_closed().expect("記録されている");
        assert_eq!(t.path, crate::pathx::canonical(&f));
        assert_eq!(t.cursor, (3, 4), "キャレット位置も覚える");
        assert_eq!(t.scroll, 120.0, "スクロール位置も覚える");
        // ファイルに紐づかないタブは積まない
        ed.new_untitled();
        ed.close(0);
        assert!(ed.closed_tabs.is_empty(), "untitled は開き直せないので積まない");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // =======================================================================
    // CSV / TSV
    // =======================================================================

    #[test]
    fn csv_quoted_fields_with_delimiters_newlines_and_escapes() {
        let text = "name,note,n\r\n\
                    a,\"x,y\",1\r\n\
                    b,\"1 行目\n2 行目\",2\r\n\
                    c,\"言った \"\"やあ\"\" と\",3\r\n";
        let t = parse_table(text, TABLE_MAX_ROWS);
        assert_eq!(t.delimiter, ',');
        assert_eq!(t.headers, vec!["name", "note", "n"]);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0][1], "x,y", "引用の中の区切り文字は中身");
        assert_eq!(t.rows[1][1], "1 行目\n2 行目", "引用の中の改行も中身");
        assert_eq!(t.rows[2][1], "言った \"やあ\" と", "\"\" は \" 1 文字");
        assert_eq!(t.columns, 3);
        assert!(!t.truncated);
    }

    #[test]
    fn csv_ragged_rows_never_panic() {
        let text = "a,b,c\n1,2\n3,4,5,6\n\n7,8,9\n";
        let t = parse_table(text, TABLE_MAX_ROWS);
        assert_eq!(t.headers.len(), 3);
        assert_eq!(t.rows.len(), 3, "空行は行として数えない");
        assert_eq!(t.rows[0], vec!["1", "2"]);
        assert_eq!(t.rows[1], vec!["3", "4", "5", "6"]);
        assert_eq!(t.columns, 4, "列数は最大値");
        // 壊れた入力でも落ちない
        for s in [
            "",
            "\n",
            "\"",
            "\"未終端,a\n",
            ",,,\n",
            "\u{feff}",
            "a\r\n\r\nb\r\n",
        ] {
            let _ = parse_table(s, TABLE_MAX_ROWS);
        }
    }

    #[test]
    fn csv_delimiter_detection_table() {
        // (説明, 本文, 期待する区切り文字)
        let cases: &[(&str, &str, char)] = &[
            ("コンマ", "a,b,c\n1,2,3\n", ','),
            ("タブ", "a\tb\tc\n1\t2\t3\n", '\t'),
            ("セミコロン", "a;b;c\n1;2;3\n", ';'),
            (
                "セミコロン区切りでフィールドにコンマ",
                "name;desc\nx;\"1,234\"\ny;\"5,678\"\n",
                ';',
            ),
            ("BOM 付き", "\u{feff}a,b\n1,2\n", ','),
            ("表に見えない", "ただの文章です\n改行があるだけ\n", ','),
        ];
        for (name, text, want) in cases.iter().copied() {
            assert_eq!(detect_delimiter(text), want, "{name}");
        }
        let t = parse_table("\u{feff}a,b\n1,2\n", TABLE_MAX_ROWS);
        assert_eq!(t.headers, vec!["a", "b"], "BOM は見出しに混ざらない");
        let tsv = parse_table("a\tb\n1\t2\n", TABLE_MAX_ROWS);
        assert_eq!(tsv.delimiter, '\t');
        assert_eq!(tsv.rows[0], vec!["1", "2"]);
    }

    #[test]
    fn csv_row_cap_truncates_huge_files() {
        let mut s = String::from("a,b\n");
        for i in 0..200_000 {
            s.push_str(&format!("{i},x\n"));
        }
        let t = parse_table(&s, TABLE_MAX_ROWS);
        assert_eq!(t.rows.len(), TABLE_MAX_ROWS, "上限で打ち切る");
        assert!(t.truncated, "打ち切ったことを伝える");
        assert_eq!(t.headers, vec!["a", "b"]);
        // ちょうど上限ぴったりなら truncated は立たない
        let small = "h1,h2\n".to_string() + &"1,2\n".repeat(5);
        let t2 = parse_table(&small, 5);
        assert_eq!(t2.rows.len(), 5);
        assert!(!t2.truncated);
        let t3 = parse_table(&small, 4);
        assert_eq!(t3.rows.len(), 4);
        assert!(t3.truncated);
    }

    #[test]
    fn table_paths_are_recognised_case_insensitively() {
        for p in ["a.csv", "b.TSV", "c.Tab"] {
            assert!(is_table_path(Path::new(p)), "{p}");
        }
        for p in ["a.rs", "b.txt", "noext"] {
            assert!(!is_table_path(Path::new(p)), "{p}");
        }
    }

    // =======================================================================
    // 巨大ファイルモード
    // =======================================================================

    #[test]
    fn large_file_decision_table() {
        // (バイト数, 開ける, バナー, 読み取り専用, ハイライト)
        let cases: &[(u64, bool, bool, bool, bool)] = &[
            (0, true, false, false, true),
            (1024, true, false, false, true),
            (HEAVY_FILE_BYTES - 1, true, false, false, true),
            (HEAVY_FILE_BYTES, true, true, false, false),
            (LARGE_FILE_BYTES - 1, true, true, false, false),
            (LARGE_FILE_BYTES, true, true, true, false),
            (MAX_OPEN_BYTES, true, true, true, false),
            (MAX_OPEN_BYTES + 1, false, false, false, false),
        ];
        for (bytes, open, active, ro, hl) in cases.iter().copied() {
            match open_decision(bytes) {
                OpenDecision::Open(m) => {
                    assert!(open, "{bytes} は開けないはず");
                    assert_eq!(m.active, active, "{bytes}: バナー");
                    assert_eq!(m.read_only, ro, "{bytes}: 読み取り専用");
                    assert_eq!(m.highlight, hl, "{bytes}: ハイライト");
                    assert_eq!(m.bytes, bytes);
                }
                OpenDecision::Refuse { bytes: b, limit } => {
                    assert!(!open, "{bytes} は開けるはず");
                    assert_eq!(b, bytes);
                    assert_eq!(limit, MAX_OPEN_BYTES);
                }
            }
        }
        assert!(
            HEAVY_FILE_BYTES < LARGE_FILE_BYTES && LARGE_FILE_BYTES < MAX_OPEN_BYTES,
            "閾値は小さい順"
        );
        assert!(PDF_MAX_BYTES < MAX_OPEN_BYTES);
    }

    #[test]
    fn large_file_mode_flags_reach_the_buffer() {
        let dir = unique_temp_dir("zaivern-editor-test", "large-mode");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("small.txt");
        std::fs::write(&f, "hello\n").unwrap();
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        ed.open(&f, &hl).unwrap();
        let b = &ed.buffers[0];
        assert!(!b.read_only(), "普通のファイルは編集できる");
        assert!(b.highlight_enabled());
        assert_eq!(b.large_file_banner(), None, "バナーは出さない");
        // 巨大ファイル扱いに差し替えると、種類が File でも読み取り専用になる
        let b = &mut ed.buffers[0];
        b.large = LargeFileMode {
            active: true,
            read_only: true,
            highlight: false,
            bytes: LARGE_FILE_BYTES,
        };
        assert!(b.read_only(), "巨大ファイルモードは編集させない");
        assert!(!b.highlight_enabled());
        assert_eq!(b.large_file_banner(), Some(LARGE_FILE_BYTES));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn buffer_folds_and_table_helpers() {
        let dir = unique_temp_dir("zaivern-editor-test", "buffer-structure");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.rs");
        std::fs::write(&f, "fn a() {\n    1;\n}\n").unwrap();
        let hl = Highlighter::new();
        let mut ed = Editor::new();
        ed.open(&f, &hl).unwrap();
        let b = &mut ed.buffers[0];
        assert!(b.refresh_folds(), "開いた直後は計算する");
        assert!(!b.refresh_folds(), "本文が変わらなければ再計算しない");
        assert!(b.folds.is_foldable(0));
        assert!(b.bookmarks.is_empty());
        b.text = "x,y\n1,2\n".into();
        let t = b.build_table();
        assert_eq!(t.headers, vec!["x", "y"]);
        assert!(b.table.is_some());
        b.drop_table();
        assert!(b.table.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
