//! Plain-text input and CSV output helpers.
//!
//! The MCP tool receives file paths rather than inline name arrays, so all
//! decoding, line splitting and CSV encoding lives here and stays free of any
//! MCP or matching concerns.

use std::fs;
use std::path::{Path, PathBuf};

use crate::MatchItem;

/// UTF-8 byte order mark.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Column headers of the generated CSV, in order.
pub const CSV_HEADERS: [&str; 3] = ["待匹配名称", "匹配名称", "匹配度"];

/// Decode raw file bytes into text.
///
/// Strict UTF-8 is tried first; anything else is decoded as GBK, which covers
/// the files produced by Chinese Windows tooling. A leading UTF-8 BOM is
/// stripped either way.
pub fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(UTF8_BOM.as_slice()).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => encoding_rs::GBK.decode(bytes).0.into_owned(),
    }
}

/// Split decoded text into names: one name per line.
///
/// Blank lines are skipped and surrounding whitespace is trimmed. Both `\n` and
/// `\r\n` line endings are accepted.
pub fn parse_names(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Read a name list from a text file.
///
/// Returns a human-readable error (including the offending path) when the file
/// is missing, is not a regular file, or cannot be read.
pub fn read_name_list(path: &Path) -> Result<Vec<String>, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(parse_names(&decode_text(&bytes)))
}

/// Quote a CSV field per RFC 4180 when it contains a comma, quote, or newline.
pub fn escape_csv_field(field: &str) -> String {
    let needs_quoting = field
        .chars()
        .any(|ch| matches!(ch, ',' | '"' | '\n' | '\r'));
    if !needs_quoting {
        return field.to_owned();
    }

    let mut escaped = String::with_capacity(field.len() + 2);
    escaped.push('"');
    for ch in field.chars() {
        if ch == '"' {
            escaped.push('"');
        }
        escaped.push(ch);
    }
    escaped.push('"');
    escaped
}

/// Render the full CSV document: UTF-8 BOM, header, then one row per result.
///
/// Rows keep the order of the target list. An unmatched entry is written as an
/// empty `匹配名称` field.
pub fn render_csv(items: &[MatchItem]) -> String {
    let mut buffer = String::with_capacity(64 + items.len() * 96);
    buffer.push('\u{FEFF}');
    for (index, header) in CSV_HEADERS.iter().enumerate() {
        if index > 0 {
            buffer.push(',');
        }
        buffer.push_str(&escape_csv_field(header));
    }
    buffer.push_str("\r\n");

    for item in items {
        buffer.push_str(&escape_csv_field(&item.name));
        buffer.push(',');
        buffer.push_str(&escape_csv_field(item.matched_name.as_deref().unwrap_or("")));
        buffer.push(',');
        buffer.push_str(&item.score.to_string());
        buffer.push_str("\r\n");
    }

    buffer
}

/// Write results to `path` as CSV, creating missing parent directories.
pub fn write_results_csv(path: &Path, items: &[MatchItem]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create directory {}: {error}", parent.display()))?;
    }
    fs::write(path, render_csv(items))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Resolve a possibly relative path against the process working directory.
