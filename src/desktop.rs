//! OS のアプリランチャー統合 (`zai app install` / `zai app uninstall`)。
//!
//! ワンライナーでインストールした後、`zai` をターミナルからだけでなく
//! OS の「アプリ」として起動できるようにする:
//!   - macOS:   `~/Applications/Zaivern Code.app` (Launchpad / Spotlight / Dock)
//!   - Linux:   `~/.local/share/applications/zaivern-code.desktop` (アプリメニュー)
//!   - Windows: スタートメニューの「Zaivern Code」ショートカット
//!
//! 追加クレートは使わない:
//!   - .icns は「PNG データをそのまま格納する現行チャンク形式」を自前で組み立てる
//!   - .ico は image クレート (ico feature) でエンコードする
//!   - Windows の .lnk は powershell (WScript.Shell) へのシェルアウトで作る
//!
//! macOS だけは「参照」ではなく **バンドルの実体そのものをバイナリにする**:
//! `Contents/MacOS/Zaivern` をハードリンク (→ symlink → コピー) で置き、
//! `CFBundleExecutable` をそれに向ける。カーネルはこの basename を
//! プロセス名 (p_comm) にするので、アクティビティモニタ / `ps` / `pgrep -i zaivern`
//! から「Zaivern」で見つけられる。ランチャースクリプトを噛ませていた頃は
//! `zai` としか出なかった。実体の更新には `zai app install` の再実行が要るが、
//! install.sh が更新のたびに呼ぶので運用上は自動で追従する。
//! Linux / Windows のショートカットは従来どおりインストール済みバイナリへの参照。
//! アプリとして起動されたとき (Finder / メニュー / スタートメニュー) は
//! 作業ディレクトリが `/` や system32 になるため、ホームを既定ワークスペースにする
//! (macOS は [`normalize_app_launch_cwd`]、他 OS はショートカット側の指定)。

use std::path::{Path, PathBuf};

/// アプリアイコンの原本 (main.rs のウィンドウアイコンと共用)。
pub const ICON_PNG: &[u8] = include_bytes!("../assets/Zaivern.png");

/// OS に表示するアプリ名。
pub const APP_NAME: &str = "Zaivern Code";

/// macOS の `.app` が持つ実行ファイル名 (= `Contents/MacOS/<これ>`)。
///
/// カーネルはこの basename をそのままプロセス名 (p_comm) にするため、
/// アクティビティモニタ / `ps -o comm=` / `pgrep -i zaivern` で見つかる名前は
/// **ここで決まる**。以前はランチャースクリプト `zai` を挟んでいたので
/// `zai` としか出ず「Zaivern」で検索しても引っかからなかった。
/// CLI 名の `zai` は PATH 上のバイナリ名であり、これとは無関係に不変。
pub const MACOS_EXEC_NAME: &str = "Zaivern";

// ───────────────────────── サブコマンド入口 ─────────────────────────

/// `zai app <install|uninstall>` のディスパッチ。戻り値は終了コード。
/// 副作用を持つ操作なので、サブコマンドは必ず明示させる
/// (`zai app` 単独は cli.rs 側で ./app ディレクトリの GUI 起動にも譲る)。
pub fn run(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let result = match sub {
        "install" => install(),
        "uninstall" | "remove" => uninstall(),
        "" => Err("app のサブコマンドを指定してください: install / uninstall".to_string()),
        other => Err(format!(
            "不明な app サブコマンドです: {other} (install / uninstall)"
        )),
    };
    match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

// ───────────────────────── 共通ヘルパ ─────────────────────────

/// 自分自身 (インストール済み zai) の絶対パス。
/// シンボリックリンク経由でも実体を指すよう canonicalize する。
fn resolve_bin() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("自分の実行ファイルの場所を特定できません: {e}"))?;
    // `\\?\` 付きのまま .lnk の TargetPath 等へ渡すと表示・解決が崩れるので、
    // 素のパスに直す (pathx に一本化してある)。
    Ok(crate::pathx::plain(exe.canonicalize().unwrap_or(exe)))
}

fn home_dir() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "ホームディレクトリが見つかりません".to_string())
}

/// 埋め込み PNG を size×size に縮小して PNG バイト列にする。
fn png_square(src: &image::DynamicImage, size: u32) -> Result<Vec<u8>, String> {
    let resized = src.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
    let mut cur = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut cur, image::ImageFormat::Png)
        .map_err(|e| format!("アイコン PNG の生成に失敗: {e}"))?;
    Ok(cur.into_inner())
}

#[allow(dead_code)] // 実行時に使うのは Linux のみ
fn load_icon_image() -> Result<image::DynamicImage, String> {
    image::load_from_memory(ICON_PNG).map_err(|e| format!("アイコン画像を読めません: {e}"))
}

