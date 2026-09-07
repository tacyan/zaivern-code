//! レビュー結果の読み取りと、そこから決まるタスクの行き先。
//!
//! ## なぜ別モジュールなのか
//!
//! 「レビューを通ったか」は Completed の唯一の入口なので、判定を
//! 1 か所に閉じ込める。散らすと「このパスだけレビューを飛ばす」が生える。
//!
//! ## 形式
//!
//! ```text
//! [ZAI-TEAM-REVIEW]
//! {
//!   "task_id": 12,
//!   "verdict": "APPROVE",
//!   "findings": []
//! }
//! [/ZAI-TEAM-REVIEW]
//! ```

use serde::{Deserialize, Serialize};

use super::model::{ReviewVerdict, TaskId};
use super::result_parser::{ARRAY_MAX, BLOCK_MAX_BYTES};

/// 新しい成果物契約を持つ依頼は形式に関係なく内容レビューを要求する。
/// 旧スキル計画も互換性のため対象に含める。
pub fn requires_content_review(goal: &super::model::TeamGoal) -> bool {
    if goal.specification.contains(super::acceptance::OPEN) { return true; }
    let text = format!("{}\n{}", goal.title, goal.specification).to_lowercase();
    text.contains("skill.md") || text.contains("skills") || text.contains("スキル")
}

pub const QUALITY_CRITERIA: [&str; 5] = [
    "requirements", "truthfulness", "deliverables", "reproducibility", "scope",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityCheck {
    pub criterion: String,
    pub source_path: String,
    pub excerpt: String,
    pub expected: String,
    pub actual: String,
}

/// JSON の自己宣言だけでは承認しない。根拠を実際のファイルと照合する。
/// 意味上の適否は別担当が判定し、アプリは証拠の存在と報告の充足を検証する。
pub fn validate_quality(review: &AcceptedReview, workspace: &std::path::Path) -> Result<(), String> {
    use std::io::Read;
    if review.verdict != ReviewVerdict::Approve { return Ok(()); }
    if !review.findings.is_empty() {
        return Err("指摘が残るため REQUEST_CHANGES で修正を依頼してください".into());
    }
    let root = workspace.canonicalize().map_err(|e| format!("作業フォルダを確認できません: {e}"))?;
    // 同じ資料を複数観点で読む場合も I/O は一度だけ。メモリと読み取り量を制限。
    let mut files = std::collections::HashMap::new();
    for criterion in QUALITY_CRITERIA {
        let check = review.quality_checks.iter().find(|c| c.criterion == criterion)
            .ok_or_else(|| format!("quality_checks に {criterion} の検証が必要です。source_path, excerpt, expected, actual を実物に基づき報告してください"))?;
        if check.excerpt.trim().len() < 12 || check.expected.trim().is_empty() || check.actual.trim().is_empty() {
            return Err(format!("{criterion}: 根拠の引用・期待結果・観測結果を具体的に記載してください"));
        }
        let relative = std::path::Path::new(&check.source_path);
        if relative.as_os_str().is_empty() || relative.is_absolute()
            || relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err(format!("{criterion}: source_path は作業フォルダ内の相対パスにしてください"));
        }
        let path = root.join(relative).canonicalize().map_err(|_| format!("{criterion}: 根拠ファイル {} がありません", check.source_path))?;
        if !path.starts_with(&root) { return Err(format!("{criterion}: 作業フォルダ外の根拠は使用できません")); }
        if !path.is_file() { return Err("根拠は通常のテキストファイルにしてください".into()); }
        if !files.contains_key(&path) {
            let file = std::fs::File::open(&path).map_err(|e| format!("根拠を読めません: {e}"))?;
            let meta = file.metadata().map_err(|e| e.to_string())?;
            const LIMIT: u64 = 1024 * 1024;
            if !meta.is_file() || meta.len() > LIMIT { return Err("根拠は1MiB以下のテキストファイルにしてください".into()); }
            let mut text = String::new();
            file.take(LIMIT + 1).read_to_string(&mut text).map_err(|e| format!("根拠を読めません: {e}"))?;
            if text.len() as u64 > LIMIT { return Err("根拠ファイルが大きすぎます".into()); }
            files.insert(path.clone(), text);
        }
        if !files[&path].contains(check.excerpt.trim()) {
            return Err(format!("{criterion}: 引用が {} の実際の内容と一致しません", check.source_path));
        }
    }
    Ok(())
}

