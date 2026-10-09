//! End-to-end tests driving the real binary over stdio: spawn the server, run
//! initialize -> tools/list -> tools/call, and inspect the workbook it writes.

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
        tokio::process::Command::new(env!("CARGO_BIN_EXE_name-match-mcp")).configure(|cmd| {
            cmd.stderr(Stdio::null());
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());
        }),
    )?;
    let client = ().serve(transport).await?;
    Ok(client)
}

/// Build a workbook with a reference sheet and a target sheet.
fn fixture(dir: &Path, reference: &[&str], target: &[&str]) -> PathBuf {
    let path = dir.join("统计.xlsx");
    let reference_rows: Vec<Vec<&str>> = std::iter::once(vec!["标准名称"])
        .chain(reference.iter().map(|value| vec![*value]))
        .collect();
    let target_rows: Vec<Vec<&str>> = std::iter::once(vec!["原始名称"])
        .chain(target.iter().map(|value| vec![*value]))
        .collect();
    let reference_slice: Vec<&[&str]> = reference_rows.iter().map(|row| row.as_slice()).collect();
    let target_slice: Vec<&[&str]> = target_rows.iter().map(|row| row.as_slice()).collect();

    name_match_mcp::test_support::build_workbook(
        &path,
        &[("参考", &reference_slice), ("目标", &target_slice)],
    );
    path
}

fn arguments(path: &Path, threshold: Option<f64>) -> serde_json::Map<String, serde_json::Value> {
    let mut arguments = serde_json::Map::new();
    arguments.insert(
        "xlsx_path".into(),
        serde_json::json!(path.display().to_string()),
    );
    arguments.insert("reference_sheet".into(), serde_json::json!("参考"));
    arguments.insert("reference_column".into(), serde_json::json!("标准名称"));
    arguments.insert("target_sheet".into(), serde_json::json!("目标"));
    arguments.insert("target_column".into(), serde_json::json!("原始名称"));
    if let Some(threshold) = threshold {
        arguments.insert("threshold".into(), serde_json::json!(threshold));
    }
    arguments
}

async fn call_match(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    arguments: serde_json::Map<String, serde_json::Value>,
) -> Result<CallToolResult, rmcp::ServiceError> {
    client
        .call_tool(CallToolRequestParams::new("match_workbook_column").with_arguments(arguments))
        .await
}

fn summary(result: &CallToolResult) -> serde_json::Value {
    assert_ne!(
        result.is_error,
        Some(true),
        "tool returned an error: {:?}",
        result.content
    );
    result
        .structured_content
        .clone()
        .expect("tool result should carry a structured summary")
}

/// Read a sheet's XML so tests can assert on the written cells.
fn sheet_xml(path: &Path, entry: &str) -> String {
    let file = fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut content = String::new();
    std::io::Read::read_to_string(&mut archive.by_name(entry).unwrap(), &mut content).unwrap();
    content
}