// ───────────────────────── アイコン生成 (純関数) ─────────────────────────

/// .icns を組み立てる。現行の icns は PNG データをそのまま
/// `ic07`(128) / `ic08`(256) / `ic09`(512) チャンクに格納できる。
/// 構造: "icns" + 全長(BE u32) + [タグ4B + チャンク長(BE u32, ヘッダ込み) + PNG]…
#[allow(dead_code)] // 実行時に使うのは macOS のみ (テストは全 OS で走る)
fn icns_bytes(png: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png).map_err(|e| format!("アイコン画像を読めません: {e}"))?;
    let entries: &[(&[u8; 4], u32)] = &[(b"ic07", 128), (b"ic08", 256), (b"ic09", 512)];
    let mut chunks: Vec<u8> = Vec::new();
    for (tag, size) in entries {
        let data = png_square(&img, *size)?;
        chunks.extend_from_slice(*tag);
        chunks.extend_from_slice(&((data.len() as u32 + 8).to_be_bytes()));
        chunks.extend_from_slice(&data);
    }
    let mut out = Vec::with_capacity(chunks.len() + 8);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&((chunks.len() as u32 + 8).to_be_bytes()));
    out.extend_from_slice(&chunks);
    Ok(out)
}

/// .ico を生成する (Windows のショートカットアイコン用、256×256)。
#[allow(dead_code)] // 実行時に使うのは Windows のみ
fn ico_bytes(png: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png).map_err(|e| format!("アイコン画像を読めません: {e}"))?;
    let resized = img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3);
    let mut cur = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut cur, image::ImageFormat::Ico)
        .map_err(|e| format!(".ico の生成に失敗: {e}"))?;
    Ok(cur.into_inner())
}

// ───────────────────────── 登録内容の生成 (純関数) ─────────────────────────

/// macOS の Info.plist。CFBundleExecutable はバンドル内の実体 [`MACOS_EXEC_NAME`]
/// (ランチャースクリプトではない) — プロセス一覧に出る名前がこれで決まる。
#[allow(dead_code)]
fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>{APP_NAME}</string>
    <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>io.github.tacyan.zaivern-code</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleExecutable</key><string>{MACOS_EXEC_NAME}</string>
    <key>CFBundleIconFile</key><string>Zaivern</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSMicrophoneUsageDescription</key><string>音声入力に使用します</string>
    <key>NSSpeechRecognitionUsageDescription</key><string>音声入力の文字起こしに使用します</string>
</dict>
</plist>
"#
    )
}

// ───── macOS: バンドル内実行ファイルの用意 (ランチャースクリプトの置き換え) ─────

/// `Contents/MacOS/Zaivern` をどう用意したか。診断メッセージとテスト用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // 実行時に使うのは macOS のみ (テストは全 OS で走る)
enum LinkKind {
    /// ハードリンク。同一 inode なので p_comm が "Zaivern" になり、
    /// 実体を消しても .app 側は生き残る (最良)。
    Hard,
    /// シンボリックリンク。別ファイルシステムへ跨ぐときの次善。
    /// カーネルはリンク先の basename を p_comm にするため表示名は `zai` に戻る。
    Symlink,
    /// 実コピー。最後の砦 (バイナリ更新時は再インストールが要る)。
    Copy,
}

#[allow(dead_code)]
impl LinkKind {
    fn label(self) -> &'static str {
        match self {
            LinkKind::Hard => "ハードリンク",
            LinkKind::Symlink => "シンボリックリンク",
            LinkKind::Copy => "コピー",
        }
    }
}

