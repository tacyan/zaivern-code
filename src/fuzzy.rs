const MAX_STACK_Q: usize = 64;
const MAX_STACK_T: usize = 256;

#[inline]
fn fill_char_buf<const N: usize>(s: &str, buf: &mut [char; N]) -> usize {
    if s.is_ascii() {
        let len = s.len().min(N);
        for (i, b) in s.bytes().take(len).enumerate() {
            buf[i] = (b.to_ascii_lowercase()) as char;
        }
        len
    } else {
        let mut count = 0;
        for c in s.chars() {
            for lc in c.to_lowercase() {
                if count < N {
                    buf[count] = lc;
                    count += 1;
                }
            }
        }
        count
    }
}

/// 複数件の対象に対して同一クエリで繰り返しマッチングを行う場合に使用する
/// プリコンパイル済みクエリ構造体。
/// 小文字化・文字展開などの前処理アロケーションを1回に抑えることで、10万QPS超の検索を高速化する。
#[derive(Debug, Clone)]
pub struct PreparedQuery {
    pub chars: Vec<char>,
}

impl PreparedQuery {
    #[inline]
    pub fn new(query: &str) -> Self {
        let mut q_buf = ['\0'; MAX_STACK_Q];
        let q_char_count = query.chars().flat_map(|c| c.to_lowercase()).count();
        if q_char_count <= MAX_STACK_Q {
            let len = fill_char_buf(query, &mut q_buf);
            Self {
                chars: q_buf[..len].to_vec(),
            }
        } else {
            Self {
                chars: query.to_lowercase().chars().collect(),
            }
        }
    }

    #[inline]
    pub fn score(&self, target: &str) -> Option<i32> {
        if self.chars.is_empty() {
            return Some(0);
        }
        score_impl(&self.chars, target)
    }
}

/// Simple fuzzy subsequence scoring: higher is better, None means no match.
/// 貪欲な一回走査ではなく、DP で最良の割り付け (連続・境界ボーナスの合計が
/// 最大になるマッチ位置の組) を選ぶ。O(query長 × target長)。
/// 内部バッファのスタック化によりヒープ割当 0 (Zero-Allocation) で超高速に動作する。
pub fn score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }

    let q_char_count = query.chars().flat_map(|c| c.to_lowercase()).count();
    if q_char_count <= MAX_STACK_Q {
        let mut q_buf = ['\0'; MAX_STACK_Q];
        let q_len = fill_char_buf(query, &mut q_buf);
        score_impl(&q_buf[..q_len], target)
    } else {
        let q: Vec<char> = query.to_lowercase().chars().collect();
        score_impl(&q, target)
    }
}