#[tokio::test]
async fn exposes_only_the_workbook_tool() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;

    let tools = client.list_all_tools().await?;
    assert_eq!(tools.len(), 1, "server should expose exactly one tool");
    assert_eq!(tools[0].name, "match_workbook_column");

    let properties = tools[0]
        .input_schema
        .get("properties")
        .and_then(|value| value.as_object())
        .expect("input schema should declare properties");
    for expected in [
        "xlsx_path",
        "reference_sheet",
        "reference_column",
        "target_sheet",
        "target_column",
        "header_row",
        "match_column_name",
        "score_column_name",
        "threshold",
    ] {
        assert!(properties.contains_key(expected), "missing {expected}");
    }
    // The old text/CSV interface must be gone.
    assert!(!properties.contains_key("reference_path"));
    assert!(!properties.contains_key("output_path"));

    let required = tools[0]
        .input_schema
        .get("required")
        .and_then(|value| value.as_array())
        .expect("required list");
    let required: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    for expected in [
        "xlsx_path",
        "reference_sheet",
        "reference_column",
        "target_sheet",
        "target_column",
    ] {
        assert!(required.contains(&expected), "{expected} should be required");
    }
    assert!(!required.contains(&"threshold"));
    assert!(tools[0].output_schema.is_some(), "output schema should be published");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn writes_two_new_columns_and_keeps_originals() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = tempfile::tempdir()?;
    let path = fixture(dir.path(), &["甲", "乙", "丙"], &["甲", "乙", "毫不相干ZZZ"]);

    let result = call_match(&client, arguments(&path, None)).await?;
    let summary = summary(&result);

    assert_eq!(summary["reference_count"], 3);
    assert_eq!(summary["rows_scanned"], 3);
    assert_eq!(summary["exact_count"], 2);
    assert_eq!(summary["unmatched_count"], 1);
    assert_eq!(
        summary["matched_count"].as_u64().unwrap(),
        summary["exact_count"].as_u64().unwrap() + summary["fuzzy_count"].as_u64().unwrap()
    );
    assert_eq!(summary["match_column"], "B");
    assert_eq!(summary["score_column"], "C");
    assert_eq!(summary["reused_columns"], false);
    assert!(Path::new(summary["backup_path"].as_str().unwrap()).exists());

    // Original values survive, and the new cells carry the results.
    let sheet = sheet_xml(&path, "xl/worksheets/sheet2.xml");
    assert!(sheet.contains(r#"<c r="A1""#), "original header kept");
    assert!(sheet.contains(r#"<c r="B1""#), "match header added");
    assert!(sheet.contains(r#"<c r="C1""#), "score header added");
    assert_eq!(sheet.matches(r#"r="A2""#).count(), 1, "original column untouched");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn exact_hits_win_and_fuzzy_covers_near_misses() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = tempfile::tempdir()?;
    let path = fixture(
        dir.path(),
        &["贴面18厘9层7627ENF-襄阳天湘", "压面18厘林音逸梦ENF4*8-金兔万华"],
        &["贴面18厘9层7627ENF-襄阳天湘", "压面18厘林音逸梦ENF4*9-金兔万华"],
    );

    let result = call_match(&client, arguments(&path, Some(0.6))).await?;
    let summary = summary(&result);

    assert_eq!(summary["rows_scanned"], 2);
    assert_eq!(summary["exact_count"], 1, "the identical value is an exact hit");
    assert_eq!(summary["fuzzy_count"], 1, "the 8-vs-9 variant is a fuzzy hit");
    assert_eq!(summary["unmatched_count"], 0);

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn a_high_threshold_leaves_the_match_cell_empty() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = tempfile::tempdir()?;
    let path = fixture(dir.path(), &["Acme Corporation"], &["Acme Corporaton"]);

    let result = call_match(&client, arguments(&path, Some(0.99))).await?;
    let summary = summary(&result);

    assert_eq!(summary["rows_scanned"], 1);
    assert_eq!(summary["matched_count"], 0);
    assert_eq!(summary["unmatched_count"], 1);

    let sheet = sheet_xml(&path, "xl/worksheets/sheet2.xml");
    assert!(
        sheet.contains(r#"<c r="B2" s="1"/>"#),
        "unmatched cell should be empty but keep the style: {sheet}"
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn rerunning_reuses_the_result_columns() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = tempfile::tempdir()?;
    let path = fixture(dir.path(), &["甲", "乙"], &["甲", "乙"]);

    let first = summary(&call_match(&client, arguments(&path, None)).await?);
    assert_eq!(first["reused_columns"], false);

    let second = summary(&call_match(&client, arguments(&path, None)).await?);
    assert_eq!(second["reused_columns"], true, "second run should reuse");
    assert_eq!(second["match_column"], "B");
    assert_eq!(second["score_column"], "C");

    let sheet = sheet_xml(&path, "xl/worksheets/sheet2.xml");
    assert_eq!(
        sheet.matches(r#"r="B1""#).count(),
        1,
        "no duplicate match header"
    );
    assert_eq!(sheet.matches(r#"r="C1""#).count(), 1, "no duplicate score header");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn missing_sheet_returns_an_error() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = tempfile::tempdir()?;
    let path = fixture(dir.path(), &["甲"], &["甲"]);

    let mut args = arguments(&path, None);
    args.insert("reference_sheet".into(), serde_json::json!("不存在的表"));
    let error = call_match(&client, args)
        .await
        .expect_err("unknown sheet must be rejected");

    match error {
        rmcp::ServiceError::McpError(error) => {
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert!(error.message.contains("不存在的表"), "{}", error.message);
            assert!(error.message.contains("参考"), "{}", error.message);
        }
        other => panic!("expected an MCP error, got: {other:?}"),
    }
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn out_of_range_threshold_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect().await?;
    let dir = tempfile::tempdir()?;
    let path = fixture(dir.path(), &["甲"], &["甲"]);
    let before = fs::read(&path)?;

    let error = call_match(&client, arguments(&path, Some(1.5)))
        .await
        .expect_err("threshold above 1.0 must be rejected");
    assert!(error.to_string().contains("threshold"), "{error}");
    assert_eq!(fs::read(&path)?, before, "workbook must be untouched on rejection");

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
