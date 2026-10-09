//! End-to-end tests driving the compiled `name-match` binary as a subprocess.
//!
//! They cover the contract a script depends on: JSON on stdout, a stable exit
//! code, and the side effect on the workbook.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use name_match::test_support::{build_workbook, read_all_text};

/// The binary under test, provided by Cargo.
fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_name-match")
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

    build_workbook(
        &path,
        &[("参考", &reference_slice), ("目标", &target_slice)],
    );
    path
}

/// Standard argument list for the fixture above.
fn match_args(path: &Path) -> Vec<String> {
    [
        "match",
        "--workbook",
        &path.display().to_string(),
        "--reference-sheet",
        "参考",
        "--reference-column",
        "标准名称",
        "--target-sheet",
        "目标",
        "--target-column",
        "原始名称",
    ]
    .iter()
    .map(|value| (*value).to_string())
    .collect()
}

fn run(args: &[String]) -> Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("failed to run the name-match binary")
}

fn stdout_json(output: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim())
        .unwrap_or_else(|error| panic!("stdout was not valid JSON ({error}): {text}"))
}

#[test]
fn matches_a_workbook_and_prints_a_json_summary() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), &["甲", "乙", "丙"], &["甲", "乙", "完全不相关ZZZ"]);

    let output = run(&match_args(&path));

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let json = stdout_json(&output);
    assert_eq!(json["reference_count"], 3);
    assert_eq!(json["rows_scanned"], 3);
    assert_eq!(json["exact_count"], 2);
    assert_eq!(json["unmatched_count"], 1);
    assert_eq!(json["matched_count"], 2);
    assert_eq!(json["match_column"], "B");
    assert_eq!(json["score_column"], "C");
    assert_eq!(json["reused_columns"], false);
    assert_eq!(json["sheet"], "目标");
    assert_eq!(json["column"], "原始名称");
    assert!(json["xlsx_path"].as_str().unwrap().ends_with("统计.xlsx"));

    // The workbook really changed: the result headers are now present.
    let sheet = read_all_text(&path, "xl/worksheets/sheet2.xml");
    assert!(sheet.contains("<t>匹配名称</t>"), "{sheet}");
    assert!(sheet.contains("<t>匹配度</t>"), "{sheet}");
    // ...and no stray files were left behind.
    let leftovers: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "统计.xlsx")
        .collect();
    assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
}

#[test]
fn honours_optional_flags() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), &["Acme Corporation"], &["Acme Corporaton"]);

    let mut args = match_args(&path);
    args.extend(
        [
            "--threshold",
            "0.99",
            "--match-column-name",
            "对照名称",
            "--score-column-name",
            "相似度",
        ]
        .iter()
        .map(|value| (*value).to_string()),
    );
    let output = run(&args);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let json = stdout_json(&output);
    assert_eq!(json["matched_count"], 0, "0.99 exceeds the fuzzy score");
    assert_eq!(json["unmatched_count"], 1);

    let sheet = read_all_text(&path, "xl/worksheets/sheet2.xml");
    assert!(sheet.contains("<t>对照名称</t>"), "{sheet}");
    assert!(sheet.contains("<t>相似度</t>"), "{sheet}");
}

#[test]
fn rerunning_reuses_the_result_columns_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), &["甲"], &["甲"]);

    assert_eq!(run(&match_args(&path)).status.code(), Some(0));
    let second = run(&match_args(&path));
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(stdout_json(&second)["reused_columns"], true);

    let sheet = read_all_text(&path, "xl/worksheets/sheet2.xml");
    for reference in ["B1", "C1", "B2", "C2"] {
        assert_eq!(
            sheet.matches(&format!(r#"r="{reference}""#)).count(),
            1,
            "cell {reference} must appear exactly once"
        );
    }
}

#[test]
fn missing_arguments_exit_nonzero_with_a_json_error() {
    let output = run(&["match".to_string(), "--workbook".to_string(), "a.xlsx".to_string()]);

    assert_eq!(output.status.code(), Some(1));
    let json = stdout_json(&output);
    assert_eq!(json["error"]["kind"], "usage");
    assert!(
        json["error"]["message"].as_str().unwrap().contains("--reference-sheet"),
        "{json}"
    );
    assert!(!output.stderr.is_empty(), "the reason should also reach stderr");
}

#[test]
fn unknown_flag_and_unknown_subcommand_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), &["甲"], &["甲"]);
    let mut args = match_args(&path);
    args.push("--nope".to_string());
    let output = run(&args);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout_json(&output)["error"]["message"].as_str().unwrap().contains("--nope"));

    let output = run(&["merge".to_string()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout_json(&output)["error"]["message"].as_str().unwrap().contains("未知子命令"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn missing_sheet_or_column_reports_an_invalid_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), &["甲"], &["甲"]);

    let mut args = match_args(&path);
    let index = args.iter().position(|value| value == "参考").unwrap();
    args[index] = "不存在的表".to_string();
    let output = run(&args);
    assert_eq!(output.status.code(), Some(1));
    let json = stdout_json(&output);
    assert_eq!(json["error"]["kind"], "invalid");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(message.contains("不存在的表"), "{message}");
    assert!(message.contains("参考"), "candidate list should be included: {message}");

    // The workbook must be untouched after a failed lookup.
    let sheet = read_all_text(&path, "xl/worksheets/sheet2.xml");
    assert!(!sheet.contains("匹配名称"), "no columns may be written on failure");
}

#[test]
fn non_xlsx_and_out_of_range_threshold_are_rejected_without_touching_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), &["甲"], &["甲"]);
    let before = fs::read(&path).unwrap();

    let xls = dir.path().join("旧格式.xls");
    fs::write(&xls, b"not a workbook").unwrap();
    let output = run(&match_args(&xls));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout_json(&output)["error"]["message"].as_str().unwrap().contains("只支持 .xlsx"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let mut args = match_args(&path);
    args.extend(["--threshold", "1.5"].iter().map(|value| (*value).to_string()));
    let output = run(&args);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout_json(&output)["error"]["message"].as_str().unwrap().contains("0.0~1.0")
    );
    assert_eq!(fs::read(&path).unwrap(), before, "workbook must be untouched");
}

#[test]
fn help_and_version_succeed() {
    let help = run(&["--help".to_string()]);
    assert_eq!(help.status.code(), Some(0));
    let text = String::from_utf8_lossy(&help.stdout);
    for expected in ["--workbook", "--reference-sheet", "--target-column", "--threshold"] {
        assert!(text.contains(expected), "help should document {expected}");
    }

    let version = run(&["--version".to_string()]);
    assert_eq!(version.status.code(), Some(0));
    let text = String::from_utf8_lossy(&version.stdout);
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "got: {text}");
}
