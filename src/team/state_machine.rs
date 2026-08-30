//! タスクの状態遷移を**純粋関数**で決める層。
//!
//! ## なぜ表にするのか
//!
//! 状態を書き換える場所が散らばると、「Running から直に Completed へ飛ぶ」
//! ような抜け道が必ず 1 つ生える。この製品の中核の主張は
//! **「エージェントが完了と言っただけでは Completed にしない」**なので、
//! 抜け道が 1 本でもあると主張ごと崩れる。
//!
//! だからここでは**許す遷移だけを列挙**し、それ以外は理由付きで断る。
//! 呼び出し側は [`apply`] を通してしか状態を変えられない。

use super::model::TeamTaskState as S;

/// 遷移を断った理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionError {
    /// 表に無い遷移。
    NotAllowed { from: S, to: S },
    /// 終端状態からは動かさない (人が明示的に戻す場合を除く)。
    Terminal { from: S },
}

impl TransitionError {
    /// 画面に出す説明 (i18n の鍵ではなく、開発者向けの内訳)。
    pub fn detail(self) -> String {
        match self {
            TransitionError::NotAllowed { from, to } => {
                format!("{} → {} は許されていない遷移です", from.key(), to.key())
            }
            TransitionError::Terminal { from } => {
                format!("{} は終端状態なので自動では動かしません", from.key())
            }
        }
    }
}

/// 許可する遷移の表。**ここが唯一の真実。**
///
/// `NeedsUser` へはどこからでも行ける (人へ上げるのを妨げない)。
/// `NeedsUser` から出るのは人の操作 ([`force`]) だけ。
pub fn allowed(from: S, to: S) -> bool {
    if from == to {
        // 同じ状態への遷移は「何もしない」として許す (冪等性)。
        return true;
    }
    // 人を呼ぶのはいつでもできる。ただし完了済みは呼び戻さない。
    if to == S::NeedsUser {
        return from != S::Completed;
    }
    matches!(
        (from, to),
        (S::Pending, S::Ready)
            | (S::Pending, S::Blocked)
            | (S::Ready, S::Assigned)
            | (S::Ready, S::Blocked)
            | (S::Assigned, S::Running)
            | (S::Assigned, S::Ready)
            | (S::Assigned, S::Blocked)
            | (S::Assigned, S::Failed)
            | (S::Running, S::Validating)
            | (S::Running, S::Blocked)
            | (S::Running, S::Failed)
            | (S::Validating, S::Reviewing)
            | (S::Validating, S::Failed)
            | (S::Validating, S::Running)
            | (S::Reviewing, S::Completed)
            | (S::Reviewing, S::RevisionRequired)
            | (S::Reviewing, S::Failed)
            | (S::RevisionRequired, S::Ready)
            | (S::Blocked, S::Ready)
            | (S::Blocked, S::Failed)
            | (S::Failed, S::Ready)
    )
}

/// 遷移を試す。許されていれば新しい状態を返す。
pub fn apply(from: S, to: S) -> Result<S, TransitionError> {
    if from == S::Completed && to != S::Completed {
        return Err(TransitionError::Terminal { from });
    }
    if allowed(from, to) {
        Ok(to)
    } else {
        Err(TransitionError::NotAllowed { from, to })
    }
}

/// 人が明示的に動かす経路 (`NeedsUser` からの復帰、`Completed` の取り消し)。
///
/// **自動処理からは呼ばない。** 呼び出し元は必ず人の操作
/// ([`super::runtime::TeamAction`]) から来ること。
pub fn force(_from: S, to: S) -> S {
    to
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 実装から直接完了へは飛べない() {
        assert!(apply(S::Running, S::Completed).is_err());
        assert!(apply(S::Assigned, S::Completed).is_err());
        assert!(apply(S::Validating, S::Completed).is_err());
        // 完了できるのはレビューを通った後だけ
        assert_eq!(apply(S::Reviewing, S::Completed), Ok(S::Completed));
    }

    #[test]
    fn 検証を飛ばしてレビューへ行けない() {
        assert!(apply(S::Running, S::Reviewing).is_err());
        assert_eq!(apply(S::Running, S::Validating), Ok(S::Validating));
        assert_eq!(apply(S::Validating, S::Reviewing), Ok(S::Reviewing));
    }

    #[test]
    fn 指摘は再実装へ戻す() {
        assert_eq!(
            apply(S::Reviewing, S::RevisionRequired),
            Ok(S::RevisionRequired)
        );
        assert_eq!(apply(S::RevisionRequired, S::Ready), Ok(S::Ready));
        // 指摘から直接完了はできない
        assert!(apply(S::RevisionRequired, S::Completed).is_err());
    }

    #[test]
    fn 依存待ちからいきなり割り当てない() {
        assert!(apply(S::Pending, S::Assigned).is_err());
        assert_eq!(apply(S::Pending, S::Ready), Ok(S::Ready));
        assert_eq!(apply(S::Ready, S::Assigned), Ok(S::Assigned));
    }

    #[test]
    fn 人へはどこからでも上げられるが完了からは上げない() {
        for s in S::ALL {
            if s == S::Completed {
                assert!(apply(s, S::NeedsUser).is_err(), "{}", s.key());
            } else {
                assert_eq!(apply(s, S::NeedsUser), Ok(S::NeedsUser), "{}", s.key());
            }
        }
    }

    #[test]
    fn 同じ状態への遷移は冪等に通る() {
        for s in S::ALL {
            assert_eq!(apply(s, s), Ok(s), "{}", s.key());
        }
    }

    #[test]
    fn 完了は自動では動かない() {
        for s in S::ALL {
            if s == S::Completed {
                continue;
            }
            assert!(
                matches!(
                    apply(S::Completed, s),
                    Err(TransitionError::Terminal { .. })
                ),
                "Completed → {} を許してしまった",
                s.key()
            );
        }
        // 人の操作だけが取り消せる
        assert_eq!(force(S::Completed, S::Ready), S::Ready);
    }

    /// 表の網羅。**許可の総数を固定する**ので、うっかり 1 本足すと落ちる
    /// (足したいときはこの数も一緒に直す = 意図的な変更になる)。
    #[test]
    fn 許可した遷移の本数を固定する() {
        let mut n = 0;
        for a in S::ALL {
            for b in S::ALL {
                if a != b && allowed(a, b) {
                    n += 1;
                }
            }
        }
        assert_eq!(n, 30, "遷移表を変えたら本数も見直すこと");
    }
}
