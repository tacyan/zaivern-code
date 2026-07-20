/// Simple fuzzy subsequence scoring: higher is better, None means no match.
pub fn score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = target.to_lowercase().chars().collect();
    if q.len() > t.len() {
        return None;
    }

    let mut qi = 0usize;
    let mut score = 0i32;
    let mut streak = 0i32;
    let mut last_match = -10i32;

    for (ti, &c) in t.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if c == q[qi] {
            let ti = ti as i32;
            score += 10;
            if ti == last_match + 1 {
                streak += 1;
                score += 15 * streak.min(4);
            } else {
                streak = 0;
            }
            if ti == 0 {
                score += 25;
            } else {
                let prev = t[(ti - 1) as usize];
                if matches!(prev, '/' | '\\' | '_' | '-' | '.' | ' ') {
                    score += 20;
                }
            }
            last_match = ti;
            qi += 1;
        }
    }

    if qi == q.len() {
        Some(score - (t.len() as i32) / 4)
    } else {
        None
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

    // ---- 現在の挙動の記録: 貪欲マッチは最良の並びを選ばない ----

    #[test]
    fn greedy_matching_can_pick_a_worse_alignment() {
        // "xbyybc": 貪欲に先頭側の 'b' を消費するため、末尾の "bc" という
        // 連続一致を取り逃がす。同じ長さでダミーの 'b' が無い "xxxxbc" のほうが
        // 高スコアになる (現在の実装の既知の限界)。
        let with_decoy = score("bc", "xbyybc").expect("should match");
        let without_decoy = score("bc", "xxxxbc").expect("should match");
        assert!(
            with_decoy < without_decoy,
            "greedy alignment loses the consecutive bonus ({with_decoy} vs {without_decoy})"
        );
    }
}
