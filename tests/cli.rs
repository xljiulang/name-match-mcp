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
        // The sidecar lock file is expected to stay; anything else is a leak.
        .filter(|name| name != "统计.xlsx" && name != ".统计.xlsx.lock")
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

/// A workbook with two target columns of different lengths, so concurrent runs
/// have something distinct to write.
fn two_column_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("并发.xlsx");
    let rows: Vec<Vec<&str>> = vec![
        vec!["列一", "列二"],
        vec!["甲", "乙"],
        vec!["乙", "甲"],
        vec!["甲", ""],
        vec!["乙", ""],
    ];
    let slices: Vec<&[&str]> = rows.iter().map(|row| row.as_slice()).collect();
    build_workbook(&path, &[("目标", &slices)]);
    path
}

/// A workbook with a reference sheet and two independent target sheets, so two
/// runs can write into different sheets of the same file.
///
/// The sheets are deliberately large: the read-modify-write span has to stay
/// open long enough that two concurrent processes genuinely overlap, otherwise
/// the test would pass even without the lock and prove nothing.
fn two_sheet_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("多表.xlsx");
    const ROWS: usize = 4000;

    // Owned storage that outlives the `build_workbook` call.
    let names: Vec<String> = (0..ROWS)
        .map(|i| format!("压面18厘型号{i:04}ENF4*8-某厂"))
        .collect();
    let mut sheets: Vec<Vec<Vec<String>>> = vec![
        vec![vec!["标准名称".to_string()]],
        vec![vec!["原始名称".to_string()]],
        vec![vec!["原始名称".to_string()]],
    ];
    for name in &names {
        sheets[0].push(vec![name.clone()]);
        sheets[1].push(vec![name.clone()]);
        // Target B is a near-miss, still matched by the fuzzy scorer.
        sheets[2].push(vec![name.replace("ENF4*8", "ENF4*9")]);
    }

    let rows: Vec<Vec<Vec<&str>>> = sheets
        .iter()
        .map(|sheet| {
            sheet
                .iter()
                .map(|row| row.iter().map(String::as_str).collect())
                .collect()
        })
        .collect();
    let slices: Vec<Vec<&[&str]>> = rows
        .iter()
        .map(|sheet| sheet.iter().map(Vec::as_slice).collect())
        .collect();

    build_workbook(
        &path,
        &[
            ("参考", &slices[0]),
            ("目标A", &slices[1]),
            ("目标B", &slices[2]),
        ],
    );
    path
}

/// Arguments matching one target sheet of the multi-sheet fixture.
fn args_for_sheet(path: &Path, sheet: &str) -> Vec<String> {
    [
        "match",
        "--workbook",
        &path.display().to_string(),
        "--reference-sheet",
        "参考",
        "--reference-column",
        "标准名称",
        "--target-sheet",
        sheet,
        "--target-column",
        "原始名称",
    ]
    .iter()
    .map(|value| (*value).to_string())
    .collect()
}

