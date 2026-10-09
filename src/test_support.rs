//! Helpers for building small xlsx fixtures inside unit tests.
//!
//! The fixture writer emits a minimal but valid xlsx (shared strings, two
//! worksheets, merged cells) so tests exercise the real parsing and rewriting
//! paths without shipping binary test data.

use std::fs;
use std::io::Write;
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::ZipWriter;

/// One worksheet: a name plus its rows of plain text cells.
pub type SheetRows<'a> = (&'a str, &'a [&'a [&'a str]]);

/// Write a minimal valid workbook to `path`.
pub fn build_workbook(path: &Path, sheets: &[SheetRows<'_>]) {
    let mut shared: Vec<String> = Vec::new();
    let intern = |text: &str, shared: &mut Vec<String>| -> usize {
        if let Some(index) = shared.iter().position(|value| value == text) {
            return index;
        }
        shared.push(text.to_string());
        shared.len() - 1
    };

    // Materialize every cell as a shared string so the reader must resolve indices.
    let mut sheet_xml = Vec::new();
    for (_, rows) in sheets {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData>"#,
        );
        for (row_index, row) in rows.iter().enumerate() {
            let row_number = row_index + 1;
            xml.push_str(&format!(r#"<row r="{row_number}" spans="1:{len}">"#, len = row.len().max(1)));
            for (column_index, value) in row.iter().enumerate() {
                let letters = column_letter(column_index as u32 + 1);
                let index = intern(value, &mut shared);
                xml.push_str(&format!(
                    r#"<c r="{letters}{row_number}" s="1" t="s"><v>{index}</v></c>"#
                ));
            }
            xml.push_str("</row>");
        }
        xml.push_str("</sheetData></worksheet>");
        sheet_xml.push(xml);
    }

    let mut workbook_xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
    );
    let mut rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for (index, (name, _)) in sheets.iter().enumerate() {
        let rel_id = format!("rId{}", index + 1);
        workbook_xml.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{id}" r:id="{rel_id}"/>"#,
            id = index + 1
        ));
        rels.push_str(&format!(
            r#"<Relationship Id="{rel_id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{sheet}.xml"/>"#,
            sheet = index + 1
        ));
    }
    workbook_xml.push_str("</sheets></workbook>");
    rels.push_str("</Relationships>");

    let mut shared_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{}" uniqueCount="{}">"#,
        shared.len(),
        shared.len()
    );
    for value in &shared {
        shared_xml.push_str(&format!("<si><t>{}</t></si>", escape(value)));
    }
    shared_xml.push_str("</sst>");

    let content_types = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>{}<Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#,
        (1..=sheets.len())
            .map(|index| format!(
                r#"<Override PartName="/xl/worksheets/sheet{index}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
            ))
            .collect::<String>()
    );

    let file = fs::File::create(path).expect("create fixture workbook");
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    let mut put = |name: &str, data: String| {
        zip.start_file(name, options).expect("start zip entry");
        zip.write_all(data.as_bytes()).expect("write zip entry");
    };

    put("[Content_Types].xml", content_types);
    put(
        "_rels/.rels",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
    );
    put("xl/workbook.xml", workbook_xml);
    put("xl/_rels/workbook.xml.rels", rels);
    put("xl/sharedStrings.xml", shared_xml);
    for (index, xml) in sheet_xml.into_iter().enumerate() {
        put(&format!("xl/worksheets/sheet{}.xml", index + 1), xml);
    }
    zip.finish().expect("finish fixture workbook");
}

/// Read every `<t>`-ish value back out of a sheet for assertions.
pub fn read_all_text(path: &Path, sheet_entry: &str) -> String {
    let file = fs::File::open(path).expect("open workbook");
    let mut archive = zip::ZipArchive::new(file).expect("read zip");
    let mut entry = archive.by_name(sheet_entry).expect("sheet entry");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut entry, &mut text).expect("read sheet xml");
    text
}

/// Zip entry names present in a workbook, excluding directory markers.
pub fn entry_names(path: &Path) -> Vec<String> {
    let file = fs::File::open(path).expect("open workbook");
    let archive = zip::ZipArchive::new(file).expect("read zip");
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|name| !name.ends_with('/'))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn column_letter(mut column: u32) -> String {
    let mut letters = Vec::new();
    while column > 0 {
        letters.push((b'A' + ((column - 1) % 26) as u8) as char);
        column = (column - 1) / 26;
    }
    letters.iter().rev().collect()
}
