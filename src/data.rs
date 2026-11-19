//! Spreadsheet data source: Excel (xlsx/xlsm/xlsb/xls), OpenDocument (ods), CSV/TSV.
//! Each selected row prints one label; its cells fill `{Column}` placeholders.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

pub const EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xlsb", "xls", "ods", "csv", "tsv", "txt"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataSource {
    pub path: String,
    /// Sheet name; empty or unknown = first sheet.
    pub sheet: String,
    /// The first non-empty row holds the column names.
    pub header: bool,
}

impl Default for DataSource {
    fn default() -> Self {
        Self { path: String::new(), sheet: String::new(), header: true }
    }
}

impl DataSource {
    pub fn file_name(&self) -> String {
        Path::new(&self.path).file_name().map_or_else(|| self.path.clone(), |f| f.to_string_lossy().into_owned())
    }
}

/// One row plus the column names, for `{Column}` lookups.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Record {
    pub headers: Arc<Vec<String>>,
    pub values: Vec<String>,
}

impl Record {
    /// Value of a column by name (exact match first, then case-insensitive).
    pub fn get(&self, key: &str) -> Option<&str> {
        let k = key.trim();
        let i = self
            .headers
            .iter()
            .position(|h| h == k)
            .or_else(|| self.headers.iter().position(|h| h.to_lowercase() == k.to_lowercase()))?;
        Some(self.values.get(i).map_or("", String::as_str))
    }
}

pub struct Table {
    pub sheets: Vec<String>,
    /// The sheet that was loaded.
    pub sheet: String,
    pub headers: Arc<Vec<String>>,
    pub rows: Vec<Vec<String>>,
    /// Row number as shown in the spreadsheet (1-based), per row.
    pub row_numbers: Vec<usize>,
    pub mtime: Option<SystemTime>,
}

impl Table {
    pub fn record(&self, i: usize) -> Option<Arc<Record>> {
        let values = self.rows.get(i)?.clone();
        Some(Arc::new(Record { headers: self.headers.clone(), values }))
    }
}

pub fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn load(src: &DataSource) -> Result<Table> {
    let path = Path::new(&src.path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let (sheets, sheet, grid, first_row) = if matches!(ext.as_str(), "csv" | "tsv" | "txt") {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        (vec![], String::new(), parse_csv(text.trim_start_matches('\u{feff}')), 0)
    } else {
        read_sheet(path, &src.sheet)?
    };
    Ok(build(grid, first_row, src.header, sheets, sheet, mtime(&src.path)))
}

type Sheet = (Vec<String>, String, Vec<Vec<String>>, usize);

fn read_sheet(path: &Path, want: &str) -> Result<Sheet> {
    use calamine::{Reader, open_workbook_auto};
    let mut wb = open_workbook_auto(path).map_err(|e| anyhow!("{e}"))?;
    let sheets = wb.sheet_names();
    let name = match sheets.iter().find(|s| *s == want) {
        Some(s) => s.clone(),
        None => sheets.first().cloned().ok_or_else(|| anyhow!("the file has no sheets"))?,
    };
    let range = wb.worksheet_range(&name).map_err(|e| anyhow!("{e}"))?;
    let first_row = range.start().map_or(0, |(r, _)| r as usize);
    let grid = range.rows().map(|r| r.iter().map(cell).collect()).collect();
    Ok((sheets, name, grid, first_row))
}

fn cell(d: &calamine::Data) -> String {
    use calamine::Data;
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => number(*f),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Data::DateTime(dt) => excel_date(dt.as_f64(), dt.is_datetime()),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#{e}"),
    }
}

