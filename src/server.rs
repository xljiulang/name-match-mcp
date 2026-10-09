//! MCP wiring for the single `match_workbook_column` tool.

use std::path::PathBuf;

use rmcp::{
    ErrorData as McpError, Json, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::replacement::{match_columns, to_writes};
use crate::xlsx::{Workbook, XlsxError};
use crate::{DEFAULT_THRESHOLD, backup_path, resolved_path};

/// Arguments accepted by the `match_workbook_column` tool.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct MatchWorkbookRequest {
    #[schemars(
        description = "Path of the .xlsx workbook to modify in place. A timestamped backup is written next to it first."
    )]
    pub xlsx_path: String,
    #[schemars(
        description = "Worksheet that holds the reference (standard) names, e.g. \"9月压面\"."
    )]
    pub reference_sheet: String,
    #[schemars(
        description = "Header text of the reference column that holds the standard names, e.g. \"商品名称\"."
    )]
    pub reference_column: String,
    #[schemars(
        description = "Worksheet whose column should be matched, e.g. \"9月贴面\"."
    )]
    pub target_sheet: String,
    #[schemars(
        description = "Header text of the column to match, e.g. \"组装商品1\". Its original values are left untouched."
    )]
    pub target_column: String,
    #[schemars(
        description = "1-based row number of the header row. Data is read from the row below it. Defaults to 1."
    )]
    pub header_row: Option<u32>,
    #[schemars(description = "Header written above the matched names. Defaults to \"匹配名称\".")]
    pub match_column_name: Option<String>,
    #[schemars(description = "Header written above the scores. Defaults to \"匹配度\".")]
    pub score_column_name: Option<String>,
    #[schemars(
        description = "Minimum score in 0.0..=1.0 for a fuzzy match to be kept. Defaults to 0.6."
    )]
    pub threshold: Option<f64>,
}

/// Summary returned by the tool; the per-row detail lives in the workbook.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MatchWorkbookSummary {
    /// Absolute path of the workbook that was modified.
    pub xlsx_path: String,
    /// Absolute path of the backup taken before the change.
    pub backup_path: String,
    /// Number of names read from the reference column.
    pub reference_count: usize,
    /// Number of non-empty target cells that were matched.
    pub rows_scanned: usize,
    /// Rows matched, exact plus fuzzy.
    pub matched_count: usize,
    /// Rows left empty because nothing reached the threshold.
    pub unmatched_count: usize,
    /// Rows whose normalized text was already in the reference column.
    pub exact_count: usize,
    /// Rows matched by fuzzy scoring.
    pub fuzzy_count: usize,
    /// Column letter the matched names were written to.
    pub match_column: String,
    /// Column letter the scores were written to.
    pub score_column: String,
    /// Whether pre-existing result columns were reused instead of appended.
    pub reused_columns: bool,
    /// Wall-clock duration of the matching step, in milliseconds.
    pub elapsed_ms: u64,
}

/// Server name advertised during the MCP handshake.
pub const SERVER_NAME: &str = "name-match-mcp";
/// Server version advertised during the MCP handshake.
///
/// Kept as a literal because the `#[tool_handler]` attribute is parsed at
/// compile time; `server_version_matches_package_version` guards against drift.
pub const SERVER_VERSION: &str = "0.2.0";

/// Stateless stdio MCP server exposing the workbook matching tool.
#[derive(Debug, Clone, Default)]
pub struct NameMatchServer;

impl NameMatchServer {
    /// Create the server.
    pub fn new() -> Self {
        Self
    }

