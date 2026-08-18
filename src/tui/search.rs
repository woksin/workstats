//! Fuzzy matching for the explorer's live filter and its search overlay.
//!
//! Nothing here knows about `app.rs` or `state.rs` on purpose: the scorer is
//! pure string logic, which makes it the one part of the TUI that can be tested
//! without a terminal, a report, or a repository.

use std::cmp::Reverse;

/// Every matched character is worth this before any bonus, so a needle that
/// covers more of a label always beats a shorter one that happened to land
/// prettily.
const SCORE_MATCH: i64 = 16;
/// A character continuing the previous match. Runs read as words.
const BONUS_CONSECUTIVE: i64 = 8;
/// A character starting a token — the string itself, or anything after
/// punctuation. This is what makes `lib` prefer `src/lib.rs` over `collide.rs`.
const BONUS_BOUNDARY: i64 = 10;
/// A capital opening a camel hump. Weaker than real punctuation because the
/// signal is weaker: `IOError` and `Ios` disagree about where the word ends.
const BONUS_CAMEL: i64 = 6;
/// Opening a gap, and then each further character skipped inside it. Opening
/// costs more than widening so that two gaps lose to one long one.
const PENALTY_GAP_START: i64 = 5;
const PENALTY_GAP_EXTEND: i64 = 1;
/// The skipped prefix, capped. A hit near the front of a path is more relevant,
/// but past a point a deep path should not be ranked purely by its depth.
const PENALTY_LEADING_MAX: i64 = 12;
/// How many alignments of the needle to try per candidate. The optimum needs an
/// N x M dynamic program, which at 200 000 candidates per keystroke is not on
/// offer; a small fixed number of probes keeps the work linear in the label.
const MAX_ALIGNMENTS: usize = 4;

/// Fuzzy subsequence score for `needle` inside `haystack`; higher is better,
/// `None` when `needle` is not a subsequence at all. Case-insensitive.
///
/// An empty needle scores `Some(0)`: the live row filter calls this on every
/// keystroke and an empty filter has to keep everything.
pub fn score(needle: &str, haystack: &str) -> Option<i64> {
    let needle: Vec<char> = needle.chars().map(fold).collect();
    let haystack: Vec<char> = haystack.chars().collect();
    best_score(&needle, &haystack, None)
}

/// One thing the user can jump to.
pub struct Target {
    /// `"repo"`, `"commit"` or `"file"`.
    pub kind: &'static str,
    /// Displayed, and what the needle is matched against.
    pub label: String,
    /// Drill-down keys from the overview downwards.
    pub path: Vec<String>,
}

/// One match, ordered best-first by `Index::query` so the score never escapes
/// this module and cannot be mistaken for something comparable across queries.
pub struct Hit {
    /// Index into the targets the index was built from.
    pub target: usize,
    /// Matched character positions in that target's label.
    pub indices: Vec<usize>,
}

/// A prebuilt haystack over repository names, commit lines and file paths.
///
/// The only thing precomputed per entry is a presence bitmask. Storing a folded
/// copy of every label would double the explorer's memory for a monorepo's
/// history, and folding one character at a time during the scan is cheaper than
/// the cache misses that copy would cost.
pub struct Index {
    targets: Vec<Target>,
    masks: Vec<u64>,
}

impl Index {
    pub fn build(targets: Vec<Target>) -> Self {
        let masks = targets
            .iter()
            .map(|target| mask_of(target.label.chars().map(fold)))
            .collect();
        Self { targets, masks }
    }

    /// The best `limit` matches, best first. An empty needle returns nothing:
    /// the overlay shows results only once something has been typed.
    pub fn query(&self, needle: &str, limit: usize) -> Vec<Hit> {
        let needle: Vec<char> = needle.chars().map(fold).collect();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        let wanted = mask_of(needle.iter().copied());
        // Two buffers for the whole keystroke rather than two per candidate:
        // the candidate set itself is built once, in `build`.
        let mut label: Vec<char> = Vec::new();
        let mut ranked: Vec<Ranked> = Vec::with_capacity(limit.min(512));
        for (index, target) in self.targets.iter().enumerate() {
            // A character the label does not contain anywhere rejects it for
            // the cost of one AND, which is what keeps a large index typable.
            if (wanted & !self.masks[index]) != 0 {
                continue;
            }
            // A byte count is never below a character count, so this can only
            // reject labels that were too short to match anyway.
            if needle.len() > target.label.len() {
                continue;
            }
            label.clear();
            label.extend(target.label.chars());
            let Some(score) = best_score(&needle, &label, None) else {
                continue;
            };
            let candidate = Ranked {
                score,
                length: label.len(),
                target: index,
            };
            if ranked.len() >= limit
                && ranked
                    .last()
                    .is_some_and(|last| last.key() <= candidate.key())
            {
                continue;
            }
            let position = ranked.partition_point(|entry| entry.key() < candidate.key());
            ranked.insert(position, candidate);
            ranked.truncate(limit);
        }
        // Match positions are computed only for the handful of rows that will
        // actually be drawn, never for every candidate that scored.
        ranked
            .into_iter()
            .map(|entry| {
                label.clear();
                label.extend(self.targets[entry.target].label.chars());
                let mut indices = Vec::new();
                best_score(&needle, &label, Some(&mut indices));
                Hit {
                    target: entry.target,
                    indices,
                }
            })
            .collect()
    }

