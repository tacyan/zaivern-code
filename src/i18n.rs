//! UI 文字列の実行時翻訳。
//!
//! **日本語の原文そのものを辞書キー**にする gettext 方式。訳が無ければ原文を
//! そのまま返すので、辞書が欠けても UI は日本語で表示され続ける (壊れない)。
//!
//! 辞書は言語プラグイン (`plugin.toml` の `[language]`) が供給し、
//! [`set_dict`] で差し替える。プラグインを無効にすれば `None` に戻って日本語へ
//! 復帰する。呼び出し側は [`tr`] (そのままの文字列) と [`trf`]
//! (`{name}` プレースホルダ入りテンプレート) だけを使う。
//!
//! ## 設計メモ
//! - グローバル状態は `RwLock` 1 本。egui は毎フレーム全ラベルを描き直すので、
//!   読み取りロック + HashMap 参照のコストは format! と同程度で問題にならない。
//! - `trf` のテンプレートは Rust の `format!` を使わない独自置換。翻訳文字列は
//!   実行時に外部ファイルから来るため、`format!` のコンパイル時検証は使えない。
//!   置換に失敗しても panic せず、プレースホルダが残るだけに留める。

use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLock;

/// 現在の翻訳辞書。`None` = 翻訳なし (原文 = 日本語のまま)。
static DICT: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

/// 翻訳辞書を差し替える。`None` で日本語へ戻す。
pub fn set_dict(dict: Option<HashMap<String, String>>) {
    if let Ok(mut g) = DICT.write() {
        *g = dict;
    }
}

/// いま翻訳が有効か (辞書が入っているか)。
#[allow(dead_code)]
pub fn active() -> bool {
    DICT.read().map(|g| g.is_some()).unwrap_or(false)
}

/// 文字列を翻訳する。辞書に無ければ原文をそのまま返す。
pub fn tr(s: &str) -> String {
    if let Ok(g) = DICT.read() {
        if let Some(d) = g.as_ref() {
            if let Some(t) = d.get(s) {
                return t.clone();
            }
        }
    }
    s.to_string()
}

/// `{name}` プレースホルダ入りテンプレートを翻訳して埋める。
///
/// 辞書キーはプレースホルダを**含んだ原文そのまま** (例: `"{n} 件を保存"`)。
/// 翻訳後の文字列に含まれる `{name}` を args の値で置換するので、言語ごとに
/// 語順を変えられる。訳が無ければ原文テンプレートに対して同じ置換を行う。
pub fn trf(template: &str, args: &[(&str, String)]) -> String {
    let mut s = tr(template);
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// 辞書ファイル (TOML: `"原文" = "訳文"` の平テーブル) を 1 枚読む。
///
/// 文字列以外の値・入れ子テーブルはエラーにする (書き間違いを黙って捨てない)。
pub fn load_dict_file(path: &Path) -> Result<HashMap<String, String>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("{} を読めません: {e}", path.display()))?;
    let table: toml::Table =
        toml::from_str(&raw).map_err(|e| format!("{} の解析に失敗: {e}", path.display()))?;
    let mut out = HashMap::new();
    for (k, v) in table {
        match v {
            toml::Value::String(s) => {
                out.insert(k, s);
            }
            other => {
                return Err(format!(
                    "{}: キー {k:?} の値が文字列ではありません: {other}",
                    path.display()
                ))
            }
        }
    }
    Ok(out)
}

/// 辞書のパスを読む。ディレクトリなら直下の `*.toml` を**ファイル名順**に
/// 読んで合成する (後勝ち)。ファイルならそれ 1 枚。
///
/// ファイル名順に固定するのは、同じキーが複数ファイルにあったときの勝敗を
/// 環境に依らず決めるため。
pub fn load_dict(path: &Path) -> Result<HashMap<String, String>, String> {
    if path.is_file() {
        return load_dict_file(path);
    }
    if !path.is_dir() {
        return Err(format!("辞書が見つかりません: {}", path.display()));
    }
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("{} を読めません: {e}", path.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file() && p.extension().map(|x| x == "toml").unwrap_or(false)
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("{} に辞書 (*.toml) がありません", path.display()));
    }
    let mut out = HashMap::new();
    for f in &files {
        out.extend(load_dict_file(f)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// グローバル辞書を触るテストの直列化。並走すると他のテストの tr() 結果が
    /// 揺れるため、辞書を入れるテストは必ずこのロックを取り、抜ける前に None へ
    /// 戻す。
    static GLOBAL: Mutex<()> = Mutex::new(());

    fn dict(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn 辞書なしなら原文のまま() {
        let _g = GLOBAL.lock().unwrap();
        set_dict(None);
        assert_eq!(tr("設定"), "設定");
        assert!(!active());
    }

    #[test]
    fn 辞書があれば訳し無ければ原文へフォールバック() {
        let _g = GLOBAL.lock().unwrap();
        set_dict(Some(dict(&[("設定", "Settings")])));
        assert_eq!(tr("設定"), "Settings");
        // 訳漏れは日本語のまま = UI が壊れない
        assert_eq!(tr("未翻訳の文字列"), "未翻訳の文字列");
        assert!(active());
        set_dict(None);
    }

    #[test]
    fn trfは語順を変えられる() {
        let _g = GLOBAL.lock().unwrap();
        set_dict(Some(dict(&[("{n} 件を保存しました", "Saved {n} files")])));
        assert_eq!(
            trf("{n} 件を保存しました", &[("n", "3".to_string())]),
            "Saved 3 files"
        );
        // 訳が無いテンプレートも同じ置換が効く
        assert_eq!(
            trf("{x} を開く", &[("x", "a.rs".to_string())]),
            "a.rs を開く"
        );
        set_dict(None);
    }

    #[test]
    fn 辞書ファイルとディレクトリを読める() {
        let root = std::env::temp_dir().join(format!("zai-i18n-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("10-a.toml"), "\"開く\" = \"Open\"\n\"閉じる\" = \"Close\"\n")
            .unwrap();
        std::fs::write(root.join("20-b.toml"), "\"閉じる\" = \"Close!\"\n").unwrap();
        std::fs::write(root.join("readme.txt"), "not a dict").unwrap();

        // 1 枚読み
        let one = load_dict_file(&root.join("10-a.toml")).unwrap();
        assert_eq!(one.get("開く").unwrap(), "Open");

        // ディレクトリはファイル名順の後勝ち
        let all = load_dict(&root).unwrap();
        assert_eq!(all.get("開く").unwrap(), "Open");
        assert_eq!(all.get("閉じる").unwrap(), "Close!");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 文字列以外の値はエラーにする() {
        let root = std::env::temp_dir().join(format!("zai-i18n-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("bad.toml"), "\"開く\" = 42\n").unwrap();
        assert!(load_dict_file(&root.join("bad.toml")).is_err());
        // 存在しないパスもエラー
        assert!(load_dict(&root.join("nope")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
