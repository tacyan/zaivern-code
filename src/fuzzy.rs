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
