//! End-to-end tests driving the real binary over stdio: spawn the server, run
//! initialize -> tools/list -> tools/call, and inspect the CSV it writes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ProtocolVersion},
    transport::{ConfigureCommandExt, TokioChildProcess},
};

/// Spawn the compiled server binary and complete the MCP handshake.
async fn connect()
-> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, Box<dyn std::error::Error>> {
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_name-match-mcp"))
            .configure(|cmd| {
                cmd.stderr(Stdio::null());
                cmd.stdin(Stdio::piped());
                cmd.stdout(Stdio::piped());
            }),
    )?;
    let client = ().serve(transport).await?;
    Ok(client)
}

/// Create an isolated working directory with the two input files.
fn fixture(reference: &str, target: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::write(dir.path().join("参考名称.txt"), reference).unwrap();
    fs::write(dir.path().join("待匹配名称.txt"), target).unwrap();
    dir
}

fn call_arguments(
    dir: &Path,
    output: &str,
    threshold: Option<f64>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut arguments = serde_json::Map::new();
    arguments.insert(
        "reference_path".into(),
        serde_json::json!(dir.join("参考名称.txt").display().to_string()),
    );
    arguments.insert(
        "target_path".into(),
        serde_json::json!(dir.join("待匹配名称.txt").display().to_string()),
    );
    arguments.insert(
        "output_path".into(),
        serde_json::json!(dir.join(output).display().to_string()),
    );
    if let Some(threshold) = threshold {
        arguments.insert("threshold".into(), serde_json::json!(threshold));
    }
    arguments
}

async fn call_match_names(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    arguments: serde_json::Map<String, serde_json::Value>,
) -> Result<CallToolResult, rmcp::ServiceError> {
    client
        .call_tool(CallToolRequestParams::new("match_names").with_arguments(arguments))
        .await
}

/// Pull the summary out of a successful tool result.
fn summary(result: &CallToolResult) -> serde_json::Value {
    assert_ne!(result.is_error, Some(true), "tool returned an error: {:?}", result.content);
    result
        .structured_content
        .clone()
        .expect("tool result should carry a structured summary")
}