pub fn resolved_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether two paths refer to the same file on disk.
///
/// Existing files are compared through their canonical form (which normalizes
/// separators and, on Windows, letter case); otherwise the absolute forms are
/// compared case-insensitively.
pub fn is_same_file(left: &Path, right: &Path) -> bool {
    if let (Ok(left), Ok(right)) = (fs::canonicalize(left), fs::canonicalize(right)) {
        return left == right;
    }
    let left = resolved_path(left).to_string_lossy().to_lowercase();
    let right = resolved_path(right).to_string_lossy().to_lowercase();
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(rows: &[(&str, Option<&str>, f64)]) -> Vec<MatchItem> {
        rows.iter()
            .map(|(name, matched, score)| MatchItem {
                name: (*name).to_owned(),
                matched_name: matched.map(str::to_owned),
                score: *score,
            })
            .collect()
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    #[test]
    fn decodes_utf8_with_and_without_bom() {
        let plain = "京东\n阿里巴巴";
        assert_eq!(decode_text(plain.as_bytes()), plain);

        let mut with_bom = UTF8_BOM.to_vec();
        with_bom.extend_from_slice(plain.as_bytes());
        assert_eq!(decode_text(&with_bom), plain);
    }

    #[test]
    fn decodes_gbk_fallback() {
        let original = "12厘W2510云灰ENF-长沙万华";
        let (encoded, _, had_errors) = encoding_rs::GBK.encode(original);
        assert!(!had_errors);
        assert!(std::str::from_utf8(&encoded).is_err(), "sample must not be valid UTF-8");
        assert_eq!(decode_text(&encoded), original);
    }

    #[test]
    fn parses_mixed_line_endings_and_skips_blank_lines() {
        let text = "  第一行  \r\n\r\n第二行\n   \n第三行\r\n";
        assert_eq!(parse_names(text), vec!["第一行", "第二行", "第三行"]);
    }

    #[test]
    fn parses_empty_text_into_no_names() {
        assert!(parse_names("").is_empty());
        assert!(parse_names("\r\n  \n\t\n").is_empty());
    }

    #[test]
    fn reads_utf8_and_gbk_files() {
        let dir = temp_dir();

        let utf8_path = dir.path().join("utf8.txt");
        fs::write(&utf8_path, "甲\r\n乙\n丙\n").unwrap();
        assert_eq!(read_name_list(&utf8_path).unwrap(), vec!["甲", "乙", "丙"]);

        let gbk_path = dir.path().join("gbk.txt");
        let (encoded, _, _) = encoding_rs::GBK.encode("甲\r\n乙\n");
        fs::write(&gbk_path, &encoded[..]).unwrap();
        assert_eq!(read_name_list(&gbk_path).unwrap(), vec!["甲", "乙"]);
    }

    #[test]
    fn read_errors_mention_the_path() {
        let dir = temp_dir();
        let missing = dir.path().join("没这个文件.txt");
        let message = read_name_list(&missing).unwrap_err();
        assert!(message.contains("没这个文件.txt"), "got: {message}");

        let message = read_name_list(dir.path()).unwrap_err();
        assert!(message.contains("not a regular file"), "got: {message}");
    }

    #[test]
    fn escapes_csv_fields_per_rfc4180() {
        assert_eq!(escape_csv_field("普通名称"), "普通名称");
        assert_eq!(escape_csv_field("含,逗号"), "\"含,逗号\"");
        assert_eq!(escape_csv_field("含\"引号"), "\"含\"\"引号\"");
        assert_eq!(escape_csv_field("含\n换行"), "\"含\n换行\"");
        assert_eq!(escape_csv_field("含\r回车"), "\"含\r回车\"");
    }

    #[test]
    fn renders_header_only_for_empty_results() {
        assert_eq!(render_csv(&[]), "\u{FEFF}待匹配名称,匹配名称,匹配度\r\n");
    }

    #[test]
    fn renders_rows_in_input_order() {
        let csv = render_csv(&items(&[
            ("甲", Some("甲A"), 1.0),
            ("乙", None, 0.42),
        ]));
        assert_eq!(
            csv,
            "\u{FEFF}待匹配名称,匹配名称,匹配度\r\n甲,甲A,1\r\n乙,,0.42\r\n"
        );
    }

    #[test]
    fn writes_csv_and_creates_missing_parent_directories() {
        let dir = temp_dir();
        let nested = dir.path().join("a").join("b").join("结果.csv");
        write_results_csv(&nested, &items(&[("甲", None, 0.0)])).unwrap();

        let written = fs::read_to_string(&nested).unwrap();
        assert_eq!(
            written,
            "\u{FEFF}待匹配名称,匹配名称,匹配度\r\n甲,,0\r\n"
        );
    }

    #[test]
    fn detects_the_same_file_and_distinct_files() {
        let dir = temp_dir();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        fs::write(&first, "甲\n").unwrap();
        fs::write(&second, "乙\n").unwrap();

        assert!(is_same_file(&first, &first));
        assert!(is_same_file(&first, &dir.path().join(".").join("a.txt")));
        assert!(!is_same_file(&first, &second));
        assert!(!is_same_file(&first, &dir.path().join("missing.txt")));
    }

    #[test]
    fn resolves_relative_paths_against_the_working_directory() {
        let resolved = resolved_path(Path::new("dist/输出.csv"));
        assert!(resolved.is_absolute());
        assert!(resolved.to_string_lossy().ends_with("输出.csv"));
    }
}
