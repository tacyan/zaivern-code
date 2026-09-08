//! 計画時に固定した期待値を成果物へ適用する。エージェントのテスト実装や
//! 成功報告に依存しない。任意コマンドを実行せず、作業フォルダ内だけを読む。
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path};

pub const OPEN: &str = "[ZAI-ACCEPTANCE]";
pub const CLOSE: &str = "[/ZAI-ACCEPTANCE]";
/// 生成と補修で共用する、パーサーと同じ必須項目を持つ完全な例。
pub const EXAMPLE: &str = r#"{"checks":[{"requirement":"REQ-01","paths":["output.txt"],"kind":"contains","text":"依頼で確定した必須値"}],"scenarios":[{"requirement":"REQ-01","input":"依頼で指定された入力値を使用する","procedure":"実物を開いて入力に対応する出力値を照合する","expected":"出力値が原依頼の指定値と一致している","reject_when":"一時コピーの出力値を変更すると照合が不一致になる","artifacts":["output.txt"],"evidence":"tests/verification.md"}]}"#;
pub const EVIDENCE_POLICY: &str = r#"各scenariosのevidenceは説明文だけでは不合格。JSON単体、またはMarkdown内の[ZAI-VERIFICATION]と[/ZAI-VERIFICATION]の間に次の形式を保存する。{"cases":[{"requirement":"REQ-01","input_path":"tests/input.json","output_path":"tests/output.txt","reproduce_path":"tests/reproduce.md","log_path":"tests/normal.log","mutation_log_path":"tests/mutation.log","expected":"30,000円","actual":"30,000円","normal_exit_code":0,"mutation_exit_code":1}]}。requirementは計画のシナリオと完全一致。全パスは作業フォルダ内の相対パスで、実在する別々の非空テキストファイル。画像等のバイナリは入出力記録から実ファイルを参照し、描画・計測結果をテキストで保存する。証跡自身や検証対象artifactsをこれら5ファイルの代わりにしない。reproduce_pathには使用ツール・版・実行コマンドまたは具体的な操作手順を記録し、誰でも追試できるようにする。正常時と破損時に同じ検査を実行して生ログを保存する。expectedとactualは具体的な比較値で一致させ、PASS/OK等の成功宣言にしない。output_pathには本体を実際に利用した出力を保存する。コード例や説明文の読み合わせを実動作確認と呼ばない。ログ・実行結果を捏造しない。未実行は未検証でありcompleted/APPROVEにしない。証跡と関連ファイルの生成をtesterタスクのfilesに含める。レビュー担当はreproduce_pathを使い独立して追試し、数値・エラー処理・説明と実装の一致を確認する。"#;
const FILE_LIMIT: u64 = 2 * 1024 * 1024;
const TOTAL_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub checks: Vec<Check>,
    #[serde(default)]
    pub scenarios: Vec<Scenario>,
}

/// 要件ごとの実物検証。構造検査とは分離し、意味上の適否は計画レビューで確認する。
#[derive(Clone, Debug, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub requirement: String,
    pub input: String,
    pub procedure: String,
    pub expected: String,
    pub reject_when: String,
    pub artifacts: Vec<String>,
    pub evidence: String,
}

