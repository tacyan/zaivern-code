//! UI 文字列の実行時翻訳。
//!
//! 入口は [`tr`] (そのままの文字列) と [`trf`] (`{name}` プレースホルダ入り
//! テンプレート) の 2 つだけ。**訳が無ければ渡された文字列をそのまま返す**ので、
//! 辞書が欠けても UI は壊れない。
//!
//! ## 2 通りのキーを同時に受ける
//!
//! | 渡すもの | 例 | 引き先 |
//! |---|---|---|
//! | **安定 ID** (推奨・新規コード) | `tr("agent.approve")` | `locales/<lang>.json` |
//! | **日本語の原文** (既存の 3000 箇所) | `tr("承認")` | `locales/ja.json` の逆引き経由 |
//!
//! 後者があるのは、このリポジトリの既存コードが原文そのものを渡しているから。
//! `locales/ja.json` は `ID → 日本語原文` なので、その逆引きを 1 枚作れば
//! **呼び出し側を 1 行も書き換えずに**全部の文字列が 6 言語になる。
//! 新しいコードは ID を使うこと (原文を書き換えても訳が生き残る)。
//!
//! ## 解決の順番
//! 1. 選択中の言語 (フォールバック連鎖は [`crate::locale::fallback_chain`])
//! 2. 言語プラグインの旧形式辞書 (`plugin.toml` の `[language] dict`、日本語→訳)
//! 3. 渡された文字列そのもの
//!
//! ## 設計メモ
//! - グローバル状態は `RwLock` **1 本**。egui は毎フレーム全ラベルを描き直すので、
//!   ロックの本数はそのまま 1 フレームあたりの取得回数になる。
//! - 言語の切り替えは辞書を作り直して差し替えるだけ。**再起動は要らない**。
//! - `trf` のテンプレートは Rust の `format!` を使わない独自置換。翻訳文字列は
//!   実行時に外部ファイルから来るため、`format!` のコンパイル時検証は使えない。
//!   置換に失敗しても panic せず、プレースホルダが残るだけに留める。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

/// 選択中の言語で組み上がった辞書。
#[derive(Default)]
struct Active {
    /// 実際に使っている言語 ID (`"ja"` / `"zh-CN"` / …)。
    id: String,
    /// 安定 ID → 訳文。フォールバック連鎖はここへ畳み込み済み。
    by_id: HashMap<String, String>,
    /// 日本語原文 → 訳文。**原文言語 (ja) のときは None** (引く必要が無い)。
    by_source: Option<HashMap<String, String>>,
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    /// 旧形式 (日本語原文をキーにした TOML) の言語プラグイン辞書。
    legacy: Option<HashMap<String, String>>,
}

static STATE: RwLock<Option<State>> = RwLock::new(None);

fn with_state<R>(f: impl FnOnce(&State) -> R, default: R) -> R {
    match STATE.read() {
        Ok(g) => match g.as_ref() {
            Some(s) => f(s),
            None => default,
        },
        Err(_) => default,
    }
}

fn mutate(f: impl FnOnce(&mut State)) {
    if let Ok(mut g) = STATE.write() {
        f(g.get_or_insert_with(State::default));
    }
}

/// 旧形式の翻訳辞書を差し替える。`None` で外す。
///
/// `plugin.toml` の `[language] dict`（日本語原文をキーにした TOML）用。
/// Language Pack (`locales/*.json`) が訳を持っている文字列はそちらが勝つ。
pub fn set_dict(dict: Option<HashMap<String, String>>) {
    mutate(|s| s.legacy = dict);
}

/// UI 言語を切り替える。戻り値は**読めなかった辞書の理由**（空なら全部読めた）。
///
/// `extra` はプラグインが供給する `locales` ディレクトリ。
pub fn set_locale(id: &str, extra: &[PathBuf]) -> Vec<String> {
    let mut errs = Vec::new();
    let by_id = crate::locale::resolved(id, extra, &mut errs);

    // 日本語原文 → 訳文の逆引き。原文言語そのものなら引く必要が無い。
    let by_source = if id == crate::locale::SOURCE_LANG {
        None
    } else {
        let src = crate::locale::load_one(crate::locale::SOURCE_LANG, extra, &mut errs);
        // **ID の順に畳んで、先に来たものを勝たせる。**
        // 同じ日本語原文を複数の ID が持つことがある (`"送信"` = `agent.send` /
        // `acp.sent` / `remote.send`)。HashMap の並びのまま入れると**実行のたびに
        // 勝者が変わり**、同じボタンが起動ごとに別の訳になる。並びを固定すれば
        // 決定的になる。さらに「同じ原文なら訳も同じ」を辞書側の不変条件に
        // してあるので (番人テスト `同じ原文を持つIDは訳も一致する`)、
        // どれが勝っても結果は変わらない。
        let mut sorted: Vec<(String, String)> = src.into_iter().collect();
        sorted.sort();
        let mut m: HashMap<String, String> = HashMap::with_capacity(sorted.len());
        for (key, ja) in sorted {
            match by_id.get(&key) {
                // 訳が原文と同じなら入れない (引く意味が無く、地図が太るだけ)
                Some(v) if v != &ja => {
                    m.entry(ja).or_insert_with(|| v.clone());
                }
                _ => {}
            }
        }
        Some(m)
    };

    mutate(|s| {
        s.active = Some(Active {
            id: id.to_string(),
            by_id,
            by_source,
        })
    });
    errs
}