/// `src` を `dst` へ「ハードリンク → シンボリックリンク → コピー」の順で用意する。
///
/// fs 操作を注入できるようにしてあるのは、各段の失敗 (別ファイルシステム・
/// symlink 不可な環境) をテストから再現するため。
/// 既存の `dst` (旧レイアウトのランチャースクリプトや古いリンク) は必ず
/// 取り除いてから張り直すので、再インストールで最新のバイナリを指し直せる。
#[allow(dead_code)]
fn place_executable_with<H, S, C>(
    src: &Path,
    dst: &Path,
    hard: H,
    sym: S,
    copy: C,
) -> Result<LinkKind, String>
where
    H: Fn(&Path, &Path) -> std::io::Result<()>,
    S: Fn(&Path, &Path) -> std::io::Result<()>,
    C: Fn(&Path, &Path) -> std::io::Result<()>,
{
    // バンドル内のバイナリ自身から `zai app install` した場合、src == dst になる。
    // 消してから張り直すと自分自身が消えるので、その場合は何もしない。
    if same_path(src, dst) {
        return Ok(LinkKind::Hard);
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        std::fs::remove_file(dst)
            .map_err(|e| format!("{} を置き換えられません: {e}", dst.display()))?;
    }
    // 段は**必ず遅延評価**する。配列リテラルに並べると 3 つとも実行され、
    // ハードリンク成功後に copy(src, dst) が「同じ実体へのコピー」になって
    // 本体バイナリを 0 バイトに切り詰めてしまう (実機で踏んだ)。
    let mut why: Vec<String> = Vec::new();
    macro_rules! step {
        ($kind:expr, $f:expr) => {
            match $f(src, dst) {
                Ok(()) => return Ok($kind),
                Err(e) => why.push(format!("{}: {e}", LinkKind::label($kind))),
            }
        };
    }
    step!(LinkKind::Hard, hard);
    step!(LinkKind::Symlink, sym);
    step!(LinkKind::Copy, copy);
    Err(format!(
        "{} を用意できません — {}",
        dst.display(),
        why.join(" / ")
    ))
}

/// 2 つのパスが同じ場所を指すか (存在しない側は素のパス比較にフォールバック)。
#[allow(dead_code)]
fn same_path(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// `.app` から起動されたときにホームへ移るべきか判定する純関数。
///
/// Finder / Launchpad / Spotlight から起動されたプロセスの作業ディレクトリは
/// 必ず `/` になる。そのまま起動するとファイルツリーのルートが `/` になって
/// しまうため、以前はランチャースクリプトが `cd "$HOME"` していた。
/// スクリプトを廃してバイナリ自身をバンドルの実体にしたので、その代わりを
/// ここで判定する。ターミナルから `zai` を叩く経路は cwd が `/` でない
/// ので一切影響を受けない。
#[allow(dead_code)]
fn app_launch_cwd_fix(exe: &Path, cwd: &Path, home: &Path) -> Option<PathBuf> {
    let in_bundle = exe
        .parent()
        .map(|p| p.ends_with("Contents/MacOS"))
        .unwrap_or(false)
        && exe.components().any(|c| {
            std::path::Path::new(c.as_os_str())
                .extension()
                .map(|e| e == "app")
                .unwrap_or(false)
        });
    let root = Path::new("/");
    if in_bundle && cwd == root && home.is_absolute() && home != root {
        Some(home.to_path_buf())
    } else {
        None
    }
}

/// 起動直後に呼ぶ (main → `instances::set_process_name` 経由)。
/// `.app` 経由の起動なら作業ディレクトリをホームにする。それ以外は何もしない。
pub fn normalize_app_launch_cwd() {
    #[cfg(target_os = "macos")]
    {
        let (Ok(exe), Ok(cwd), Some(home)) = (
            std::env::current_exe(),
            std::env::current_dir(),
            dirs::home_dir(),
        ) else {
            return;
        };
        if let Some(to) = app_launch_cwd_fix(&exe, &cwd, &home) {
            let _ = std::env::set_current_dir(to);
        }
    }
}

/// Linux の .desktop エントリ。Icon 名と StartupWMClass は
/// main.rs の `with_app_id("zaivern-code")` と一致させること。
#[allow(dead_code)]
fn desktop_entry(bin: &Path, home: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         Comment=AI ネイティブなコックピットエディタ\n\
         Comment[en]=Rust-native AI cockpit editor\n\
         Exec={} %F\n\
         Icon=zaivern-code\n\
         Terminal=false\n\
         Categories=Development;IDE;TextEditor;\n\
         Path={}\n\
         StartupWMClass=zaivern-code\n",
        bin.display(),
        home.display()
    )
}

/// PowerShell の単一引用符文字列用エスケープ (`'` → `''`)。
#[allow(dead_code)]
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// スタートメニューの .lnk を作る PowerShell スクリプト (WScript.Shell)。
#[allow(dead_code)]
fn shortcut_ps(lnk: &Path, bin: &Path, home: &Path, ico: &Path) -> String {
    format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $s = $ws.CreateShortcut('{}'); \
         $s.TargetPath = '{}'; \
         $s.WorkingDirectory = '{}'; \
         $s.IconLocation = '{},0'; \
         $s.Description = '{APP_NAME}'; \
         $s.Save()",
        ps_quote(&lnk.to_string_lossy()),
        ps_quote(&bin.to_string_lossy()),
        ps_quote(&home.to_string_lossy()),
        ps_quote(&ico.to_string_lossy()),
    )
}

// ───────────────────────── macOS ─────────────────────────

#[cfg(target_os = "macos")]
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

#[cfg(target_os = "macos")]
fn app_bundle_path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join("Applications").join(format!("{APP_NAME}.app")))
}