/// 新しい生成計画の品質門。過去の保存済み計画の読み取りとは分離する。
/// 文字数や網羅性だけでは意味上の妥当性を証明できないため、別実行の監査も必須。
pub fn audit(spec: &str) -> Result<(), String> {
    let contract =
        parse(spec)?.ok_or("成果物検証がありません。[ZAI-ACCEPTANCE] に条件を定義してください")?;
    if contract.scenarios.is_empty() || contract.scenarios.len() > 128 {
        return Err("存在・サイズ・見出し確認だけでは不十分です。scenariosに各REQのinput/procedure/expected/reject_when/artifacts/evidenceを定義してください".into());
    }
    let ids = |text: &str| -> std::collections::BTreeSet<String> {
        let mut found = std::collections::BTreeSet::new();
        for part in text.split("REQ-").skip(1) {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                found.insert(format!("REQ-{digits}"));
            }
        }
        found
    };
    let sections = super::planner::parse_sections(spec);
    let mut required = std::collections::BTreeSet::new();
    for section in &sections {
        // tasksだけに書いた要件も取りこぼさない。フェンス内の検証JSONは含めない。
        for text in std::iter::once(&section.title)
            .chain(&section.bullets)
            .chain(&section.prose)
        {
            required.extend(ids(text));
        }
    }
    if required.is_empty() {
        return Err("原要件にREQ番号を付け、検証シナリオと対応させてください".into());
    }
    let mut covered = std::collections::BTreeSet::new();
    for case in &contract.scenarios {
        let case_ids = ids(&case.requirement);
        if case_ids.len() != 1 || !case_ids.is_subset(&required) {
            return Err(format!(
                "{}: シナリオは原要件のREQを一件ずつ指定してください",
                case.requirement
            ));
        }
        for (label, text) in [
            ("input", &case.input),
            ("procedure", &case.procedure),
            ("expected", &case.expected),
            ("reject_when", &case.reject_when),
        ] {
            let t = text.trim();
            if t.chars().count() < 8
                || [
                    "PASS",
                    "OK",
                    "確認済み",
                    "実物を確認する",
                    "期待どおり",
                    "適切に動作する",
                    "問題がない",
                    "TODO",
                ]
                .contains(&t)
            {
                return Err(format!(
                    "{}: {label}に具体的な入力・観測手順・比較値・失敗例を記載してください",
                    case.requirement
                ));
            }
        }
        if case.expected.trim() == case.reject_when.trim()
            || case.procedure.trim() == case.expected.trim()
        {
            return Err(format!(
                "{}: 合格条件と不合格条件・観測手順を区別してください",
                case.requirement
            ));
        }
        if case.artifacts.is_empty()
            || case.artifacts.len() > 16
            || case.artifacts.iter().any(|p| !relative(p))
            || !relative(&case.evidence)
            || case.artifacts.contains(&case.evidence)
        {
            return Err(format!(
                "{}: 実物artifactsと別ファイルの観測証跡evidenceを指定してください",
                case.requirement
            ));
        }
        if !contract
            .checks
            .iter()
            .any(|c| ids(&c.requirement) == case_ids)
        {
            return Err(format!(
                "{}: 補助となる実ファイル検査checksがありません",
                case.requirement
            ));
        }
        covered.extend(case_ids);
    }
    let missing: Vec<_> = required.difference(&covered).cloned().collect();
    if !missing.is_empty() {
        return Err(format!("実物検証のない要件: {}", missing.join(", ")));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
pub struct Check {
    pub requirement: String,
    pub paths: Vec<String>,
    #[serde(flatten)]
    pub rule: Rule,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rule {
    Exists {
        min_bytes: u64,
    },
    Contains {
        text: String,
    },
    NotContains {
        text: String,
    },
    JsonEquals {
        pointer: String,
        value: serde_json::Value,
    },
    Contrast {
        foreground: String,
        background: String,
        minimum: f64,
    },
}

fn contrast(foreground: &str, background: &str) -> Option<f64> {
    fn luminance(color: &str) -> Option<f64> {
        let hex = color.strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let mut value = 0.0;
        for (i, weight) in [0.2126, 0.7152, 0.0722].iter().enumerate() {
            let c = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()? as f64 / 255.0;
            value += weight
                * if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                };
        }
        Some(value)
    }
    let (a, b) = (luminance(foreground)?, luminance(background)?);
    Some((a.max(b) + 0.05) / (a.min(b) + 0.05))
}

#[derive(Deserialize)]
struct Evidence {
    cases: Vec<Observation>,
}
#[derive(Deserialize)]
struct Observation {
    requirement: String,
    input_path: String,
    output_path: String,
    reproduce_path: String,
    log_path: String,
    mutation_log_path: String,
    expected: String,
    actual: String,
    normal_exit_code: i32,
    mutation_exit_code: i32,
}

/// 証跡も実ファイルへ突き合わせる。存在だけ・成功宣言だけでは通さない。
fn verify_evidence(
    root: &Path,
    case: &Scenario,
    cache: &mut HashMap<String, String>,
    total: &mut usize,
) -> Result<(), String> {
    fn read(
        root: &Path,
        name: &str,
        cache: &mut HashMap<String, String>,
        total: &mut usize,
    ) -> Result<String, String> {
        if !relative(name) {
            return Err(format!("証跡のパスが不正です: {name}"));
        }
        if let Some(text) = cache.get(name) {
            return Ok(text.clone());
        }
        let path = root
            .join(name)
            .canonicalize()
            .map_err(|_| format!("証跡ファイルがありません: {name}"))?;
        if !path.starts_with(root) || !path.is_file() {
            return Err(format!(
                "証跡が作業フォルダ内の通常ファイルではありません: {name}"
            ));
        }
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        if size > FILE_LIMIT || total.saturating_add(size as usize) > TOTAL_LIMIT {
            return Err("証跡の読取り上限を超えています".into());
        }
        let mut text = String::new();
        file.take(FILE_LIMIT + 1)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        *total += text.len();
        if text.trim().is_empty() || text.len() as u64 > FILE_LIMIT || *total > TOTAL_LIMIT {
            return Err(format!("証跡が空または大きすぎます: {name}"));
        }
        cache.insert(name.into(), text.clone());
        Ok(text)
    }
    let text = read(root, &case.evidence, cache, total)?;
    let json = if let Some((_, after)) = text.split_once("[ZAI-VERIFICATION]") {
        after
            .split_once("[/ZAI-VERIFICATION]")
            .ok_or("証跡の終了マーカーがありません")?
            .0
    } else {
        text.as_str()
    };
    let evidence: Evidence = serde_json::from_str(json)
        .map_err(|e| format!("{}: 再現可能な証跡JSONが必要です: {e}", case.evidence))?;
    if evidence.cases.len() > 128 {
        return Err("証跡のcasesは128件までです".into());
    }
    let matching: Vec<_> = evidence
        .cases
        .iter()
        .filter(|c| c.requirement == case.requirement)
        .collect();
    if matching.len() != 1 {
        return Err(format!(
            "{}: シナリオと一致する証跡を一件記録してください",
            case.requirement
        ));
    }
    let observed = matching[0];
    if observed.expected.trim().is_empty()
        || observed.expected != observed.actual
        || ["PASS", "PASSED", "OK", "SUCCESS", "確認済み"]
            .contains(&observed.actual.trim().to_uppercase().as_str())
        || observed.normal_exit_code != 0
        || observed.mutation_exit_code == 0
    {
        return Err(format!(
            "{}: 期待値と観測値の一致・正常時成功・破損時失敗が必要です",
            case.requirement
        ));
    }
    let names = [
        &observed.input_path,
        &observed.output_path,
        &observed.reproduce_path,
        &observed.log_path,
        &observed.mutation_log_path,
    ];
    let mut seen = std::collections::HashSet::new();
    for name in names {
        read(root, name, cache, total)?;
        let path = root.join(name).canonicalize().map_err(|e| e.to_string())?;
        if !seen.insert(path.clone())
            || std::iter::once(&case.evidence)
                .chain(&case.artifacts)
                .any(|artifact| root.join(artifact).canonicalize().ok().as_ref() == Some(&path))
        {
            return Err(
                "実入力・実出力・再現手順・正常/破損ログは対象や証跡と別ファイルで保存してください"
                    .into(),
            );
        }
    }
    if !cache[&observed.output_path].contains(&observed.actual) {
        let preview = |s: &str| s.chars().take(160).collect::<String>();
        return Err(format!("{}: 観測値が保存された実出力と一致しません。証跡={}、出力={}、actual={:?}、出力先頭={:?}。actualは成功の説明文ではなく実出力からの連続した引用にしてください。expectedは原要件に基づく同じ比較値です。改行・空白・句読点も照合します。要件を満たす出力があるなら証跡の引用を修正し、出力自体が違うなら本体を修正して再実行してください。RESULTの再送だけでは解消しません。",
            case.requirement,case.evidence,observed.output_path,preview(&observed.actual),preview(&cache[&observed.output_path])));
    }
    if cache[&observed.log_path] == cache[&observed.mutation_log_path] {
        return Err("正常時と破損時のログが同一です".into());
    }
    Ok(())
}

fn relative(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

pub fn parse(spec: &str) -> Result<Option<Contract>, String> {
    let Some((_, after)) = spec.split_once(OPEN) else {
        return if spec.contains(CLOSE) {
            Err("成果物検証の開始マーカーがありません".into())
        } else {
            Ok(None)
        };
    };
    let (body, tail) = after
        .split_once(CLOSE)
        .ok_or("成果物検証の終了マーカーがありません")?;
    if tail.contains(OPEN) || tail.contains(CLOSE) || body.contains(OPEN) {
        return Err("成果物検証は一つのブロックにまとめてください".into());
    }
    if body.len() > 64 * 1024 {
        return Err("成果物検証の定義が大きすぎます".into());
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("成果物検証JSON: {e}"))?;
    // 一つ直すたびに別の欠落で再生成させない。全シナリオの不備を一度に返す。
    let mut defects = Vec::new();
    if let Some(cases) = value.get("scenarios").and_then(serde_json::Value::as_array) {
        for (i, case) in cases.iter().enumerate() {
            for field in [
                "requirement",
                "input",
                "procedure",
                "expected",
                "reject_when",
                "artifacts",
                "evidence",
            ] {
                let valid = case.get(field).is_some_and(|v| {
                    if field == "artifacts" {
                        v.as_array().is_some_and(|a| {
                            !a.is_empty()
                                && a.iter()
                                    .all(|p| p.as_str().is_some_and(|s| !s.trim().is_empty()))
                        })
                    } else {
                        v.as_str().is_some_and(|s| !s.trim().is_empty())
                    }
                });
                if !valid {
                    defects.push(format!(
                        "scenarios[{i}].{field}: {}が必要",
                        if field == "artifacts" {
                            "空でない文字列配列"
                        } else {
                            "空でない文字列"
                        }
                    ));
                }
            }
        }
    }
    if !defects.is_empty() {
        return Err(format!(
            "成果物検証JSONの不足・型不正:\n{}",
            defects.join("\n")
        ));
    }
    let contract: Contract =
        serde_json::from_value(value).map_err(|e| format!("成果物検証JSON: {e}"))?;
    if contract.checks.is_empty() || contract.checks.len() > 128 {
        return Err("成果物検証は1〜128件にしてください".into());
    }
    if contract.scenarios.len() > 128 {
        return Err("シナリオが多すぎます".into());
    }
    for case in &contract.scenarios {
        if !relative(&case.evidence)
            || case.artifacts.is_empty()
            || case.artifacts.len() > 16
            || case.artifacts.iter().any(|p| !relative(p))
        {
            return Err("シナリオの実物と証跡は作業フォルダ内の相対パスで指定してください".into());
        }
    }
    for check in &contract.checks {
        if check.requirement.trim().is_empty()
            || check.paths.is_empty()
            || check.paths.len() > 16
            || check.paths.iter().any(|p| !relative(p))
        {
            return Err(
                "成果物検証には要件名と作業フォルダ内の相対パス（1〜16件）が必要です".into(),
            );
        }
        match &check.rule {
            Rule::Contrast {
                foreground,
                background,
                minimum,
            } => {
                let measured = contrast(foreground, background)
                    .ok_or("コントラストの色は#RRGGBBで指定してください")?;
                if !(1.0..=21.0).contains(minimum) || measured < *minimum {
                    return Err(format!(
                        "{}: コントラスト実計算{measured:.4}:1が指定基準{minimum}:1を満たしません",
                        check.requirement
                    ));
                }
            }
            Rule::Contains { text } | Rule::NotContains { text } if text.is_empty() => {
                return Err("空の文字列は検証条件にできません".into())
            }
            Rule::JsonEquals { pointer, .. }
                if !pointer.is_empty() && !pointer.starts_with('/') =>
            {
                return Err("JSON pointer は / で始めてください".into())
            }
            Rule::Exists { min_bytes: 0 } => {
                return Err("存在確認には1以上のmin_bytesが必要です".into())
            }
            _ => {}
        }
    }
    Ok(Some(contract))
}

/// 自分の担当ファイルだけを先に検証し、統合時は全条件を再検証する。
/// キャッシュはこの呼び出しだけ。後から変更されたファイルを見逃さない。
pub fn verify(
    spec: &str,
    workspace: &Path,
    scope: &[String],
    final_pass: bool,
) -> Result<(), String> {
    let Some(contract) = parse(spec)? else {
        return Ok(());
    };
    let checks: Vec<_> = contract
        .checks
        .iter()
        .flat_map(|c| c.paths.iter().map(move |p| (c, p)))
        .filter(|(_, p)| final_pass || scope.iter().any(|s| crate::lease::overlaps(s, p)))
        .collect();
    if !contract.scenarios.is_empty() {
        let root = workspace.canonicalize().map_err(|e| e.to_string())?;
        let mut verified = std::collections::HashSet::new();
        let mut evidence_cache = HashMap::new();
        let mut evidence_bytes = 0;
        for case in contract
            .scenarios
            .iter()
            .filter(|c| final_pass || scope.iter().any(|p| crate::lease::overlaps(p, &c.evidence)))
        {
            for name in case.artifacts.iter().chain(std::iter::once(&case.evidence)) {
                if !verified.insert(name) {
                    continue;
                }
                let path = root.join(name).canonicalize().map_err(|_| {
                    format!(
                        "{}: 実物検証の証跡または対象がありません: {name}",
                        case.requirement
                    )
                })?;
                if !path.starts_with(&root)
                    || !path.is_file()
                    || path.metadata().map_err(|e| e.to_string())?.len() == 0
                {
                    return Err(format!(
                        "{}: 無効な実物検証ファイル: {name}",
                        case.requirement
                    ));
                }
            }
            verify_evidence(&root, case, &mut evidence_cache, &mut evidence_bytes)?;
        }
    }
    if checks.is_empty() {
        return Ok(());
    }
    let root = workspace
        .canonicalize()
        .map_err(|e| format!("成果物の作業フォルダ: {e}"))?;
    let mut cache = HashMap::new();
    let mut total = 0usize;
    for (check, relative) in checks {
        let fail = |why: &str| format!("{}: {} — {}", check.requirement, relative, why);
        let path = root
            .join(relative)
            .canonicalize()
            .map_err(|_| fail("成果物が存在しません"))?;
        if !path.starts_with(&root) || !path.is_file() {
            return Err(fail("作業フォルダ内の通常ファイルではありません"));
        }
        if let Rule::Exists { min_bytes } = check.rule {
            if std::fs::metadata(&path)
                .map_err(|e| fail(&e.to_string()))?
                .len()
                < min_bytes
            {
                return Err(fail(&format!("最低{min_bytes}バイトを満たしていません")));
            }
            continue;
        }
        if !cache.contains_key(&path) {
            let file = std::fs::File::open(&path).map_err(|e| fail(&e.to_string()))?;
            let size = file.metadata().map_err(|e| fail(&e.to_string()))?.len();
            if size > FILE_LIMIT || total.saturating_add(size as usize) > TOTAL_LIMIT {
                return Err(fail("内容照合の上限（1ファイル2MiB・合計16MiB）を超えています。検証対象を分割してください"));
            }
            let mut text = String::new();
            file.take(FILE_LIMIT + 1)
                .read_to_string(&mut text)
                .map_err(|_| {
                    fail("テキストとして読めません。バイナリはexistsと専門の検証を使ってください")
                })?;
            total += text.len();
            if text.len() as u64 > FILE_LIMIT || total > TOTAL_LIMIT {
                return Err(fail("検証中にサイズ上限を超えました"));
            }
            cache.insert(path.clone(), text);
        }
        let text = &cache[&path];
        let ok = match &check.rule {
            Rule::Contrast {
                foreground,
                background,
                minimum,
            } => {
                let lower = text.to_lowercase();
                lower.contains(&foreground.to_lowercase())
                    && lower.contains(&background.to_lowercase())
                    && contrast(foreground, background).is_some_and(|value| value >= *minimum)
            }
            Rule::Contains { text: expected } => text.contains(expected),
            Rule::NotContains { text: forbidden } => !text.contains(forbidden),
            Rule::JsonEquals { pointer, value } => {
                let json: serde_json::Value =
                    serde_json::from_str(text).map_err(|_| fail("JSONが不正です"))?;
                json.pointer(pointer) == Some(value)
            }
            Rule::Exists { .. } => unreachable!(),
        };
        if !ok {
            return Err(fail(&format!("計画時の期待値に不一致: {:?}", check.rule)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(checks: serde_json::Value) -> String {
        format!("{OPEN}\n{}\n{CLOSE}", serde_json::json!({"checks": checks}))
    }
    fn scenario(req: &str, artifact: &str, evidence: &str) -> serde_json::Value {
        serde_json::json!({"requirement":req,"input":"依頼に指定された値を含む入力データ","procedure":"実物を対象のツールで開いて結果を記録する","expected":"入力値が結果へ反映され原資料と一致する","reject_when":"一時コピーの入力値を変更して不一致を検出する","artifacts":[artifact],"evidence":evidence})
    }
    fn audited_spec(scenarios: serde_json::Value) -> String {
        format!(
            "## 詳細要件\n- REQ-01: 正常な生成結果\n- REQ-02: 変更入力の反映\n{OPEN}{} {CLOSE}",
            serde_json::json!({"checks":[
                {"requirement":"REQ-01","paths":["output.bin"],"kind":"exists","min_bytes":1},
                {"requirement":"REQ-02","paths":["output.bin"],"kind":"exists","min_bytes":1}],"scenarios":scenarios})
        )
    }
    #[test]
    fn 存在と見出しだけの旧条件は生成計画として不合格() {
        let s = format!(
            "## 詳細要件\n- REQ-01: WCAG準拠\n{}",
            spec(serde_json::json!([
                {"requirement":"REQ-01","paths":["hero.html"],"kind":"exists","min_bytes":200},
                {"requirement":"REQ-01","paths":["report.md"],"kind":"contains","text":"## Verification Results"}
            ]))
        );
        // 旧計画を読めることと、新しく生成する計画を承認できることは別。
        assert!(parse(&s).unwrap().is_some());
        assert!(audit(&s).unwrap_err().contains("scenarios"));
    }
    #[test]
    fn 要件漏れと同じ合否条件と証跡の自己参照を拒否する() {
        let one = scenario("REQ-01", "output.bin", "tests/result.json");
        assert!(audit(&audited_spec(serde_json::json!([one])))
            .unwrap_err()
            .contains("REQ-02"));
        let mut two = scenario("REQ-02", "output.bin", "tests/result2.json");
        two["reject_when"] = two["expected"].clone();
        assert!(audit(&audited_spec(serde_json::json!([one, two])))
            .unwrap_err()
            .contains("区別"));
        let mut two = scenario("REQ-02", "output.bin", "output.bin");
        assert!(audit(&audited_spec(serde_json::json!([one, two])))
            .unwrap_err()
            .contains("別ファイル"));
        two["evidence"] = "tests/result2.json".into();
        two["expected"] = "PASS".into();
        assert!(audit(&audited_spec(serde_json::json!([one, two]))).is_err());
    }
    #[test]
    fn コード文書画像とも実物検証の計画を記述できる() {
        for artifact in ["src/main.rs", "report.md", "images/hero.png"] {
            let s = audited_spec(serde_json::json!([
                scenario("REQ-01", artifact, "tests/normal.json"),
                scenario("REQ-02", artifact, "tests/variation.json")
            ]));
            assert!(audit(&s).is_ok(), "{artifact}");
        }
    }
    #[test]
    fn 統合完了はシナリオの実物と証跡が揃うまで通さない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-acceptance", "scenario-evidence");
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("output.bin"), [1, 2, 3]).unwrap();
        let s = audited_spec(serde_json::json!([
            scenario("REQ-01", "output.bin", "tests/results.json"),
            scenario("REQ-02", "output.bin", "tests/results.json")
        ]));
        assert!(verify(&s, &dir, &["output.bin".into()], false).is_ok());
        assert!(verify(&s, &dir, &[], true).unwrap_err().contains("証跡"));
        std::fs::write(dir.join("tests/results.json"), "観測した結果を記録").unwrap();
        assert!(verify(&s, &dir, &[], true)
            .unwrap_err()
            .contains("証跡JSON"));
        write_evidence(&dir);
        assert!(verify(&s, &dir, &[], true).is_ok());
    }

    fn write_evidence(dir: &Path) {
        for (name, body) in [
            ("input.json", r#"{"price":30000}"#),
            ("output.txt", "価格: 30000円"),
            (
                "reproduce.md",
                "入力ファイルを対象スキルに渡して生成後、価格を比較する実行手順",
            ),
            ("normal.log", "price expected=30000 actual=30000 exit=0"),
            ("mutation.log", "price expected=30000 actual=5000 exit=1"),
        ] {
            std::fs::write(dir.join("tests").join(name), body).unwrap();
        }
        let cases: Vec<_> = ["REQ-01","REQ-02"].iter().map(|req| serde_json::json!({
            "requirement":req,"input_path":"tests/input.json","output_path":"tests/output.txt",
            "reproduce_path":"tests/reproduce.md","log_path":"tests/normal.log","mutation_log_path":"tests/mutation.log",
            "expected":"30000円","actual":"30000円","normal_exit_code":0,"mutation_exit_code":1
        })).collect();
        std::fs::write(
            dir.join("tests/results.json"),
            serde_json::json!({"cases":cases}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn 証跡の成功宣言だけでは実出力の不一致や未実施を通さない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-acceptance", "false-evidence");
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("output.bin"), [1]).unwrap();
        let s = audited_spec(serde_json::json!([
            scenario("REQ-01", "output.bin", "tests/results.json"),
            scenario("REQ-02", "output.bin", "tests/results.json")
        ]));
        write_evidence(&dir);
        std::fs::write(dir.join("tests/output.txt"), "価格: 5000円").unwrap();
        assert!(verify(&s, &dir, &[], true).unwrap_err().contains("実出力"));
        write_evidence(&dir);
        std::fs::remove_file(dir.join("tests/reproduce.md")).unwrap();
        assert!(verify(&s, &dir, &[], true)
            .unwrap_err()
            .contains("reproduce.md"));
        write_evidence(&dir);
        std::fs::copy(dir.join("tests/normal.log"), dir.join("tests/mutation.log")).unwrap();
        assert!(verify(&s, &dir, &[], true).unwrap_err().contains("同一"));
        write_evidence(&dir);
        let path = dir.join("tests/results.json");
        let good = std::fs::read_to_string(&path).unwrap();
        for (field, value) in [
            ("mutation_exit_code", serde_json::json!(0)),
            ("actual", serde_json::json!("5000円")),
            ("output_path", serde_json::json!("../outside.txt")),
            ("log_path", serde_json::json!("tests/input.json")),
        ] {
            let mut json: serde_json::Value = serde_json::from_str(&good).unwrap();
            json["cases"][0][field] = value;
            std::fs::write(&path, json.to_string()).unwrap();
            assert!(verify(&s, &dir, &[], true).is_err(), "{field}");
        }
        std::fs::write(
            &path,
            format!("# 証跡\n[ZAI-VERIFICATION]{good}[/ZAI-VERIFICATION]"),
        )
        .unwrap();
        assert!(verify(&s, &dir, &[], true).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn コントラストの誤計算と境界値丸めを通さない() {
        assert!((contrast("#94A3B8", "#FFFFFF").unwrap() - 2.56398).abs() < 0.0001);
        assert_eq!(contrast("#000000", "#FFFFFF"), Some(21.0));
        assert_eq!(contrast("#xyzxyz", "#FFFFFF"), None);
        for (foreground, minimum, pass) in [
            ("#94A3B8", 3.0, false),
            ("#F59E0B", 4.5, false),
            ("#767676", 4.5, true),
            ("#777777", 4.5, false),
        ] {
            let s = spec(
                serde_json::json!([{"requirement":"文字色","paths":["theme.css"],"kind":"contrast","foreground":foreground,"background":"#FFFFFF","minimum":minimum}]),
            );
            assert_eq!(parse(&s).is_ok(), pass, "{foreground}");
        }
    }

    #[test]
    fn 模擬テストが成功しても実物の価格と部数の変化を検出する() {
        let dir = crate::test_util::unique_temp_dir("zaivern-acceptance", "real");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["skill.md", "template.md"] {
            std::fs::write(dir.join(name), "価格: 29,800円 限定10部").unwrap();
        }
        let spec = spec(serde_json::json!([
            {"requirement":"価格", "paths":["skill.md","template.md"], "kind":"contains", "text":"29,800円"},
            {"requirement":"部数", "paths":["skill.md","template.md"], "kind":"contains", "text":"限定10部"}
        ]));
        assert!(verify(&spec, &dir, &[], true).is_ok());
        std::fs::write(dir.join("template.md"), "価格: 1円 限定999部").unwrap();
        assert!(verify(&spec, &dir, &[], true).unwrap_err().contains("価格"));
        std::fs::write(dir.join("template.md"), "価格: 29,800円 限定30部").unwrap();
        assert!(verify(&spec, &dir, &[], true).unwrap_err().contains("部数"));
        assert!(verify(&spec, &dir, &["skill.md".into()], false).is_ok());
    }
    #[test]
    fn jsonの値と不要な記述とバイナリの実在を検証する() {
        let dir = crate::test_util::unique_temp_dir("zaivern-acceptance", "types");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.json"), r#"{"price":39800,"items":[1,2,3]}"#).unwrap();
        std::fs::write(dir.join("report.md"), "調査の結論").unwrap();
        std::fs::write(dir.join("image.bin"), [0xff, 0, 1]).unwrap();
        let s = spec(serde_json::json!([
            {"requirement":"入力反映", "paths":["data.json"], "kind":"json_equals", "pointer":"/price", "value":39800},
            {"requirement":"範囲", "paths":["report.md"], "kind":"not_contains", "text":"100万PV"},
            {"requirement":"画像", "paths":["image.bin"], "kind":"exists", "min_bytes":3}
        ]));
        assert!(verify(&s, &dir, &[], true).is_ok());
        std::fs::write(dir.join("report.md"), "100万PV").unwrap();
        assert!(verify(&s, &dir, &[], true).is_err());
        std::fs::write(dir.join("report.md"), "調査の結論").unwrap();
        std::fs::write(dir.join("data.json"), r#"{"price":29800}"#).unwrap();
        assert!(verify(&s, &dir, &[], true).is_err());
    }
    #[test]
    fn 壊れた契約やフォルダ外の指定を黙って省略しない() {
        assert!(parse("仕様のみ").unwrap().is_none());
        for s in [
            OPEN.to_string(),
            CLOSE.to_string(),
            spec(serde_json::json!([])),
            spec(
                serde_json::json!([{"requirement":"a", "paths":["../a"],"kind":"exists","min_bytes":1}]),
            ),
            spec(
                serde_json::json!([{"requirement":"a", "paths":["a"],"kind":"contains","text":""}]),
            ),
        ] {
            assert!(parse(&s).is_err(), "{s}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn 外部リンクを読まず巨大ファイルを拒否する() {
        let dir = crate::test_util::unique_temp_dir("zaivern-acceptance", "limits");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), dir.join("outside")).unwrap();
        let s = spec(
            serde_json::json!([{"requirement":"a", "paths":["outside/a"],"kind":"exists","min_bytes":1}]),
        );
        assert!(verify(&s, &dir, &[], true).is_err());
        std::fs::write(dir.join("big"), vec![b'a'; FILE_LIMIT as usize + 1]).unwrap();
        let s = spec(
            serde_json::json!([{"requirement":"a", "paths":["big"],"kind":"contains","text":"a"}]),
        );
        assert!(verify(&s, &dir, &[], true).is_err());
    }
}