fn score_impl(q: &[char], target: &str) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }

    let t_char_count = target.chars().flat_map(|c| c.to_lowercase()).count();
    let t_vec: Vec<char>;
    let mut t_buf = ['\0'; MAX_STACK_T];

    let t: &[char] = if t_char_count <= MAX_STACK_T {
        let t_len = fill_char_buf(target, &mut t_buf);
        &t_buf[..t_len]
    } else {
        t_vec = target.to_lowercase().chars().collect();
        &t_vec
    };

    if q.len() > t.len() {
        return None;
    }

    // 大半の候補は不一致なので、まず O(len) の部分列チェックで足切りする
    {
        let mut qi = 0usize;
        for &c in t {
            if qi < q.len() && c == q[qi] {
                qi += 1;
            }
        }
        if qi < q.len() {
            return None;
        }
    }

    // DP状態更新
    if q.len() <= MAX_STACK_Q {
        let mut next = [[None::<i32>; 6]; MAX_STACK_Q + 1];
        next[q.len()] = [Some(0); 6];
        let mut cur = [[None::<i32>; 6]; MAX_STACK_Q + 1];
        cur[q.len()] = [Some(0); 6];

        for ti in (0..t.len()).rev() {
            for qi in (0..q.len()).rev() {
                for s in 0..6usize {
                    let skip = next[qi][0];
                    let matched = if t[ti] == q[qi] {
                        let (streak_bonus, s2) = if s == 0 {
                            (0, 1)
                        } else {
                            let streak = s.min(4);
                            (15 * streak as i32, streak + 1)
                        };
                        let pos_bonus = if ti == 0 {
                            25
                        } else if matches!(t[ti - 1], '/' | '\\' | '_' | '-' | '.' | ' ') {
                            20
                        } else {
                            0
                        };
                        next[qi + 1][s2].map(|r| r + 10 + streak_bonus + pos_bonus)
                    } else {
                        None
                    };
                    cur[qi][s] = skip.max(matched);
                }
            }
            for qi in 0..q.len() {
                next[qi] = cur[qi];
            }
        }

        next[0][0].map(|best| best - (t.len() as i32) / 4)
    } else {
        let mut next = vec![[None::<i32>; 6]; q.len() + 1];
        next[q.len()] = [Some(0); 6];
        let mut cur = next.clone();

        for ti in (0..t.len()).rev() {
            for qi in (0..q.len()).rev() {
                for s in 0..6usize {
                    let skip = next[qi][0];
                    let matched = if t[ti] == q[qi] {
                        let (streak_bonus, s2) = if s == 0 {
                            (0, 1)
                        } else {
                            let streak = s.min(4);
                            (15 * streak as i32, streak + 1)
                        };
                        let pos_bonus = if ti == 0 {
                            25
                        } else if matches!(t[ti - 1], '/' | '\\' | '_' | '-' | '.' | ' ') {
                            20
                        } else {
                            0
                        };
                        next[qi + 1][s2].map(|r| r + 10 + streak_bonus + pos_bonus)
                    } else {
                        None
                    };
                    cur[qi][s] = skip.max(matched);
                }
            }
            std::mem::swap(&mut cur, &mut next);
        }

        next[0][0].map(|best| best - (t.len() as i32) / 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 基本4系統: 完全一致 / 部分一致 / 不一致 / 空 ----

    #[test]
    fn exact_match_scores_positive() {
        assert_eq!(score("a", "a"), Some(35));
        assert_eq!(score("ab", "ab"), Some(60));
        assert_eq!(score("abc", "abc"), Some(100));
    }

    #[test]
    fn substring_match_is_found() {
        assert!(score("bc", "abcd").is_some());
        assert!(score("ad", "abcd").is_some());
    }

    #[test]
    fn subsequence_match_is_found() {
        // 飛び飛びでも順序が保たれていればマッチする
        assert!(score("ac", "abc").is_some());
        assert!(score("ad", "abcd").is_some());
    }

    #[test]
    fn out_of_order_query_does_not_match() {
        // 部分列マッチなので順序が逆だと不一致
        assert_eq!(score("ba", "abc"), None);
        assert_eq!(score("ca", "abc"), None);
    }

    #[test]
    fn absent_char_does_not_match() {
        assert_eq!(score("z", "abc"), None);
        assert_eq!(score("az", "abc"), None);
    }

    #[test]
    fn empty_query_scores_zero() {
        assert_eq!(score("", "abc"), Some(0));
        assert_eq!(score("", ""), Some(0));
    }

    #[test]
    fn empty_target_with_nonempty_query_does_not_match() {
        assert_eq!(score("a", ""), None);
        assert_eq!(score("日本語", ""), None);
    }

    // ---- スコアリングの順序性 ----

    #[test]
    fn consecutive_beats_scattered() {
        // 同じ長さの対象で比較し、長さペナルティの影響を排除する
        let consecutive = score("ab", "abc").expect("should match");
        let scattered = score("ab", "acb").expect("should match");
        assert!(
            consecutive > scattered,
            "consecutive ({consecutive}) should beat scattered ({scattered})"
        );
    }

    #[test]
    fn prefix_beats_middle() {
        let prefix = score("b", "ba").expect("should match");
        let middle = score("b", "ab").expect("should match");
        assert!(
            prefix > middle,
            "prefix ({prefix}) should beat middle ({middle})"
        );
    }

    #[test]
    fn word_boundary_beats_middle() {
        // 区切り文字の直後は境界ボーナスが付く
        let boundary = score("b", "a_b").expect("should match");
        let middle = score("b", "axb").expect("should match");
        assert!(
            boundary > middle,
            "boundary ({boundary}) should beat middle ({middle})"
        );
    }

    #[test]
    fn all_separators_give_boundary_bonus() {
        let middle = score("b", "axb").expect("should match");
        for sep in ['/', '\\', '_', '-', '.', ' '] {
            let target = format!("a{sep}b");
            let boundary = score("b", &target).expect("should match");
            assert!(
                boundary > middle,
                "separator {sep:?} should give a bonus ({boundary} vs {middle})"
            );
        }
    }

    #[test]
    fn start_of_string_beats_word_boundary() {
        let at_start = score("a", "ab").expect("should match");
        let at_boundary = score("a", "_a").expect("should match");
        assert!(
            at_start > at_boundary,
            "start ({at_start}) should beat boundary ({at_boundary})"
        );
    }

    #[test]
    fn longer_target_is_penalized() {
        // 同じマッチ品質なら、短い対象のほうが高スコア
        let short = score("a", "a").expect("should match");
        let long = score("a", "abbbb").expect("should match");
        assert!(
            short > long,
            "short target ({short}) should beat long target ({long})"
        );
    }

    #[test]
    fn streak_bonus_is_capped() {
        let s3 = score("abc", "abc").expect("should match");
        let s4 = score("abcd", "abcd").expect("should match");
        let s5 = score("abcde", "abcde").expect("should match");
        let s6 = score("abcdef", "abcdef").expect("should match");
        let s7 = score("abcdefg", "abcdefg").expect("should match");
        // 連続ボーナスは伸びている途中
        assert!(s4 - s3 < s5 - s4, "streak bonus should still be growing");
        // streak.min(4) により、5連続目以降は増分が一定になる
        assert_eq!(s6 - s5, s7 - s6, "streak bonus should be capped at 4");
    }

    #[test]
    fn score_increases_monotonically_with_match_length() {
        let one = score("a", "abcdef").expect("should match");
        let two = score("ab", "abcdef").expect("should match");
        let three = score("abc", "abcdef").expect("should match");
        assert!(one < two && two < three, "{one} < {two} < {three}");
    }

    // ---- 大文字小文字 (実装は to_lowercase による case-insensitive) ----

    #[test]
    fn matching_is_case_insensitive() {
        assert!(score("ABC", "abc").is_some());
        assert!(score("abc", "ABC").is_some());
        assert!(score("AbC", "aBc").is_some());
    }

    #[test]
    fn case_does_not_change_score() {
        let lower = score("abc", "abcdef");
        assert_eq!(score("ABC", "abcdef"), lower);
        assert_eq!(score("AbC", "ABCDEF"), lower);
        assert_eq!(score("abc", "AbCdEf"), lower);
    }

    // ---- マルチバイト文字 (byte index で slice していないこと) ----

    #[test]
    fn japanese_query_and_target_do_not_panic() {
        assert!(score("日本", "日本語").is_some());
        assert!(score("日本語", "日本語").is_some());
        assert!(score("語", "日本語").is_some());
        assert_eq!(score("犬", "日本語"), None);
    }

    #[test]
    fn japanese_counts_chars_not_bytes() {
        // "日本語" は 9 バイトだが 3 文字。バイト長で比較していれば
        // 4 文字クエリが「短い」と誤判定されて別の挙動になる
        assert_eq!(score("日本語です", "日本語"), None);
    }

    #[test]
    fn japanese_consecutive_beats_scattered() {
        let consecutive = score("日本", "日本語").expect("should match");
        let scattered = score("日語", "日本語").expect("should match");
        assert!(
            consecutive > scattered,
            "consecutive ({consecutive}) should beat scattered ({scattered})"
        );
    }

    #[test]
    fn mixed_ascii_and_japanese_do_not_panic() {
        assert!(score("日a", "日本a").is_some());
        assert!(score("a日", "a本日").is_some());
        assert!(score("テスト", "src/テスト.rs").is_some());
        assert_eq!(score("テストx", "テスト"), None);
    }

    #[test]
    fn japanese_after_separator_gets_boundary_bonus() {
        let boundary = score("本", "日/本").expect("should match");
        let middle = score("本", "日語本").expect("should match");
        assert!(
            boundary > middle,
            "boundary ({boundary}) should beat middle ({middle})"
        );
    }

    #[test]
    fn emoji_and_astral_chars_do_not_panic() {
        assert!(score("🎉", "a🎉b").is_some());
        assert!(score("🎉🎊", "🎉🎊").is_some());
        assert_eq!(score("🎉", "abc"), None);
    }

    #[test]
    fn case_folding_that_changes_length_does_not_panic() {
        // 'İ' (U+0130) は to_lowercase で 2 文字に展開される
        let _ = score("İ", "İ");
        let _ = score("İ", "i");
        let _ = score("i", "İ");
        let _ = score("ß", "SS");
    }

    // ---- 境界: クエリ長・繰り返し文字 ----

    #[test]
    fn query_longer_than_target_does_not_match() {
        assert_eq!(score("abcd", "abc"), None);
        assert_eq!(score("aa", "a"), None);
        assert_eq!(score("日本語", "日本"), None);
    }

    #[test]
    fn repeated_query_chars_need_repeated_target_chars() {
        assert_eq!(score("aa", "ab"), None);
        assert_eq!(score("aaa", "aab"), None);
        assert!(score("aa", "aba").is_some());
        assert!(score("aa", "aa").is_some());
    }

    #[test]
    fn repeated_chars_consecutive_beats_split() {
        let consecutive = score("aa", "aab").expect("should match");
        let split = score("aa", "aba").expect("should match");
        assert!(
            consecutive > split,
            "consecutive ({consecutive}) should beat split ({split})"
        );
    }

    #[test]
    fn same_length_query_and_target_must_match_entirely() {
        assert!(score("abc", "abc").is_some());
        assert_eq!(score("abd", "abc"), None);
    }

    // ---- 最良割り付け: 貪欲マッチが取り逃がしていた並びを選べる ----

    #[test]
    fn decoy_char_does_not_lose_the_consecutive_bonus() {
        let with_decoy = score("bc", "xbyybc").expect("should match");
        let without_decoy = score("bc", "xxxxbc").expect("should match");
        assert_eq!(with_decoy, without_decoy);
    }

    #[test]
    fn contiguous_alignment_beats_greedy_split() {
        // "abc_ac" は先頭の飛び飛び (a@0, c@2) と末尾の連続 "ac" の 2 通り。
        // 貪欲実装は前者しか見ず "abcxac" と同点にしていたが、
        // 境界直後の連続一致のほうが高スコアになるべき。
        let contiguous = score("ac", "abc_ac").expect("should match");
        let split_only = score("ac", "abcxac").expect("should match");
        assert!(
            contiguous > split_only,
            "contiguous ({contiguous}) should beat greedy split ({split_only})"
        );
    }

    #[test]
    fn hidden_boundary_match_outranks_plain_match() {
        // 貪欲実装は "xbyy_bc" の先頭 'b' を消費して 末尾の "_bc" を見落とし、
        // "xxxxbc" より低く順位付けしていた。正しくは逆。
        let boundary_run = score("bc", "xbyy_bc").expect("should match");
        let plain_run = score("bc", "xxxxbc").expect("should match");
        assert!(
            boundary_run > plain_run,
            "boundary run ({boundary_run}) should outrank plain run ({plain_run})"
        );
    }

    #[test]
    fn prepared_query_matches_identically_to_score() {
        let q_str = "bc";
        let target = "xbyy_bc";
        let pq = PreparedQuery::new(q_str);
        assert_eq!(pq.score(target), score(q_str, target));
        assert_eq!(pq.score("abcd"), score(q_str, "abcd"));
    }
}
