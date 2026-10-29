//! Fuzzy matching for the quick switcher: does `query` match `candidate` as an ordered run of
//! characters, how well, and which characters matched (for highlighting).
//!
//! The match is case-insensitive and does not fold accents (the same rule as search). It prefers
//! matches that start the candidate or a word, that run together, and that sit in short candidates,
//! so typing `gen` finds `general` before `engineering` and `de` finds `design` before `modest`.

/// A successful match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Higher is better. Only comparable between matches of the same query.
    pub score: i32,
    /// The matched characters, as indexes into the candidate's characters, strictly increasing.
    pub positions: Vec<usize>,
}

const MATCH: i32 = 16;
const START: i32 = 24;
const WORD_START: i32 = 20;
const CONSECUTIVE: i32 = 18;
const GAP: i32 = 1;
const MAX_GAP_PENALTY: i32 = 8;
const EXACT: i32 = 200;
/// Candidates longer than this are matched on their first characters only, which bounds the cost.
const MAX_CANDIDATE_CHARS: usize = 120;
const MAX_QUERY_CHARS: usize = 40;

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_word_start(candidate: &[char], at: usize) -> bool {
    at == 0 || {
        let before = candidate[at - 1];
        let here = candidate[at];
        !before.is_alphanumeric() || (before.is_lowercase() && here.is_uppercase())
    }
}

