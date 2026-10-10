//! Command line interface: argument parsing, orchestration and JSON output.
//!
//! A single subcommand keeps the surface small enough to parse by hand, so the
//! binary has no argument-parsing dependency.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::replacement::{match_columns, to_writes};
use crate::xlsx::{Workbook, XlsxError};
use crate::lockfile::WorkbookLock;
use crate::{DEFAULT_THRESHOLD, resolved_path};

/// Usage text shown by `--help`.
pub const HELP: &str = "\
name-match — 在同一工作簿内按列匹配名称并回写结果

用法:
  name-match match --workbook <文件> --reference-sheet <表> --reference-column <列>
                   --target-sheet <表> --target-column <列>
                   [--header-row <行号>] [--match-column-name <列名>]
                   [--score-column-name <列名>] [--threshold <0.0~1.0>]
  name-match --help
  name-match --version

说明:
  用「标准名称列」去匹配「待匹配列」，把『匹配名称』『匹配度』两列写到目标列
  右侧。工作簿原地修改、原列保留、不会生成备份。先做精确比对（忽略全角半角、
  大小写、空白与标点括号），未命中再按相似度模糊匹配；低于阈值的行匹配名称
  留空、匹配度照写。可重复调用：同一对结果列会被整体覆盖，不会重复追加。

参数:
  --workbook <路径>              待处理的 .xlsx 工作簿（必填，原地修改）
  --reference-sheet <名称>       标准名称所在工作表（必填）
  --reference-column <列名>      标准名称列的列名/表头（必填）
  --target-sheet <名称>          待匹配工作表（必填）
  --target-column <列名>         待匹配列的列名/表头（必填）
  --header-row <行号>            表头行号，从 1 开始；默认 1
  --match-column-name <列名>     匹配名称列表头；默认 匹配名称
  --score-column-name <列名>     匹配度列表头；默认 匹配度
  --threshold <数值>             模糊匹配最低分 0.0~1.0；默认 0.6

输出:
  仅向 stdout 输出 JSON。成功时输出匹配摘要，失败时输出 {\"error\":{...}}，
  退出码：成功 0、任何失败 1。诊断信息一律写 stderr。
";

/// Validated inputs of the `match` subcommand.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArgs {
    /// Workbook to modify in place.
    pub workbook: String,
    /// Worksheet holding the standard names.
    pub reference_sheet: String,
    /// Header of the standard-name column.
    pub reference_column: String,
    /// Worksheet holding the column to match.
    pub target_sheet: String,
    /// Header of the column to match.
    pub target_column: String,
    /// 1-based header row.
    pub header_row: u32,
    /// Header written above the matched names.
    pub match_column_name: String,
    /// Header written above the scores.
    pub score_column_name: String,
    /// Minimum fuzzy score kept as a match.
    pub threshold: f64,
}

/// Summary printed on success; mirrors the field names used before the CLI.
#[derive(Debug, Clone, Serialize)]
pub struct MatchSummary {
    /// Absolute path of the workbook that was modified.
    pub xlsx_path: String,
    /// Sheet that was matched.
    pub sheet: String,
    /// Column that was matched.
    pub column: String,
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
    /// How long this run waited for another process to release the workbook.
    ///
    /// Non-zero means a concurrent run was in progress and this call was
    /// serialized behind it.
    pub waited_ms: u64,
}

/// A failure with enough detail for the caller to classify it.
#[derive(Debug, Clone, PartialEq)]
pub struct CliError {
    /// Short machine-readable category, surfaced in the JSON error object.
    pub kind: ErrorKind,
    /// Human-readable explanation (Chinese, includes the offending value).
    pub message: String,
}

/// Category of a CLI failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Bad or missing arguments, or a workbook that does not match the request.
    Usage,
    /// The request was well formed but referred to something missing.
    Invalid,
    /// Reading or writing failed.
    Io,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Usage,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Invalid,
            message: message.into(),
        }
    }

    fn io(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Io,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// What the caller asked the binary to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Match a workbook column.
    Match(Box<MatchArgs>),
    /// Print usage.
    Help,
    /// Print the version.
    Version,
}