pub const REVIEW_OPEN: &str = "[ZAI-TEAM-REVIEW]";
pub const REVIEW_CLOSE: &str = "[/ZAI-TEAM-REVIEW]";

/// レビュー報告の JSON。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDoc {
    pub task_id: TaskId,
    pub verdict: String,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub quality_checks: Vec<QualityCheck>,
}

/// レビュー報告を断った理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewReject {
    BadJson(String),
    TooLarge {
        bytes: usize,
    },
    TaskMismatch {
        got: TaskId,
        want: TaskId,
    },
    UnknownVerdict(String),
    /// `REQUEST_CHANGES` なのに指摘が 1 件も無い。
    ///
    /// 「直せ」とだけ言われても次の担当は何をすればよいか分からない。
    NoFindings,
    ArrayTooLong,
}

impl ReviewReject {
    pub fn detail(&self) -> String {
        match self {
            ReviewReject::BadJson(e) => format!("レビュー報告の JSON を読めません: {e}"),
            ReviewReject::TooLarge { bytes } => {
                format!("レビュー報告が大きすぎます ({bytes} バイト)")
            }
            ReviewReject::TaskMismatch { got, want } => {
                format!("レビュー対象 #{got} が担当 #{want} と一致しません")
            }
            ReviewReject::UnknownVerdict(v) => {
                format!("未知の判定「{v}」(APPROVE / REQUEST_CHANGES のいずれか)")
            }
            ReviewReject::NoFindings => "REQUEST_CHANGES には具体的な指摘が必要です".to_string(),
            ReviewReject::ArrayTooLong => "指摘が多すぎます".to_string(),
        }
    }
}

/// 受理されたレビュー。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedReview {
    pub task_id: TaskId,
    pub verdict: ReviewVerdict,
    pub findings: Vec<String>,
    pub summary: String,
    pub quality_checks: Vec<QualityCheck>,
}

/// レビュー報告を読む。
pub fn parse_review(body: &str, want_task: TaskId) -> Result<AcceptedReview, ReviewReject> {
    if body.len() > BLOCK_MAX_BYTES {
        return Err(ReviewReject::TooLarge { bytes: body.len() });
    }
    let doc: ReviewDoc =
        super::result_parser::parse_lenient(body).map_err(ReviewReject::BadJson)?;
    if doc.task_id != want_task {
        return Err(ReviewReject::TaskMismatch {
            got: doc.task_id,
            want: want_task,
        });
    }
    if doc.findings.len() > ARRAY_MAX || doc.quality_checks.len() > QUALITY_CRITERIA.len() {
        return Err(ReviewReject::ArrayTooLong);
    }
    let verdict = match doc.verdict.trim().to_ascii_uppercase().as_str() {
        "APPROVE" | "APPROVED" => ReviewVerdict::Approve,
        "REQUEST_CHANGES" | "REQUEST CHANGES" | "CHANGES_REQUESTED" => {
            ReviewVerdict::RequestChanges
        }
        other => return Err(ReviewReject::UnknownVerdict(other.to_string())),
    };
    let findings: Vec<String> = doc
        .findings
        .iter()
        .map(|s| super::model::clamp_text(s.trim()))
        .filter(|s| !s.is_empty())
        .collect();
    if verdict == ReviewVerdict::RequestChanges && findings.is_empty() {
        return Err(ReviewReject::NoFindings);
    }
    Ok(AcceptedReview {
        task_id: doc.task_id,
        verdict,
        findings,
        summary: super::model::clamp_text(doc.summary.trim()),
        quality_checks: doc.quality_checks,
    })
}