/// UI 言語を外す（原文＝日本語へ戻す）。
#[allow(dead_code)]
pub fn clear_locale() {
    mutate(|s| s.active = None);
}

/// いま使っている言語 ID。未設定なら原文言語。
pub fn current() -> String {
    with_state(
        |s| {
            s.active
                .as_ref()
                .map(|a| a.id.clone())
                .unwrap_or_else(|| crate::locale::SOURCE_LANG.to_string())
        },
        crate::locale::SOURCE_LANG.to_string(),
    )
}

/// いま何らかの翻訳が効いているか。
pub fn active() -> bool {
    with_state(
        |s| {
            s.legacy.is_some()
                || s.active
                    .as_ref()
                    .is_some_and(|a| a.id != crate::locale::SOURCE_LANG)
        },
        false,
    )
}

/// 文字列を翻訳する。訳が無ければ渡されたものをそのまま返す。
///
/// `s` は**安定 ID** (`"agent.approve"`) でも**日本語の原文** (`"承認"`) でもよい。
pub fn tr(s: &str) -> String {
    let hit = with_state(
        |st| {
            if let Some(a) = st.active.as_ref() {
                if let Some(v) = a.by_id.get(s) {
                    return Some(v.clone());
                }
                if let Some(m) = a.by_source.as_ref() {
                    if let Some(v) = m.get(s) {
                        return Some(v.clone());
                    }
                }
            }
            st.legacy.as_ref().and_then(|d| d.get(s)).cloned()
        },
        None,
    );
    match hit {
        Some(v) => v,
        None => {
            trace_missing(s);
            s.to_string()
        }
    }
}