    /// Run the tool end to end.
    ///
    /// Exposed separately from the MCP plumbing so tests can call it directly.
    pub fn run_match_workbook(
        &self,
        request: MatchWorkbookRequest,
    ) -> Result<MatchWorkbookSummary, McpError> {
        let MatchWorkbookRequest {
            xlsx_path,
            reference_sheet,
            reference_column,
            target_sheet,
            target_column,
            header_row,
            match_column_name,
            score_column_name,
            threshold,
        } = request;

        let threshold = validate_threshold(threshold)?;
        let header_row = header_row.unwrap_or(1);
        if header_row == 0 {
            return Err(McpError::invalid_params(
                "header_row is 1-based, so it must be at least 1".to_string(),
                None,
            ));
        }
        let match_header = match_column_name.unwrap_or_else(|| "匹配名称".to_string());
        let score_header = score_column_name.unwrap_or_else(|| "匹配度".to_string());
        if match_header.trim().is_empty() || score_header.trim().is_empty() {
            return Err(McpError::invalid_params(
                "match_column_name and score_column_name must not be blank".to_string(),
                None,
            ));
        }

        let workbook_path = PathBuf::from(&xlsx_path);
        ensure_xlsx_extension(&workbook_path)?;

        let mut workbook = Workbook::open(&workbook_path).map_err(map_xlsx_error)?;

        let (reference_cells, _) =
            read_column(&workbook, &reference_sheet, &reference_column, header_row, "reference")?;
        if reference_cells.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "reference column {reference_column:?} in worksheet {reference_sheet:?} has no data rows below row {header_row}"
                ),
                None,
            ));
        }
        let reference_values: Vec<String> = reference_cells
            .iter()
            .map(|cell| cell.value.clone())
            .collect();
        let (target_cells, target_column_index) =
            read_column(&workbook, &target_sheet, &target_column, header_row, "target")?;

        let started = std::time::Instant::now();
        let (rows, tally) = match_columns(&reference_values, &target_cells, threshold);
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Back up before the first mutation so a failure leaves the original.
        let backup = backup_path(&workbook_path);
        std::fs::copy(&workbook_path, &backup).map_err(|error| {
            McpError::internal_error(
                format!("cannot create backup {}: {error}", backup.display()),
                None,
            )
        })?;

        let written = workbook
            .write_result_columns(
                &target_sheet,
                header_row,
                &match_header,
                &score_header,
                target_column_index,
                &to_writes(&rows),
            )
            .map_err(map_xlsx_error)?;

        workbook.save(&workbook_path).map_err(|error| {
            McpError::internal_error(
                format!(
                    "failed to write {}: {error}. The original file is unchanged and a backup exists at {}",
                    workbook_path.display(),
                    backup.display()
                ),
                None,
            )
        })?;

        Ok(MatchWorkbookSummary {
            xlsx_path: resolved_path(&workbook_path).display().to_string(),
            backup_path: resolved_path(&backup).display().to_string(),
            reference_count: reference_values.len(),
            rows_scanned: rows.len(),
            matched_count: tally.matched(),
            unmatched_count: tally.miss,
            exact_count: tally.exact,
            fuzzy_count: tally.fuzzy,
            match_column: written.match_letter,
            score_column: written.score_letter,
            reused_columns: written.reused,
            elapsed_ms,
        })
    }
}

#[tool_router]
impl NameMatchServer {
    /// Match a workbook column against a reference column and write the results
    /// back into the same workbook as two new columns.
    #[tool(
        description = "Match the values of one worksheet column against the values of a reference worksheet column of the same .xlsx workbook, then write the matched name and score into two new columns to the right of the target column. Original values are kept, a timestamped backup is written first, and the workbook is modified in place. Exact matches (after width/case/punctuation folding) win before fuzzy scoring. Returns a summary, not the rows."
    )]
    fn match_workbook_column(
        &self,
        Parameters(request): Parameters<MatchWorkbookRequest>,
    ) -> Result<Json<MatchWorkbookSummary>, McpError> {
        self.run_match_workbook(request).map(Json)
    }
}

#[tool_handler(name = "name-match-mcp", version = "0.2.0")]
impl ServerHandler for NameMatchServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                SERVER_NAME,
                SERVER_VERSION,
            ))
            .with_instructions(
                "Workbook name matching: call `match_workbook_column` with xlsx_path plus the reference and target sheet/column headers to append 匹配名称 and 匹配度 columns.",
            )
    }
}

/// Read one column, turning lookup failures into `invalid_params`.
fn read_column(
    workbook: &Workbook,
    sheet: &str,
    column: &str,
    header_row: u32,
    role: &str,
) -> Result<(Vec<crate::xlsx::ColumnCell>, u32), McpError> {
    workbook
        .read_column(sheet, column, header_row)
        .map_err(|error| map_xlsx_error_for(error, role))
}

/// Reject anything that is not a `.xlsx` file.
fn ensure_xlsx_extension(path: &std::path::Path) -> Result<(), McpError> {
    if !path.exists() {
        return Err(McpError::invalid_params(
            format!("workbook not found: {}", resolved_path(path).display()),
            None,
        ));
    }
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "xlsx" => Ok(()),
        other => Err(McpError::invalid_params(
            format!(
                "only .xlsx workbooks are supported, got {}",
                if other.is_empty() {
                    "(no extension)".to_string()
                } else {
                    format!(".{other}")
                }
            ),
            None,
        )),
    }
}

/// Validate the caller-supplied threshold, defaulting to [`DEFAULT_THRESHOLD`].
fn validate_threshold(threshold: Option<f64>) -> Result<f64, McpError> {
    let value = threshold.unwrap_or(DEFAULT_THRESHOLD);
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(McpError::invalid_params(
            format!("threshold must be a finite number in 0.0..=1.0, got {value}"),
            None,
        ));
    }
    Ok(value)
}

fn map_xlsx_error(error: XlsxError) -> McpError {
    match error {
        XlsxError::Invalid(message) => McpError::invalid_params(message, None),
        XlsxError::Io(message) => McpError::internal_error(message, None),
    }
}

