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
}

/// レビュー報告を読む。
pub fn parse_review(body: &str, want_task: TaskId) -> Result<AcceptedReview, ReviewReject> {
    if body.len() > BLOCK_MAX_BYTES {
        return Err(ReviewReject::TooLarge { bytes: body.len() });
    }
    let doc: ReviewDoc =
        serde_json::from_str(&super::result_parser::escape_raw_controls(body))
            .map_err(|e| ReviewReject::BadJson(e.to_string()))?;
    if doc.task_id != want_task {
        return Err(ReviewReject::TaskMismatch {
            got: doc.task_id,
            want: want_task,
        });
    }
    if doc.findings.len() > ARRAY_MAX {
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