    pub fn target(&self, index: usize) -> Option<&Target> {
        self.targets.get(index)
    }
}

struct Ranked {
    score: i64,
    length: usize,
    target: usize,
}

impl Ranked {
    /// Sorted ascending this key puts the best hit first: higher score, then
    /// the shorter label — a needle covering more of a name is the better
    /// answer — then build order, so the same needle always ranks the same way.
    fn key(&self) -> (Reverse<i64>, usize, usize) {
        (Reverse(self.score), self.length, self.target)
    }
}

/// The best alignment of `needle` inside `haystack`, optionally recording the
/// matched character positions.
///
/// Two alignments are scored per probe because neither alone is right on its
/// own: the leftmost greedy one finds `myTestCase` for `tc`, while the tightest
/// one ending at the same place finds `.rs` rather than the `r` of `src` for
/// `rs`. The probe then restarts past the tight start, up to `MAX_ALIGNMENTS`
/// times, so a later and better placement is still reachable.
fn best_score(
    needle: &[char],
    haystack: &[char],
    positions: Option<&mut Vec<usize>>,
) -> Option<i64> {
    if needle.is_empty() {
        if let Some(sink) = positions {
            sink.clear();
        }
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let mut best: Option<(i64, usize, usize)> = None;
    let mut from = 0;
    for _ in 0..MAX_ALIGNMENTS {
        let Some((leftmost, end)) = scan_forward(needle, haystack, from) else {
            break;
        };
        let tight = scan_backward(needle, haystack, end);
        let mut start = leftmost;
        let mut score = score_alignment(needle, haystack, leftmost, end, None);
        if tight != leftmost {
            let tighter = score_alignment(needle, haystack, tight, end, None);
            if tighter > score {
                start = tight;
                score = tighter;
            }
        }
        if best.is_none_or(|(current, _, _)| score > current) {
            best = Some((score, start, end));
        }
        from = tight + 1;
        if from + needle.len() > haystack.len() {
            break;
        }
    }
    let (score, start, end) = best?;
    if let Some(sink) = positions {
        sink.clear();
        score_alignment(needle, haystack, start, end, Some(sink));
    }
    Some(score)
}

/// Greedy left-to-right pass from `from`: where the first needle character
/// landed, and one past where the last one did.
fn scan_forward(needle: &[char], haystack: &[char], from: usize) -> Option<(usize, usize)> {
    let mut wanted = 0;
    let mut leftmost = from;
    for (offset, character) in haystack[from..].iter().enumerate() {
        if fold(*character) != needle[wanted] {
            continue;
        }
        if wanted == 0 {
            leftmost = from + offset;
        }
        wanted += 1;
        if wanted == needle.len() {
            return Some((leftmost, from + offset + 1));
        }
    }
    None
}

/// Greedy right-to-left pass ending at `end`: where the tightest alignment
/// finishing there begins.
fn scan_backward(needle: &[char], haystack: &[char], end: usize) -> usize {
    let mut wanted = needle.len();
    for (index, character) in haystack[..end].iter().enumerate().rev() {
        if fold(*character) != needle[wanted - 1] {
            continue;
        }
        wanted -= 1;
        if wanted == 0 {
            return index;
        }
    }
    // Unreachable: a forward pass already proved an alignment ends at `end`.
    0
}

fn score_alignment(
    needle: &[char],
    haystack: &[char],
    start: usize,
    end: usize,
    mut positions: Option<&mut Vec<usize>>,
) -> i64 {
    let mut score = 0;
    let mut wanted = 0;
    let mut previous: Option<usize> = None;
    for (offset, character) in haystack[start..end].iter().enumerate() {
        if wanted == needle.len() {
            break;
        }
        if fold(*character) != needle[wanted] {
            continue;
        }
        let index = start + offset;
        wanted += 1;
        if let Some(sink) = positions.as_deref_mut() {
            sink.push(index);
        }
        score += SCORE_MATCH;
        match previous {
            Some(before) if before + 1 == index => score += BONUS_CONSECUTIVE,
            Some(before) => {
                let skipped = (index - before - 1) as i64;
                score += boundary_bonus(haystack, index)
                    - PENALTY_GAP_START
                    - PENALTY_GAP_EXTEND * (skipped - 1);
            }
            // Where a match starts says more about it than where it continues.
            None => score += 2 * boundary_bonus(haystack, index),
        }
        previous = Some(index);
    }
    score - (start as i64).min(PENALTY_LEADING_MAX)
}

/// Where a token starts. "Not alphanumeric" is the separator set on purpose:
/// paths, commit subjects and identifiers all break on punctuation, and
/// enumerating that punctuation only gets it wrong for somebody's language.
fn boundary_bonus(haystack: &[char], index: usize) -> i64 {
    let Some(previous) = index.checked_sub(1).map(|before| haystack[before]) else {
        return BONUS_BOUNDARY;
    };
    if !previous.is_alphanumeric() {
        BONUS_BOUNDARY
    } else if !previous.is_uppercase() && haystack[index].is_uppercase() {
        BONUS_CAMEL
    } else {
        0
    }
}

/// Case folding that keeps exactly one character per character, so a recorded
/// position still indexes the original label the renderer highlights.
/// `char::to_lowercase` can expand one character into several, which would
/// silently desynchronise the two.
fn fold(character: char) -> char {
    if character.is_ascii() {
        character.to_ascii_lowercase()
    } else {
        character.to_lowercase().next().unwrap_or(character)
    }
}

/// A presence bitmask over folded characters. Everything outside the buckets
/// below shares the last bit, so the mask can reject but never wrongly accept —
/// and never wrongly reject, which is the property that matters.
fn mask_of(characters: impl Iterator<Item = char>) -> u64 {
    let mut mask = 0;
    for character in characters {
        mask |= 1u64 << bucket(character);
    }
    mask
}

fn bucket(character: char) -> u32 {
    match character {
        'a'..='z' => character as u32 - 'a' as u32,
        '0'..='9' => 26 + character as u32 - '0' as u32,
        '.' => 36,
        '/' => 37,
        '_' => 38,
        '-' => 39,
        ' ' => 40,
        _ => 41,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(label: &str) -> Target {
        Target {
            kind: "file",
            label: label.to_string(),
            path: vec![label.to_string()],
        }
    }

    fn labels(index: &Index, hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|hit| index.target(hit.target).expect("hit").label.clone())
            .collect()
    }

    #[test]
    fn an_empty_needle_keeps_everything() {
        assert_eq!(Some(0), score("", "src/lib.rs"));
        assert_eq!(Some(0), score("", ""));
    }

    #[test]
    fn a_needle_that_is_not_a_subsequence_does_not_match() {
        assert_eq!(None, score("zzz", "src/lib.rs"));
        assert_eq!(None, score("source", "src"));
        assert_eq!(None, score("a", ""));
        // Order is part of being a subsequence.
        assert!(score("sc", "src").is_some());
        assert_eq!(None, score("cs", "src"));
    }

    #[test]
    fn matching_ignores_case_on_both_sides() {
        assert!(score("README", "docs/readme.md").is_some());
        assert_eq!(
            score("readme", "docs/README.md"),
            score("README", "docs/README.md")
        );
    }

    #[test]
    fn a_token_start_outscores_the_middle_of_a_word() {
        let boundary = score("test", "tests/api.rs").expect("matches");
        let buried = score("test", "latest/api.rs").expect("matches");
        assert!(boundary > buried, "{boundary} should beat {buried}");
    }

    #[test]
    fn a_run_outscores_the_same_characters_scattered() {
        let run = score("abc", "abc.rs").expect("matches");
        let scattered = score("abc", "a-b-c.rs").expect("matches");
        assert!(run > scattered, "{run} should beat {scattered}");
    }

    #[test]
    fn a_shallow_hit_outscores_the_same_hit_further_in() {
        let shallow = score("src", "src/lib.rs").expect("matches");
        let deep = score("src", "crates/core/src/lib.rs").expect("matches");
        assert!(shallow > deep, "{shallow} should beat {deep}");
    }

    #[test]
    fn a_longer_needle_outscores_a_shorter_one_on_the_same_label() {
        let long = score("search", "src/tui/search.rs").expect("matches");
        let short = score("sea", "src/tui/search.rs").expect("matches");
        assert!(long > short, "{long} should beat {short}");
    }

    #[test]
    fn the_tightest_alignment_wins_over_the_first_one_found() {
        let index = Index::build(vec![target("src/lib.rs")]);
        let hits = index.query("rs", 5);
        assert_eq!(1, hits.len());
        // The `.rs` extension, not the `r` of `src` with the trailing `s`.
        assert_eq!(vec![8, 9], hits[0].indices);
    }

    #[test]
    fn a_camel_hump_counts_as_a_token_start() {
        let index = Index::build(vec![target("myTestCase")]);
        let hits = index.query("tc", 5);
        assert_eq!(1, hits.len());
        // `T`est`C`ase, not the tighter but meaningless `t` of Tes`t` and `C`.
        assert_eq!(vec![2, 6], hits[0].indices);
    }

    #[test]
    fn matched_positions_are_character_offsets_not_byte_offsets() {
        let index = Index::build(vec![target("spørsmål/test.rs")]);
        let hits = index.query("test", 5);
        assert_eq!(1, hits.len());
        assert_eq!(vec![9, 10, 11, 12], hits[0].indices);
    }

    #[test]
    fn hits_come_back_best_first() {
        let index = Index::build(vec![
            target("src/tui/app.rs"),
            target("app.rs"),
            target("docs/appendix.md"),
        ]);
        let hits = index.query("app", 10);
        assert_eq!(
            vec!["app.rs", "docs/appendix.md", "src/tui/app.rs"],
            labels(&index, &hits)
        );
    }

    #[test]
    fn ranking_is_stable_for_labels_that_score_the_same() {
        let index = Index::build(vec![target("a/x.rs"), target("b/x.rs"), target("c/x.rs")]);
        let first = labels(&index, &index.query("x.rs", 10));
        let second = labels(&index, &index.query("x.rs", 10));
        assert_eq!(first, second);
        assert_eq!(vec!["a/x.rs", "b/x.rs", "c/x.rs"], first);
    }

    #[test]
    fn the_limit_bounds_the_result_and_keeps_the_best() {
        let index = Index::build(vec![
            target("src/tui/app.rs"),
            target("app.rs"),
            target("docs/appendix.md"),
        ]);
        let hits = index.query("app", 1);
        assert_eq!(vec!["app.rs"], labels(&index, &hits));
        assert!(index.query("app", 0).is_empty());
    }

    #[test]
    fn an_empty_needle_or_an_empty_index_answers_nothing() {
        let index = Index::build(vec![target("app.rs")]);
        assert!(index.query("", 10).is_empty());
        let empty = Index::build(Vec::new());
        assert!(empty.query("anything", 10).is_empty());
        assert!(empty.target(0).is_none());
    }

    #[test]
    fn a_target_is_reachable_by_the_index_a_hit_carries() {
        let index = Index::build(vec![target("a.rs"), target("b.rs")]);
        let hits = index.query("b.rs", 5);
        assert_eq!(1, hits.len());
        let found = index.target(hits[0].target).expect("hit");
        assert_eq!("b.rs", found.label);
        assert_eq!("file", found.kind);
        assert_eq!(vec!["b.rs".to_string()], found.path);
        assert!(index.target(2).is_none());
    }

    #[test]
    fn the_cheap_reject_never_hides_a_real_match() {
        // Every prefix of a label is a subsequence of it, so the bitmask and the
        // byte-length shortcut must let all of them through — including the
        // non-ASCII, the punctuation and the digits.
        for label in ["Æther/Test.rs", "a_b-c.d", "0123456789", "  spaced  ", "ß"] {
            let index = Index::build(vec![target(label)]);
            for length in 1..=label.chars().count() {
                let needle: String = label.chars().take(length).collect();
                assert!(
                    !index.query(&needle, 5).is_empty(),
                    "{needle:?} should match {label:?}"
                );
                assert!(score(&needle, label).is_some(), "{needle:?} in {label:?}");
            }
        }
    }

    #[test]
    fn a_needle_longer_than_the_label_is_rejected_without_scanning() {
        let index = Index::build(vec![target("a.rs")]);
        assert!(index.query("a.rs.backup", 5).is_empty());
    }
}
