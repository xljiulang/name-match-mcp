//! Minimal xlsx surgery: read a column, then append derived columns in place.
//!
//! The workbook is treated as a plain zip. Only the target worksheet's XML is
//! rewritten; every other zip entry is copied through byte for byte. That keeps
//! pivot caches, drawings, printer settings and everything else intact — a full
//! re-serialization through a spreadsheet library does not.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

/// A failure while reading or rewriting a workbook.
#[derive(Debug)]
pub enum XlsxError {
    /// The caller asked for something that does not exist in the workbook.
    Invalid(String),
    /// The file could not be read or written.
    Io(String),
}

impl XlsxError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }
}

impl std::fmt::Display for XlsxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Io(message) => f.write_str(message),
        }
    }
}

/// One value extracted from a column, together with its row number.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnCell {
    /// 1-based row number in the worksheet.
    pub row: u32,
    /// Trimmed cell text as stored in the workbook.
    pub value: String,
}

/// A cell to write into a row of the target worksheet.
#[derive(Debug, Clone, PartialEq)]
pub struct RowWrite {
    /// 1-based row number the values belong to.
    pub row: u32,
    /// Matched name, or `None` to leave the match cell empty.
    pub matched: Option<String>,
    /// Score written as a number.
    pub score: f64,
}

/// Which columns the results were written to.
#[derive(Debug, Clone, PartialEq)]
pub struct WrittenColumns {
    /// Column letter of the match-name column.
    pub match_letter: String,
    /// Column letter of the score column.
    pub score_letter: String,
    /// Whether pre-existing result columns were reused instead of appended.
    pub reused: bool,
}

struct Item {
    name: String,
    data: Vec<u8>,
    method: CompressionMethod,
    last_modified: Option<DateTime>,
}

/// A workbook held in memory as its raw zip entries.
pub struct Workbook {
    items: Vec<Item>,
    shared_strings: Vec<String>,
}