/// レビュー指摘を、次の実装担当へ渡す文脈の形に整える。
pub fn findings_as_context(findings: &[String]) -> Vec<String> {
    findings
        .iter()
        .enumerate()
        .map(|(i, f)| format!("レビュー指摘 {}: {}", i + 1, f))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality_fixture() -> (std::path::PathBuf, AcceptedReview) {
        let dir = crate::test_util::unique_temp_dir("zaivern-quality", "evidence");
        std::fs::create_dir_all(&dir).unwrap();
        let excerpt = "入力価格を全出力に反映。実績は架空例と明示。";
        std::fs::write(dir.join("SKILL.md"), excerpt).unwrap();
        let mut review = parse_review(r#"{"task_id":1,"verdict":"APPROVE"}"#, 1).unwrap();
        review.quality_checks = QUALITY_CRITERIA.iter().map(|c| QualityCheck {
            criterion: c.to_string(), source_path: "SKILL.md".into(), excerpt: excerpt.into(),
            expected: "要件に沿うこと".into(), actual: "入力価格2種類と不足入力を確認した".into(),
        }).collect();
        (dir, review)
    }

    #[test]
    fn 実物と一致する全観点の根拠を受理する() {
        let (dir, review) = quality_fixture();
        assert_eq!(validate_quality(&review, &dir), Ok(()));
        let doc = serde_json::json!({"task_id":1,"verdict":"APPROVE","quality_checks":review.quality_checks});
        assert_eq!(parse_review(&doc.to_string(), 1).unwrap(), review);
    }

    #[test]
    fn 自己申告だけの承認と不足観点を拒否する() {
        let (dir, mut review) = quality_fixture();
        review.quality_checks.clear();
        assert!(validate_quality(&review, &dir).unwrap_err().contains("requirements"));
        let (_, mut review) = quality_fixture();
        review.quality_checks.pop();
        assert!(validate_quality(&review, &dir).unwrap_err().contains("scope"));
    }

    #[test]
    fn 存在しない同梱物と架空引用と未解決指摘を拒否する() {
        let (dir, mut review) = quality_fixture();
        review.quality_checks[0].source_path = "missing.pdf".into();
        assert!(validate_quality(&review, &dir).is_err());
        review.quality_checks[0].source_path = "SKILL.md".into();
        review.quality_checks[0].excerpt = "実際には書かれていない売上の根拠".into();
        assert!(validate_quality(&review, &dir).is_err());
        let (_, mut review) = quality_fixture();
        review.findings.push("出力例が3件必要だが2件しかない".into());
        assert!(validate_quality(&review, &dir).is_err());
        review.verdict = ReviewVerdict::RequestChanges;
        assert!(validate_quality(&review, &dir).is_ok());
    }

    #[test]
    fn 根拠パスの逸脱と巨大ファイルを拒否する() {
        let (dir, mut review) = quality_fixture();
        for path in ["../SKILL.md".to_string(), dir.join("SKILL.md").display().to_string()] {
            review.quality_checks[0].source_path = path;
            assert!(validate_quality(&review, &dir).is_err());
        }
        review.quality_checks[0].source_path = "SKILL.md".into();
        std::fs::write(dir.join("SKILL.md"), vec![b'a'; 1024 * 1024 + 1]).unwrap();
        assert!(validate_quality(&review, &dir).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn 外部へのリンクを根拠にしない() {
        let (dir, mut review) = quality_fixture();
        let (outside, _) = quality_fixture();
        std::os::unix::fs::symlink(outside.join("SKILL.md"), dir.join("link.md")).unwrap();
        review.quality_checks[0].source_path = "link.md".into();
        assert!(validate_quality(&review, &dir).is_err());
    }

    #[test]
    fn approveを読む() {
        let r = parse_review(r#"{"task_id":12,"verdict":"APPROVE"}"#, 12).unwrap();
        assert_eq!(r.verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn request_changesは指摘が要る() {
        assert_eq!(
            parse_review(r#"{"task_id":12,"verdict":"REQUEST_CHANGES"}"#, 12),
            Err(ReviewReject::NoFindings)
        );
        let ok = parse_review(
            r#"{"task_id":12,"verdict":"REQUEST_CHANGES","findings":["境界値のテストが無い"]}"#,
            12,
        )
        .unwrap();
        assert_eq!(ok.verdict, ReviewVerdict::RequestChanges);
        assert_eq!(ok.findings.len(), 1);
    }

    #[test]
    fn 対象タスク不一致を拒否する() {
        assert_eq!(
            parse_review(r#"{"task_id":9,"verdict":"APPROVE"}"#, 12),
            Err(ReviewReject::TaskMismatch { got: 9, want: 12 })
        );
    }

    #[test]
    fn 未知の判定を拒否する() {
        assert!(matches!(
            parse_review(r#"{"task_id":12,"verdict":"LGTM?"}"#, 12),
            Err(ReviewReject::UnknownVerdict(_))
        ));
    }

    #[test]
    fn 壊れたjsonを拒否する() {
        assert!(matches!(
            parse_review("{oops", 12),
            Err(ReviewReject::BadJson(_))
        ));
    }

    #[test]
    fn 指摘は次の指示に載る形になる() {
        let c = findings_as_context(&["a".into(), "b".into()]);
        assert_eq!(c, vec!["レビュー指摘 1: a", "レビュー指摘 2: b"]);
    }
}