/// Excel stores 1001 as 1001.0 and 0.1+0.2 as 0.30000000000000004; print what Excel shows.
fn number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        return format!("{}", f as i64);
    }
    let s = format!("{f:.10}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Excel serial date (days since 1899-12-30) as ISO date, plus time if it has one.
fn excel_date(serial: f64, is_datetime: bool) -> String {
    let secs = (serial.fract() * 86_400.0).round() as i64;
    let (h, m) = (secs / 3600, secs / 60 % 60);
    if !is_datetime {
        return format!("{h}:{m:02}"); // duration
    }
    let days = serial.trunc() as i64;
    let base = chrono::NaiveDate::from_ymd_opt(1899, 12, 30).unwrap();
    let date = base + chrono::Duration::days(days);
    match (days, secs) {
        (0, _) => format!("{h:02}:{m:02}"),
        (_, 0) => date.format("%Y-%m-%d").to_string(),
        _ => format!("{} {h:02}:{m:02}", date.format("%Y-%m-%d")),
    }
}

/// RFC 4180-style CSV (quotes, doubled quotes, newlines in quotes). The delimiter
/// (`,` `;` or tab) is whichever appears most in the first line.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let first = text.lines().next().unwrap_or("");
    let delim = [',', ';', '\t'].into_iter().max_by_key(|d| first.matches(*d).count()).unwrap_or(',');
    let (mut rows, mut row, mut field, mut quoted) = (Vec::new(), Vec::new(), String::new(), false);
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if quoted {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    field.push('"');
                    it.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c if c == delim => row.push(std::mem::take(&mut field)),
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

pub fn col_letter(mut i: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

fn build(
    grid: Vec<Vec<String>>,
    first_row: usize,
    header: bool,
    sheets: Vec<String>,
    sheet: String,
    mtime: Option<SystemTime>,
) -> Table {
    let mut rows: Vec<(usize, Vec<String>)> = grid
        .into_iter()
        .enumerate()
        .map(|(i, r)| (first_row + i + 1, r))
        .filter(|(_, r)| r.iter().any(|c| !c.trim().is_empty()))
        .collect();
    let width = rows.iter().filter_map(|(_, r)| r.iter().rposition(|c| !c.trim().is_empty())).max().map_or(0, |p| p + 1);
    for (_, r) in &mut rows {
        r.resize(width, String::new());
    }
    let mut headers: Vec<String> = if header && !rows.is_empty() {
        let (_, h) = rows.remove(0);
        h.into_iter()
            .enumerate()
            .map(|(i, s)| {
                let s = s.replace(['{', '}'], "").split_whitespace().collect::<Vec<_>>().join(" ");
                if s.is_empty() { col_letter(i) } else { s }
            })
            .collect()
    } else {
        (0..width).map(col_letter).collect()
    };
    // Duplicate names get " 2", " 3"… so every column can be addressed.
    for i in 1..headers.len() {
        let mut k = 2;
        let base = headers[i].clone();
        while headers[..i].contains(&headers[i]) {
            headers[i] = format!("{base} {k}");
            k += 1;
        }
    }
    let (row_numbers, rows) = rows.into_iter().unzip();
    Table { sheets, sheet, headers: Arc::new(headers), rows, row_numbers, mtime }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_delimiters_and_headers() {
        let g = parse_csv("Part;Value;Note\r\nR1;10k;\"say \"\"hi\"\"; ok\"\n\nC1;100n;\"two\nlines\"\n");
        let t = build(g, 0, true, vec![], String::new(), None);
        assert_eq!(*t.headers, vec!["Part", "Value", "Note"]);
        assert_eq!(t.rows[0], vec!["R1", "10k", "say \"hi\"; ok"]);
        assert_eq!(t.rows[1][2], "two\nlines");
        assert_eq!(t.row_numbers, vec![2, 4]);
        let r = t.record(1).unwrap();
        assert_eq!(r.get("value"), Some("100n"));
        assert_eq!(r.get("Missing"), None);
    }

    #[test]
    fn letters_dupes_and_numbers() {
        assert_eq!((col_letter(0), col_letter(25), col_letter(26), col_letter(701)), ("A".into(), "Z".into(), "AA".into(), "ZZ".into()));
        let t = build(vec![vec!["X".into(), "".into(), "X".into()], vec!["1".into(), "2".into(), "3".into()]], 4, true, vec![], String::new(), None);
        assert_eq!(*t.headers, vec!["X", "B", "X 2"]);
        assert_eq!(t.row_numbers, vec![6]);
        assert_eq!((number(1001.0), number(0.1 + 0.2), number(2.5)), ("1001".into(), "0.3".into(), "2.5".into()));
        assert_eq!(excel_date(45000.0, true), "2023-03-15");
        assert_eq!(excel_date(45000.5, true), "2023-03-15 12:00");
    }

    #[test]
    fn reads_example_xlsx() {
        let src = DataSource { path: concat!(env!("CARGO_MANIFEST_DIR"), "/examples/parts.xlsx").into(), ..Default::default() };
        let t = load(&src).unwrap();
        assert_eq!(t.headers[0], "Part");
        assert!(t.rows.len() >= 5, "{} rows", t.rows.len());
        assert_eq!(t.record(0).unwrap().get("Qty"), Some("100"));
        assert!(t.rows.iter().any(|r| r.iter().any(|c| c.len() == 10 && c.starts_with("20"))), "a date column formats as ISO");
    }
}