/// Read a CSV as non-empty physical lines.
fn csv_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .split("\r\n")
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn lists_match_names_tool_with_expected_schema() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;

    let tools = client.list_all_tools().await?;
    assert_eq!(tools.len(), 1, "server should expose exactly one tool");

    let tool = &tools[0];
    assert_eq!(tool.name, "match_names");

    let properties = tool
        .input_schema
        .get("properties")
        .and_then(|value| value.as_object())
        .expect("input schema should declare properties");
    assert!(properties.contains_key("reference_path"));
    assert!(properties.contains_key("target_path"));
    assert!(properties.contains_key("output_path"));
    assert!(properties.contains_key("threshold"));
    // The old inline-array parameters must be gone.
    assert!(!properties.contains_key("reference_names"));
    assert!(!properties.contains_key("target_names"));

    let required = tool
        .input_schema
        .get("required")
        .and_then(|value| value.as_array())
        .expect("input schema should declare required fields");
    let required: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(required.contains(&"reference_path"));
    assert!(required.contains(&"target_path"));
    assert!(required.contains(&"output_path"));
    assert!(!required.contains(&"threshold"));

    assert!(tool.output_schema.is_some(), "tool should publish an output schema");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn writes_csv_for_chinese_com_names() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = fixture(
        "北京京东世纪贸易有限公司\n阿里巴巴（中国）有限公司\n",
        "京东世纪贸易\n阿里巴巴(中国)有限公司\n",
    );

    let result = call_match_names(&client, call_arguments(dir.path(), "结果.csv", None)).await?;
    let summary = summary(&result);

    assert_eq!(summary["reference_count"], 2);
    assert_eq!(summary["target_count"], 2);
    assert_eq!(summary["result_count"], 2);
    assert_eq!(summary["exact_matches"], 1, "base-name exact hit is folded by normalization");
    assert!(summary["elapsed_ms"].as_u64().is_some());

    let csv_path = PathBuf::from(summary["csv_path"].as_str().expect("csv_path is a string"));
    assert!(csv_path.is_absolute());
    let lines = csv_lines(&csv_path);
    assert_eq!(lines.len(), 3, "header plus two rows");
    assert_eq!(lines[0], "\u{FEFF}待匹配名称,匹配名称,匹配度");
    assert!(lines[1].starts_with("京东世纪贸易,北京京东世纪贸易有限公司,"));
    // Bracket/width folding makes this an exact hit.
    assert_eq!(lines[2], "阿里巴巴(中国)有限公司,阿里巴巴（中国）有限公司,1");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn below_threshold_writes_empty_matched_column() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = fixture(
        "Acme Corporation\n",
        "完全不相关的名字\nAcme Corporaton\n",
    );

    let result =
        call_match_names(&client, call_arguments(dir.path(), "结果.csv", Some(0.95))).await?;
    let summary = summary(&result);

    assert_eq!(summary["result_count"], 2);
    assert_eq!(summary["matched_count"], 0);
    assert_eq!(summary["unmatched_count"], 2);

    let csv_path = PathBuf::from(summary["csv_path"].as_str().unwrap());
    let lines = csv_lines(&csv_path);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1].split(',').next(), Some("完全不相关的名字"));
    // Empty 匹配名称 field: the row is "name,,score".
    assert_eq!(lines[1].matches(',').count(), 2, "got: {}", lines[1]);
    assert!(lines[1].starts_with("完全不相关的名字,,"));

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn encodes_special_characters_without_breaking_rows() -> Result<(), Box<dyn std::error::Error>>
{
    let client = connect().await?;
    let dir = fixture(
        "含,逗号公司\n含\"引号公司\n",
        "含,逗号公司\n含\"引号公司\n",
    );

    let result = call_match_names(&client, call_arguments(dir.path(), "结果.csv", None)).await?;
    let summary = summary(&result);
    assert_eq!(summary["result_count"], 2);
    assert_eq!(summary["exact_matches"], 2);

    let csv_path = PathBuf::from(summary["csv_path"].as_str().unwrap());
    let csv = fs::read_to_string(&csv_path).unwrap();
    assert!(csv.contains("\"含,逗号公司\",\"含,逗号公司\",1"), "got: {csv}");
    assert!(csv.contains("\"含\"\"引号公司\",\"含\"\"引号公司\",1"), "got: {csv}");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn reports_counts_for_larger_mixed_input() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let reference: Vec<String> = (0..200).map(|i| format!("型号 {i} 板材")).collect();
    let target: Vec<String> = (0..150)
        .map(|i| format!("型号 {} 板材", i % 200))
        .chain(std::iter::once("完全不存在的东西".to_string()))
        .collect();
    let dir = fixture(
        &(reference.join("\n") + "\n"),
        &(target.join("\n") + "\n"),
    );

    let result = call_match_names(&client, call_arguments(dir.path(), "结果.csv", None)).await?;
    let summary = summary(&result);

    assert_eq!(summary["reference_count"], 200);
    assert_eq!(summary["target_count"], 151);
    assert_eq!(summary["result_count"], 151, "C must have one row per target");
    assert_eq!(summary["exact_matches"], 150);
    assert_eq!(summary["matched_count"], 150);
    assert_eq!(summary["unmatched_count"], 1);

    let csv_path = PathBuf::from(summary["csv_path"].as_str().unwrap());
    let lines = csv_lines(&csv_path);
    assert_eq!(lines.len(), 152, "header plus one row per target");
    assert_eq!(lines[0], "\u{FEFF}待匹配名称,匹配名称,匹配度");
    assert_eq!(lines[1].split(',').next(), Some("型号 0 板材"));
    assert!(lines[151].starts_with("完全不存在的东西,,"));

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn missing_input_file_returns_an_error_result() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = fixture("甲\n", "甲\n");
    let mut arguments = call_arguments(dir.path(), "结果.csv", None);
    arguments.insert(
        "reference_path".into(),
        serde_json::json!(dir.path().join("不存在.txt").display().to_string()),
    );

    // A missing input is a caller mistake, so the server answers with a
    // JSON-RPC invalid-params error rather than a tool result.
    let error = call_match_names(&client, arguments)
        .await
        .expect_err("missing reference file must be rejected");
    match error {
        rmcp::ServiceError::McpError(error) => {
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert!(error.message.contains("不存在.txt"), "got: {}", error.message);
        }
        other => panic!("expected an MCP error, got: {other:?}"),
    }
    assert!(!dir.path().join("结果.csv").exists(), "no CSV on failure");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn refuses_output_path_equal_to_an_input() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = fixture("甲\n", "甲\n");
    let reference_path = dir.path().join("参考名称.txt").display().to_string();

    let mut arguments = call_arguments(dir.path(), "结果.csv", None);
    arguments.insert("output_path".into(), serde_json::json!(reference_path));

    let error = call_match_names(&client, arguments)
        .await
        .expect_err("output_path equal to an input must be rejected");
    match error {
        rmcp::ServiceError::McpError(error) => {
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert!(
                error.message.contains("output_path must differ"),
                "got: {}",
                error.message
            );
        }
        other => panic!("expected an MCP error, got: {other:?}"),
    }
    // The input file must be untouched.
    assert_eq!(fs::read_to_string(dir.path().join("参考名称.txt")).unwrap(), "甲\n");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn out_of_range_threshold_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = fixture("甲\n", "甲\n");

    let error = call_match_names(&client, call_arguments(dir.path(), "结果.csv", Some(1.5)))
        .await
        .expect_err("threshold above 1.0 must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("threshold"),
        "error should mention threshold, got: {message}"
    );
    assert!(!dir.path().join("结果.csv").exists());

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn negotiates_a_supported_protocol_version() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let info = client.peer_info().expect("initialize should record peer info");
    assert!(
        ProtocolVersion::KNOWN_VERSIONS.contains(&info.protocol_version),
        "negotiated version {} should be a known protocol version",
        info.protocol_version
    );
    client.cancel().await?;
    Ok(())
}
