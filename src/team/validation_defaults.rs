//! SPEC が検証コマンドを書いていないときに、**リポジトリの実体を見て**
//! 候補を決める層。
//!
//! ## なぜ固定値にしないのか
//!
//! 0.23 までは `cargo fmt --check` / `cargo test` の 2 本を固定で返していた。
//! Zaivern は任意のリポジトリで動くので、**Next.js のリポジトリで
//! `cargo test` を走らせる**という嘘が出る。しかも「検証を実行した」と
//! いう記録だけは残るので、完了の関門が素通りになる。
//!
//! ## 決め方
//!
//! 目印になるファイルの**存在**だけで決める。中身の解釈は
//! `package.json` の `scripts` だけで、それも**書いてあるものを読む**
//! だけ — 無いスクリプトを推測して作らない (`npm test` が定義されて
//! いないリポジトリで `npm test` を候補にすると、検証は必ず失敗する)。
//!
//! ## 決められないときは決めない
//!
//! 目印が 1 つも無ければ [`DetectError::Undetermined`]。**勝手に
//! Rust 扱いへ倒さない** — 「何となく動かす」は fail-closed の反対である。
//!
//! ここが返すのは**文字列の候補**でしかない。危険度の判定と承認ゲートは
//! 従来どおり [`super::graph::parse_command`] 以降が持つ (第 2 の判定を
//! 作らない)。

use std::path::Path;

/// 候補を決められなかった理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetectError {
    /// 目印が 1 つも見つからない。
    Undetermined,
    /// 目印はあるが、そこから候補を決められない
    /// (`package.json` に test / lint 系のスクリプトが無い、など)。
    NoCandidate { marker: String },
    /// 目印が壊れていて読めない。**読めないものを黙って無視しない。**
    Unreadable { marker: String, reason: String },
}

impl DetectError {
    pub fn detail(&self) -> String {
        match self {
            DetectError::Undetermined => "検証コマンドを自動決定できません \
                 (Cargo.toml / go.mod / package.json などの目印が見つかりません)。\
                 SPEC の「検証」セクションに検証コマンドを書いてください"
                .to_string(),
            DetectError::NoCandidate { marker } => format!(
                "検証コマンドを自動決定できません ({marker} はありますが、\
                 実行できる検証が見つかりません)。\
                 SPEC の「検証」セクションに検証コマンドを書いてください"
            ),
            DetectError::Unreadable { marker, reason } => format!(
                "{marker} を読めません ({reason})。\
                 SPEC の「検証」セクションに検証コマンドを書いてください"
            ),
        }
    }
}

/// `package.json` の `scripts` から候補にしてよい名前 (**この順**)。
///
/// 「テストがあるなら必ずテスト」を先頭に置く。lint / 型検査は
/// テストの代わりにはならないが、**何も無いよりは検証になる**。
const NODE_SCRIPTS: &[&str] = &["test", "lint", "typecheck", "check"];

/// lockfile → パッケージマネージャ。
///
/// **綴りで決める。** ここに無い lockfile は「知らない」であって
/// 「npm だろう」ではない。
const NODE_LOCKFILES: &[(&str, &str)] = &[
    ("pnpm-lock.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("bun.lockb", "bun"),
    ("bun.lock", "bun"),
    ("package-lock.json", "npm"),
];

/// Python で `pytest` を使っていると**言い切れる**証拠。
///
/// `requirements.txt` があるだけでは足りない (Django の `manage.py test` かも
/// しれないし、テストが無いかもしれない)。**推測しすぎない。**
fn python_uses_pytest(ws: &Path) -> bool {
    // 専用の設定ファイルがあれば、それ自体が証拠。
    if ws.join("pytest.ini").is_file() {
        return true;
    }
    for (name, needle) in [
        ("pyproject.toml", "pytest"),
        ("setup.cfg", "[tool:pytest]"),
        ("tox.ini", "[pytest]"),
        ("requirements.txt", "pytest"),
        ("requirements-dev.txt", "pytest"),
    ] {
        if let Ok(body) = std::fs::read_to_string(ws.join(name)) {
            if body.contains(needle) {
                return true;
            }
        }
    }
    false
}

/// Python プロジェクトの目印 (pytest を使うかは別に見る)。
fn python_marker(ws: &Path) -> Option<&'static str> {
    [
        "pyproject.toml",
        "pytest.ini",
        "setup.cfg",
        "setup.py",
        "requirements.txt",
    ]
    .into_iter()
    .find(|name| ws.join(name).is_file())
}