/// Matches `query` against `candidate`. An empty query matches everything with score 0. `None`
/// when the query's characters do not all appear in order.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    let wanted: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(MAX_QUERY_CHARS)
        .map(fold)
        .collect();
    if wanted.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let original: Vec<char> = candidate.chars().take(MAX_CANDIDATE_CHARS).collect();
    let folded: Vec<char> = original.iter().copied().map(fold).collect();
    let (m, n) = (wanted.len(), folded.len());
    if m > n {
        return None;
    }
    // best[i][j]: the best score matching wanted[..=i] with wanted[i] at folded[j]; with the
    // previous choice for walking back.
    const NONE: i32 = i32::MIN;
    let mut best = vec![vec![NONE; n]; m];
    let mut from = vec![vec![usize::MAX; n]; m];
    for j in 0..n {
        if folded[j] == wanted[0] {
            let mut score = MATCH;
            if j == 0 {
                score += START;
            } else if is_word_start(&original, j) {
                score += WORD_START;
            }
            best[0][j] = score - (j as i32 * GAP).min(MAX_GAP_PENALTY);
        }
    }
    for i in 1..m {
        for j in i..n {
            if folded[j] != wanted[i] {
                continue;
            }
            let mut choice = (NONE, usize::MAX);
            for (offset, &before) in best[i - 1][(i - 1)..j].iter().enumerate() {
                if before == NONE {
                    continue;
                }
                let k = i - 1 + offset;
                let gap = (j - k - 1) as i32;
                let mut score = before + MATCH;
                if gap == 0 {
                    score += CONSECUTIVE;
                } else {
                    score -= (gap * GAP).min(MAX_GAP_PENALTY);
                }
                if is_word_start(&original, j) {
                    score += WORD_START;
                }
                if score > choice.0 {
                    choice = (score, k);
                }
            }
            best[i][j] = choice.0;
            from[i][j] = choice.1;
        }
    }
    let (mut score, mut at) = (NONE, usize::MAX);
    for (j, &candidate_score) in best[m - 1].iter().enumerate().skip(m - 1) {
        if candidate_score > score {
            score = candidate_score;
            at = j;
        }
    }
    if score == NONE {
        return None;
    }
    let mut positions = vec![0; m];
    let mut i = m - 1;
    loop {
        positions[i] = at;
        if i == 0 {
            break;
        }
        at = from[i][at];
        i -= 1;
    }
    if folded == wanted {
        score += EXACT;
    }
    // Shorter candidates win ties.
    score -= (n as i32) / 8;
    Some(FuzzyMatch { score, positions })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn score(query: &str, candidate: &str) -> i32 {
        fuzzy_match(query, candidate)
            .unwrap_or_else(|| panic!("{query:?} should match {candidate:?}"))
            .score
    }

    #[test]
    fn the_query_must_appear_in_order() {
        assert!(fuzzy_match("gen", "general").is_some());
        assert!(fuzzy_match("gnl", "general").is_some());
        assert!(fuzzy_match("lng", "general").is_none());
        assert!(fuzzy_match("generalx", "general").is_none());
        assert!(fuzzy_match("x", "").is_none());
        // Case does not matter, spaces in the query are ignored, an empty query matches all.
        assert!(fuzzy_match("GEN", "General").is_some());
        assert!(fuzzy_match("de sign", "design").is_some());
        assert_eq!(fuzzy_match("", "anything").unwrap().score, 0);
        assert_eq!(
            fuzzy_match("  ", "").unwrap().positions,
            Vec::<usize>::new()
        );
    }

    #[test]
    fn positions_say_which_characters_matched() {
        let m = fuzzy_match("gnl", "general").unwrap();
        assert_eq!(m.positions, [0, 2, 6]);
        let m = fuzzy_match("de", "model design").unwrap();
        // Prefers the start of the word `design` to the `d` inside `model`.
        assert_eq!(m.positions, [6, 7]);
    }

    #[test]
    fn starts_of_candidates_and_words_and_runs_score_higher() {
        assert!(score("gen", "general") > score("gen", "engineering"));
        assert!(score("de", "design") > score("de", "modest"));
        assert!(score("dev", "dev-team") > score("dev", "a-dev-team"));
        assert!(score("ab", "a-b") > score("ab", "axxxb"));
        assert!(score("ab", "ab") > score("ab", "abc"));
        // A word start inside the candidate beats a middle of a word.
        assert!(score("ops", "team-ops") > score("ops", "stopsign"));
        // CamelCase counts as word starts.
        assert!(score("pp", "PartyParrot") > score("pp", "apple"));
        // An exact match wins by a lot.
        assert!(score("design", "design") > score("design", "design-team"));
    }

    #[test]
    fn any_script_works_and_nothing_panics() {
        assert!(fuzzy_match("日本", "日本語チャンネル").is_some());
        assert!(fuzzy_match("café", "Café Crème").is_some());
        // No accent folding, as in search.
        assert!(fuzzy_match("cafe", "café").is_none());
        assert!(fuzzy_match("🎉", "party 🎉 room").is_some());
        let long = "a".repeat(10_000);
        assert!(fuzzy_match("aaaa", &long).is_some());
        assert!(fuzzy_match(&"a".repeat(200), &long).is_some());
        assert!(fuzzy_match("b", &long).is_none());
    }

    proptest! {
        #[test]
        fn a_match_is_exactly_a_case_insensitive_subsequence(
            query in "[a-zA-Z0-9 é日🎉]{0,6}",
            candidate in "[a-zA-Z0-9 -_é日🎉]{0,24}",
        ) {
            let wanted: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(fold).collect();
            let chars: Vec<char> = candidate.chars().collect();
            let mut at = 0;
            let is_subsequence = wanted.iter().all(|w| {
                while at < chars.len() && fold(chars[at]) != *w {
                    at += 1;
                }
                let found = at < chars.len();
                at += 1;
                found
            });
            let result = fuzzy_match(&query, &candidate);
            prop_assert_eq!(result.is_some(), is_subsequence);
            if let Some(m) = result {
                prop_assert_eq!(m.positions.len(), wanted.len());
                prop_assert!(m.positions.windows(2).all(|w| w[0] < w[1]));
                for (p, w) in m.positions.iter().zip(&wanted) {
                    prop_assert_eq!(fold(chars[*p]), *w);
                }
            }
        }

        #[test]
        fn a_candidate_always_matches_itself_better_than_anything_longer(candidate in "[a-z]{1,12}") {
            let exact = fuzzy_match(&candidate, &candidate).unwrap().score;
            let longer = fuzzy_match(&candidate, &format!("{candidate}-extra")).unwrap().score;
            prop_assert!(exact > longer);
        }
    }
}
