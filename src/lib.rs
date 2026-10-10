//! Core name-matching logic for the `name-match` CLI.
//!
//! The crate is intentionally free of any I/O concerns beyond plain file access
//! so everything can be unit tested as plain functions: normalization,
//! similarity metrics, candidate recall, and the workbook surgery in [`xlsx`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub mod cli;
pub mod lockfile;
pub mod replacement;
pub mod test_support;
pub mod xlsx;

/// Default match threshold applied when the caller does not supply one.
pub const DEFAULT_THRESHOLD: f64 = 0.6;
/// Maximum number of recall candidates scored for a single target name.
pub const MAX_CANDIDATES: usize = 200;
/// Weight of the Jaro-Winkler signal inside the combined score.
pub const JARO_WEIGHT: f64 = 0.7;
/// Weight of the character bigram Jaccard signal inside the combined score.
pub const BIGRAM_WEIGHT: f64 = 0.3;

/// One entry of the output set `C`.
///
/// Exactly one entry is produced per input target name, in the same order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchItem {
    /// The original target name, echoed back unchanged.
    pub name: String,
    /// Best reference name, or `None` when the best score is below the threshold.
    pub matched_name: Option<String>,
    /// Best similarity score, in `0.0..=1.0`, rounded to four decimal places.
    pub score: f64,
}

/// Normalize a name into a comparison key.
///
/// Full-width forms are folded to half-width, letters are lowercased, and every
/// character that is neither a letter nor a digit (whitespace, punctuation,
/// brackets, symbols) is dropped.
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let folded = fold_width(ch);
        for lower in folded.to_lowercase() {
            if lower.is_alphanumeric() {
                out.push(lower);
            }
        }
    }
    out
}

/// Map full-width ASCII forms (and the ideographic space) to their half-width
/// equivalents.
fn fold_width(ch: char) -> char {
    match ch {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => {
            char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch)
        }
        _ => ch,
    }
}

/// Extract every run of ASCII digits, in order of appearance.
///
/// Full-width digits are folded first, and the scan runs over the **raw** text
/// rather than the normalized form: normalization drops punctuation, which
/// would turn the size `4*8` into a single `48` and lose a meaningful segment.
///
/// `压面18厘AF5110中古柚木ENF4*8-纵豪` therefore yields `["18", "5110", "4", "8"]`.
pub fn numbers(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        let folded = fold_width(ch);
        if folded.is_ascii_digit() {
            current.push(folded);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Whether two names carry the identical sequence of numbers.
///
/// The comparison is positional and exact, so `4*8` (yielding `["4","8"]`) does
/// not match `48`, and a different sheet thickness never matches.
pub fn numbers_equal(left: &[String], right: &[String]) -> bool {
    left == right
}

/// Key used to group names by their numeric sequence.
///
/// Numbers are joined with a separator that cannot appear inside a digit run,
/// so `["4","8"]` and `["48"]` produce different keys.
pub fn number_key(numbers: &[String]) -> String {
    numbers.join("\u{1F}")
}

/// Jaro similarity over character sequences.
fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_matched = vec![false; a.len()];
    let mut b_matched = vec![false; b.len()];

    let mut matches = 0usize;
    for (i, ch) in a.iter().enumerate() {
        let start = i.saturating_sub(window);
        let end = (i + window + 1).min(b.len());
        for j in start..end {
            if !b_matched[j] && b[j] == *ch {
                a_matched[i] = true;
                b_matched[j] = true;
                matches += 1;
                break;
            }
        }
    }

    if matches == 0 {
        return 0.0;
    }

    let mut transpositions = 0usize;
    let mut cursor = 0usize;
    for i in 0..a.len() {
        if a_matched[i] {
            while !b_matched[cursor] {
                cursor += 1;
            }
            if a[i] != b[cursor] {
                transpositions += 1;
            }
            cursor += 1;
        }
    }

    let m = matches as f64;
    let t = (transpositions / 2) as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - t) / m) / 3.0
}

/// Jaro-Winkler similarity over character sequences.
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let base = jaro(&a, &b);
    if base <= 0.7 {
        return base;
    }
    let prefix = a
        .iter()
        .zip(b.iter())
        .take(4)
        .take_while(|(x, y)| x == y)
        .count() as f64;
    base + prefix * 0.1 * (1.0 - base)
}