fn map_xlsx_error_for(error: XlsxError, role: &str) -> McpError {
    match error {
        XlsxError::Invalid(message) => {
            McpError::invalid_params(format!("{role}: {message}"), None)
        }
        XlsxError::Io(message) => McpError::internal_error(format!("{role}: {message}"), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_workbook;

    #[test]
    fn server_version_matches_package_version() {
        assert_eq!(SERVER_VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn threshold_defaults_and_rejects_out_of_range() {
        assert_eq!(validate_threshold(None).unwrap(), DEFAULT_THRESHOLD);
        assert_eq!(validate_threshold(Some(0.0)).unwrap(), 0.0);
        assert_eq!(validate_threshold(Some(1.0)).unwrap(), 1.0);
        assert!(validate_threshold(Some(-0.1)).is_err());
        assert!(validate_threshold(Some(1.1)).is_err());
        assert!(validate_threshold(Some(f64::NAN)).is_err());
        assert!(validate_threshold(Some(f64::INFINITY)).is_err());
    }

    fn request(path: &std::path::Path) -> MatchWorkbookRequest {
        MatchWorkbookRequest {
            xlsx_path: path.display().to_string(),
            reference_sheet: "参考".into(),
            reference_column: "标准名称".into(),
            target_sheet: "目标".into(),
            target_column: "原始名称".into(),
            header_row: None,
            match_column_name: None,
            score_column_name: None,
            threshold: None,
        }
    }

    #[test]
    fn matches_and_appends_two_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(
            &path,
            &[
                ("参考", &[&["标准名称"], &["甲"], &["乙"], &["丙"]]),
                (
                    "目标",
                    &[&["原始名称"], &["甲"], &["乙"], &["完全不相关ZZZ"]],
                ),
            ],
        );

        let summary = NameMatchServer::new().run_match_workbook(request(&path)).unwrap();

        assert_eq!(summary.reference_count, 3);
        assert_eq!(summary.rows_scanned, 3);
        assert_eq!(summary.exact_count, 2);
        assert_eq!(summary.unmatched_count, 1);
        assert_eq!(summary.matched_count, summary.exact_count + summary.fuzzy_count);
        assert_eq!(summary.match_column, "B");
        assert_eq!(summary.score_column, "C");
        assert!(!summary.reused_columns);
        assert!(std::path::Path::new(&summary.backup_path).exists());
    }

    #[test]
    fn rejects_missing_sheet_with_candidate_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(&path, &[("参考", &[&["标准名称"], &["甲"]])]);

        let mut req = request(&path);
        req.reference_sheet = "不存在的表".into();
        let error = NameMatchServer::new().run_match_workbook(req).unwrap_err();

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("不存在的表"), "{}", error.message);
        assert!(error.message.contains("参考"), "{}", error.message);
    }

    #[test]
    fn rejects_missing_column_with_candidate_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(&path, &[("参考", &[&["标准名称", "序号"], &["甲", "1"]])]);

        let mut req = request(&path);
        req.reference_column = "没有这一列".into();
        let error = NameMatchServer::new().run_match_workbook(req).unwrap_err();

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("没有这一列"), "{}", error.message);
        assert!(error.message.contains("标准名称"), "{}", error.message);
        assert!(error.message.contains("序号"), "{}", error.message);
    }

    #[test]
    fn rejects_non_xlsx_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let xls = dir.path().join("旧格式.xls");
        std::fs::write(&xls, b"not a workbook").unwrap();

        let error = NameMatchServer::new().run_match_workbook(request(&xls)).unwrap_err();
        assert!(error.message.contains("only .xlsx"), "{}", error.message);

        let missing = dir.path().join("没有这个.xlsx");
        let error = NameMatchServer::new().run_match_workbook(request(&missing)).unwrap_err();
        assert!(error.message.contains("not found"), "{}", error.message);
    }

    #[test]
    fn rejects_header_row_without_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(
            &path,
            &[("参考", &[&["标准名称"]]), ("目标", &[&["原始名称"]])],
        );

        let error = NameMatchServer::new().run_match_workbook(request(&path)).unwrap_err();
        assert!(error.message.contains("no data rows"), "{}", error.message);
    }

    #[test]
    fn rejects_out_of_range_header_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(&path, &[("参考", &[&["标准名称"], &["甲"]])]);

        let mut req = request(&path);
        req.header_row = Some(0);
        let error = NameMatchServer::new().run_match_workbook(req).unwrap_err();
        assert!(error.message.contains("1-based"), "{}", error.message);
    }

    #[test]
    fn writes_a_backup_holding_the_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(
            &path,
            &[("参考", &[&["标准名称"], &["甲"]]), ("目标", &[&["原始名称"], &["甲"]])],
        );
        let before = std::fs::read(&path).unwrap();

        let summary = NameMatchServer::new().run_match_workbook(request(&path)).unwrap();

        let backup = std::fs::read(&summary.backup_path).unwrap();
        assert_eq!(backup, before, "backup must be the pre-change workbook");
        assert_ne!(std::fs::read(&path).unwrap(), before, "workbook must have changed");
    }
}
