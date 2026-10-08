//! MCP wiring: the `match_names` tool and its `ServerHandler` implementation.
//!
//! The tool takes plain-text file paths and writes a CSV file, so a run never
//! pushes the name lists through the JSON-RPC payload.

use std::path::{Path, PathBuf};

use rmcp::{
    ErrorData as McpError, Json, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::files::{is_same_file, read_name_list, resolved_path, write_results_csv};
use crate::{DEFAULT_THRESHOLD, match_names};

/// Arguments accepted by the `match_names` tool.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct MatchNamesRequest {
    #[schemars(
        description = "Path to the reference name set A: a text file with one name per line (UTF-8, falling back to GBK). Every target name is compared against all of these."
    )]
    pub reference_path: String,
    #[schemars(
        description = "Path to the target name set B: a text file with one name per line (UTF-8, falling back to GBK). Exactly one CSV row is produced per non-blank line, in file order."
    )]
    pub target_path: String,
    #[schemars(
        description = "Path of the CSV file to write. Relative paths resolve against the server process working directory. Missing parent directories are created and an existing file is overwritten. Must differ from reference_path and target_path."
    )]
    pub output_path: String,
    #[schemars(
        description = "Minimum score in 0.0..=1.0 for a match to be reported. Defaults to 0.6."
    )]
    pub threshold: Option<f64>,
}

/// Summary returned by the `match_names` tool.
///
/// The per-row detail lives in the generated CSV; this keeps the tool response
/// small no matter how large the input files are.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MatchNamesSummary {
    /// Absolute path of the CSV that was written.
    pub csv_path: String,
    /// Number of names read from `reference_path`.
    pub reference_count: usize,
    /// Number of names read from `target_path`.
    pub target_count: usize,
    /// Number of rows written; always equal to `target_count`.
    pub result_count: usize,
    /// Rows whose score reached `threshold`.
    pub matched_count: usize,
    /// Rows whose score fell below `threshold` (empty `匹配名称` in the CSV).
    pub unmatched_count: usize,
    /// Rows whose normalized name matched a reference name exactly (score 1.0).
    pub exact_matches: usize,
    /// Wall-clock duration of the matching step, in milliseconds.
    pub elapsed_ms: u64,
}

/// Server name advertised during the MCP handshake.
pub const SERVER_NAME: &str = "name-match-mcp";
/// Server version advertised during the MCP handshake.
///
/// Kept as a literal because the `#[tool_handler]` attribute is parsed at
/// compile time; `server_version_matches_package_version` guards against drift.
pub const SERVER_VERSION: &str = "0.1.0";

/// Stateless stdio MCP server exposing the `match_names` tool.
#[derive(Debug, Clone, Default)]
pub struct NameMatchServer;

impl NameMatchServer {
    /// Create the server.
    pub fn new() -> Self {
        Self
    }
}

#[tool_router]
impl NameMatchServer {
    /// Match each name in the `target_path` file against the `reference_path`
    /// file and write the results to `output_path` as CSV.
    #[tool(
        description = "Match each name in the target_path text file against the reference_path text file, and write the results to output_path as a CSV with columns 待匹配名称,匹配名称,匹配度. One row per non-blank target line, in file order; 匹配名称 is empty when the score is below threshold. Returns a summary (paths and counts), not the rows."
    )]
    fn match_names(
        &self,
        Parameters(MatchNamesRequest {
            reference_path,
            target_path,
            output_path,
            threshold,
        }): Parameters<MatchNamesRequest>,
    ) -> Result<Json<MatchNamesSummary>, McpError> {
        let threshold = validate_threshold(threshold)?;

        let reference_file = PathBuf::from(&reference_path);
        let target_file = PathBuf::from(&target_path);
        let output_file = PathBuf::from(&output_path);
        ensure_output_is_distinct(&output_file, &reference_file, &target_file)?;

        let reference_names = read_name_list(&reference_file).map_err(invalid_params)?;
        let target_names = read_name_list(&target_file).map_err(invalid_params)?;

        let started = std::time::Instant::now();
        let results = match_names(&reference_names, &target_names, threshold);
        let elapsed_ms = started.elapsed().as_millis() as u64;

        write_results_csv(&output_file, &results).map_err(|error| {
            McpError::internal_error(format!("failed to write CSV: {error}"), None)
        })?;

        let matched_count = results
            .iter()
            .filter(|item| item.matched_name.is_some())
            .count();
        let exact_matches = results.iter().filter(|item| item.score >= 1.0).count();
        let summary = MatchNamesSummary {
            csv_path: resolved_path(&output_file).display().to_string(),
            reference_count: reference_names.len(),
            target_count: target_names.len(),
            result_count: results.len(),
            matched_count,
            unmatched_count: results.len() - matched_count,
            exact_matches,
            elapsed_ms,
        };
        Ok(Json(summary))
    }
}

#[tool_handler(name = "name-match-mcp", version = "0.1.0")]
impl ServerHandler for NameMatchServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Name matching service: call `match_names` with reference_path (set A), target_path (set B) and output_path to write a CSV with one 待匹配名称/匹配名称/匹配度 row per target line.",
        )
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

/// Reject a CSV destination that would clobber one of the input files.
fn ensure_output_is_distinct(
    output: &Path,
    reference: &Path,
    target: &Path,
) -> Result<(), McpError> {
    for (label, input) in [("reference_path", reference), ("target_path", target)] {
        if is_same_file(output, input) {
            return Err(McpError::invalid_params(
                format!(
                    "output_path must differ from {label}: {}",
                    resolved_path(input).display()
                ),
                None,
            ));
        }
    }
    Ok(())
}