impl Workbook {
    /// Read the whole workbook into memory.
    pub fn open(path: &Path) -> Result<Self, XlsxError> {
        let file = fs::File::open(path)
            .map_err(|error| XlsxError::io(format!("cannot open {}: {error}", path.display())))?;
        let mut archive = ZipArchive::new(file).map_err(|error| {
            XlsxError::invalid(format!("{} is not a readable xlsx file: {error}", path.display()))
        })?;

        let mut items = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|error| {
                XlsxError::invalid(format!("cannot read zip entry #{index}: {error}"))
            })?;
            let name = entry.name().to_string();
            let method = entry.compression();
            let last_modified = entry.last_modified();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|error| {
                XlsxError::invalid(format!("cannot decompress {name}: {error}"))
            })?;
            items.push(Item {
                name,
                data,
                method,
                last_modified,
            });
        }

        let shared_strings = items
            .iter()
            .find(|item| item.name == "xl/sharedStrings.xml")
            .map(|item| parse_shared_strings(&String::from_utf8_lossy(&item.data)))
            .unwrap_or_default();

        Ok(Self {
            items,
            shared_strings,
        })
    }

    fn entry(&self, name: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.name == name)
    }

    fn text(&self, name: &str) -> Result<String, XlsxError> {
        self.entry(name)
            .map(|item| String::from_utf8_lossy(&item.data).into_owned())
            .ok_or_else(|| XlsxError::invalid(format!("workbook is missing {name}")))
    }

    /// Names of all worksheets, in workbook order.
    pub fn sheet_names(&self) -> Result<Vec<String>, XlsxError> {
        Ok(parse_sheets(&self.text("xl/workbook.xml")?)
            .into_iter()
            .map(|sheet| sheet.name)
            .collect())
    }

    /// Resolve a worksheet name to its zip entry path.
    fn sheet_entry(&self, sheet: &str) -> Result<String, XlsxError> {
        let sheets = parse_sheets(&self.text("xl/workbook.xml")?);
        let found = sheets.iter().find(|candidate| candidate.name == sheet);
        let Some(found) = found else {
            let available: Vec<String> = sheets.into_iter().map(|sheet| sheet.name).collect();
            return Err(XlsxError::invalid(format!(
                "worksheet {sheet:?} not found; available worksheets: {}",
                available.join(", ")
            )));
        };

        let rels = self.text("xl/_rels/workbook.xml.rels")?;
        let target = parse_rel_target(&rels, &found.rel_id).ok_or_else(|| {
            XlsxError::invalid(format!(
                "worksheet {sheet:?} has no relationship target in xl/_rels/workbook.xml.rels"
            ))
        })?;
        Ok(normalize_part_path(&target))
    }

    /// Cell text as stored, resolving shared-string cells.
    fn cell_text(&self, cell: &RawCell) -> Option<String> {
        let raw = cell.text()?;
        match cell.kind.as_deref() {
            Some("s") => raw
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|index| self.shared_strings.get(index).cloned()),
            _ => Some(raw),
        }
    }

    /// Locate a column by its header text.
    ///
    /// Returns the 1-based column index. Matches trivially on exact trimmed
    /// text, then case-insensitively, so callers rarely need to know the case.
    fn locate_column(&self, sheet: &str, header: &str, header_row: u32) -> Result<u32, XlsxError> {
        let sheet_path = self.sheet_entry(sheet)?;
        let xml = self.text(&sheet_path)?;
        let Some((_, start, end)) = find_row(&xml, header_row) else {
            return Err(XlsxError::invalid(format!(
                "worksheet {sheet:?} has no row {header_row}, so header {header:?} cannot be located"
            )));
        };
        let cells = parse_cells(&xml[start..end]);

        let mut candidates = Vec::new();
        for cell in &cells {
            let Some(text) = self.cell_text(cell) else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            candidates.push((cell.column, text.to_string()));
        }

        let wanted = header.trim();
        if let Some((column, _)) = candidates.iter().find(|(_, text)| text == wanted) {
            return Ok(*column);
        }
        let lower = wanted.to_lowercase();
        if let Some((column, _)) = candidates
            .iter()
            .find(|(_, text)| text.to_lowercase() == lower)
        {
            return Ok(*column);
        }

        let available: Vec<String> = candidates.into_iter().map(|(_, text)| text).collect();
        if available.is_empty() {
            return Err(XlsxError::invalid(format!(
                "column {header:?} not found: row {header_row} of worksheet {sheet:?} is empty"
            )));
        }
        Err(XlsxError::invalid(format!(
            "column {header:?} not found in row {header_row} of worksheet {sheet:?}; that row has: {}",
            available.join(", ")
        )))
    }

    /// Read the non-empty values of a column, below the header row.
    pub fn read_column(
        &self,
        sheet: &str,
        header: &str,
        header_row: u32,
    ) -> Result<(Vec<ColumnCell>, u32), XlsxError> {
        let column = self.locate_column(sheet, header, header_row)?;
        let sheet_path = self.sheet_entry(sheet)?;
        let xml = self.text(&sheet_path)?;

        let mut values = Vec::new();
        for (row_number, row_start, row_end) in iterate_rows(&xml) {
            if row_number <= header_row {
                continue;
            }
            let cells = parse_cells(&xml[row_start..row_end]);
            let Some(cell) = cells.iter().find(|cell| cell.column == column) else {
                continue;
            };
            let Some(text) = self.cell_text(cell) else {
                continue;
            };
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            values.push(ColumnCell {
                row: row_number,
                value: text,
            });
        }
        Ok((values, column))
    }

    /// Append (or reuse) two result columns and write `rows` into them.
    ///
    /// New columns are placed to the right of the last used column of the
    /// worksheet, so an existing neighbour column is never overwritten. If the
    /// header row already has columns with the requested names, those are
    /// reused and overwritten instead.
    pub fn write_result_columns(
        &mut self,
        sheet: &str,
        header_row: u32,
        match_header: &str,
        score_header: &str,
        target_column: u32,
        rows: &[RowWrite],
    ) -> Result<WrittenColumns, XlsxError> {
        let sheet_path = self.sheet_entry(sheet)?;
        let xml = self.text(&sheet_path)?;

        let header_span = find_row(&xml, header_row).ok_or_else(|| {
            XlsxError::invalid(format!(
                "worksheet {sheet:?} has no row {header_row} to hold the result headers"
            ))
        })?;
        let header_cells = parse_cells(&xml[header_span.1..header_span.2]);

        let existing = |wanted: &str| -> Option<u32> {
            header_cells
                .iter()
                .find(|cell| {
                    self.cell_text(cell)
                        .map(|text| text.trim() == wanted)
                        .unwrap_or(false)
                })
                .map(|cell| cell.column)
        };

        let existing_match = existing(match_header);
        let existing_score = existing(score_header);
        let reused = existing_match.is_some() || existing_score.is_some();

        let last_used = last_used_column(&xml);
        let mut next = last_used + 1;
        let mut allocate = || {
            let column = next;
            next += 1;
            column
        };
        let match_column = existing_match.unwrap_or_else(&mut allocate);
        let score_column = existing_score.unwrap_or_else(&mut allocate);
        let last_column = match_column.max(score_column).max(last_used);

        // Style to fall back on: the header row's target column, else its last cell.
        let header_style = style_of(&header_cells, target_column)
            .or_else(|| header_cells.last().and_then(|cell| cell.style.clone()));

        let mut writes: std::collections::HashMap<u32, &RowWrite> =
            std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            writes.insert(row.row, row);
        }

        let mut out = String::with_capacity(xml.len() + rows.len() * 160);
        out.push_str(&xml[..0]);
        let mut cursor = 0usize;
        for (row_number, row_start, row_end) in iterate_rows(&xml) {
            // Everything before this row, plus its opening tag.
            let (row_open_start, row_open_end) = row_open_bounds(&xml, row_start);
            out.push_str(&xml[cursor..row_open_start]);
            let open_tag = &xml[row_open_start..row_open_end];
            let inner = &xml[row_open_end..row_end];
            let closing = &xml[row_end..row_end + "</row>".len()];
            debug_assert_eq!(closing, "</row>");

            let is_self_closing = open_tag.trim_end().ends_with("/>");
            let mut new_open = open_tag.to_string();
            if !is_self_closing {
                new_open = set_span(new_open, last_column);
            }

            let extra = if row_number == header_row {
                let mut cells = String::new();
                if existing_match.is_none() {
                    cells.push_str(&inline_cell(
                        match_column,
                        row_number,
                        header_style.as_deref(),
                        match_header,
                    ));
                }
                if existing_score.is_none() {
                    cells.push_str(&inline_cell(
                        score_column,
                        row_number,
                        header_style.as_deref(),
                        score_header,
                    ));
                }
                cells
            } else if let Some(write) = writes.get(&row_number) {
                let style = style_of(&parse_cells(inner), target_column)
                    .or_else(|| header_style.clone());
                let mut cells = String::new();
                match &write.matched {
                    Some(name) => cells.push_str(&inline_cell(
                        match_column,
                        row_number,
                        style.as_deref(),
                        name,
                    )),
                    None => cells.push_str(&empty_cell(
                        match_column,
                        row_number,
                        style.as_deref(),
                    )),
                }
                cells.push_str(&number_cell(
                    score_column,
                    row_number,
                    style.as_deref(),
                    write.score,
                ));
                cells
            } else {
                String::new()
            };

            if is_self_closing {
                let tag = open_tag.trim_end().trim_end_matches('/').trim_end();
                out.push_str(tag);
                out.push('>');
            } else {
                out.push_str(&new_open);
            }
            out.push_str(inner);
            out.push_str(&extra);
            out.push_str("</row>");
            cursor = row_end + "</row>".len();
        }
        out.push_str(&xml[cursor..]);

        let last_row = iterate_rows(&out)
            .iter()
            .map(|(row, _, _)| *row)
            .max()
            .unwrap_or(1);
        let out = widen_dimension(&out, last_column, last_row);
        let out = widen_auto_filter(&out, last_column, last_row);

        if let Some(item) = self.items.iter_mut().find(|item| item.name == sheet_path) {
            item.data = out.into_bytes();
        }

        Ok(WrittenColumns {
            match_letter: column_letter(match_column),
            score_letter: column_letter(score_column),
            reused,
        })
    }

    /// Write the workbook to `path` through a temporary file.
    pub fn save(&self, path: &Path) -> Result<(), XlsxError> {
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workbook.xlsx".to_string());
        let temp: PathBuf = directory.join(format!(".{file_name}.tmp"));

        let bytes = self.to_bytes()?;
        {
            let mut handle = fs::File::create(&temp).map_err(|error| {
                XlsxError::io(format!("cannot create {}: {error}", temp.display()))
            })?;
            handle.write_all(&bytes).map_err(|error| {
                XlsxError::io(format!("cannot write {}: {error}", temp.display()))
            })?;
            handle.sync_all().map_err(|error| {
                XlsxError::io(format!("cannot flush {}: {error}", temp.display()))
            })?;
        }
        fs::rename(&temp, path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            XlsxError::io(format!(
                "cannot replace {}: {error} (is the file open in Excel?)",
                path.display()
            ))
        })
    }

    fn to_bytes(&self) -> Result<Vec<u8>, XlsxError> {
        let mut buffer = Cursor::new(Vec::with_capacity(1024 * 1024));
        {
            let mut writer = ZipWriter::new(&mut buffer);
            for item in &self.items {
                let mut options = SimpleFileOptions::default().compression_method(item.method);
                if let Some(time) = item.last_modified {
                    options = options.last_modified_time(time);
                }
                writer.start_file(item.name.clone(), options).map_err(|error| {
                    XlsxError::io(format!("cannot write zip entry {}: {error}", item.name))
                })?;
                writer.write_all(&item.data).map_err(|error| {
                    XlsxError::io(format!("cannot write zip entry {}: {error}", item.name))
                })?;
            }
            writer
                .finish()
                .map_err(|error| XlsxError::io(format!("cannot finish zip: {error}")))?;
        }
        Ok(buffer.into_inner())
    }
}

