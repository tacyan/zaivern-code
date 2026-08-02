//! ビルドスクリプト — いまのところ **Windows の版情報リソース埋め込み専用**。
//!
//! Windows のタスクマネージャーは実行ファイル名 (`zai.exe`) の隣に
//! VERSIONINFO リソースの `FileDescription` を「説明」列として出す。
//! リソースが無いとこの列が空になり、ユーザーが「Zaivern」で探しても
//! 何も引っかからない。そこでここで版情報とアイコンを埋め込む。
//!
//! 設計上の約束:
//!   * winresource は `[target.'cfg(windows)'.build-dependencies]` にしか
//!     居ないので、macOS / Linux の `cargo build` には**一切**入らない。
//!     このファイルも `#[cfg(windows)]` の外側では何もしない。
//!   * リソースのコンパイルには rc.exe (MSVC) / windres (GNU) が要る。
//!     見つからない環境でビルドを壊さないよう、失敗は warning に落として
//!     続行する — 版情報が無いだけで、動く exe はできる。
//!   * CLI 名 `zai` は変えない。`OriginalFilename` も `zai.exe` のまま。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/Zaivern.ico");
    #[cfg(windows)]
    embed_windows_version_info();
}

/// 埋め込む文字列フィールド。タスクマネージャー / エクスプローラーの
/// プロパティに出る値で、「Zaivern」で検索して見つかる導線そのもの。
#[cfg(windows)]
const FIELDS: &[(&str, &str)] = &[
    ("FileDescription", "Zaivern Code"),
    ("ProductName", "Zaivern Code"),
    ("CompanyName", "Zaivern Code Project"),
    ("InternalName", "zai.exe"),
    ("OriginalFilename", "zai.exe"),
    ("LegalCopyright", "Licensed under Apache-2.0"),
];

/// `"0.4.15"` → `("0.4.15.0", 0x0000_0004_000F_0000)`。
///
/// Windows の VERSIONINFO は 16bit×4 の数値と、それとは別の文字列を持つ。
/// Cargo の版は 3 桁 (+ プレリリース) なので 4 桁目を 0 で埋め、
/// `-rc.1` のような非数値部分は 0 に潰す (数値フィールドは数字しか持てない)。
#[cfg(windows)]
fn version_quad(v: &str) -> (String, u64) {
    let mut parts = [0u16; 4];
    for (i, seg) in v.split(['.', '-', '+']).take(4).enumerate() {
        let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
        parts[i] = digits.parse().unwrap_or(0);
    }
    let text = format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3]);
    let packed = ((parts[0] as u64) << 48)
        | ((parts[1] as u64) << 32)
        | ((parts[2] as u64) << 16)
        | (parts[3] as u64);
    (text, packed)
}

#[cfg(windows)]
fn embed_windows_version_info() {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let (text, packed) = version_quad(&pkg);

    let mut res = winresource::WindowsResource::new();
    for (key, value) in FIELDS {
        res.set(key, value);
    }
    res.set("FileVersion", &text);
    res.set("ProductVersion", &text);
    res.set_version_info(winresource::VersionInfo::FILEVERSION, packed);
    res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);

    // アイコンはリポジトリに置いてある multi-size .ico (assets/Zaivern.ico)。
    // ビルド時に画像クレートへ依存しないよう、事前生成した成果物を使う。
    let ico = std::path::Path::new("assets/Zaivern.ico");
    if ico.exists() {
        res.set_icon(&ico.to_string_lossy());
    } else {
        println!("cargo:warning=assets/Zaivern.ico が無いのでアイコンは埋め込みません");
    }

    if let Err(e) = res.compile() {
        // rc.exe / windres が無い環境でもビルド自体は通す (fail-soft)。
        println!("cargo:warning=Windows 版情報リソースを埋め込めませんでした: {e}");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    // build.rs のテストは `cargo test` では走らない。手で回すときは
    //   rustc --test build.rs -o /tmp/buildrs-test && /tmp/buildrs-test
    // 実際の値の契約 (FIELDS に何が載っているか) は
    // src/desktop.rs の windows_version_resource_declares_expected_fields が
    // このファイルを include_str! して検証している。
    #[test]
    fn version_quad_pads_and_packs() {
        assert_eq!(version_quad("0.4.15").0, "0.4.15.0");
        assert_eq!(
            version_quad("0.4.15").1,
            (0u64 << 48) | (4 << 32) | (15 << 16)
        );
        assert_eq!(version_quad("1.2.3.4").0, "1.2.3.4");
        assert_eq!(version_quad("1.2.3-rc.1").0, "1.2.3.0", "非数値部分は 0");
        assert_eq!(version_quad("").0, "0.0.0.0");
    }
}