/// Parse command line arguments (excluding the program name).
///
/// Accepts `--flag value` and `--flag=value`, tolerates `--` before the
/// subcommand, and rejects unknown or repeated flags with a usage error.
pub fn parse<I, S>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args: Vec<String> = args.into_iter().map(Into::into).collect();
    if args.first().map(String::as_str) == Some("--") {
        args.remove(0);
    }
    let Some(command) = args.first().cloned() else {
        return Err(CliError::usage("缺少子命令；用法见 name-match --help"));
    };

    match command.as_str() {
        "match" => parse_match(&args[1..]).map(|args| Command::Match(Box::new(args))),
        "--help" | "-h" | "help" => Ok(Command::Help),
        "--version" | "-V" | "version" => Ok(Command::Version),
        other => Err(CliError::usage(format!(
            "未知子命令 {other:?}；目前只支持 match，用法见 name-match --help"
        ))),
    }
}

/// A tiny `--flag value` reader that reports repeated or unknown flags.
struct Flags {
    values: Vec<(String, String)>,
}

impl Flags {
    /// Consume a flat argument list, splitting `--flag=value` pairs.
    fn parse(args: &[String]) -> Result<Self, CliError> {
        let mut values: Vec<(String, String)> = Vec::with_capacity(args.len());
        let mut index = 0usize;
        while index < args.len() {
            let token = &args[index];
            if !token.starts_with("--") {
                return Err(CliError::usage(format!(
                    "无法识别的参数 {token:?}；参数必须以 -- 开头"
                )));
            }
            let (name, inline) = match token.split_once('=') {
                Some((name, value)) => (name.to_string(), Some(value.to_string())),
                None => (token.clone(), None),
            };
            if values.iter().any(|(seen, _)| *seen == name) {
                return Err(CliError::usage(format!("参数 {name} 重复出现")));
            }
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index).cloned().ok_or_else(|| {
                        CliError::usage(format!("参数 {name} 缺少取值"))
                    })?
                }
            };
            values.push((name, value));
            index += 1;
        }
        Ok(Self { values })
    }

    /// Take a required string flag.
    fn required(&mut self, name: &str) -> Result<String, CliError> {
        self.values
            .iter()
            .position(|(key, _)| key == name)
            .map(|index| self.values.remove(index).1)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CliError::usage(format!("缺少必填参数 {name}")))
    }

    /// Take an optional string flag.
    fn optional(&mut self, name: &str) -> Result<Option<String>, CliError> {
        let Some(index) = self.values.iter().position(|(key, _)| key == name) else {
            return Ok(None);
        };
        let value = self.values.remove(index).1;
        if value.trim().is_empty() {
            return Err(CliError::usage(format!("参数 {name} 不能为空白")));
        }
        Ok(Some(value))
    }

    /// Take an optional flag parsed as a number.
    fn number<T: std::str::FromStr>(&mut self, name: &str) -> Result<Option<T>, CliError> {
        match self.optional(name)? {
            None => Ok(None),
            Some(raw) => raw
                .trim()
                .parse::<T>()
                .map(Some)
                .map_err(|_| CliError::usage(format!("参数 {name} 的取值 {raw:?} 不是有效数字"))),
        }
    }

    /// Fail if any flag was left over.
    fn finish(self) -> Result<(), CliError> {
        match self.values.first() {
            None => Ok(()),
            Some((name, _)) => Err(CliError::usage(format!("未知参数 {name}"))),
        }
    }
}

fn parse_match(args: &[String]) -> Result<MatchArgs, CliError> {
    let mut flags = Flags::parse(args)?;

    let workbook = flags.required("--workbook")?;
    let reference_sheet = flags.required("--reference-sheet")?;
    let reference_column = flags.required("--reference-column")?;
    let target_sheet = flags.required("--target-sheet")?;
    let target_column = flags.required("--target-column")?;
    let header_row: u32 = flags.number("--header-row")?.unwrap_or(1);
    let match_column_name = flags
        .optional("--match-column-name")?
        .unwrap_or_else(|| "匹配名称".to_string());
    let score_column_name = flags
        .optional("--score-column-name")?
        .unwrap_or_else(|| "匹配度".to_string());
    let threshold = flags.number("--threshold")?.unwrap_or(DEFAULT_THRESHOLD);
    flags.finish()?;

    if header_row == 0 {
        return Err(CliError::usage("--header-row 从 1 开始计数，必须大于等于 1"));
    }
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err(CliError::usage(format!(
            "--threshold 必须是 0.0~1.0 之间的有限数字，当前为 {threshold}"
        )));
    }

    Ok(MatchArgs {
        workbook,
        reference_sheet,
        reference_column,
        target_sheet,
        target_column,
        header_row,
        match_column_name,
        score_column_name,
        threshold,
    })
}

