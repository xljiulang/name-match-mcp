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
use crate::{DEFAULT_THRESHOLD, resolved_path};

/// Arguments accepted by the `match_workbook_column` tool.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct MatchWorkbookRequest {
    #[schemars(
        description = "待处理的 .xlsx 工作簿路径，工具会原地修改该文件（不会生成备份，请自行保留副本）"
    )]
    pub xlsx_path: String,
    #[schemars(
        description = "标准名称所在的工作表名，例如 \"9月压面\""
    )]
    pub reference_sheet: String,
    #[schemars(
        description = "标准名称列的列名（表头文字），例如 \"商品名称\""
    )]
    pub reference_column: String,
    #[schemars(
        description = "需要匹配的工作表名，例如 \"9月贴面\""
    )]
    pub target_sheet: String,
    #[schemars(
        description = "需要匹配的列名（表头文字），例如 \"组装商品1\"；该列原有内容不会被修改"
    )]
    pub target_column: String,
    #[schemars(
        description = "表头所在行号，从 1 开始计数，数据从下一行开始读取；默认 1"
    )]
    pub header_row: Option<u32>,
    #[schemars(description = "写入匹配名称那两列中「匹配名称」列的表头；默认 \"匹配名称\"")]
    pub match_column_name: Option<String>,
    #[schemars(description = "写入匹配度那一列的表头；默认 \"匹配度\"")]
    pub score_column_name: Option<String>,
    #[schemars(
        description = "模糊匹配的最低分数，取值范围 0.0~1.0，低于该分数视为未匹配；默认 0.6"
    )]
    pub threshold: Option<f64>,
}

/// Summary returned by the tool; the per-row detail lives in the workbook.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MatchWorkbookSummary {
    /// Absolute path of the workbook that was modified.
    pub xlsx_path: String,
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
pub const SERVER_VERSION: &str = "0.3.0";

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
                "header_row 从 1 开始计数，必须大于等于 1".to_string(),
                None,
            ));
        }
        let match_header = match_column_name.unwrap_or_else(|| "匹配名称".to_string());
        let score_header = score_column_name.unwrap_or_else(|| "匹配度".to_string());
        if match_header.trim().is_empty() || score_header.trim().is_empty() {
            return Err(McpError::invalid_params(
                "match_column_name 与 score_column_name 不能为空白".to_string(),
                None,
            ));
        }

        let workbook_path = PathBuf::from(&xlsx_path);
        ensure_xlsx_extension(&workbook_path)?;

        let mut workbook = Workbook::open(&workbook_path).map_err(map_xlsx_error)?;

        let (reference_cells, _) =
            read_column(&workbook, &reference_sheet, &reference_column, header_row, "标准名称列")?;
        if reference_cells.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "工作表 {reference_sheet:?} 的 {reference_column:?} 列在第 {header_row} 行之下没有任何数据"
                ),
                None,
            ));
        }
        let reference_values: Vec<String> = reference_cells
            .iter()
            .map(|cell| cell.value.clone())
            .collect();
        let (target_cells, target_column_index) =
            read_column(&workbook, &target_sheet, &target_column, header_row, "待匹配列")?;

        let started = std::time::Instant::now();
        let (rows, tally) = match_columns(&reference_values, &target_cells, threshold);
        let elapsed_ms = started.elapsed().as_millis() as u64;

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
                    "写入 {} 失败：{error}。原文件未被修改",
                    workbook_path.display()
                ),
                None,
            )
        })?;

        Ok(MatchWorkbookSummary {
            xlsx_path: resolved_path(&workbook_path).display().to_string(),
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
    /// 按列匹配名称并把结果回写到工作簿。
    #[tool(
        title = "按列匹配名称并回写工作簿",
        description = "把同一个 .xlsx 工作簿里某张表某列的名称，按另一张表某列的标准名称做匹配，并把「匹配名称」「匹配度」两列写到目标列右侧，工作簿原地修改、原列保留。匹配先做精确比对（忽略全角半角、大小写、空白与标点括号），未命中再按相似度模糊匹配，低于阈值的行匹配名称为空、匹配度照写。可重复调用：同一对结果列会被整体覆盖，不会重复追加；每次调用结果列只反映当次匹配。返回摘要信息，不返回明细行。"
    )]
    fn match_workbook_column(
        &self,
        Parameters(request): Parameters<MatchWorkbookRequest>,
    ) -> Result<Json<MatchWorkbookSummary>, McpError> {
        self.run_match_workbook(request).map(Json)
    }
}