/// A sheet element from `xl/workbook.xml`.
struct SheetRef {
    name: String,
    rel_id: String,
}

fn parse_sheets(workbook_xml: &str) -> Vec<SheetRef> {
    let mut sheets = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = workbook_xml[cursor..].find("<sheet ") {
        let start = cursor + offset;
        let Some(end) = workbook_xml[start..].find('>') else {
            break;
        };
        let tag = &workbook_xml[start..start + end];
        let name = tag_attr(tag, "name");
        let rel_id = tag_attr(tag, "r:id").or_else(|| tag_attr(tag, "id"));
        if let (Some(name), Some(rel_id)) = (name, rel_id) {
            sheets.push(SheetRef {
                name: decode_entities(&name),
                rel_id,
            });
        }
        cursor = start + end;
    }
    sheets
}

fn parse_rel_target(rels_xml: &str, rel_id: &str) -> Option<String> {
    let mut cursor = 0usize;
    while let Some(offset) = rels_xml[cursor..].find("<Relationship ") {
        let start = cursor + offset;
        let end = rels_xml[start..].find('>')?;
        let tag = &rels_xml[start..start + end];
        if tag_attr(tag, "Id").as_deref() == Some(rel_id) {
            return tag_attr(tag, "Target");
        }
        cursor = start + end;
    }
    None
}

