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
//! どの OS でもショートカットの実体は「インストール済みバイナリへの参照」なので、
//! インストーラが同じ場所へ上書き更新すれば登録し直しは不要。
//! アプリとして起動されたとき (Finder / メニュー / スタートメニュー) は
//! 作業ディレクトリが `/` や system32 になるため、ホームを既定ワークスペースにする。

use std::path::{Path, PathBuf};

/// アプリアイコンの原本 (main.rs のウィンドウアイコンと共用)。
pub const ICON_PNG: &[u8] = include_bytes!("../assets/Zaivern.png");

/// OS に表示するアプリ名。
pub const APP_NAME: &str = "Zaivern Code";

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
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(strip_verbatim(exe))
}

/// Windows の canonicalize が付ける `\\?\` 接頭辞を外す
/// (.lnk の TargetPath 等に渡すと表示・解決が崩れるため)。他 OS では素通し。
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
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

/// macOS の Info.plist。CFBundleExecutable はランチャースクリプト "zai"。
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
    <key>CFBundleExecutable</key><string>zai</string>
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

/// macOS のランチャースクリプト。実体バイナリへ exec するだけの薄い殻。
/// バイナリを更新しても .app を作り直す必要が無いよう、コピーではなく参照にする。
#[allow(dead_code)]
fn launcher_script(bin: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # {APP_NAME} ランチャー (`zai app install` が自動生成)\n\
         # Finder 起動は作業ディレクトリが / になるため、ホームを既定ワークスペースにする。\n\
         BIN=\"{}\"\n\
         [ -x \"$BIN\" ] || BIN=\"$(command -v zai || true)\"\n\
         [ -n \"$BIN\" ] || exit 127\n\
         cd \"$HOME\" || true\n\
         exec \"$BIN\" \"$@\"\n",
        bin.display()
    )
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

#[cfg(target_os = "macos")]
fn install() -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let bin = resolve_bin()?;
    let app = app_bundle_path()?;
    let macos_dir = app.join("Contents/MacOS");
    let res_dir = app.join("Contents/Resources");
    std::fs::create_dir_all(&macos_dir)
        .and_then(|_| std::fs::create_dir_all(&res_dir))
        .map_err(|e| format!("{} を作成できません: {e}", app.display()))?;
    std::fs::write(
        app.join("Contents/Info.plist"),
        info_plist(env!("CARGO_PKG_VERSION")),
    )
    .map_err(|e| format!("Info.plist を書けません: {e}"))?;
    let launcher = macos_dir.join("zai");
    std::fs::write(&launcher, launcher_script(&bin))
        .map_err(|e| format!("ランチャーを書けません: {e}"))?;
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("実行権限を付与できません: {e}"))?;
    // アイコンは失敗しても登録自体は続行する
    if let Ok(icns) = icns_bytes(ICON_PNG) {
        let _ = std::fs::write(res_dir.join("Zaivern.icns"), icns);
    }
    // Launch Services へ即時登録 (失敗しても Launchpad の次回スキャンで拾われる)
    let _ = std::process::Command::new(LSREGISTER)
        .args(["-f", &app.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(format!(
        "✅ アプリとして登録しました: {}\n   Launchpad / Spotlight から「{APP_NAME}」で起動できます。",
        app.display()
    ))
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<String, String> {
    let app = app_bundle_path()?;
    if !app.exists() {
        return Ok("アプリ登録は見つかりませんでした (何もしていません)。".into());
    }
    std::fs::remove_dir_all(&app).map_err(|e| format!("{} を削除できません: {e}", app.display()))?;
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
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("powershell を実行できません: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ショートカットの作成に失敗しました: {}",
            String::from_utf8_lossy(&out.stderr).trim()
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
            "<key>CFBundleExecutable</key><string>zai</string>",
            "<key>CFBundleIconFile</key><string>Zaivern</string>",
            "<key>CFBundlePackageType</key><string>APPL</string>",
            "<string>9.9.9</string>",
            APP_NAME,
        ] {
            assert!(p.contains(needle), "Info.plist に {needle} が無い");
        }
    }

    #[test]
    fn launcher_script_execs_recorded_binary_from_home() {
        let s = launcher_script(Path::new("/opt/bin/zai"));
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("BIN=\"/opt/bin/zai\""));
        assert!(s.contains("cd \"$HOME\""), "Finder 起動 (cwd=/) の対策");
        assert!(s.contains("exec \"$BIN\" \"$@\""));
        assert!(s.contains("command -v zai"), "実体が消えても PATH から拾う");
    }

    #[test]
    fn desktop_entry_has_required_fields() {
        let d = desktop_entry(Path::new("/home/u/.local/bin/zai"), Path::new("/home/u"));
        for needle in [
            "[Desktop Entry]",
            "Type=Application",
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

    // ── ディスパッチ ──

    #[test]
    fn unknown_app_subcommand_fails() {
        assert_eq!(run(&["frobnicate".to_string()]), 1);
    }
}