#[tool_handler(name = "name-match-mcp", version = "0.3.0")]
impl ServerHandler for NameMatchServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                rmcp::model::Implementation::new(SERVER_NAME, SERVER_VERSION)
                    .with_title("名称匹配")
                    .with_description("在工作簿内按列匹配名称并回写结果"),
            )
            .with_instructions(
                "把同一个工作簿内某张表某列的名称，按另一张表某列的标准名称匹配，结果写入目标列右侧的『匹配名称』『匹配度』两列；原列保留；可重复调用，结果列会被覆盖。",
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
            format!("工作簿不存在：{}", resolved_path(path).display()),
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
                "只支持 .xlsx 工作簿，当前文件为 {}",
                if other.is_empty() {
                    "无扩展名".to_string()
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
            format!("threshold 必须是 0.0~1.0 之间的有限数字，当前为 {value}"),
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

    /// A request that keeps everything on one sheet, matching `column` against
    /// itself. Handy for tests that only need columns of differing lengths.
    fn on_sheet(path: &std::path::Path, column: &str) -> MatchWorkbookRequest {
        MatchWorkbookRequest {
            xlsx_path: path.display().to_string(),
            reference_sheet: "目标".into(),
            reference_column: "列一".into(),
            target_sheet: "目标".into(),
            target_column: column.into(),
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
        assert!(std::path::Path::new(&summary.xlsx_path).exists());
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
        assert!(error.message.contains("只支持 .xlsx"), "{}", error.message);

        let missing = dir.path().join("没有这个.xlsx");
        let error = NameMatchServer::new().run_match_workbook(request(&missing)).unwrap_err();
        assert!(error.message.contains("工作簿不存在"), "{}", error.message);
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
        assert!(error.message.contains("没有任何数据"), "{}", error.message);
    }

    #[test]
    fn rejects_out_of_range_header_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(&path, &[("参考", &[&["标准名称"], &["甲"]])]);

        let mut req = request(&path);
        req.header_row = Some(0);
        let error = NameMatchServer::new().run_match_workbook(req).unwrap_err();
        assert!(error.message.contains("从 1 开始计数"), "{}", error.message);
    }

    #[test]
    fn modifies_the_workbook_in_place_without_creating_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(
            &path,
            &[("参考", &[&["标准名称"], &["甲"]]), ("目标", &[&["原始名称"], &["甲"]])],
        );
        let before = std::fs::read(&path).unwrap();

        NameMatchServer::new().run_match_workbook(request(&path)).unwrap();

        assert_ne!(std::fs::read(&path).unwrap(), before, "workbook must have changed");
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "统计.xlsx")
            .collect();
        assert!(leftovers.is_empty(), "no backup or temp file should remain: {leftovers:?}");
    }

    #[test]
    fn repeated_runs_do_not_duplicate_result_cells() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(
            &path,
            &[("参考", &[&["标准名称"], &["甲"], &["乙"]]), ("目标", &[&["原始名称"], &["甲"], &["乙"]])],
        );

        let server = NameMatchServer::new();
        let first = server.run_match_workbook(request(&path)).unwrap();
        assert!(!first.reused_columns, "first run appends the columns");
        let second = server.run_match_workbook(request(&path)).unwrap();
        assert!(second.reused_columns, "second run reuses the same columns");
        assert_eq!(second.match_column, first.match_column);
        assert_eq!(second.score_column, first.score_column);

        let sheet = crate::test_support::read_all_text(&path, "xl/worksheets/sheet2.xml");
        for reference in ["B1", "C1", "B2", "C2", "B3", "C3"] {
            assert_eq!(
                sheet.matches(&format!(r#"r="{reference}""#)).count(),
                1,
                "cell {reference} must appear exactly once: {sheet}"
            );
        }
        // The declared extents must not creep either.
        assert_eq!(sheet.matches("<dimension").count(), 1);
        assert_eq!(sheet.matches(r#"ref="A1:C3""#).count(), 1, "dimension covers the new columns");
    }

    #[test]
    fn switching_the_target_column_clears_the_previous_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("两列.xlsx");
        // Two columns of different lengths on one sheet: the long column runs
        // first, so the second run must clear the rows it never visits.
        build_workbook(
            &path,
            &[(
                "目标",
                &[&["列一", "列二"], &["甲", "甲"], &["乙", "乙"], &["甲", ""], &["乙", ""]],
            )],
        );

        let server = NameMatchServer::new();
        let first = server.run_match_workbook(on_sheet(&path, "列一")).unwrap();
        assert_eq!(first.rows_scanned, 4);

        // Now match the shorter column two: rows only column one covered must end
        // up empty rather than keeping stale values.
        let second = server.run_match_workbook(on_sheet(&path, "列二")).unwrap();
        assert_eq!(second.rows_scanned, 2, "only rows 2 and 3 carry 列二");
        assert!(second.reused_columns);

        let sheet = crate::test_support::read_all_text(&path, "xl/worksheets/sheet1.xml");
        let match_column = second.match_column.clone();
        let score_column = second.score_column.clone();
        // Headers plus the two rows this run visited must each appear once.
        for reference in [
            format!("{match_column}1"),
            format!("{score_column}1"),
            format!("{match_column}2"),
            format!("{score_column}2"),
            format!("{match_column}3"),
            format!("{score_column}3"),
        ] {
            assert_eq!(
                sheet.matches(&format!(r#"r="{reference}""#)).count(),
                1,
                "cell {reference} must appear exactly once"
            );
        }
        // Rows 4 and 5 were covered only by the first run: their result cells are
        // gone (blank) rather than duplicated.
        for reference in [
            format!("{match_column}4"),
            format!("{score_column}4"),
            format!("{match_column}5"),
            format!("{score_column}5"),
        ] {
            assert_eq!(
                sheet.matches(&format!(r#"r="{reference}""#)).count(),
                0,
                "cell {reference} should have been cleared"
            );
        }
        // Rows 4 and 5 were covered only by the first run, so their match cell is
        // now empty: no inline string may survive there.
        for row in ["4", "5"] {
            let start = sheet.find(&format!(r#"r="{row}""#)).unwrap();
            let end = sheet[start..].find("</row>").unwrap() + start;
            let fragment = &sheet[start..end];
            assert_eq!(
                fragment.matches("<is>").count(),
                0,
                "stale matched value survived in row {row}: {fragment}"
            );
        }
    }

    #[test]
    fn custom_result_column_names_are_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("统计.xlsx");
        build_workbook(
            &path,
            &[("参考", &[&["标准名称"], &["甲"]]), ("目标", &[&["原始名称"], &["甲"]])],
        );

        let mut req = request(&path);
        req.match_column_name = Some("对照名称".into());
        req.score_column_name = Some("相似度".into());
        let summary = NameMatchServer::new().run_match_workbook(req).unwrap();
        assert!(!summary.reused_columns);

        let sheet = crate::test_support::read_all_text(&path, "xl/worksheets/sheet2.xml");
        assert!(sheet.contains("<t>对照名称</t>"), "{sheet}");
        assert!(sheet.contains("<t>相似度</t>"), "{sheet}");

        // Re-running with the same custom names reuses rather than appends.
        let mut again = request(&path);
        again.match_column_name = Some("对照名称".into());
        again.score_column_name = Some("相似度".into());
        let second = NameMatchServer::new().run_match_workbook(again).unwrap();
        assert!(second.reused_columns);
        assert_eq!(second.match_column, summary.match_column);
    }
}
