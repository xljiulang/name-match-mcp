//! Pure orchestration of "read a column, match it, prepare the writes".

use std::collections::HashMap;

use crate::xlsx::{ColumnCell, RowWrite};
use crate::{NameIndex, normalize, round_score};

/// Why a target value matched (or did not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Normalized text was already present in the reference column.
    Exact,
    /// Best fuzzy candidate reached the threshold.
    Fuzzy,
    /// Nothing reached the threshold.
    Miss,
}

/// One matched row, ready to be written back.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchedRow {
    /// 1-based row number in the target worksheet.
    pub row: u32,
    /// The original cell text.
    pub source: String,
    /// Matched reference name, absent when below the threshold.
    pub matched: Option<String>,
    /// Similarity score, `1.0` for exact hits.
    pub score: f64,
    /// Which path produced this row.
    pub kind: MatchKind,
}

/// Counts describing one matching run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MatchTally {
    /// Rows whose normalized text was already in the reference set.
    pub exact: usize,
    /// Rows matched by the fuzzy scorer above the threshold.
    pub fuzzy: usize,
    /// Rows that matched nothing.
    pub miss: usize,
}

impl MatchTally {
    /// Rows that produced a match.
    pub fn matched(&self) -> usize {
        self.exact + self.fuzzy
    }
}

/// Match every value of a reference column against every value of a target
/// column.
///
/// Exact hits are resolved by a normalized lookup table before the fuzzy
/// scorer runs, which is both faster and more predictable than scoring alone:
/// on the real workbook 1400 of 1409 targets are exact.
pub fn match_columns(
    reference: &[String],
    targets: &[ColumnCell],
    threshold: f64,
) -> (Vec<MatchedRow>, MatchTally) {
    let index = NameIndex::new(reference);

    // Normalized reference text -> reference value, first occurrence wins.
    let mut exact: HashMap<String, &str> = HashMap::with_capacity(reference.len());
    for value in reference {
        let key = normalize(value);
        if !key.is_empty() {
            exact.entry(key).or_insert(value.as_str());
        }
    }

    let mut scratch = index.scratch();
    let mut rows = Vec::with_capacity(targets.len());
    let mut tally = MatchTally::default();

    for cell in targets {
        let key = normalize(&cell.value);
        let (matched, score, kind) = if key.is_empty() {
            (None, 0.0, MatchKind::Miss)
        } else if let Some(hit) = exact.get(&key) {
            (Some((*hit).to_string()), 1.0, MatchKind::Exact)
        } else {
            let (candidate, score) = index.match_one(&cell.value, threshold, &mut scratch);
            match candidate {
                Some(position) => (Some(reference[position].clone()), score, MatchKind::Fuzzy),
                None => (None, score, MatchKind::Miss),
            }
        };

        match kind {
            MatchKind::Exact => tally.exact += 1,
            MatchKind::Fuzzy => tally.fuzzy += 1,
            MatchKind::Miss => tally.miss += 1,
        }

        rows.push(MatchedRow {
            row: cell.row,
            source: cell.value.clone(),
            matched,
            score: round_score(score),
            kind,
        });
    }

    (rows, tally)
}