/// Turn a relationship target such as `worksheets/sheet1.xml` into a zip path.
fn normalize_part_path(target: &str) -> String {
    let target = target.trim_start_matches('/');
    if target.starts_with("xl/") {
        target.to_string()
    } else {
        format!("xl/{target}")
    }
}

/// Read an attribute value from a start tag.
fn tag_attr(tag: &str, name: &str) -> Option<String> {
    let mut search = 0usize;
    while let Some(offset) = tag[search..].find(name) {
        let at = search + offset;
        let before_ok = at == 0
            || tag[..at]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_whitespace());
        let after = &tag[at + name.len()..];
        if before_ok && after.starts_with('=') {
            let rest = after[1..].trim_start();
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let value_start = 1;
                if let Some(end) = rest[value_start..].find(quote) {
                    return Some(decode_entities(&rest[value_start..value_start + end]));
                }
            }
        }
        search = at + name.len();
    }
    None
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let Some(semi) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|digits| {
                    if let Some(hex) = digits.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        digits.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => out.push(ch),
            None => out.push_str(&tail[..=semi]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

fn escape_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control characters are not representable in XML 1.0 text.
            ch if (ch as u32) < 0x20 && ch != '\t' && ch != '\n' && ch != '\r' => {}
            ch => out.push(ch),
        }
    }
    out
}

fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = xml[cursor..].find("<si") {
        let start = cursor + offset;
        let after = &xml[start + 3..];
        if !after.starts_with(['>', ' ', '/']) {
            cursor = start + 3;
            continue;
        }
        let Some(tag_end) = xml[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end;
        if xml[start..tag_end].trim_end().ends_with('/') {
            strings.push(String::new());
            cursor = tag_end + 1;
            continue;
        }
        let Some(close_offset) = xml[tag_end..].find("</si>") else {
            break;
        };
        let close = tag_end + close_offset;
        strings.push(collect_texts(&xml[tag_end + 1..close]));
        cursor = close + "</si>".len();
    }
    strings
}

/// Concatenate every `<t>` run inside a fragment.
fn collect_texts(fragment: &str) -> String {
    let mut out = String::new();
    let mut cursor = 0usize;
    while let Some(offset) = fragment[cursor..].find("<t") {
        let start = cursor + offset;
        let after = &fragment[start + 2..];
        if !after.starts_with(['>', ' ', '/']) {
            cursor = start + 2;
            continue;
        }
        let Some(tag_end_rel) = fragment[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end_rel;
        if fragment[start..tag_end].trim_end().ends_with('/') {
            cursor = tag_end + 1;
            continue;
        }
        let Some(close_rel) = fragment[tag_end..].find("</t>") else {
            break;
        };
        let close = tag_end + close_rel;
        out.push_str(&decode_entities(&fragment[tag_end + 1..close]));
        cursor = close + "</t>".len();
    }
    out
}

/// A parsed cell, keeping the raw XML so the caller can inspect style/type.
struct RawCell {
    column: u32,
    style: Option<String>,
    kind: Option<String>,
    body: String,
}

impl RawCell {
    /// Raw text content: `<v>` for typed cells, `<t>` runs for inline strings.
    fn text(&self) -> Option<String> {
        if let Some(open) = self.body.find("<v>") {
            let value_start = open + "<v>".len();
            if let Some(close) = self.body[value_start..].find("</v>") {
                return Some(decode_entities(
                    &self.body[value_start..value_start + close],
                ));
            }
        }
        if self.body.contains("<is") {
            return Some(collect_texts(&self.body));
        }
        None
    }
}

/// Parse the `<c>` cells inside a row fragment.
fn parse_cells(fragment: &str) -> Vec<RawCell> {
    let mut cells = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = fragment[cursor..].find("<c") {
        let start = cursor + offset;
        let after = &fragment[start + 2..];
        if !after.starts_with(['>', ' ', '/']) {
            cursor = start + 2;
            continue;
        }
        let Some(tag_end_rel) = fragment[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end_rel;
        let open_tag = &fragment[start..=tag_end];
        let reference = tag_attr(open_tag, "r").unwrap_or_default();
        let style = tag_attr(open_tag, "s");
        let kind = tag_attr(open_tag, "t");
        let column = column_of_reference(&reference).unwrap_or(0);

        if open_tag.trim_end().ends_with("/>") {
            cells.push(RawCell {
                column,
                style,
                kind,
                body: String::new(),
            });
            cursor = tag_end + 1;
            continue;
        }
        let Some(close_rel) = fragment[tag_end..].find("</c>") else {
            break;
        };
        let close = tag_end + close_rel;
        cells.push(RawCell {
            column,
            style,
            kind,
            body: fragment[tag_end + 1..close].to_string(),
        });
        cursor = close + "</c>".len();
    }
    cells
}

fn style_of(cells: &[RawCell], column: u32) -> Option<String> {
    cells
        .iter()
        .find(|cell| cell.column == column)
        .and_then(|cell| cell.style.clone())
}

/// Iterate `<row>` elements as `(row number, inner start, inner end)`.
fn iterate_rows(xml: &str) -> Vec<(u32, usize, usize)> {
    let mut rows = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = xml[cursor..].find("<row") {
        let start = cursor + offset;
        let after = &xml[start + 4..];
        if !after.starts_with(['>', ' ', '/']) {
            cursor = start + 4;
            continue;
        }
        let Some(tag_end_rel) = xml[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end_rel;
        if xml[start..tag_end].trim_end().ends_with('/') {
            if let Some(row) = tag_attr(&xml[start..=tag_end], "r").and_then(|r| r.parse().ok()) {
                rows.push((row, tag_end + 1, tag_end + 1));
            }
            cursor = tag_end + 1;
            continue;
        }
        let Some(close_rel) = xml[tag_end..].find("</row>") else {
            break;
        };
        let close = tag_end + close_rel;
        if let Some(row) = tag_attr(&xml[start..=tag_end], "r").and_then(|r| r.parse().ok()) {
            rows.push((row, tag_end + 1, close));
        }
        cursor = close + "</row>".len();
    }
    rows
}

/// Find one row, returning `(row number, inner start, inner end)`.
fn find_row(xml: &str, row: u32) -> Option<(u32, usize, usize)> {
    iterate_rows(xml)
        .into_iter()
        .find(|(number, _, _)| *number == row)
}

/// Bounds of a row's opening tag, given the inner start offset.
fn row_open_bounds(xml: &str, inner_start: usize) -> (usize, usize) {
    let before = &xml[..inner_start];
    let start = before.rfind("<row").expect("row open tag precedes inner");
    let end = xml[start..]
        .find('>')
        .expect("row open tag is terminated")
        + start
        + 1;
    (start, end)
}

/// Highest 1-based column index used anywhere in the worksheet.
fn last_used_column(xml: &str) -> u32 {
    let mut last = 0u32;
    for (_, inner_start, inner_end) in iterate_rows(xml) {
        for cell in parse_cells(&xml[inner_start..inner_end]) {
            last = last.max(cell.column);
        }
    }
    if last == 0 {
        // Fall back to the declared dimension, then to a sane minimum.
        let declared = xml.find("<dimension").and_then(|open| {
            let end = xml[open..].find('>')?;
            tag_attr(&xml[open..open + end + 1], "ref")
        });
        if let Some(reference) = declared {
            let end_ref = reference
                .split_once(':')
                .map(|(_, tail)| tail)
                .unwrap_or(&reference);
            if let Some(column) = column_of_reference(end_ref) {
                last = column;
            }
        }
    }
    last.max(1)
}

/// Rewrite `<dimension ref="..."/>` so it covers the new columns.
///
/// The end row is taken from the sheet's own rows rather than the existing
/// dimension, which may still be the placeholder `A1` written by whichever tool
/// produced the file.
fn widen_dimension(xml: &str, last_column: u32, last_row: u32) -> String {
    let Some(open) = xml.find("<dimension") else {
        return xml.to_string();
    };
    let Some(offset) = xml[open..].find('>') else {
        return xml.to_string();
    };
    let end = open + offset;
    let tag = &xml[open..=end];
    let replacement = format!("A1:{}{last_row}", column_letter(last_column));
    let mut out = String::with_capacity(xml.len() + 8);
    out.push_str(&xml[..open]);
    out.push_str(&replace_attr(tag, "ref", &replacement));
    out.push_str(&xml[end + 1..]);
    out
}

/// Rewrite `<autoFilter ref="...">` so the filter covers the new columns.
fn widen_auto_filter(xml: &str, last_column: u32, last_row: u32) -> String {
    let Some(open) = xml.find("<autoFilter") else {
        return xml.to_string();
    };
    let Some(offset) = xml[open..].find('>') else {
        return xml.to_string();
    };
    let end = open + offset;
    let tag = &xml[open..=end];
    let replacement = format!("A1:{}{last_row}", column_letter(last_column));
    let mut out = String::with_capacity(xml.len() + 8);
    out.push_str(&xml[..open]);
    out.push_str(&replace_attr(tag, "ref", &replacement));
    out.push_str(&xml[end + 1..]);
    out
}

fn replace_attr(tag: &str, name: &str, value: &str) -> String {
    let mut search = 0usize;
    while let Some(offset) = tag[search..].find(name) {
        let at = search + offset;
        let before_ok = at == 0
            || tag[..at]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_whitespace());
        let after = &tag[at + name.len()..];
        if before_ok && after.starts_with('=') {
            let rest = &after[1..];
            let leading = rest.len() - rest.trim_start().len();
            let trimmed = rest.trim_start();
            let quoted = trimmed
                .chars()
                .next()
                .filter(|ch| *ch == '"' || *ch == '\'')
                .and_then(|quote| trimmed[1..].find(quote).map(|end| (quote, end)));
            if let Some((_, end)) = quoted {
                // Skip `=`, any whitespace, and the opening quote, so only the
                // value itself is replaced and the quotes survive.
                let value_start = at + name.len() + 1 + leading + 1;
                let value_end = value_start + end;
                let mut out = String::with_capacity(tag.len() + value.len());
                out.push_str(&tag[..value_start]);
                out.push_str(value);
                out.push_str(&tag[value_end..]);
                return out;
            }
        }
        search = at + name.len();
    }
    tag.to_string()
}

/// Update `spans="1:N"` on a row's opening tag.
fn set_span(open_tag: String, last_column: u32) -> String {
    if !open_tag.contains("spans=") {
        return open_tag;
    }
    replace_attr(&open_tag, "spans", &format!("1:{last_column}"))
}

fn inline_cell(column: u32, row: u32, style: Option<&str>, text: &str) -> String {
    let letters = column_letter(column);
    let style = style
        .map(|value| format!(" s=\"{value}\""))
        .unwrap_or_default();
    let escaped = escape_entities(text);
    let preserve = if escaped.trim() != escaped {
        " xml:space=\"preserve\""
    } else {
        ""
    };
    format!(
        "<c r=\"{letters}{row}\"{style} t=\"inlineStr\"><is><t{preserve}>{escaped}</t></is></c>"
    )
}

fn empty_cell(column: u32, row: u32, style: Option<&str>) -> String {
    let letters = column_letter(column);
    let style = style
        .map(|value| format!(" s=\"{value}\""))
        .unwrap_or_default();
    format!("<c r=\"{letters}{row}\"{style}/>")
}

fn number_cell(column: u32, row: u32, style: Option<&str>, value: f64) -> String {
    let letters = column_letter(column);
    let style = style
        .map(|value| format!(" s=\"{value}\""))
        .unwrap_or_default();
    format!("<c r=\"{letters}{row}\"{style}><v>{value}</v></c>")
}

/// 1-based column index to spreadsheet letters (`1 -> A`, `27 -> AA`).
pub fn column_letter(mut column: u32) -> String {
    let mut letters = Vec::new();
    while column > 0 {
        let remainder = (column - 1) % 26;
        letters.push((b'A' + remainder as u8) as char);
        column = (column - 1) / 26;
    }
    letters.iter().rev().collect()
}

/// Spreadsheet letters to a 1-based column index.
pub fn column_of_reference(reference: &str) -> Option<u32> {
    let mut column = 0u32;
    let mut seen = false;
    for ch in reference.chars() {
        if !ch.is_ascii_alphabetic() {
            break;
        }
        seen = true;
        column = column * 26 + (ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
    }
    seen.then_some(column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_letters_round_trip() {
        assert_eq!(column_letter(1), "A");
        assert_eq!(column_letter(16), "P");
        assert_eq!(column_letter(17), "Q");
        assert_eq!(column_letter(26), "Z");
        assert_eq!(column_letter(27), "AA");
        assert_eq!(column_of_reference("A1"), Some(1));
        assert_eq!(column_of_reference("P2488"), Some(16));
        assert_eq!(column_of_reference("AA3"), Some(27));
        assert_eq!(column_of_reference("1"), None);
    }

    #[test]
    fn parses_shared_strings_with_rich_text() {
        let xml = r#"<sst><si><t>甲</t></si><si><r><t>乙</t></r><r><t>丙</t></r></si><si/></sst>"#;
        assert_eq!(parse_shared_strings(xml), vec!["甲", "乙丙", ""]);
    }

    #[test]
    fn parses_cells_with_types_and_empty_bodies() {
        let row = r#"<c r="A1" s="8" t="s"><v>326</v></c><c r="B1" s="8"/><c r="C1" t="inlineStr"><is><t>直写</t></is></c>"#;
        let cells = parse_cells(row);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].column, 1);
        assert_eq!(cells[0].kind.as_deref(), Some("s"));
        assert_eq!(cells[0].text().as_deref(), Some("326"));
        assert_eq!(cells[1].column, 2);
        assert_eq!(cells[1].text(), None);
        assert_eq!(cells[2].text().as_deref(), Some("直写"));
    }

    #[test]
    fn iterates_rows_including_self_closing() {
        let xml = r#"<sheetData><row r="1" spans="1:2"><c r="A1"><v>1</v></c></row><row r="2" spans="1:2"/><row r="3" spans="1:2"><c r="A3"><v>3</v></c></row></sheetData>"#;
        let rows = iterate_rows(xml);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.iter().map(|(r, _, _)| *r).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(rows[1].1, rows[1].2, "self-closing row has empty inner");
    }

    #[test]
    fn finds_last_used_column_from_cells_then_dimension() {
        let xml = r#"<dimension ref="A1:C9"/><sheetData><row r="1"><c r="A1"/><c r="C1"/></row></sheetData>"#;
        assert_eq!(last_used_column(xml), 3);
        let empty = r#"<dimension ref="A1:E9"/><sheetData><row r="1"/></sheetData>"#;
        assert_eq!(last_used_column(empty), 5);
    }

    #[test]
    fn widens_dimension_and_auto_filter() {
        let xml = r#"<dimension ref="A1:P2488"/><sheetData/><autoFilter ref="A1:P2488"/>"#;
        let wide = widen_auto_filter(&widen_dimension(xml, 18, 2488), 18, 2488);
        assert!(wide.contains(r#"<dimension ref="A1:R2488"/>"#), "{wide}");
        assert!(wide.contains(r#"<autoFilter ref="A1:R2488"/>"#), "{wide}");
    }

    #[test]
    fn widening_uses_real_row_count_not_a_placeholder_dimension() {
        // Tools commonly leave `<dimension ref="A1"/>` behind; the widened range
        // must still cover every data row.
        let xml = r#"<dimension ref="A1"/><sheetData><row r="1"><c r="A1"/></row><row r="9"><c r="A9"/></row></sheetData>"#;
        let wide = widen_dimension(xml, 3, 9);
        assert!(wide.contains(r#"<dimension ref="A1:C9"/>"#), "{wide}");
    }

    #[test]
    fn writes_cells_with_style_and_escaping() {
        assert_eq!(
            inline_cell(17, 2, Some("10"), "甲&乙"),
            r#"<c r="Q2" s="10" t="inlineStr"><is><t>甲&amp;乙</t></is></c>"#
        );
        assert_eq!(empty_cell(18, 2, None), r#"<c r="R2"/>"#);
        assert_eq!(number_cell(18, 2, Some("10"), 1.0), r#"<c r="R2" s="10"><v>1</v></c>"#);
    }

    #[test]
    fn decodes_and_escapes_entities() {
        assert_eq!(decode_entities("a&amp;b&lt;c"), "a&b<c");
        assert_eq!(escape_entities("a&b<c>\"d\""), "a&amp;b&lt;c&gt;&quot;d&quot;");
        assert_eq!(tag_attr(r#"<sheet name="9月压面" r:id="rId16"/>"#, "name").as_deref(), Some("9月压面"));
        assert_eq!(tag_attr(r#"<c r="D2" s="10" t="s">"#, "s").as_deref(), Some("10"));
        // "s" must not be matched inside a longer attribute name.
        assert_eq!(tag_attr(r#"<row spans="1:16" r="2">"#, "r").as_deref(), Some("2"));
    }

    #[test]
    fn normalizes_relationship_targets() {
        assert_eq!(normalize_part_path("worksheets/sheet14.xml"), "xl/worksheets/sheet14.xml");
        assert_eq!(normalize_part_path("/xl/worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(normalize_part_path("xl/worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
    }
}