/// Turn an I/O failure message into an MCP `invalid_params` error.
fn invalid_params(message: String) -> McpError {
    McpError::invalid_params(message, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{CSV_HEADERS, read_name_list};
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn run(
        reference: &Path,
        target: &Path,
        output: &Path,
        threshold: Option<f64>,
    ) -> Result<MatchNamesSummary, McpError> {
        let request = MatchNamesRequest {
            reference_path: reference.display().to_string(),
            target_path: target.display().to_string(),
            output_path: output.display().to_string(),
            threshold,
        };
        NameMatchServer::new()
            .match_names(Parameters(request))
            .map(|Json(summary)| summary)
    }

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

    #[test]
    fn writes_csv_and_reports_counts() {
        let dir = temp_dir();
        let reference = write(dir.path(), "参考名称.txt", "甲\n乙\n丙\n");
        let target = write(dir.path(), "待匹配名称.txt", "甲\n乙丙\n完全无关的丁\n");
        let output = dir.path().join("结果.csv");

        let summary = run(&reference, &target, &output, None).unwrap();

        assert_eq!(summary.reference_count, 3);
        assert_eq!(summary.target_count, 3);
        assert_eq!(summary.result_count, 3);
        assert_eq!(summary.matched_count + summary.unmatched_count, 3);
        assert_eq!(summary.exact_matches, 1);
        assert!(summary.csv_path.ends_with("结果.csv"));
        assert!(Path::new(&summary.csv_path).is_absolute());

        let csv = fs::read_to_string(&output).unwrap();
        let lines: Vec<&str> = csv.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 4, "header plus one row per target");
        assert_eq!(lines[0], format!("\u{FEFF}{}", CSV_HEADERS.join(",")));
        assert!(lines[1].starts_with("甲,甲,"));
        assert!(lines[3].starts_with("完全无关的丁,,"));
    }

    #[test]
    fn empty_target_file_writes_header_only() {
        let dir = temp_dir();
        let reference = write(dir.path(), "参考名称.txt", "甲\n");
        let target = write(dir.path(), "待匹配名称.txt", "\n  \n");
        let output = dir.path().join("结果.csv");

        let summary = run(&reference, &target, &output, None).unwrap();

        assert_eq!(summary.target_count, 0);
        assert_eq!(summary.result_count, 0);
        assert_eq!(summary.unmatched_count, 0);
        let csv = fs::read_to_string(&output).unwrap();
        assert_eq!(csv, format!("\u{FEFF}{}\r\n", CSV_HEADERS.join(",")));
    }

    #[test]
    fn high_threshold_reports_unmatched_rows() {
        let dir = temp_dir();
        let reference = write(dir.path(), "参考名称.txt", "Acme Corporation\n");
        let target = write(dir.path(), "待匹配名称.txt", "Acme Corporaton\n");
        let output = dir.path().join("结果.csv");

        let summary = run(&reference, &target, &output, Some(0.99)).unwrap();

        assert_eq!(summary.result_count, 1);
        assert_eq!(summary.matched_count, 0);
        assert_eq!(summary.unmatched_count, 1);
        assert_eq!(summary.exact_matches, 0);
        let csv = fs::read_to_string(&output).unwrap();
        assert!(csv.contains("Acme Corporaton,,"), "got: {csv}");
    }

    #[test]
    fn overwrites_an_existing_output_file() {
        let dir = temp_dir();
        let reference = write(dir.path(), "参考名称.txt", "甲\n");
        let target = write(dir.path(), "待匹配名称.txt", "甲\n");
        let output = write(dir.path(), "结果.csv", "旧的、很长的内容".repeat(50).as_str());

        let summary = run(&reference, &target, &output, None).unwrap();

        assert_eq!(summary.result_count, 1);
        let csv = fs::read_to_string(&output).unwrap();
        assert!(!csv.contains("旧的"));
        assert_eq!(csv, "\u{FEFF}待匹配名称,匹配名称,匹配度\r\n甲,甲,1\r\n");
    }

    #[test]
    fn refuses_output_that_would_clobber_an_input() {
        let dir = temp_dir();
        let reference = write(dir.path(), "参考名称.txt", "甲\n");
        let target = write(dir.path(), "待匹配名称.txt", "甲\n");

        let error = run(&reference, &target, &reference, None).unwrap_err();
        assert!(error.message.contains("output_path must differ"), "got: {}", error.message);
        assert!(error.message.contains("reference_path"));

        let error = run(&reference, &target, &target, None).unwrap_err();
        assert!(error.message.contains("target_path"), "got: {}", error.message);

        // The input file must still be intact.
        assert_eq!(read_name_list(&reference).unwrap(), vec!["甲"]);
    }

    #[test]
    fn missing_input_file_is_reported_as_invalid_params() {
        let dir = temp_dir();
        let reference = dir.path().join("不存在.txt");
        let target = write(dir.path(), "待匹配名称.txt", "甲\n");
        let output = dir.path().join("结果.csv");

        let error = run(&reference, &target, &output, None).unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("不存在.txt"), "got: {}", error.message);
        assert!(!output.exists(), "no CSV should be written on failure");
    }

    #[test]
    fn creates_missing_output_directories() {
        let dir = temp_dir();
        let reference = write(dir.path(), "参考名称.txt", "甲\n");
        let target = write(dir.path(), "待匹配名称.txt", "甲\n");
        let output = dir.path().join("深").join("层").join("结果.csv");

        let summary = run(&reference, &target, &output, None).unwrap();

        assert_eq!(summary.result_count, 1);
        assert!(output.exists());
    }
}
