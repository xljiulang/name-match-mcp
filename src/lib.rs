//! Core name-matching logic for the `name-match-mcp` server.
//!
//! The module is intentionally free of any MCP concerns so it can be unit tested
//! as plain functions: normalization, similarity metrics, candidate recall, and
//! helpers for the workbook surgery in [`xlsx`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub mod replacement;
pub mod server;
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
/// circuit to `1.0`.
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
    exact: HashMap<String, usize>,
    postings: HashMap<u64, Vec<u32>>,
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

        let mut exact = HashMap::with_capacity(normalized.len());
        let mut postings: HashMap<u64, Vec<u32>> = HashMap::new();
        let mut short_refs = Vec::new();

        for (idx, name) in normalized.iter().enumerate() {
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
            exact,
            postings,
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
    fn candidates(&self, target_keys: &[u64], scratch: &mut Scratch) -> Vec<u32> {
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

        candidates
    }

    /// Match one raw target name against the reference set.
    ///
    /// Returns the reference index and the score. The reference index is `None`
    /// when nothing reaches `threshold`; the score is still returned.
    pub fn match_one(&self, target: &str, threshold: f64, scratch: &mut Scratch) -> (Option<usize>, f64) {
        let normalized = normalize(target);
        if normalized.is_empty() || self.normalized.is_empty() {
            return (None, 0.0);
        }

        if let Some(&idx) = self.exact.get(&normalized) {
            return (Some(idx), 1.0);
        }

        let keys = bigrams(&normalized);
        let candidates = self.candidates(&keys, scratch);

        let mut best_idx: Option<usize> = None;
        let mut best_score = 0.0f64;
        for idx in candidates {
            let idx = idx as usize;
            let candidate = &self.normalized[idx];
            if candidate.is_empty() {
                continue;
            }
            let score = score_normalized(&normalized, candidate);
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
            "12厘瑞尔白橡ENF-科嘉康格森",
        ]);
        let target = owned(&[
            "18厘云峰白橡ENF-科嘉康格森",
            "18厘云峰白橡ENF-科嘉康格森",
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