/// `package.json` を読んで候補を組む。
///
/// **実在する script だけ**を候補にする。JSON が壊れていれば
/// [`DetectError::Unreadable`] — 黙って「候補なし」にすると、
/// 壊れた設定が「目印が無い」と同じ扱いになって気付けない。
fn node_candidates(ws: &Path) -> Result<Vec<String>, DetectError> {
    let path = ws.join("package.json");
    let body = std::fs::read_to_string(&path).map_err(|e| DetectError::Unreadable {
        marker: "package.json".into(),
        reason: e.to_string(),
    })?;
    let doc: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| DetectError::Unreadable {
            marker: "package.json".into(),
            reason: e.to_string(),
        })?;
    let scripts = doc.get("scripts").and_then(|v| v.as_object());
    let Some(scripts) = scripts else {
        return Err(DetectError::NoCandidate {
            marker: "package.json".into(),
        });
    };

    // **パッケージマネージャは lockfile から決める。**
    // 2 つ以上あるなら、どれで入れたのか分からない — 決めない。
    let found: Vec<&str> = NODE_LOCKFILES
        .iter()
        .filter(|(f, _)| ws.join(f).is_file())
        .map(|(_, pm)| *pm)
        .collect();
    let mut uniq: Vec<&str> = Vec::new();
    for pm in found {
        if !uniq.contains(&pm) {
            uniq.push(pm);
        }
    }
    let pm = match uniq.as_slice() {
        [one] => *one,
        // lockfile が無い / 複数ある。**勝手に選ばない。**
        _ => {
            return Err(DetectError::NoCandidate {
                marker: "package.json".into(),
            })
        }
    };

    let mut out = Vec::new();
    for name in NODE_SCRIPTS {
        if scripts.get(*name).is_some() {
            // `npm run test` は `npm test` と同じものを、**どの
            // パッケージマネージャでも同じ綴りで**呼べる。
            out.push(format!("{pm} run {name}"));
        }
    }
    if out.is_empty() {
        return Err(DetectError::NoCandidate {
            marker: "package.json".into(),
        });
    }
    Ok(out)
}