/// Sorted, deduplicated character-bigram keys used for Jaccard and recall.
fn bigrams(value: &str) -> Vec<u64> {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 2 {
        return Vec::new();
    }
    let mut keys: Vec<u64> = chars
        .windows(2)
        .map(|pair| ((pair[0] as u64) << 32) | pair[1] as u64)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Jaccard similarity of two sorted, deduplicated bigram key slices.
fn jaccard_sorted(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut i = 0usize;
    let mut j = 0usize;
    let mut intersection = 0usize;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - intersection;
    intersection as f64 / union as f64
}

/// Character bigram Jaccard similarity of two raw strings.
pub fn bigram_jaccard(a: &str, b: &str) -> f64 {
    jaccard_sorted(&bigrams(a), &bigrams(b))
}

/// Combined similarity score in `0.0..=1.0`.
///
/// Both inputs are expected to be normalized already. Identical strings short
/// circuit to `1.0`. This is the *base* score; [`score_with_numbers`] applies
/// the numeric-consistency rule on top.
pub fn score_normalized(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let jw = jaro_winkler(a, b);
    let jc = bigram_jaccard(a, b);
    (JARO_WEIGHT * jw + BIGRAM_WEIGHT * jc).clamp(0.0, 1.0)
}

/// Score two normalized names, treating a numeric mismatch as a different item.
///
/// Names that agree on every number keep their base score. Names that disagree
/// describe different sizes, thicknesses or model codes, so they are not the
/// same product at all and score `0.0` — no threshold can rescue them, which is
/// intentional: a cross-number match is simply wrong.
pub fn score_with_numbers(
    a: &str,
    b: &str,
    a_numbers: &[String],
    b_numbers: &[String],
) -> f64 {
    if numbers_equal(a_numbers, b_numbers) {
        score_normalized(a, b)
    } else {
        0.0
    }
}

/// Round to four decimal places so output scores stay readable and stable.
pub fn round_score(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// Resolve a possibly relative path against the process working directory.
pub fn resolved_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Pre-built search index over the normalized reference names.
pub struct NameIndex {
    normalized: Vec<String>,
    numbers: Vec<Vec<String>>,
    exact: HashMap<String, usize>,
    postings: HashMap<u64, Vec<u32>>,
    /// Reference indices grouped by their exact numeric sequence, so candidates
    /// that agree on every number always survive the bigram candidate cap.
    by_numbers: HashMap<String, Vec<u32>>,
    short_refs: Vec<u32>,
}

/// Scratch buffers reused across targets on a single worker thread.
pub struct Scratch {
    counts: Vec<u32>,
    touched: Vec<u32>,
}

impl NameIndex {
    /// Build an index over the reference set.
    pub fn new(reference_names: &[String]) -> Self {
        let normalized: Vec<String> = reference_names.iter().map(|n| normalize(n)).collect();
        let numbers: Vec<Vec<String>> = reference_names.iter().map(|n| numbers(n)).collect();

        let mut exact = HashMap::with_capacity(normalized.len());
        let mut postings: HashMap<u64, Vec<u32>> = HashMap::new();
        let mut by_numbers: HashMap<String, Vec<u32>> = HashMap::new();
        let mut short_refs = Vec::new();

        for (idx, name) in normalized.iter().enumerate() {
            by_numbers
                .entry(number_key(&numbers[idx]))
                .or_default()
                .push(idx as u32);
            if name.is_empty() {
                continue;
            }
            exact.entry(name.clone()).or_insert(idx);
            let keys = bigrams(name);
            if keys.is_empty() {
                short_refs.push(idx as u32);
            }
            for key in keys {
                postings.entry(key).or_default().push(idx as u32);
            }
        }

        Self {
            normalized,
            numbers,
            exact,
            postings,
            by_numbers,
            short_refs,
        }
    }

    /// Number of indexed reference names.
    pub fn len(&self) -> usize {
        self.normalized.len()
    }

    /// Whether the reference set is empty.
    pub fn is_empty(&self) -> bool {
        self.normalized.is_empty()
    }

    /// Allocate reusable scratch buffers for one worker thread.
    pub fn scratch(&self) -> Scratch {
        Scratch {
            counts: vec![0; self.normalized.len()],
            touched: Vec::with_capacity(MAX_CANDIDATES),
        }
    }

    /// Collect candidate reference indices for one normalized target.
    ///
    /// The shared-bigram counter array lives in `Scratch` and is fully reset on
    /// every exit path, so no stale counts can leak into the next target.
    ///
    /// References that carry the target's exact numeric sequence are always
    /// included, even when the bigram ranking would have pushed them past
    /// [`MAX_CANDIDATES`].
    fn candidates(
        &self,
        target_keys: &[u64],
        target_numbers: &[String],
        scratch: &mut Scratch,
    ) -> Vec<u32> {
        for key in target_keys {
            if let Some(postings) = self.postings.get(key) {
                for &idx in postings {
                    let slot = &mut scratch.counts[idx as usize];
                    if *slot == 0 {
                        scratch.touched.push(idx);
                    }
                    *slot += 1;
                }
            }
        }

        // Snapshot (shared bigram count, index) pairs before clearing counters.
        let mut scored: Vec<(u32, u32)> = scratch
            .touched
            .iter()
            .map(|&idx| (scratch.counts[idx as usize], idx))
            .collect();
        for &idx in &scratch.touched {
            scratch.counts[idx as usize] = 0;
        }
        scratch.touched.clear();

        if scored.len() > MAX_CANDIDATES {
            // Total order (shared bigram count desc, then index asc) so the
            // retained candidate set is deterministic even when counts tie.
            scored.select_nth_unstable_by(MAX_CANDIDATES, |a, b| {
                b.0.cmp(&a.0).then(a.1.cmp(&b.1))
            });
            scored.truncate(MAX_CANDIDATES);
        }

        let mut candidates: Vec<u32> = scored.into_iter().map(|(_, idx)| idx).collect();

        if candidates.is_empty() {
            candidates.extend(0..self.normalized.len() as u32);
        } else {
            candidates.extend(self.short_refs.iter().copied());
        }

        if let Some(same_numbers) = self.by_numbers.get(&number_key(target_numbers)) {
            candidates.extend(same_numbers.iter().copied());
        }

        candidates
    }

    /// Match one raw target name against the reference set.
    ///
    /// Returns the reference index and the score. The reference index is `None`
    /// when nothing reaches `threshold`; the score is still returned.
    ///
    /// The score is the numeric-adjusted value that drives the threshold, so a
    /// candidate whose numbers disagree is reported with its discounted score.
    pub fn match_one(&self, target: &str, threshold: f64, scratch: &mut Scratch) -> (Option<usize>, f64) {
        let normalized = normalize(target);
        if normalized.is_empty() || self.normalized.is_empty() {
            return (None, 0.0);
        }

        let target_numbers = numbers(target);

        // An exact text match only short circuits when the numbers agree too;
        // otherwise it falls through so the numbers are checked while scoring.
        if let Some(&idx) = self.exact.get(&normalized)
            && numbers_equal(&target_numbers, &self.numbers[idx])
        {
            return (Some(idx), 1.0);
        }

        let keys = bigrams(&normalized);
        let candidates = self.candidates(&keys, &target_numbers, scratch);

        let mut best_idx: Option<usize> = None;
        let mut best_score = 0.0f64;
        for idx in candidates {
            let idx = idx as usize;
            let candidate = &self.normalized[idx];
            if candidate.is_empty() {
                continue;
            }
            let score = score_with_numbers(
                &normalized,
                candidate,
                &target_numbers,
                &self.numbers[idx],
            );
            if score > best_score {
                best_score = score;
                best_idx = Some(idx);
            }
        }

        if best_score < threshold {
            (None, best_score)
        } else {
            (best_idx, best_score)
        }
    }
}

/// Match every target name against the reference set.
///
/// The returned vector always has exactly `target_names.len()` entries, in the
/// same order as the input. A single reference name may match several targets.
pub fn match_names(
    reference_names: &[String],
    target_names: &[String],
    threshold: f64,
) -> Vec<MatchItem> {
    let index = NameIndex::new(reference_names);

    target_names
        .par_iter()
        .map_init(
            || index.scratch(),
            |scratch, target| {
                let (matched, score) = index.match_one(target, threshold, scratch);
                MatchItem {
                    name: target.clone(),
                    matched_name: matched.map(|idx| reference_names[idx].clone()),
                    score: round_score(score),
                }
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn normalize_folds_width_case_and_punctuation() {
        assert_eq!(normalize("京东 科技（北京）有限公司"), "京东科技北京有限公司");
        assert_eq!(normalize("ＡＣＭＥ　Ｃｏ., Ltd."), "acmecoltd");
        assert_eq!(normalize("  Hello, World!  "), "helloworld");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("《三体》"), "三体");
    }

    #[test]
    fn jaro_winkler_matches_known_values() {
        assert!((jaro_winkler("martha", "marhta") - 0.9611).abs() < 0.001);
        assert!((jaro_winkler("dixon", "dicksonx") - 0.8133).abs() < 0.001);
        assert_eq!(jaro_winkler("abc", "xyz"), 0.0);
        assert_eq!(jaro_winkler("same", "same"), 1.0);
    }

    #[test]
    fn bigram_jaccard_matches_known_values() {
        assert_eq!(bigram_jaccard("abc", "abc"), 1.0);
        assert_eq!(bigram_jaccard("ab", "cd"), 0.0);
        // "abcd" bigrams {ab,bc,cd}, "abce" bigrams {ab,bc,ce} -> 2/4
        assert!((bigram_jaccard("abcd", "abce") - 0.5).abs() < 1e-9);
        assert_eq!(bigram_jaccard("a", "a"), 1.0);
    }

    #[test]
    fn exact_normalized_match_scores_one() {
        let reference = owned(&["Acme Corp"]);
        let items = match_names(&reference, &owned(&["ＡＣＭＥ ＣＯＲＰ"]), DEFAULT_THRESHOLD);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].matched_name.as_deref(), Some("Acme Corp"));
        assert_eq!(items[0].score, 1.0);
    }

    #[test]
    fn threshold_boundary_is_inclusive() {
        let reference = owned(&["北京京东科技有限公司"]);
        let target = owned(&["京东科技"]);
        let score = score_normalized(&normalize("京东科技"), &normalize("北京京东科技有限公司"));

        let hit = match_names(&reference, &target, score);
        assert_eq!(hit[0].matched_name.as_deref(), Some("北京京东科技有限公司"));

        let miss = match_names(&reference, &target, score + 1e-6);
        assert_eq!(miss[0].matched_name, None);
        assert!((miss[0].score - round_score(score)).abs() < 1e-9);
    }

    #[test]
    fn output_length_and_order_follow_targets() {
        let reference = owned(&["Alpha", "Beta", "Gamma"]);
        let target = owned(&["Gamma", "Alpha", "Unrelated XYZ"]);
        let items = match_names(&reference, &target, DEFAULT_THRESHOLD);
        assert_eq!(items.len(), target.len());
        assert_eq!(items[0].name, "Gamma");
        assert_eq!(items[0].matched_name.as_deref(), Some("Gamma"));
        assert_eq!(items[1].matched_name.as_deref(), Some("Alpha"));
        assert_eq!(items[2].name, "Unrelated XYZ");
        assert_eq!(items[2].matched_name, None);
    }

    #[test]
    fn one_reference_can_match_several_targets() {
        let reference = owned(&["Acme Corporation"]);
        let target = owned(&["Acme Corporaton", "ACME  corporation"]);
        let items = match_names(&reference, &target, 0.5);
        assert_eq!(items[0].matched_name.as_deref(), Some("Acme Corporation"));
        assert_eq!(items[1].matched_name.as_deref(), Some("Acme Corporation"));
    }

    #[test]
    fn empty_inputs_are_handled() {
        assert!(match_names(&owned(&[]), &owned(&[]), DEFAULT_THRESHOLD).is_empty());

        let no_reference = match_names(&owned(&[]), &owned(&["Anything"]), DEFAULT_THRESHOLD);
        assert_eq!(no_reference.len(), 1);
        assert_eq!(no_reference[0].matched_name, None);
        assert_eq!(no_reference[0].score, 0.0);

        let blank_target = match_names(&owned(&["Acme"]), &owned(&["   "]), DEFAULT_THRESHOLD);
        assert_eq!(blank_target[0].matched_name, None);
        assert_eq!(blank_target[0].score, 0.0);

        let blank_reference = match_names(&owned(&["  "]), &owned(&["Acme"]), DEFAULT_THRESHOLD);
        assert_eq!(blank_reference[0].matched_name, None);
    }

    #[test]
    fn ties_break_on_lowest_reference_index() {
        let reference = owned(&["Acme Ltd", "Acme Ltd"]);
        let items = match_names(&reference, &owned(&["Acme Ltd"]), DEFAULT_THRESHOLD);
        // Both entries normalize identically, so the first index wins.
        assert_eq!(items[0].score, 1.0);
        let index = NameIndex::new(&reference);
        let mut scratch = index.scratch();
        assert_eq!(index.match_one("Acme Ltd", DEFAULT_THRESHOLD, &mut scratch).0, Some(0));
    }

    #[test]
    fn short_names_still_recall_candidates() {
        let reference = owned(&["京", "京东"]);
        let items = match_names(&reference, &owned(&["京"]), DEFAULT_THRESHOLD);
        assert_eq!(items[0].score, 1.0);
        assert_eq!(items[0].matched_name.as_deref(), Some("京"));
    }

    #[test]
    fn recall_truncation_keeps_the_best_candidate() {
        // More than MAX_CANDIDATES references share the target's bigrams. The
        // true best candidate is inserted last, so it only survives if the
        // shared-bigram ranking (and its tie-break) is applied correctly.
        let mut reference: Vec<String> =
            (0..MAX_CANDIDATES + 50).map(|i| format!("Acme 工业 板材 型号{i}")).collect();
        reference.push("Acme 工业 板材 型号0".to_string());

        let target = owned(&["Acme 工业 板材 型号0"]);
        let items = match_names(&reference, &target, DEFAULT_THRESHOLD);
        assert_eq!(items[0].matched_name.as_deref(), Some("Acme 工业 板材 型号0"));

        // Repeating the run must give the identical result: no stale counters.
        for _ in 0..3 {
            let again = match_names(&reference, &target, DEFAULT_THRESHOLD);
            assert_eq!(again[0].matched_name, items[0].matched_name);
            assert_eq!(again[0].score, items[0].score);
        }
    }

    #[test]
    fn repeated_calls_keep_scores_stable_across_targets() {
        // A single worker thread reuses one Scratch across many targets; a
        // missed counter reset would make later scores depend on earlier ones.
        let reference = owned(&[
            "18厘云峰白橡ENF4*8-科嘉/康格森",
            "18厘迪奥皮纹灰ENF4*8-科嘉/康格森",
            "12厘瑞尔白橡ENF4*8-科嘉康格森",
        ]);
        let target = owned(&[
            "18厘云峰白橡ENF4*8-科嘉康格森",
            "18厘云峰白橡ENF4*8-科嘉康格森",
        ]);

        let items = match_names(&reference, &target, 0.0);
        assert_eq!(items[0].matched_name, items[1].matched_name);
        assert_eq!(items[0].score, items[1].score);
        assert!(items[0].score > 0.9, "expected a near match, got {}", items[0].score);
    }

    #[test]
    fn scores_are_rounded_to_four_decimals() {
        let reference = owned(&["Shanghai Pudong Development Bank"]);
        let items = match_names(
            &reference,
            &owned(&["Shanghai Pudong Developmnt Bank"]),
            DEFAULT_THRESHOLD,
        );
        let score = items[0].score;
        assert!((score * 10_000.0).fract().abs() < 1e-9);
        assert!((0.0..=1.0).contains(&score));
    }

    #[test]
    fn extracts_numbers_in_order_from_raw_text() {
        assert_eq!(numbers("压面18厘林音逸梦ENF4*8-金兔万华"), ["18", "4", "8"]);
        assert_eq!(numbers("4*8"), ["4", "8"], "the separator keeps the segments apart");
        assert_eq!(numbers("48"), ["48"], "a single run stays whole");
        assert_eq!(numbers("ＡＥ３５０３"), ["3503"], "full-width digits are folded");
        assert_eq!(numbers("压面9厘AF5110中古柚木ENF4*8-纵豪"), ["9", "5110", "4", "8"]);
        assert!(numbers("无数字名称").is_empty());
        assert!(numbers("").is_empty());

        // Numbering must come from the raw string: normalization drops `*`,
        // which would merge `4*8` into `48`.
        assert_eq!(normalize("ENF4*8"), "enf48");
        assert_eq!(numbers("ENF4*8"), ["4", "8"]);
    }

    #[test]
    fn a_numeric_mismatch_scores_zero_regardless_of_text_similarity() {
        const LEFT: &str = "压面18厘abcdef";
        const RIGHT_SAME: &str = "压面18厘abcdeg";
        const RIGHT_DIFF: &str = "压面9厘abcdeg";

        let same = score_with_numbers(
            LEFT,
            RIGHT_SAME,
            &numbers(LEFT),
            &numbers(RIGHT_SAME),
        );
        assert!(
            (same - score_normalized(LEFT, RIGHT_SAME)).abs() < 1e-12,
            "equal numbers keep the base score"
        );

        // `LEFT` and `RIGHT_DIFF` differ only in the thickness, so the text
        // similarity alone would comfortably clear the default threshold — yet a
        // different thickness means a different product, so the score must
        // collapse to zero instead.
        let different = score_with_numbers(
            LEFT,
            RIGHT_DIFF,
            &numbers(LEFT),
            &numbers(RIGHT_DIFF),
        );
        assert!(
            score_normalized(LEFT, RIGHT_DIFF) > DEFAULT_THRESHOLD,
            "the text alone would have matched, so only the numbers can separate them"
        );
        assert_eq!(different, 0.0, "a numeric mismatch is not the same product");
    }

    #[test]
    fn numbers_compare_positionally_not_as_a_set() {
        assert!(numbers_equal(&numbers("ENF4*8"), &numbers("4-8")));
        assert!(!numbers_equal(&numbers("ENF4*8"), &numbers("ENF8*4")));
        assert!(!numbers_equal(&numbers("48"), &numbers("4*8")));
        assert_ne!(number_key(&numbers("4*8")), number_key(&numbers("48")));
    }

    #[test]
    fn exact_text_with_different_numbers_is_not_short_circuited() {
        // `ENF4*8` and `ENF48` normalize to the same key but carry different
        // numbers (`["4","8"]` vs `["48"]`), so the exact hit must not be
        // reported as a perfect score.
        let index = NameIndex::new(&owned(&["ABCDENF4*8"]));
        let mut scratch = index.scratch();

        let (matched, score) = index.match_one("ABCDENF4*8", DEFAULT_THRESHOLD, &mut scratch);
        assert_eq!(matched, Some(0));
        assert_eq!(score, 1.0, "identical text and numbers still short circuits");

        let (matched, score) = index.match_one("ABCDENF48", DEFAULT_THRESHOLD, &mut scratch);
        assert_eq!(matched, None, "the only candidate has a different number sequence");
        assert_eq!(score, 0.0);

        // No threshold can rescue it, not even the most permissive one.
        let (matched, _) = index.match_one("ABCDENF48", 0.0, &mut scratch);
        assert_eq!(matched, None, "lowering the threshold must not enable a cross-number match");
    }

    #[test]
    fn same_model_different_thickness_picks_the_matching_thickness() {
        // Both entries normalize to nearly the same text; only the numbers tell
        // them apart, and the old scorer happily returned the 18厘 one.
        let reference = owned(&[
            "压面18厘梵高ENF4*8-金兔万华",
            "压面9厘梵高ENF4*8-金兔万华",
        ]);
        let items = match_names(&reference, &owned(&["压面9厘梵高ENF4*8-金兔万华"]), DEFAULT_THRESHOLD);
        assert_eq!(items[0].matched_name.as_deref(), Some("压面9厘梵高ENF4*8-金兔万华"));
        assert_eq!(items[0].score, 1.0);
    }

    #[test]
    fn a_missing_thickness_is_no_longer_answered_with_a_neighbour() {
        // Only the 18厘 variant exists; the 9厘 request must not borrow it.
        let reference = owned(&["压面18厘样板ENF4*8"]);
        let items = match_names(&reference, &owned(&["压面9厘样板ENF4*8"]), DEFAULT_THRESHOLD);
        assert_eq!(items[0].matched_name, None, "no thickness-compatible candidate exists");
        assert!(items[0].score < DEFAULT_THRESHOLD, "score {}", items[0].score);
    }

    #[test]
    fn numeric_matches_survive_recall_truncation() {
        // Far more references than MAX_CANDIDATES share the target's bigrams.
        // The one carrying the same numbers is inserted last and has no bigram
        // advantage, so it only survives because of the numeric index.
        let mut reference: Vec<String> = (0..MAX_CANDIDATES + 50)
            .map(|i| format!("压面18厘型号{i}ENF4*8-某厂"))
            .collect();
        reference.push("型号0ENF4*8".to_string());

        let items = match_names(&reference, &owned(&["型号0ENF4*8"]), DEFAULT_THRESHOLD);
        assert_eq!(items[0].matched_name.as_deref(), Some("型号0ENF4*8"));
        assert_eq!(items[0].score, 1.0);
    }

    #[test]
    fn large_inputs_stay_fast() {
        let reference: Vec<String> = (0..10_000).map(|i| format!("Company Number {i}")).collect();
        let target: Vec<String> = (0..10_000).map(|i| format!("Company Number {i}")).collect();
        let started = std::time::Instant::now();
        let items = match_names(&reference, &target, DEFAULT_THRESHOLD);
        assert_eq!(items.len(), 10_000);
        assert!(items.iter().all(|item| item.score == 1.0));
        assert!(
            started.elapsed().as_secs() < 30,
            "10k x 10k matching took {:?}",
            started.elapsed()
        );
    }
}