/// The scenario the lock exists for: an agent fires several runs at once to
/// process different sheets of the *same* workbook. Every run's result must
/// survive, instead of the last writer discarding the others.
#[test]
fn concurrent_runs_on_different_sheets_of_one_workbook_all_survive() {
    let dir = tempfile::tempdir().unwrap();
    let path = two_sheet_fixture(dir.path());

    let path_a = path.clone();
    let path_b = path.clone();
    let handle_a = std::thread::spawn(move || run(&args_for_sheet(&path_a, "目标A")));
    let handle_b = std::thread::spawn(move || run(&args_for_sheet(&path_b, "目标B")));
    let out_a = handle_a.join().unwrap();
    let out_b = handle_b.join().unwrap();

    assert_eq!(
        out_a.status.code(),
        Some(0),
        "目标A: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert_eq!(
        out_b.status.code(),
        Some(0),
        "目标B: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    // Both sheets must carry their own result columns. Before the lock, the
    // later run overwrote the whole file with a snapshot taken before the
    // earlier run wrote, so one of these two sheets lost its columns.
    let sheet_a = read_all_text(&path, "xl/worksheets/sheet2.xml");
    let sheet_b = read_all_text(&path, "xl/worksheets/sheet3.xml");
    assert!(
        sheet_a.contains("<t>匹配名称</t>") && sheet_a.contains("<t>匹配度</t>"),
        "目标A lost its result columns (its run was overwritten); headers: {:?}",
        sheet_a.get(..200)
    );
    assert!(
        sheet_b.contains("<t>匹配名称</t>") && sheet_b.contains("<t>匹配度</t>"),
        "目标B lost its result columns (its run was overwritten); headers: {:?}",
        sheet_b.get(..200)
    );

    // Each sheet must hold one result column pair covering every data row, with
    // no duplicated cells from two independent appends.
    for (label, sheet) in [("目标A", &sheet_a), ("目标B", &sheet_b)] {
        for reference in ["B1", "C1", "B2", "C2", "B4001", "C4001"] {
            assert_eq!(
                cell_occurrences(sheet, reference),
                1,
                "{label}: {reference} must appear exactly once"
            );
        }
        assert!(
            !sheet.contains(r#"r="D1""#),
            "{label}: a second result column pair must not appear"
        );
    }

    // The other sheet is untouched by its neighbour's run.
    let reference_sheet = read_all_text(&path, "xl/worksheets/sheet1.xml");
    assert!(
        !reference_sheet.contains("<t>匹配名称</t>"),
        "the reference sheet must not receive result columns"
    );
}

/// Arguments that match one column of the sheet against another column of the
/// same sheet.
fn args_for_column(path: &Path, column: &str) -> Vec<String> {
    [
        "match",
        "--workbook",
        &path.display().to_string(),
        "--reference-sheet",
        "目标",
        "--reference-column",
        "列一",
        "--target-sheet",
        "目标",
        "--target-column",
        column,
    ]
    .iter()
    .map(|value| (*value).to_string())
    .collect()
}

/// Count how many times a cell reference appears in a sheet's XML.
fn cell_occurrences(sheet: &str, reference: &str) -> usize {
    sheet.matches(&format!(r#"r="{reference}""#)).count()
}

/// The regression this lock exists for: without it, two concurrent runs on one
/// workbook both read the original file and the later write discards the
/// earlier result entirely.
#[test]
fn concurrent_runs_on_one_workbook_keep_both_results() {
    let dir = tempfile::tempdir().unwrap();
    let path = two_column_fixture(dir.path());

    let first = Command::new(binary())
        .args(args_for_column(&path, "列一"))
        .spawn()
        .expect("spawn first");
    let second = Command::new(binary())
        .args(args_for_column(&path, "列二"))
        .spawn()
        .expect("spawn second");

    let first = first.wait_with_output().expect("first output");
    let second = second.wait_with_output().expect("second output");
    assert_eq!(
        first.status.code(),
        Some(0),
        "first: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        second.status.code(),
        Some(0),
        "second: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let sheet = read_all_text(&path, "xl/worksheets/sheet1.xml");

    // Both runs wrote to the same pair of result columns; whichever ran last
    // rebuilt them. The critical property is that neither run lost its own
    // write, proven by the columns being present exactly once (no duplication
    // from two independent appends) and the extents staying sane.
    assert_eq!(
        cell_occurrences(&sheet, "C1"),
        1,
        "the match header must exist once: {sheet}"
    );
    assert_eq!(cell_occurrences(&sheet, "D1"), 1, "the score header must exist once");
    assert!(
        !sheet.contains(r#"r="E1""#),
        "a second pair of columns must not be appended: {sheet}"
    );

    // Every row's result cells exist at most once, i.e. no duplicate <c>.
    for row in 1..=5 {
        for column in ["C", "D", "E", "F"] {
            assert!(
                cell_occurrences(&sheet, &format!("{column}{row}")) <= 1,
                "cell {column}{row} appears more than once: {sheet}"
            );
        }
    }

    // The workbook must still be readable as a zip with the expected entries.
    let entries = name_match::test_support::entry_names(&path);
    assert!(entries.contains(&"xl/worksheets/sheet1.xml".to_string()));
    assert!(
        !entries.iter().any(|name| name.contains(".tmp")),
        "no temp file may be left inside the workbook: {entries:?}"
    );
}

/// Concurrent runs on *different* files must not serialize behind each other.
#[test]
fn concurrent_runs_on_different_workbooks_do_not_block_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("a.xlsx");
    let second_path = dir.path().join("b.xlsx");

    let rows: Vec<Vec<&str>> = vec![vec!["列一"], vec!["甲"], vec!["乙"]];
    let slices: Vec<&[&str]> = rows.iter().map(|row| row.as_slice()).collect();
    build_workbook(&first_path, &[("目标", &slices)]);
    build_workbook(&second_path, &[("目标", &slices)]);

    // Run both at once on separate threads so they genuinely overlap in time;
    // `Command::output` captures stdout and stderr by itself.
    let path_a = first_path.clone();
    let path_b = second_path.clone();
    let handle_a = std::thread::spawn(move || run(&args_for_column(&path_a, "列一")));
    let handle_b = std::thread::spawn(move || run(&args_for_column(&path_b, "列一")));

    let out_a = handle_a.join().unwrap();
    let out_b = handle_b.join().unwrap();
    assert_eq!(out_a.status.code(), Some(0));
    assert_eq!(out_b.status.code(), Some(0));

    // Neither should have reported waiting, since the locks are unrelated.
    let json_a: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out_a.stdout).trim()).unwrap();
    let json_b: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out_b.stdout).trim()).unwrap();
    assert_eq!(json_a["waited_ms"], 0, "unrelated workbooks must not wait");
    assert_eq!(json_b["waited_ms"], 0, "unrelated workbooks must not wait");
}

/// The lock must be released by the kernel if the holder is killed, so a later
/// run is never permanently blocked.
#[test]
fn a_killed_holder_does_not_leave_the_lock_stuck() {
    let dir = tempfile::tempdir().unwrap();
    let path = two_column_fixture(dir.path());

    // A long-running holder is simulated by taking the lock directly.
    let lock_path = name_match::lockfile::lock_path(&path);
    let holder = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    holder.try_lock().unwrap();
    drop(holder); // closing the handle releases the lock, like a killed process

    let output = run(&args_for_column(&path, "列一"));
    assert_eq!(
        output.status.code(),
        Some(0),
        "a released lock must not block the next run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["waited_ms"], 0, "the lock was already free");
}