/// ワークスペースを見て、検証コマンドの候補を決める。
///
/// **決められないときは決めない。** 返る文字列はまだ検証されていない
/// 候補なので、呼び出し側は必ず [`super::graph::parse_command`] を通す。
pub fn detect(ws: &Path) -> Result<Vec<String>, DetectError> {
    // **空のパスは「どのリポジトリでもない」。**
    //
    // `Path::new("").join("Cargo.toml")` は `"Cargo.toml"` という相対パスに
    // なるので、そのまま `is_file()` を呼ぶと**プロセスの作業ディレクトリ**を
    // 見てしまう。Zaivern 自身が Rust リポジトリなので、テストでも実行時でも
    // 「たまたま Rust だと判定される」という嘘が出る。
    if ws.as_os_str().is_empty() {
        return Err(DetectError::Undetermined);
    }
    let mut out: Vec<String> = Vec::new();
    // 目印はあったが候補が出せなかった、を覚えておく
    // (「目印が無い」と「目印はあるが決められない」は別の話をする)。
    let mut blocked: Option<DetectError> = None;

    if ws.join("Cargo.toml").is_file() {
        out.push("cargo fmt --check".to_string());
        out.push("cargo test".to_string());
    }
    if ws.join("go.mod").is_file() {
        out.push("go test ./...".to_string());
    }
    if ws.join("package.json").is_file() {
        match node_candidates(ws) {
            Ok(mut c) => out.append(&mut c),
            Err(e) => blocked = Some(e),
        }
    }
    if let Some(marker) = python_marker(ws) {
        if python_uses_pytest(ws) {
            out.push("pytest".to_string());
        } else if blocked.is_none() {
            blocked = Some(DetectError::NoCandidate {
                marker: marker.to_string(),
            });
        }
    }

    if !out.is_empty() {
        return Ok(out);
    }
    Err(blocked.unwrap_or(DetectError::Undetermined))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> std::path::PathBuf {
        let d = crate::test_util::unique_temp_dir("zaivern-team-detect", name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(d: &Path, name: &str, body: &str) {
        std::fs::write(d.join(name), body).unwrap();
    }

    #[test]
    fn 空のパスはどのリポジトリでもない() {
        // 相対パスとして cwd を見に行くと、Zaivern 自身の `Cargo.toml` を
        // 拾って「Rust だ」と言い出す。**呼ぶ前に断る。**
        assert_eq!(detect(Path::new("")), Err(DetectError::Undetermined));
    }

    #[test]
    fn 目印が無ければ決めない() {
        let d = ws("empty");
        assert_eq!(detect(&d), Err(DetectError::Undetermined));
        // **cargo へ倒れていない**ことを、説明の文面でも見る。
        assert!(!DetectError::Undetermined.detail().contains("cargo "));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rustはcargo_tomlで決まる() {
        let d = ws("rust");
        put(&d, "Cargo.toml", "[package]\nname = \"x\"\n");
        assert_eq!(
            detect(&d).unwrap(),
            vec!["cargo fmt --check".to_string(), "cargo test".to_string()]
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn goはgo_modで決まる() {
        let d = ws("go");
        put(&d, "go.mod", "module x\n");
        let got = detect(&d).unwrap();
        assert_eq!(got, vec!["go test ./...".to_string()]);
        assert!(!got.iter().any(|c| c.starts_with("cargo")), "{got:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn nodeは実在するscriptだけを候補にする() {
        let d = ws("node");
        put(
            &d,
            "package.json",
            "{\"scripts\":{\"test\":\"vitest run\",\"lint\":\"eslint .\",\"dev\":\"next dev\"}}",
        );
        put(&d, "package-lock.json", "{}");
        let got = detect(&d).unwrap();
        assert_eq!(
            got,
            vec!["npm run test".to_string(), "npm run lint".to_string()],
            "実在する script だけを、決めた順で"
        );
        // `dev` は検証ではないので入らない。
        assert!(!got.iter().any(|c| c.contains("dev")), "{got:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 無いscriptを作らない() {
        let d = ws("node-nodefs");
        put(&d, "package.json", "{\"scripts\":{\"dev\":\"next dev\"}}");
        put(&d, "package-lock.json", "{}");
        assert_eq!(
            detect(&d),
            Err(DetectError::NoCandidate {
                marker: "package.json".into()
            })
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn lockfileでパッケージマネージャを決める() {
        for (lock, pm) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lockb", "bun"),
            ("bun.lock", "bun"),
            ("package-lock.json", "npm"),
        ] {
            let d = ws(&format!("node-{pm}-{lock}"));
            put(&d, "package.json", "{\"scripts\":{\"test\":\"x\"}}");
            put(&d, lock, "{}");
            assert_eq!(detect(&d).unwrap(), vec![format!("{pm} run test")], "{lock}");
            std::fs::remove_dir_all(&d).ok();
        }
    }

    #[test]
    fn どのlockfileか分からないなら選ばない() {
        // lockfile が無い / 2 つある。**勝手に npm と決めない。**
        for locks in [vec![], vec!["pnpm-lock.yaml", "yarn.lock"]] {
            let d = ws("node-ambiguous");
            put(&d, "package.json", "{\"scripts\":{\"test\":\"x\"}}");
            for l in &locks {
                put(&d, l, "{}");
            }
            assert!(detect(&d).is_err(), "{locks:?} で決めてしまった");
            std::fs::remove_dir_all(&d).ok();
        }
    }

    #[test]
    fn 壊れたpackage_jsonは読めないと言う() {
        // 「候補なし」で黙らせると、**壊れた設定が目印無しと同じ扱い**に
        // なって気付けない。
        let d = ws("node-broken");
        put(&d, "package.json", "{ not json");
        put(&d, "package-lock.json", "{}");
        match detect(&d) {
            Err(DetectError::Unreadable { marker, reason }) => {
                assert_eq!(marker, "package.json");
                assert!(!reason.is_empty(), "理由が空");
            }
            other => panic!("読めないと言っていない: {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn pythonはpytestと言い切れるときだけ() {
        // `requirements.txt` があるだけでは決めない (Django の
        // `manage.py test` かもしれない)。**推測しすぎない。**
        let d = ws("py-unsure");
        put(&d, "requirements.txt", "django\n");
        assert!(detect(&d).is_err(), "証拠が無いのに決めた");
        std::fs::remove_dir_all(&d).ok();

        for (f, body) in [
            ("pytest.ini", "[pytest]\n"),
            ("pyproject.toml", "[tool.pytest.ini_options]\n"),
            ("setup.cfg", "[tool:pytest]\n"),
            ("requirements.txt", "pytest==8.0\n"),
        ] {
            let d = ws("py-sure");
            put(&d, f, body);
            assert_eq!(detect(&d).unwrap(), vec!["pytest".to_string()], "{f}");
            std::fs::remove_dir_all(&d).ok();
        }
    }

    #[test]
    fn 複数の目印があれば両方を候補にする() {
        let d = ws("poly");
        put(&d, "Cargo.toml", "[package]\nname = \"x\"\n");
        put(&d, "go.mod", "module x\n");
        let got = detect(&d).unwrap();
        assert!(got.contains(&"cargo test".to_string()), "{got:?}");
        assert!(got.contains(&"go test ./...".to_string()), "{got:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 候補はすべて実行の関門を通る() {
        // 自動決定した候補が許可リストの外なら、それは候補の作り方の
        // 不具合である。**ここで気付けるようにしておく。**
        for (name, marks) in [
            ("rust", vec![("Cargo.toml", "[package]\nname = \"x\"\n")]),
            ("go", vec![("go.mod", "module x\n")]),
            (
                "node",
                vec![
                    ("package.json", "{\"scripts\":{\"test\":\"x\",\"lint\":\"y\"}}"),
                    ("package-lock.json", "{}"),
                ],
            ),
            ("py", vec![("pytest.ini", "[pytest]\n")]),
        ] {
            let d = ws(&format!("gate-{name}"));
            for (f, b) in &marks {
                put(&d, f, b);
            }
            for c in detect(&d).unwrap() {
                super::super::graph::parse_command(&c)
                    .unwrap_or_else(|e| panic!("{name}: `{c}` が関門を通らない: {}", e.reason()));
            }
            std::fs::remove_dir_all(&d).ok();
        }
    }
}
