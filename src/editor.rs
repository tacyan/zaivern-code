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
        const MAX_OPEN_BYTES: u64 = 50 * 1024 * 1024;
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

    /// 全バッファの外部変更を確認する。クリーンなバッファは自動で読み直し、
    /// 未保存の編集と競合したバッファは一度だけ Conflict を報告する。
    pub fn check_external(&mut self) -> Vec<ExternalEvent> {
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
