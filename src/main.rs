#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_input;
mod agent_picker;
mod agents;
mod app;
mod breadcrumb;
mod cli;
mod commander;
mod config;
mod coordinator;
mod deck;
mod desktop;
mod diagnostician;
mod diagview;
mod diff;
mod editor;
mod editor_ops;
mod editor_split;
mod failover;
mod file_search;
mod file_tree;
mod find_buffer;
mod firewall;
mod follow;
mod fuzzy;
mod git;
mod git_panel;
mod github;
mod grammar;
mod highlight;
mod html;
mod i18n;
mod ide;
mod ignore;
mod instances;
mod jsonc;
mod kanban;
mod keybinds;
mod license;
mod lockx;
mod lsp;
mod markdown;
mod mcp;
mod menu_bar;
mod minimap;
mod notify;
mod orchestration;
mod palette;
mod panels;
mod pathx;
mod pet;
mod pet_bubble;
mod pet_variants;
mod plugins;
mod preview;
mod procx;
mod race;
mod recent;
mod remote;
mod session;
mod session_picker;
mod shellenv;
mod skills;
mod snippets;
mod sound;
mod supervisor;
mod tasks;
mod terminal;
#[cfg(test)]
mod test_util;
mod textenc;
mod theme;
mod theme_json;
mod tunnel;
mod tutorial;
mod voice;
mod worktree;
mod zoom;

use eframe::egui;

/// アプリアイコン(assets/Zaivern.png をバイナリに埋め込む)。
/// ウィンドウ/タスクバーアイコンとして 256px に縮小して使う。
/// 失敗してもアイコン無しで起動を続ける。
///
/// 縮小フィルタは Lanczos3。Windows はここで渡した 1 枚から
/// タイトルバー(16px)・タスクバー(24/32px)・Alt+Tab(48px 以上) を
/// その場で作るため、元画像がボケているとどの寸法でもガタつく。
/// Triangle(双一次)は縮小率が大きいほどエッジが甘くなるので、
/// 起動時 1 回だけのコストと引き換えに品質側を取る。
fn load_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(desktop::ICON_PNG).ok()?;
    let (w, h) = (img.width(), img.height());
    let img = if w == 256 && h == 256 {
        img
    } else {
        img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
    };
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    // どこで落ちても追えるよう、panic は必ず ~/.zaivern/panic.log にも残す。
    install_panic_log();

    // ps/top で見つけやすいプロセス名にする (Linux のみ実効。他 OS は
    // 実行ファイル名がそのままアクティビティモニタ等に出る)。
    instances::set_process_name();

    // サブコマンド指定なら CLI として処理して終了する。
    // 引数なし / パス指定のときは None が返り、そのまま GUI 起動へ進む。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::try_run_cli(&args) {
        std::process::exit(code);
    }

    // 子プロセスへ渡す PATH の解決を先に走らせておく。macOS の `.app` 起動では
    // ログインシェルへ問い合わせる必要があり (数百 ms)、エージェントを起動した
    // その瞬間にやると UI が固まって見えるため、ここで温めておく。
    shellenv::warm_up();

    // 引数はマルチルートワークスペースとして解釈する: `zai dirA dirB dirC`。
    // ディレクトリはルートに、ファイルは起動後に開くタブになる。
    // 存在しない引数・その他は黙って無視する（起動は止めない）。
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for a in &args {
        let p = std::path::PathBuf::from(a);
        if p.is_dir() {
            dirs.push(p);
        } else if p.is_file() {
            files.push(p);
        }
    }
    // 引数でフォルダを指定していない起動は「前回開いていたフォルダ」を開き直す。
    // 判定は recent::startup_folder（純粋関数）に集約している:
    //   引数指定あり → 復元しない / `--no-restore` か ZAIVERN_NO_RESTORE → 復元しない /
    //   それ以外は MRU (~/.zaivern/menu_state.toml) の先頭から実在するものを 1 つ。
    // menu_state.toml が無い・壊れている場合は None が返り、従来どおりカレントになる。
    if let Some(prev) = recent::startup_folder_for_launch(&dirs, &args) {
        dirs.push(prev);
    }

    // 引数無し = カレントディレクトリ（従来どおり）。roots は決して空にしない。
    let mut roots = file_tree::normalize_roots(dirs);
    if roots.is_empty() {
        roots = file_tree::normalize_roots(std::env::current_dir().ok());
    }
    if roots.is_empty() {
        roots.push(std::path::PathBuf::from("."));
    }

    // 実行中インスタンスとして登録する (~/.zaivern/instances/<pid>.json)。
    // `zai status` やスクリプトがどの OS でも「アプリが起動しているか」を
    // 検知できる経路になる。書けなくても起動は止めない (fail-soft)。
    let instance_guard = instances::register_current(&roots);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1480.0, 940.0])
        .with_min_inner_size([860.0, 560.0])
        .with_title("Zaivern Code")
        // Linux で .desktop (zaivern-code.desktop) と結び付ける ID。
        // desktop.rs の Icon= / StartupWMClass= と一致させること。
        .with_app_id("zaivern-code");
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        "Zaivern Code",
        options,
        Box::new(move |cc| Ok(Box::new(app::ZaivernApp::new(cc, roots, files)))),
    );
    // 正常終了: 自分のレジストリファイルを消す。panic の巻き戻しでも
    // RegistryGuard の Drop が同じ後始末をする (残骸はスキャン時にも掃除される)。
    drop(instance_guard);
    result
}