/// `{name}` プレースホルダ入りテンプレートを翻訳して埋める。
///
/// 翻訳後の文字列に含まれる `{name}` を args の値で置換するので、言語ごとに
/// 語順を変えられる。訳が無ければ原文テンプレートに対して同じ置換を行う。
pub fn trf(template: &str, args: &[(&str, String)]) -> String {
    let mut s = tr(template);
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// 訳文に残る**位置プレースホルダ** `{}` を、渡された順に埋める。
///
/// [`trf`] が置換するのは `{name}` だけだが、辞書の原文は `format!` の位置指定を
/// そのまま持っていることがある (`"{} を作成できません: {e}"` 等)。
/// **辞書は原文そのものが鍵**なので綴りを名前付きへ変えることはできず、
/// 訳したあとにここで埋める。
///
/// 分割して繋ぎ直すので、埋めた値の中に `{}` が入っていても次の穴と取り違えない。
/// 穴が足りなければ余った値は捨て、多ければ `{}` のまま残す
/// (訳が壊れていても panic しない — 訳の事故で機能を止めない)。
///
/// 使い方は `fill_positional(&trf(原文, 名前付き), &[位置引数…])`。
pub fn fill_positional(translated: &str, args: &[String]) -> String {
    let mut parts = translated.split("{}");
    let mut out = String::with_capacity(translated.len() + 32);
    out.push_str(parts.next().unwrap_or(""));
    for (i, tail) in parts.enumerate() {
        match args.get(i) {
            Some(v) => out.push_str(v),
            None => out.push_str("{}"),
        }
        out.push_str(tail);
    }
    out
}

/// 接頭辞に一致する ID だけを取り出す（スマホ側 UI へ渡す辞書など）。
/// 値は選択中の言語で解決済み。
pub fn export_prefix(prefix: &str) -> BTreeMap<String, String> {
    with_state(
        |s| match s.active.as_ref() {
            Some(a) => a
                .by_id
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            None => BTreeMap::new(),
        },
        BTreeMap::new(),
    )
}

// ─── 訳漏れの記録 (翻訳者向け) ──────────────────────────────────

/// `ZAIVERN_I18N_TRACE=1` のときだけ、訳が無かった文字列を溜める。
/// 既定では**判定 1 回ぶんの費用しか払わない** (OnceLock の読み取り)。
fn trace_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("ZAIVERN_I18N_TRACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

static MISSING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn trace_missing(s: &str) {
    if !trace_on() {
        return;
    }
    if let Ok(mut g) = MISSING.lock() {
        if g.len() < 20_000 {
            g.insert(s.to_string());
        }
    }
}

/// 溜まった訳漏れを取り出す (`ZAIVERN_I18N_TRACE=1` のときだけ中身がある)。
pub fn missing_keys() -> Vec<String> {
    MISSING
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default()
}

// ─── 旧形式 (TOML) 辞書の読み込み ───────────────────────────────

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
        .filter(|p| p.is_file() && p.extension().map(|x| x == "toml").unwrap_or(false))
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
    use std::sync::Mutex as StdMutex;

    /// **どの辞書にも絶対に入らない**番兵。「訳されないこと」を確かめる検査は
    /// 必ずこれを使う。実在しそうな日本語を使うと、辞書が育った日に静かに
    /// 訳されて検査が意味を失う (実際に「この文字列は辞書に存在しない」が
    /// 辞書へ入って落ちた)。
    const ABSENT: &str = "__zaivern_i18n_absent_probe__";

    /// グローバル辞書を触るテストの直列化。並走すると他のテストの tr() 結果が
    /// 揺れるため、辞書を入れるテストは必ずこのロックを取り、抜ける前に
    /// 元へ戻す。
    static GLOBAL: StdMutex<()> = StdMutex::new(());

    fn reset() {
        set_dict(None);
        clear_locale();
    }

    fn dict(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn 辞書なしなら原文のまま() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        assert_eq!(tr("設定"), "設定");
        assert!(!active());
        reset();
    }

    #[test]
    fn 旧形式辞書があれば訳し無ければ原文へフォールバック() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        set_dict(Some(dict(&[("設定", "Settings")])));
        assert_eq!(tr("設定"), "Settings");
        // 訳漏れは日本語のまま = UI が壊れない
        assert_eq!(tr(ABSENT), ABSENT);
        assert!(active());
        reset();
    }

    #[test]
    fn trfは語順を変えられる() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
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
        reset();
    }

    // ---- Language Pack (locales/*.json) ------------------------------------

    #[test]
    fn 言語パックは識別子でも日本語原文でも引ける() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        // 同梱の en へ切り替える
        let errs = set_locale("en", &[]);
        assert!(errs.is_empty(), "同梱辞書が読めない: {errs:?}");
        assert_eq!(current(), "en");
        assert!(active());
        // ID で引ける
        assert_eq!(tr("app.settings"), "Settings");
        // 日本語原文でも引ける (既存の 3000 箇所がそのまま英語になる)
        assert_eq!(tr("設定"), "Settings");
        // 知らないものは渡されたものがそのまま返る
        assert_eq!(tr(ABSENT), ABSENT);
        assert_eq!(tr("zzz.not.an.id"), "zzz.not.an.id");
        reset();
    }

    #[test]
    fn 原文言語では逆引きを作らない() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        set_locale("ja", &[]);
        assert_eq!(current(), "ja");
        // 原文言語なので「翻訳が効いている」とは言わない
        assert!(!active());
        // ID は引ける
        assert_eq!(tr("app.settings"), "設定");
        // 原文はそのまま
        assert_eq!(tr("設定"), "設定");
        reset();
    }

    #[test]
    fn 全同梱言語で識別子が引けて空でない() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        for (id, _, _) in crate::locale::BUILTIN {
            let errs = set_locale(id, &[]);
            assert!(errs.is_empty(), "{id}: {errs:?}");
            let v = tr("app.settings");
            assert!(!v.trim().is_empty(), "{id}: app.settings が空");
            assert_ne!(v, "app.settings", "{id}: app.settings が未翻訳");
        }
        reset();
    }

    #[test]
    fn 言語パックは旧形式辞書より優先される() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        // english-mode 相当の旧辞書 (日本語→英語) を入れたうえで中国語にする
        set_dict(Some(dict(&[("設定", "Settings")])));
        set_locale("zh-CN", &[]);
        let v = tr("設定");
        assert_ne!(v, "Settings", "旧辞書が Language Pack を上書きしている");
        assert_ne!(v, "設定", "中国語の訳が引けていない");
        // 言語パックが知らない文字列は旧辞書が拾う (後方互換)
        set_dict(Some(dict(&[(ABSENT, "fallback")])));
        assert_eq!(tr(ABSENT), "fallback");
        reset();
    }

    #[test]
    fn 接頭辞で書き出せる() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        set_locale("en", &[]);
        let m = export_prefix("remote.");
        assert!(!m.is_empty(), "remote.* が 1 件も無い");
        assert!(m.keys().all(|k| k.starts_with("remote.")));
        reset();
    }

    #[test]
    fn ユーザーのlocalesディレクトリが同梱を上書きする() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let root = crate::test_util::unique_temp_dir("zaivern-i18n", "user-locales");
        let dir = root.join("plug");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("en.json"),
            r#"{"app.settings": "Preferences", "zz.only.here": "extra"}"#,
        )
        .unwrap();
        let extra = vec![dir.clone()];
        let errs = set_locale("en", &extra);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(tr("app.settings"), "Preferences");
        assert_eq!(tr("zz.only.here"), "extra");
        // 上書きは逆引きにも効く (日本語原文からも新しい訳が出る)
        assert_eq!(tr("設定"), "Preferences");
        reset();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn コミュニティ言語はファイルを置くだけで足せる() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let root = crate::test_util::unique_temp_dir("zaivern-i18n", "community");
        let dir = root.join("locales");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fr.json"), r#"{"app.settings": "Paramètres"}"#).unwrap();
        let extra = vec![dir.clone()];
        // fr は同梱に無い。一覧に出る
        let list = crate::locale::available(&extra);
        assert!(list.iter().any(|i| i.id == "fr" && !i.builtin));
        // 切り替えると訳が効き、欠けているものは en へ落ちる
        let errs = set_locale("fr", &extra);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(tr("app.settings"), "Paramètres");
        assert_eq!(tr("agent.send"), tr_in_en("agent.send"));
        reset();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// テスト用: 同梱 en の値を直に引く。
    fn tr_in_en(key: &str) -> String {
        let mut errs = Vec::new();
        crate::locale::load_one("en", &[], &mut errs)
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    #[test]
    fn 壊れたユーザー辞書は理由を返して同梱へ落ちる() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let root = crate::test_util::unique_temp_dir("zaivern-i18n", "broken");
        let dir = root.join("locales");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("en.json"), "{ this is not json").unwrap();
        let errs = set_locale("en", &[dir.clone()]);
        assert_eq!(errs.len(), 1, "理由が返っていない: {errs:?}");
        // 壊れていても同梱ぶんは生きている (UI は英語のまま動く)
        assert_eq!(tr("app.settings"), "Settings");
        reset();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 辞書ファイルとディレクトリを読める() {
        let root = crate::test_util::unique_temp_dir("zaivern-i18n", "dict");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("10-a.toml"),
            "\"開く\" = \"Open\"\n\"閉じる\" = \"Close\"\n",
        )
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
        let root = crate::test_util::unique_temp_dir("zaivern-i18n", "bad");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("bad.toml"), "\"開く\" = 42\n").unwrap();
        assert!(load_dict_file(&root.join("bad.toml")).is_err());
        // 存在しないパスもエラー
        assert!(load_dict(&root.join("nope")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 位置プレースホルダは順に埋まり壊れても止まらない() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            fill_positional("{} を作成できません: X", &a(&["/tmp/a"])),
            "/tmp/a を作成できません: X"
        );
        // 順に埋まる
        assert_eq!(fill_positional("{}/{}", &a(&["a", "b"])), "a/b");
        // 埋めた値の中の `{}` を次の穴と取り違えない
        assert_eq!(fill_positional("{}/{}", &a(&["{}", "b"])), "{}/b");
        // 穴が多ければそのまま残す (panic しない)
        assert_eq!(fill_positional("{}/{}", &a(&["a"])), "a/{}");
        // 値が余れば捨てる
        assert_eq!(fill_positional("{}", &a(&["a", "b"])), "a");
        // 穴が無ければ素通し
        assert_eq!(fill_positional("穴なし", &a(&["a"])), "穴なし");
    }

    #[test]
    fn trfは未指定プレースホルダをそのまま残す() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        // 現挙動の固定: args に無い {b} は消えず残る (panic もしない)
        assert_eq!(trf("{a} と {b}", &[("a", "X".to_string())]), "X と {b}");
        // args が空ならテンプレートがそのまま返る
        assert_eq!(trf("{n} 件", &[]), "{n} 件");
        // テンプレートに無い args は無視される
        assert_eq!(trf("固定文", &[("n", "9".to_string())]), "固定文");
        reset();
    }
}
