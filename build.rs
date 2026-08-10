//! ビルドスクリプト — **機能レジストリの生成**と **Windows の版情報リソース埋め込み**。
//!
//! ## 機能レジストリの生成
//!
//! `src/features/*.rs` を走査して `mod` 宣言と一覧を `OUT_DIR` へ生成する。
//! これにより**機能の追加が「新規ファイルを 1 つ作る」だけ**になり、共有
//! ファイルへの追記が消えるので、並列ブランチが構造的に衝突しなくなる。
//! 詳しくは [`generate_feature_registry`] と `src/features/mod.rs` を参照。
//!
//! ## Windows の版情報リソース
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
    generate_feature_registry();
    #[cfg(windows)]
    embed_windows_version_info();
}

/// `src/features/*.rs` を走査して、`mod` 宣言とレジストリ配列を生成する。
///
/// ## なぜコード生成なのか (実測の動機)
///
/// 機能を 1 つ足すのに、以前は `app.rs` と `palette.rs` の **5 箇所**を
/// 編集する必要があった。`src/feature.rs` のレジストリでそれを
/// 「共有リストへ 1 行追記」まで減らしたが、**追記が残る限り衝突は消えない**。
/// 実際に which-key と local_history が `config.rs` の同じ設定リストへ
/// 追記して 3 ハンクで衝突した。
///
/// **git が衝突を作るのは「2 つのブランチが同じファイルの近い行を触った」時
/// だけ**なので、共有ファイルを 1 バイトも触らせなければ衝突は構造的に
/// 起こり得ない。機能の追加を「`src/features/<名前>.rs` を新規作成する」
/// だけにするのがこの関数の目的。
///
/// 生成物は `OUT_DIR` に置き、`src/features/mod.rs` が `include!` する。
/// リポジトリへコミットしないので、生成物自体が衝突することもない。
fn generate_feature_registry() {
    // ディレクトリごと監視する。ファイルが増減したら再生成が要る。
    println!("cargo:rerun-if-changed=src/features");

    let dir = std::path::Path::new("src/features");
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // mod.rs は入れ物なので登録対象ではない
            if stem == "mod" {
                continue;
            }
            // 生成コードに入るので、識別子として妥当なものだけ通す。
            // (ここで弾かないと生成物がコンパイルエラーになり、原因が
            //  分かりにくい場所で失敗する)
            let ok = !stem.is_empty()
                && stem
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && stem.starts_with(|c: char| c.is_ascii_lowercase());
            if !ok {
                println!(
                    "cargo:warning=src/features/{stem}.rs は識別子として使えない名前なので登録しません (小文字・数字・_ のみ、先頭は小文字)"
                );
                continue;
            }
            // 個々のファイルも監視対象にしておく (中身だけ変えた時の取りこぼし防止)
            println!("cargo:rerun-if-changed=src/features/{stem}.rs");
            names.push(stem.to_string());
        }
    }
    // **並びを固定する。** read_dir の順序は OS とファイルシステムで変わるので、
    // 揃えないと生成物が環境ごとに変わり、パレットの並びも変わってしまう。
    names.sort();

    // `include!` された側の `mod x;` は **インクルード先 (OUT_DIR) を基準に**
    // ファイルを探すため、そのままでは `src/features/x.rs` を見つけられない
    // (実際に E0583 file not found for module で落ちた)。`#[path]` で実体を
    // 明示する。パスは `CARGO_MANIFEST_DIR` から導出するので直書きではない。
    // Windows のバックスラッシュは `#[path]` の文字列リテラルでエスケープが
    // 要るため、`/` へ寄せる (Windows も `/` を受け付ける)。
    //
    // **`OUT_DIR` を複数のワークツリーで共有すると、この焼き込みが牙を剥く。**
    // 生成物は「最後にビルドしたワークツリーの `src/features/*.rs`」を指すので、
    // 他のワークツリーがそれをコンパイルし、**自分のチェックアウトに存在しない
    // ファイルのエラー**が出る (別セッションが 2 回踏み、こちらでも
    // `.claude/worktrees/w-spec/...` を指す E0063 という幻を追った)。
    // 値が変わったらビルドスクリプトを回し直させて、生成物を作り直す。
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest = manifest.replace('\\', "/");

    let mut out =
        String::from("// build.rs が生成したファイル。手で編集しない (次のビルドで消える)。\n");
    for n in &names {
        out.push_str(&format!(
            "#[path = \"{manifest}/src/features/{n}.rs\"]\npub mod {n};\n"
        ));
    }
    out.push_str("\n/// build.rs が `src/features/*.rs` から集めた登録一覧。\npub const GENERATED: &[&crate::feature::Feature] = &[\n");
    for n in &names {
        out.push_str(&format!("    &{n}::FEATURE,\n"));
    }
    out.push_str("];\n");

    let dest = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("features_generated.rs");
    // 中身が同じなら書かない (mtime を動かすと下流が無駄に再ビルドされる)
    if std::fs::read_to_string(&dest).ok().as_deref() != Some(out.as_str()) {
        std::fs::write(&dest, out).expect("features_generated.rs を書けない");
    }
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