/// panic の詳細を `~/.zaivern/panic.log` へ追記するフックを仕込む。
///
/// GUI 起動 (ダブルクリック / Dock) では stderr がどこにも繋がっておらず、
/// 「アプリがいきなり落ちた」の原因が二度と追えなくなる。既定フックの前段で
/// メッセージとバックトレースをファイルへ残す。app.rs のフレームガードが
/// 捕捉して継続した panic も (フックは unwind より先に走るので) ここに残る。
fn install_panic_log() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = format!(
            "=== panic v{} at {} (epoch {secs}) ===\n{info}\n{bt}\n\n",
            env!("CARGO_PKG_VERSION"),
            utc_stamp(secs),
        );
        let dir = config::zaivern_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("panic.log");
        // 肥大したら .old へローテート (term_logs と同じ流儀)
        let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000);
        if too_big {
            let _ = std::fs::rename(&path, dir.join("panic.log.old"));
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(entry.as_bytes());
        }
        default_hook(info);
    }));
}

/// epoch 秒 → `YYYY-MM-DD HH:MM:SS UTC`。
/// panic フック内で使うので、依存クレートに頼らない最小実装
/// (civil_from_days アルゴリズム)。
fn utc_stamp(secs: u64) -> String {
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mth <= 2);
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

#[cfg(test)]
mod icon_tests {
    use super::load_icon;

    #[test]
    fn load_icon_yields_a_full_256px_rgba_image() {
        // Windows はこの 1 枚から各寸法のアイコンを生成する。
        // サイズ/バッファ長がズレるとタスクバーで崩れるので、そこだけ固定する。
        let icon = load_icon().expect("埋め込みアイコンを読めない");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        // 全面透明・全面単色ではない (縮小フィルタ変更でのつぶれ検出)。
        assert!(
            icon.rgba.chunks_exact(4).any(|p| p[3] > 0),
            "アイコンが完全に透明になっている"
        );
    }
}

#[cfg(test)]
mod panic_log_tests {
    use super::utc_stamp;

    #[test]
    fn utc_stamp_formats_known_moments() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_stamp(86_399), "1970-01-01 23:59:59 UTC");
        // 10 億秒 = 2001-09-09 01:46:40 UTC (よく知られた節目)
        assert_eq!(utc_stamp(1_000_000_000), "2001-09-09 01:46:40 UTC");
        // うるう年の 2/29 をまたぐ境界: 2024-02-29 00:00:00 UTC
        assert_eq!(utc_stamp(1_709_164_800), "2024-02-29 00:00:00 UTC");
    }
}
