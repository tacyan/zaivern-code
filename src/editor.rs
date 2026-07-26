use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use std::sync::Arc;

use eframe::egui::Galley;

use crate::highlight::Highlighter;

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
    /// 画像ビューア (png/jpg 等)。ピクセルは `Buffer::image` に持つ。
    /// `path` は `Some` (外部変更の mtime 監視で再デコードするため) だが、
    /// `read_only()` が真なので保存・編集の経路には乗らない。
    Image,
    /// PDF ビューア。抽出したテキストを `text` に持つ**普通の本文タブ**なので、
    /// 検索・折り返し・コピーがそのまま効く。`path` は `Some` (mtime 監視で
    /// 再抽出するため) だが、`read_only()` が真なので抽出結果が元の PDF へ
    /// 書き戻されることはない。
    Pdf,
}

impl BufferKind {
    /// このタブが読み取り専用か。
    pub fn read_only(&self) -> bool {
        !matches!(self, BufferKind::File)
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
/// `MAX_OPEN_BYTES` (50 MB) より小さくしておくことで、「開けないファイル」
/// ではなく「開けるが抽出だけ諦めるタブ」として出せる。
pub const PDF_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// `Editor::open` が読み込みを拒否する上限。巨大ログのクリックで
/// 同期 IO がフリーズするのを防ぐ。
pub const MAX_OPEN_BYTES: u64 = 50 * 1024 * 1024;

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

impl Buffer {
    pub fn dirty(&self) -> bool {
        hash_str(&self.text) != self.saved_hash
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
}

impl Editor {
    pub fn new() -> Self {
        Self {
            buffers: Vec::new(),
            active: None,
            next_id: 1,
            cursor: (1, 1),
            untitled_count: 0,
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

        // 巨大ファイルの防壁: 読み込み自体が UI スレッドの同期 IO なので、
        // 上限なしだと数百 MB のログをクリックした瞬間にフリーズする
        if let Ok(m) = std::fs::metadata(&canon) {
            if m.len() > MAX_OPEN_BYTES {
                return Err(format!(
                    "ファイルが大きすぎます ({} MB > 50 MB)",
                    m.len() / (1024 * 1024)
                ));
            }
        }
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
        if i >= self.buffers.len() {
            return;
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
}