/// `.app` 一式を `app` へ書き出す。テストから fake root と偽の配置関数を
/// 渡せるよう cfg を持たせない (OS 依存部は `place` の中だけ)。
#[allow(dead_code)]
fn write_bundle(
    app: &Path,
    bin: &Path,
    version: &str,
    place: impl Fn(&Path, &Path) -> Result<LinkKind, String>,
) -> Result<LinkKind, String> {
    let macos_dir = app.join("Contents/MacOS");
    let res_dir = app.join("Contents/Resources");
    std::fs::create_dir_all(&macos_dir)
        .and_then(|_| std::fs::create_dir_all(&res_dir))
        .map_err(|e| format!("{} を作成できません: {e}", app.display()))?;
    std::fs::write(app.join("Contents/Info.plist"), info_plist(version))
        .map_err(|e| format!("Info.plist を書けません: {e}"))?;
    // 旧レイアウト (Contents/MacOS/zai のランチャースクリプト) の後始末。
    // 残しておくとバンドル内に使われない殻が居座るだけなので必ず消す。
    let legacy = macos_dir.join("zai");
    if std::fs::symlink_metadata(&legacy).is_ok() {
        let _ = std::fs::remove_file(&legacy);
    }
    // バンドルの実体そのものをバイナリにする = プロセス名が "Zaivern" になる。
    let exe = macos_dir.join(MACOS_EXEC_NAME);
    let kind = place(bin, &exe)?;
    // コピー経路では元の実行権限が落ちることがあるので明示的に付け直す。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
    }
    // アイコンは失敗しても登録自体は続行する
    if let Ok(icns) = icns_bytes(ICON_PNG) {
        let _ = std::fs::write(res_dir.join("Zaivern.icns"), icns);
    }
    Ok(kind)
}