/// Convert matched rows into the cells that should be written.
pub fn to_writes(rows: &[MatchedRow]) -> Vec<RowWrite> {
    rows.iter()
        .map(|row| RowWrite {
            row: row.row,
            matched: row.matched.clone(),
            score: row.score,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(values: &[&str]) -> Vec<ColumnCell> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| ColumnCell {
                row: index as u32 + 2,
                value: (*value).to_string(),
            })
            .collect()
    }

    fn reference(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn exact_hits_short_circuit_the_scorer() {
        let reference = reference(&[
            "贴面18厘9层7627ENF-襄阳天湘",
            "压面18厘林音逸梦ENF4*8-金兔万华",
        ]);
        let targets = cells(&["贴面18厘9层7627ENF-襄阳天湘"]);
        let (rows, tally) = match_columns(&reference, &targets, 0.6);

        assert_eq!(rows[0].kind, MatchKind::Exact);
        assert_eq!(rows[0].score, 1.0);
        assert_eq!(
            rows[0].matched.as_deref(),
            Some("贴面18厘9层7627ENF-襄阳天湘")
        );
        assert_eq!(tally.exact, 1);
        assert_eq!(tally.matched(), 1);
    }

    #[test]
    fn exact_matching_ignores_width_case_and_punctuation() {
        let reference = reference(&["ＡＣＭＥ Ｃｏ., Ltd."]);
        let targets = cells(&["acme co ltd"]);
        let (rows, tally) = match_columns(&reference, &targets, 0.99);

        assert_eq!(rows[0].kind, MatchKind::Exact);
        assert_eq!(rows[0].score, 1.0);
        assert_eq!(rows[0].matched.as_deref(), Some("ＡＣＭＥ Ｃｏ., Ltd."));
        assert_eq!(tally.exact, 1);
    }

    #[test]
    fn fuzzy_fallback_scores_near_misses() {
        let reference = reference(&["压面18厘林音逸梦ENF4*8-金兔万华"]);
        let targets = cells(&["压面18厘林音逸梦ENF4*9-金兔万华"]);
        let (rows, tally) = match_columns(&reference, &targets, 0.6);

        assert_eq!(rows[0].kind, MatchKind::Fuzzy);
        assert_eq!(
            rows[0].matched.as_deref(),
            Some("压面18厘林音逸梦ENF4*8-金兔万华")
        );
        assert!(
            rows[0].score > 0.6 && rows[0].score < 1.0,
            "score {}",
            rows[0].score
        );
        assert_eq!(tally.fuzzy, 1);
        assert_eq!(tally.matched(), 1);
    }

    #[test]
    fn below_threshold_reports_a_miss_but_keeps_the_score() {
        let reference = reference(&["压面18厘林音逸梦ENF4*8-金兔万华"]);
        let targets = cells(&["完全无关的名字ZZZ"]);
        let (rows, tally) = match_columns(&reference, &targets, 0.6);

        assert_eq!(rows[0].kind, MatchKind::Miss);
        assert_eq!(rows[0].matched, None);
        assert!(rows[0].score < 0.6);
        assert_eq!(tally.miss, 1);
        assert_eq!(tally.matched(), 0);
    }

    #[test]
    fn tally_partitions_every_target() {
        let reference = reference(&[
            "贴面18厘9层7627ENF-襄阳天湘",
            "压面18厘林音逸梦ENF4*8-金兔万华",
        ]);
        let targets = cells(&[
            "贴面18厘9层7627ENF-襄阳天湘",     // exact
            "压面18厘林音逸梦ENF4*8-金兔万华", // exact
            "压面18厘林音逸梦ENF4*9-金兔万华", // fuzzy
            "毫不相干XYZ",                     // miss
        ]);
        let (rows, tally) = match_columns(&reference, &targets, 0.6);

        assert_eq!(rows.len(), 4);
        assert_eq!(tally.exact, 2);
        assert_eq!(tally.fuzzy + tally.miss, 2);
        assert_eq!(tally.exact + tally.fuzzy + tally.miss, targets.len());
    }

    #[test]
    fn empty_reference_marks_everything_as_miss() {
        let targets = cells(&["甲", "乙"]);
        let (rows, tally) = match_columns(&[], &targets, 0.0);

        assert!(rows.iter().all(|row| row.kind == MatchKind::Miss));
        assert!(rows.iter().all(|row| row.score == 0.0));
        assert_eq!(tally.miss, 2);
    }

    #[test]
    fn preserves_row_numbers_from_the_source_sheet() {
        let reference = reference(&["甲"]);
        let targets = vec![
            ColumnCell {
                row: 5,
                value: "甲".into(),
            },
            ColumnCell {
                row: 9,
                value: "乙".into(),
            },
        ];
        let (rows, _) = match_columns(&reference, &targets, 0.9);
        let writes = to_writes(&rows);

        assert_eq!(writes[0].row, 5);
        assert_eq!(writes[0].matched.as_deref(), Some("甲"));
        assert_eq!(writes[1].row, 9);
        assert_eq!(writes[1].matched, None);
    }

    #[test]
    fn one_reference_can_serve_many_targets() {
        let reference = reference(&["贴面18厘9层7627ENF-襄阳天湘"]);
        let targets = cells(&["贴面18厘9层7627ENF-襄阳天湘", "贴面18厘9层7627ENF-襄阳天湘"]);
        let (rows, tally) = match_columns(&reference, &targets, 0.6);

        assert!(rows.iter().all(|row| row.matched.is_some()));
        assert_eq!(tally.exact, 2);
    }
}