/// Run the matching job described by `args`.
pub fn run(args: &MatchArgs) -> Result<MatchSummary, CliError> {
    let workbook_path = PathBuf::from(&args.workbook);
    ensure_xlsx_extension(&workbook_path)?;

    // Hold this across the whole read-modify-write span. Two runs that both read
    // the file before either writes would make the later write discard the
    // earlier result, so the lock must cover reading too, not just saving.
    let lock = WorkbookLock::acquire(&workbook_path)?;

    let mut workbook = Workbook::open(&workbook_path).map_err(map_xlsx_error)?;

    let (reference_cells, _) = read_column(
        &workbook,
        &args.reference_sheet,
        &args.reference_column,
        args.header_row,
        "标准名称列",
    )?;
    if reference_cells.is_empty() {
        return Err(CliError::invalid(format!(
            "工作表 {:?} 的 {:?} 列在第 {} 行之下没有任何数据",
            args.reference_sheet, args.reference_column, args.header_row
        )));
    }
    let reference_values: Vec<String> =
        reference_cells.iter().map(|cell| cell.value.clone()).collect();
    let (target_cells, target_column_index) = read_column(
        &workbook,
        &args.target_sheet,
        &args.target_column,
        args.header_row,
        "待匹配列",
    )?;

    let started = std::time::Instant::now();
    let (rows, tally) = match_columns(&reference_values, &target_cells, args.threshold);
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let written = workbook
        .write_result_columns(
            &args.target_sheet,
            args.header_row,
            &args.match_column_name,
            &args.score_column_name,
            target_column_index,
            &to_writes(&rows),
        )
        .map_err(map_xlsx_error)?;

    workbook.save(&workbook_path).map_err(|error| {
        CliError::io(format!(
            "写入 {} 失败：{error}。原文件未被修改",
            workbook_path.display()
        ))
    })?;

    let waited_ms = lock.waited().as_millis() as u64;
    drop(lock);

    Ok(MatchSummary {
        xlsx_path: resolved_path(&workbook_path).display().to_string(),
        sheet: args.target_sheet.clone(),
        column: args.target_column.clone(),
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
        waited_ms,
    })
}

/// Read one column, attributing failures to the role it plays in the request.
fn read_column(
    workbook: &Workbook,
    sheet: &str,
    column: &str,
    header_row: u32,
    role: &str,
) -> Result<(Vec<crate::xlsx::ColumnCell>, u32), CliError> {
    workbook.read_column(sheet, column, header_row).map_err(|error| match error {
        XlsxError::Invalid(message) => CliError::invalid(format!("{role}：{message}")),
        XlsxError::Io(message) => CliError::io(format!("{role}：{message}")),
    })
}

/// Reject anything that is not an existing `.xlsx` file.
fn ensure_xlsx_extension(path: &Path) -> Result<(), CliError> {
    if !path.exists() {
        return Err(CliError::invalid(format!(
            "工作簿不存在：{}",
            resolved_path(path).display()
        )));
    }
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "xlsx" => Ok(()),
        other => Err(CliError::invalid(format!(
            "只支持 .xlsx 工作簿，当前文件为 {}",
            if other.is_empty() {
                "无扩展名".to_string()
            } else {
                format!(".{other}")
            }
        ))),
    }
}

fn map_xlsx_error(error: XlsxError) -> CliError {
    match error {
        XlsxError::Invalid(message) => CliError::invalid(message),
        XlsxError::Io(message) => CliError::io(message),
    }
}

/// Render the JSON error document printed on failure.
pub fn render_error(error: &CliError) -> String {
    serde_json::json!({
        "error": {
            "kind": error.kind,
            "message": error.message,
        }
    })
    .to_string()
}