/// `.app` を丸ごと削除する。`Contents/MacOS/Zaivern` はハードリンク or
/// シンボリックリンクなので、消しても PATH 上の `zai` 本体は無傷。
/// 戻り値は「消すものがあったか」。
#[allow(dead_code)]
fn remove_bundle(app: &Path) -> Result<bool, String> {
    if std::fs::symlink_metadata(app).is_err() {
        return Ok(false);
    }
    std::fs::remove_dir_all(app).map_err(|e| format!("{} を削除できません: {e}", app.display()))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn install() -> Result<String, String> {
    let bin = resolve_bin()?;
    let app = app_bundle_path()?;
    let kind = write_bundle(&app, &bin, env!("CARGO_PKG_VERSION"), place_executable)?;
    // Launch Services へ即時登録 (失敗しても Launchpad の次回スキャンで拾われる)
    let _ = std::process::Command::new(LSREGISTER)
        .args(["-f", &app.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(format!(
        "✅ アプリとして登録しました: {} ({})\n   Launchpad / Spotlight から「{APP_NAME}」で起動でき、\n   アクティビティモニタ / `pgrep -i zaivern` には「{MACOS_EXEC_NAME}」で出ます。",
        app.display(),
        kind.label()
    ))
}

/// 実 fs でのハードリンク → シンボリックリンク → コピー。
#[cfg(target_os = "macos")]
fn place_executable(src: &Path, dst: &Path) -> Result<LinkKind, String> {
    place_executable_with(
        src,
        dst,
        |s: &Path, d: &Path| std::fs::hard_link(s, d),
        |s: &Path, d: &Path| std::os::unix::fs::symlink(s, d),
        |s: &Path, d: &Path| std::fs::copy(s, d).map(|_| ()),
    )
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<String, String> {
    let app = app_bundle_path()?;
    if !remove_bundle(&app)? {
        return Ok("アプリ登録は見つかりませんでした (何もしていません)。".into());
    }
    let _ = std::process::Command::new(LSREGISTER)
        .args(["-u", &app.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(format!("🗑 アプリ登録を解除しました: {}", app.display()))
}

// ───────────────────────── Linux ─────────────────────────

#[cfg(target_os = "linux")]
fn linux_paths() -> Result<(PathBuf, PathBuf), String> {
    let data = dirs::data_dir().unwrap_or(home_dir()?.join(".local/share"));
    let desktop = data.join("applications/zaivern-code.desktop");
    let icon = data.join("icons/hicolor/512x512/apps/zaivern-code.png");
    Ok((desktop, icon))
}

#[cfg(target_os = "linux")]
fn install() -> Result<String, String> {
    let bin = resolve_bin()?;
    let home = home_dir()?;
    let (desktop, icon) = linux_paths()?;
    if let Some(dir) = icon.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
    }
    let img = load_icon_image()?;
    std::fs::write(&icon, png_square(&img, 512)?)
        .map_err(|e| format!("アイコンを書けません: {e}"))?;
    if let Some(dir) = desktop.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
        std::fs::write(&desktop, desktop_entry(&bin, &home))
            .map_err(|e| format!(".desktop を書けません: {e}"))?;
        // メニューのキャッシュ更新は任意 (無いディストリでも登録自体は有効)
        let _ = std::process::Command::new("update-desktop-database")
            .arg(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(format!(
        "✅ アプリとして登録しました: {}\n   アプリメニュー (アクティビティ等) から「{APP_NAME}」で起動できます。",
        desktop.display()
    ))
}

#[cfg(target_os = "linux")]
fn uninstall() -> Result<String, String> {
    let (desktop, icon) = linux_paths()?;
    let existed = desktop.exists() || icon.exists();
    if !existed {
        return Ok("アプリ登録は見つかりませんでした (何もしていません)。".into());
    }
    let _ = std::fs::remove_file(&icon);
    std::fs::remove_file(&desktop)
        .or_else(|e| if desktop.exists() { Err(e) } else { Ok(()) })
        .map_err(|e| format!("{} を削除できません: {e}", desktop.display()))?;
    if let Some(dir) = desktop.parent() {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(format!("🗑 アプリ登録を解除しました: {}", desktop.display()))
}

// ───────────────────────── Windows ─────────────────────────

#[cfg(windows)]
fn windows_paths() -> Result<(PathBuf, PathBuf), String> {
    let programs = dirs::data_dir()
        .ok_or("APPDATA が見つかりません")?
        .join(r"Microsoft\Windows\Start Menu\Programs");
    let lnk = programs.join(format!("{APP_NAME}.lnk"));
    let ico = dirs::data_local_dir()
        .unwrap_or(home_dir()?.join(r"AppData\Local"))
        .join(r"Zaivern\Zaivern.ico");
    Ok((lnk, ico))
}

#[cfg(windows)]
fn run_powershell(script: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 失敗時のメッセージはそのままユーザーに見せる。出力を UTF-8 に固定して
    // おかないと、日本語のエラー (「アクセスが拒否されました」等) が化けて
    // 何が起きたのか分からなくなる。
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("{}{script}", crate::textenc::PS_UTF8_PRELUDE),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("powershell を実行できません: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ショートカットの作成に失敗しました: {}",
            crate::textenc::decode_output(&out.stderr).trim()
        ))
    }
}

#[cfg(windows)]
fn install() -> Result<String, String> {
    let bin = resolve_bin()?;
    let home = home_dir()?;
    let (lnk, ico) = windows_paths()?;
    if let Some(dir) = ico.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
    }
    std::fs::write(&ico, ico_bytes(ICON_PNG)?).map_err(|e| format!(".ico を書けません: {e}"))?;
    if let Some(dir) = lnk.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
    }
    run_powershell(&shortcut_ps(&lnk, &bin, &home, &ico))?;
    Ok(format!(
        "✅ スタートメニューに登録しました: {}\n   スタートメニューから「{APP_NAME}」で起動できます。",
        lnk.display()
    ))
}

#[cfg(windows)]
fn uninstall() -> Result<String, String> {
    let (lnk, ico) = windows_paths()?;
    if !lnk.exists() && !ico.exists() {
        return Ok("アプリ登録は見つかりませんでした (何もしていません)。".into());
    }
    let _ = std::fs::remove_file(&ico);
    std::fs::remove_file(&lnk)
        .or_else(|e| if lnk.exists() { Err(e) } else { Ok(()) })
        .map_err(|e| format!("{} を削除できません: {e}", lnk.display()))?;
    Ok(format!("🗑 アプリ登録を解除しました: {}", lnk.display()))
}

// ───────────────────────── その他 OS ─────────────────────────

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn install() -> Result<String, String> {
    Err("この OS ではアプリ登録に対応していません。".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn uninstall() -> Result<String, String> {
    Err("この OS ではアプリ登録に対応していません。".into())
}

// ───────────────────────── テスト ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── icns: Finder が読める最低限の構造を満たすこと ──

    #[test]
    fn icns_header_and_chunks_are_consistent() {
        let buf = icns_bytes(ICON_PNG).expect("icns 生成");
        assert_eq!(&buf[..4], b"icns");
        let total = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        assert_eq!(total, buf.len(), "ヘッダの全長がファイル長と一致すること");

        let mut pos = 8;
        let mut tags: Vec<[u8; 4]> = Vec::new();
        while pos < buf.len() {
            let tag: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
            let len = u32::from_be_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
            assert!(len > 8, "チャンク長はヘッダ 8B より大きいこと");
            // 現行形式: 各チャンクのデータは PNG そのもの
            assert_eq!(&buf[pos + 8..pos + 16], b"\x89PNG\r\n\x1a\n");
            tags.push(tag);
            pos += len;
        }
        assert_eq!(pos, buf.len(), "チャンク列がちょうどファイル末尾で終わること");
        for expect in [b"ic07", b"ic08", b"ic09"] {
            assert!(tags.contains(expect), "{:?} チャンクがあること", expect);
        }
    }

    // ── ico: ICONDIR ヘッダで始まること ──

    #[test]
    fn ico_starts_with_icondir_header() {
        let buf = ico_bytes(ICON_PNG).expect("ico 生成");
        // reserved=0, type=1(icon), count>=1
        assert_eq!(&buf[..4], &[0, 0, 1, 0]);
        assert!(buf[4] >= 1);
    }

    // ── Info.plist / ランチャー / .desktop / .lnk スクリプトの要点 ──

    #[test]
    fn info_plist_has_required_keys() {
        let p = info_plist("9.9.9");
        for needle in [
            "<key>CFBundleExecutable</key><string>Zaivern</string>",
            "<key>CFBundleIconFile</key><string>Zaivern</string>",
            "<key>CFBundlePackageType</key><string>APPL</string>",
            "<string>9.9.9</string>",
            APP_NAME,
        ] {
            assert!(p.contains(needle), "Info.plist に {needle} が無い");
        }
    }

    #[test]
    fn info_plist_executable_is_not_a_launcher_script() {
        // 退行防止: ここが "zai" に戻るとプロセス一覧が再び `zai` になる。
        let p = info_plist("1.2.3");
        assert!(!p.contains("<key>CFBundleExecutable</key><string>zai</string>"));
        assert_eq!(MACOS_EXEC_NAME, "Zaivern");
    }

    // ── ハードリンク → シンボリックリンク → コピー のフォールバック連鎖 ──

    /// 常に成功する偽の fs 操作 (中身は目印テキスト)。
    fn ok_with(tag: &'static str) -> impl Fn(&Path, &Path) -> std::io::Result<()> {
        move |_s: &Path, d: &Path| std::fs::write(d, tag)
    }

    /// 常に失敗する偽の fs 操作。
    fn fail() -> impl Fn(&Path, &Path) -> std::io::Result<()> {
        |_s: &Path, _d: &Path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::CrossesDevices,
                "別ファイルシステム",
            ))
        }
    }

    fn chain_case(dir: &Path, name: &str, hard_ok: bool, sym_ok: bool, copy_ok: bool) -> Result<LinkKind, String> {
        let src = dir.join(format!("{name}-src"));
        std::fs::write(&src, "binary").unwrap();
        let dst = dir.join(name);
        let h: Box<dyn Fn(&Path, &Path) -> std::io::Result<()>> =
            if hard_ok { Box::new(ok_with("hard")) } else { Box::new(fail()) };
        let s: Box<dyn Fn(&Path, &Path) -> std::io::Result<()>> =
            if sym_ok { Box::new(ok_with("sym")) } else { Box::new(fail()) };
        let c: Box<dyn Fn(&Path, &Path) -> std::io::Result<()>> =
            if copy_ok { Box::new(ok_with("copy")) } else { Box::new(fail()) };
        place_executable_with(&src, &dst, h, s, c)
    }

    #[test]
    fn place_executable_falls_back_hard_then_symlink_then_copy() {
        let dir = crate::test_util::unique_temp_dir("zaivern-desktop-test", "chain");
        for (hard, sym, copy, want) in [
            (true, true, true, Ok(LinkKind::Hard)),
            (false, true, true, Ok(LinkKind::Symlink)),
            (false, false, true, Ok(LinkKind::Copy)),
        ] {
            let name = format!("exe-{hard}-{sym}-{copy}");
            assert_eq!(chain_case(&dir, &name, hard, sym, copy), want);
            assert!(dir.join(&name).exists(), "{name} が用意されること");
        }
    }

    #[test]
    fn place_executable_stops_at_the_first_success() {
        // 退行防止: 3 段を配列リテラルに並べると全部実行されてしまい、
        // ハードリンク成功後の copy(src, dst) が「同じ実体へのコピー」に
        // なって**本体バイナリを 0 バイトに切り詰める**。実機で踏んだ事故。
        use std::cell::Cell;
        let dir = crate::test_util::unique_temp_dir("zaivern-desktop-test", "lazy");
        let src = dir.join("real");
        std::fs::write(&src, "binary").unwrap();
        let dst = dir.join("Zaivern");
        let (sym_called, copy_called) = (Cell::new(false), Cell::new(false));
        let kind = place_executable_with(
            &src,
            &dst,
            |s: &Path, d: &Path| std::fs::hard_link(s, d),
            |_s: &Path, _d: &Path| {
                sym_called.set(true);
                Ok(())
            },
            |_s: &Path, _d: &Path| {
                copy_called.set(true);
                Ok(())
            },
        )
        .expect("配置");
        assert_eq!(kind, LinkKind::Hard);
        assert!(!sym_called.get(), "成功後にシンボリックリンクを試さないこと");
        assert!(!copy_called.get(), "成功後にコピーを試さないこと");
        // ハードリンク先も元も中身が生きていること (切り詰め事故の直接検知)
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "binary");
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "binary");
    }

    #[test]
    fn place_executable_reports_all_failures() {
        let dir = crate::test_util::unique_temp_dir("zaivern-desktop-test", "chain-fail");
        let err = chain_case(&dir, "exe-none", false, false, false).unwrap_err();
        for needle in ["ハードリンク", "シンボリックリンク", "コピー", "exe-none"] {
            assert!(err.contains(needle), "失敗理由に {needle} が無い: {err}");
        }
        assert!(!dir.join("exe-none").exists(), "全滅なら何も残さない");
    }

    #[test]
    fn place_executable_replaces_stale_launcher_script() {
        let dir = crate::test_util::unique_temp_dir("zaivern-desktop-test", "stale");
        let dst = dir.join("Zaivern");
        std::fs::write(&dst, "#!/bin/sh\nexec zai\n").unwrap(); // 旧レイアウトの残骸
        let src = dir.join("real");
        std::fs::write(&src, "binary").unwrap();
        let kind = place_executable_with(&src, &dst, ok_with("hard"), fail(), fail()).unwrap();
        assert_eq!(kind, LinkKind::Hard);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hard", "張り直されること");
    }

    #[test]
    fn place_executable_is_noop_when_src_is_dst() {
        // バンドル内バイナリから `zai app install` した場合に自分を消さないこと。
        let dir = crate::test_util::unique_temp_dir("zaivern-desktop-test", "self");
        let p = dir.join("Zaivern");
        std::fs::write(&p, "binary").unwrap();
        assert_eq!(place_executable_with(&p, &p, fail(), fail(), fail()), Ok(LinkKind::Hard));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "binary", "自分自身は無傷");
    }

    // ── バンドル一式のレイアウト: 作った物と、uninstall が全部消すこと ──

    #[test]
    fn bundle_layout_roundtrip_and_uninstall_cleans_up() {
        let root = crate::test_util::unique_temp_dir("zaivern-desktop-test", "bundle");
        let bin = root.join("bin/zai");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "ELF-ish").unwrap();
        let app = root.join(format!("{APP_NAME}.app"));
        // 旧レイアウトからの移行も同時に確かめる: ランチャースクリプトを置いておく。
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/zai"), "#!/bin/sh\nexec zai\n").unwrap();

        let kind = write_bundle(&app, &bin, "9.9.9", |s, d| {
            std::fs::hard_link(s, d).map(|_| LinkKind::Hard).map_err(|e| e.to_string())
        })
        .expect("bundle");
        assert_eq!(kind, LinkKind::Hard);

        let plist = app.join("Contents/Info.plist");
        let exe = app.join("Contents/MacOS").join(MACOS_EXEC_NAME);
        let icns = app.join("Contents/Resources/Zaivern.icns");
        assert!(plist.exists(), "Info.plist");
        assert!(exe.exists(), "Contents/MacOS/Zaivern が実体であること");
        assert!(icns.exists(), "アイコン");
        assert!(
            !app.join("Contents/MacOS/zai").exists(),
            "旧ランチャースクリプトは消えること"
        );
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<key>CFBundleExecutable</key><string>Zaivern</string>"));
        assert!(text.contains("<string>9.9.9</string>"));
        assert!(text.contains(APP_NAME));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "ELF-ish", "中身は本体と同じ");

        // uninstall は .app 配下を全部消し、リンク元のバイナリには触らない。
        assert!(remove_bundle(&app).expect("uninstall"), "消す物があった");
        assert!(!app.exists(), ".app が丸ごと消えること");
        assert!(bin.exists(), "リンク元の zai 本体は残ること");
        assert!(!remove_bundle(&app).expect("再 uninstall"), "2 回目は何もしない");
    }

    // ── .app 起動時の作業ディレクトリ補正 (ランチャーの `cd $HOME` の代替) ──

    #[test]
    fn app_launch_cwd_fix_only_for_bundle_launched_from_root() {
        let home = Path::new("/Users/u");
        let bundled = Path::new("/Users/u/Applications/Zaivern Code.app/Contents/MacOS/Zaivern");
        assert_eq!(
            app_launch_cwd_fix(bundled, Path::new("/"), home),
            Some(home.to_path_buf()),
            "Finder 起動 (cwd=/) はホームへ"
        );
        assert_eq!(
            app_launch_cwd_fix(bundled, Path::new("/Users/u/proj"), home),
            None,
            "ターミナルからバンドル実体を叩いた場合は cwd を触らない"
        );
        assert_eq!(
            app_launch_cwd_fix(Path::new("/usr/local/bin/zai"), Path::new("/"), home),
            None,
            "PATH 上の CLI は対象外 (`cd /` して `zai` を叩く自由を奪わない)"
        );
        assert_eq!(
            app_launch_cwd_fix(bundled, Path::new("/"), Path::new("/")),
            None,
            "ホームが / なら補正しない"
        );
    }

    #[test]
    fn normalize_app_launch_cwd_does_not_move_test_process() {
        // テストプロセスの exe はバンドル内ではないので cwd は動かないこと。
        let before = std::env::current_dir().expect("cwd");
        normalize_app_launch_cwd();
        assert_eq!(std::env::current_dir().expect("cwd"), before);
    }

    #[test]
    fn desktop_entry_has_required_fields() {
        let d = desktop_entry(Path::new("/home/u/.local/bin/zai"), Path::new("/home/u"));
        for needle in [
            "[Desktop Entry]",
            "Type=Application",
            // アプリメニューで「Zaivern」を検索して見つかる導線。
            // APP_NAME を経由せずリテラルでも固定しておく (退行防止)。
            "Name=Zaivern Code",
            &format!("Name={APP_NAME}"),
            "Exec=/home/u/.local/bin/zai %F",
            "Icon=zaivern-code",
            "Terminal=false",
            "Path=/home/u",
            "StartupWMClass=zaivern-code",
        ] {
            assert!(d.contains(needle), ".desktop に {needle} が無い");
        }
    }

    #[test]
    fn shortcut_ps_quotes_and_targets() {
        let s = shortcut_ps(
            Path::new(r"C:\Users\o'brien\Start Menu\Zaivern Code.lnk"),
            Path::new(r"C:\Users\o'brien\zai.exe"),
            Path::new(r"C:\Users\o'brien"),
            Path::new(r"C:\Users\o'brien\Zaivern.ico"),
        );
        assert!(s.contains("WScript.Shell"));
        assert!(s.contains(r"$s.TargetPath = 'C:\Users\o''brien\zai.exe'"), "' は '' に畳むこと");
        assert!(s.contains("$s.Save()"));
    }

    #[test]
    fn ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("a'b"), "a''b");
        assert_eq!(ps_quote("plain"), "plain");
    }

    // ── Windows: build.rs が埋める版情報リソースの契約 ──

    /// build.rs は cargo test の対象外 (ビルドスクリプト内の #[test] は走らない)
    /// ので、埋め込む内容の契約だけはソースを取り込んでここで固定する。
    /// タスクマネージャーの「説明」列に出るのは FileDescription なので、
    /// これが消えると Windows で「Zaivern」検索が効かなくなる。
    #[test]
    fn windows_version_resource_declares_expected_fields() {
        let src = include_str!("../build.rs");
        for needle in [
            r#"("FileDescription", "Zaivern Code")"#,
            r#"("ProductName", "Zaivern Code")"#,
            r#"("CompanyName", "#,
            r#"("OriginalFilename", "zai.exe")"#, // CLI 名は変えない
            "CARGO_PKG_VERSION",
            "FileVersion",
            "ProductVersion",
            "assets/Zaivern.ico",
            "cfg(windows)", // 他 OS のビルドに影響させない
        ] {
            assert!(src.contains(needle), "build.rs に {needle} が無い");
        }
        // rc.exe が無い環境でビルドを壊さない (warning へ落とす) こと。
        assert!(src.contains("cargo:warning="), "リソース失敗は fail-soft");
    }

    #[test]
    fn windows_icon_asset_is_a_valid_multi_size_ico() {
        let ico = include_bytes!("../assets/Zaivern.ico");
        assert_eq!(&ico[..4], &[0, 0, 1, 0], "ICONDIR (reserved=0, type=1)");
        let count = u16::from_le_bytes([ico[4], ico[5]]);
        assert!(count >= 2, "複数サイズを持つこと: {count}");
    }

    // ── ディスパッチ ──

    #[test]
    fn unknown_app_subcommand_fails() {
        assert_eq!(run(&["frobnicate".to_string()]), 1);
    }
}