/// Render the JSON summary printed on success.
pub fn render_summary(summary: &MatchSummary) -> Result<String, CliError> {
    serde_json::to_string(summary)
        .map_err(|error| CliError::io(format!("无法序列化结果：{error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_workbook;

    fn workbook_with(dir: &Path, name: &str, sheets: &[(&str, &[&[&str]])]) -> PathBuf {
        let path = dir.join(name);
        build_workbook(&path, sheets);
        path
    }

    fn base_args(path: &Path) -> MatchArgs {
        MatchArgs {
            workbook: path.display().to_string(),
            reference_sheet: "参考".into(),
            reference_column: "标准名称".into(),
            target_sheet: "目标".into(),
            target_column: "原始名称".into(),
            header_row: 1,
            match_column_name: "匹配名称".into(),
            score_column_name: "匹配度".into(),
            threshold: DEFAULT_THRESHOLD,
        }
    }

    fn cli(args: &[&str]) -> Vec<String> {
        args.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_match_with_required_flags_only() {
        let parsed = parse(cli(&[
            "match",
            "--workbook",
            "统计.xlsx",
            "--reference-sheet",
            "9月压面",
            "--reference-column",
            "商品名称",
            "--target-sheet",
            "9月贴面",
            "--target-column",
            "组装商品1",
        ]))
        .unwrap();

        let Command::Match(args) = parsed else {
            panic!("expected the match subcommand");
        };
        assert_eq!(args.workbook, "统计.xlsx");
        assert_eq!(args.reference_sheet, "9月压面");
        assert_eq!(args.target_column, "组装商品1");
        assert_eq!(args.header_row, 1, "defaults to the first row");
        assert_eq!(args.match_column_name, "匹配名称");
        assert_eq!(args.score_column_name, "匹配度");
        assert_eq!(args.threshold, DEFAULT_THRESHOLD);
    }

    #[test]
    fn parses_optional_flags_and_equals_syntax() {
        let parsed = parse(cli(&[
            "match",
            "--workbook=统计.xlsx",
            "--reference-sheet=参考",
            "--reference-column=标准名称",
            "--target-sheet=目标",
            "--target-column=原始名称",
            "--header-row=3",
            "--match-column-name=对照名称",
            "--score-column-name=相似度",
            "--threshold=0.75",
        ]))
        .unwrap();

        let Command::Match(args) = parsed else {
            panic!("expected the match subcommand");
        };
        assert_eq!(args.header_row, 3);
        assert_eq!(args.match_column_name, "对照名称");
        assert_eq!(args.score_column_name, "相似度");
        assert_eq!(args.threshold, 0.75);
    }

    #[test]
    fn recognises_help_and_version() {
        assert_eq!(parse(cli(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse(cli(&["-h"])).unwrap(), Command::Help);
        assert_eq!(parse(cli(&["help"])).unwrap(), Command::Help);
        assert_eq!(parse(cli(&["--version"])).unwrap(), Command::Version);
        assert_eq!(parse(cli(&["version"])).unwrap(), Command::Version);
    }

    #[test]
    fn reports_missing_required_and_unknown_flags() {
        let error = parse(cli(&["match", "--workbook", "a.xlsx"])).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Usage);
        assert!(error.message.contains("--reference-sheet"), "{}", error.message);

        let error = parse(cli(&[
            "match",
            "--workbook", "a.xlsx",
            "--reference-sheet", "s",
            "--reference-column", "c",
            "--target-sheet", "s",
            "--target-column", "c",
            "--xlsx-path", "oops",
        ]))
        .unwrap_err();
        assert!(error.message.contains("--xlsx-path"), "{}", error.message);

        let error = parse(cli(&[])).unwrap_err();
        assert!(error.message.contains("缺少子命令"), "{}", error.message);

        let error = parse(cli(&["merge"])).unwrap_err();
        assert!(error.message.contains("未知子命令"), "{}", error.message);
    }

    #[test]
    fn rejects_repeated_flag_and_missing_value() {
        let error = parse(cli(&[
            "match",
            "--workbook", "a.xlsx",
            "--workbook", "b.xlsx",
            "--reference-sheet", "s",
            "--reference-column", "c",
            "--target-sheet", "s",
            "--target-column", "c",
        ]))
        .unwrap_err();
        assert!(error.message.contains("重复"), "{}", error.message);

        let error = parse(cli(&["match", "--workbook"])).unwrap_err();
        assert!(error.message.contains("缺少取值"), "{}", error.message);
    }

    #[test]
    fn rejects_invalid_numbers_and_ranges() {
        let base = [
            "match",
            "--workbook", "a.xlsx",
            "--reference-sheet", "s",
            "--reference-column", "c",
            "--target-sheet", "s",
            "--target-column", "c",
        ];
        let mut argv = base.to_vec();
        argv.extend_from_slice(&["--threshold", "1.5"]);
        let error = parse(cli(&argv)).unwrap_err();
        assert!(error.message.contains("0.0~1.0"), "{}", error.message);

        let mut argv = base.to_vec();
        argv.extend_from_slice(&["--header-row", "0"]);
        let error = parse(cli(&argv)).unwrap_err();
        assert!(error.message.contains("从 1 开始计数"), "{}", error.message);

        let mut argv = base.to_vec();
        argv.extend_from_slice(&["--threshold", "abc"]);
        let error = parse(cli(&argv)).unwrap_err();
        assert!(error.message.contains("不是有效数字"), "{}", error.message);

        // A blank value is rejected for optional flags too, not just required ones.
        let mut argv = base.to_vec();
        argv.extend_from_slice(&["--match-column-name", "  "]);
        let error = parse(cli(&argv)).unwrap_err();
        assert!(error.message.contains("不能为空白"), "{}", error.message);
    }

    #[test]
    fn matches_and_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = workbook_with(
            dir.path(),
            "统计.xlsx",
            &[
                ("参考", &[&["标准名称"], &["甲"], &["乙"], &["丙"]]),
                ("目标", &[&["原始名称"], &["甲"], &["乙"], &["完全不相关ZZZ"]]),
            ],
        );

        let summary = run(&base_args(&path)).unwrap();

        assert_eq!(summary.reference_count, 3);
        assert_eq!(summary.rows_scanned, 3);
        assert_eq!(summary.exact_count, 2);
        assert_eq!(summary.unmatched_count, 1);
        assert_eq!(summary.matched_count, summary.exact_count + summary.fuzzy_count);
        assert_eq!(summary.match_column, "B");
        assert_eq!(summary.score_column, "C");
        assert!(!summary.reused_columns);
        assert_eq!(summary.sheet, "目标");
        assert_eq!(summary.column, "原始名称");
        assert!(Path::new(&summary.xlsx_path).is_absolute());
    }

    #[test]
    fn repeated_runs_do_not_duplicate_result_cells() {
        let dir = tempfile::tempdir().unwrap();
        let path = workbook_with(
            dir.path(),
            "统计.xlsx",
            &[
                ("参考", &[&["标准名称"], &["甲"], &["乙"]]),
                ("目标", &[&["原始名称"], &["甲"], &["乙"]]),
            ],
        );

        let first = run(&base_args(&path)).unwrap();
        assert!(!first.reused_columns, "first run appends the columns");
        let second = run(&base_args(&path)).unwrap();
        assert!(second.reused_columns, "second run reuses the same columns");
        assert_eq!(second.match_column, first.match_column);
        assert_eq!(second.score_column, first.score_column);

        let sheet = crate::test_support::read_all_text(&path, "xl/worksheets/sheet2.xml");
        for reference in ["B1", "C1", "B2", "C2", "B3", "C3"] {
            assert_eq!(
                sheet.matches(&format!(r#"r="{reference}""#)).count(),
                1,
                "cell {reference} must appear exactly once"
            );
        }
        assert_eq!(sheet.matches("<dimension").count(), 1);
        assert_eq!(sheet.matches(r#"ref="A1:C3""#).count(), 1);
    }

    #[test]
    fn switching_the_target_column_clears_the_previous_run() {
        let dir = tempfile::tempdir().unwrap();
        // Two columns of different lengths: the long one runs first, so the
        // second run must clear the rows it never visits.
        let path = workbook_with(
            dir.path(),
            "两列.xlsx",
            &[(
                "目标",
                &[&["列一", "列二"], &["甲", "甲"], &["乙", "乙"], &["甲", ""], &["乙", ""]],
            )],
        );

        let mut wide = base_args(&path);
        wide.reference_sheet = "目标".into();
        wide.reference_column = "列一".into();
        wide.target_sheet = "目标".into();
        wide.target_column = "列一".into();
        assert_eq!(run(&wide).unwrap().rows_scanned, 4);

        let mut narrow = wide.clone();
        narrow.target_column = "列二".into();
        let second = run(&narrow).unwrap();
        assert_eq!(second.rows_scanned, 2, "only rows 2 and 3 carry 列二");
        assert!(second.reused_columns);

        let sheet = crate::test_support::read_all_text(&path, "xl/worksheets/sheet1.xml");
        for reference in [
            format!("{}1", second.match_column),
            format!("{}2", second.match_column),
            format!("{}3", second.match_column),
            format!("{}1", second.score_column),
            format!("{}2", second.score_column),
            format!("{}3", second.score_column),
        ] {
            assert_eq!(
                sheet.matches(&format!(r#"r="{reference}""#)).count(),
                1,
                "cell {reference} must appear exactly once"
            );
        }
        for reference in [
            format!("{}4", second.match_column),
            format!("{}5", second.match_column),
        ] {
            assert_eq!(
                sheet.matches(&format!(r#"r="{reference}""#)).count(),
                0,
                "stale cell {reference} should have been cleared"
            );
        }
    }

    #[test]
    fn custom_result_column_names_are_honoured_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = workbook_with(
            dir.path(),
            "统计.xlsx",
            &[("参考", &[&["标准名称"], &["甲"]]), ("目标", &[&["原始名称"], &["甲"]])],
        );

        let mut args = base_args(&path);
        args.match_column_name = "对照名称".into();
        args.score_column_name = "相似度".into();
        let summary = run(&args).unwrap();
        assert!(!summary.reused_columns);

        let sheet = crate::test_support::read_all_text(&path, "xl/worksheets/sheet2.xml");
        assert!(sheet.contains("<t>对照名称</t>"), "{sheet}");
        assert!(sheet.contains("<t>相似度</t>"), "{sheet}");

        let second = run(&args).unwrap();
        assert!(second.reused_columns);
        assert_eq!(second.match_column, summary.match_column);
    }

    #[test]
    fn rejects_missing_sheet_and_column_with_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = workbook_with(dir.path(), "统计.xlsx", &[("参考", &[&["标准名称"], &["甲"]])]);

        let mut args = base_args(&path);
        args.reference_sheet = "不存在的表".into();
        let error = run(&args).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Invalid);
        assert!(error.message.contains("不存在的表"), "{}", error.message);
        assert!(error.message.contains("参考"), "{}", error.message);

        let path = workbook_with(
            dir.path(),
            "两列.xlsx",
            &[("参考", &[&["标准名称", "序号"], &["甲", "1"]])],
        );
        let mut args = base_args(&path);
        args.reference_column = "没有这一列".into();
        let error = run(&args).unwrap_err();
        assert!(error.message.contains("没有这一列"), "{}", error.message);
        assert!(error.message.contains("序号"), "{}", error.message);
    }

    #[test]
    fn rejects_non_xlsx_missing_file_and_empty_reference() {
        let dir = tempfile::tempdir().unwrap();
        let xls = dir.path().join("旧格式.xls");
        std::fs::write(&xls, b"not a workbook").unwrap();
        let error = run(&base_args(&xls)).unwrap_err();
        assert!(error.message.contains("只支持 .xlsx"), "{}", error.message);

        let missing = dir.path().join("没有这个.xlsx");
        let error = run(&base_args(&missing)).unwrap_err();
        assert!(error.message.contains("工作簿不存在"), "{}", error.message);

        let path = workbook_with(
            dir.path(),
            "空.xlsx",
            &[("参考", &[&["标准名称"]]), ("目标", &[&["原始名称"]])],
        );
        let error = run(&base_args(&path)).unwrap_err();
        assert!(error.message.contains("没有任何数据"), "{}", error.message);
    }

    #[test]
    fn modifies_the_workbook_in_place_without_leaving_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = workbook_with(
            dir.path(),
            "统计.xlsx",
            &[("参考", &[&["标准名称"], &["甲"]]), ("目标", &[&["原始名称"], &["甲"]])],
        );
        let before = std::fs::read(&path).unwrap();

        run(&base_args(&path)).unwrap();

        assert_ne!(std::fs::read(&path).unwrap(), before, "workbook must change");
        // The sidecar lock file is expected to remain; nothing else may.
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "统计.xlsx" && name != ".统计.xlsx.lock")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no backup or temp file may remain (only the sidecar lock is allowed): {leftovers:?}"
        );
    }

    #[test]
    fn renders_json_for_success_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = workbook_with(
            dir.path(),
            "统计.xlsx",
            &[("参考", &[&["标准名称"], &["甲"]]), ("目标", &[&["原始名称"], &["甲"]])],
        );
        let summary = run(&base_args(&path)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&render_summary(&summary).unwrap()).unwrap();
        for field in [
            "xlsx_path",
            "sheet",
            "column",
            "reference_count",
            "rows_scanned",
            "matched_count",
            "unmatched_count",
            "exact_count",
            "fuzzy_count",
            "match_column",
            "score_column",
            "reused_columns",
            "elapsed_ms",
        ] {
            assert!(json.get(field).is_some(), "missing field {field} in {json}");
        }
        assert_eq!(json["matched_count"], 1);

        let error = CliError::invalid("表不存在");
        let json: serde_json::Value = serde_json::from_str(&render_error(&error)).unwrap();
        assert_eq!(json["error"]["kind"], "invalid");
        assert_eq!(json["error"]["message"], "表不存在");
    }
}
