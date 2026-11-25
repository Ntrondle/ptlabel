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
    /// Use a header row (or header column when `by_columns`) for field names.
    pub header: bool,
    /// Specific 1-based sheet row (or 1-based column when `by_columns`) holding the field names.
    /// `None` auto-detects the header row (skipping sparse title/note rows above a table).
    pub header_row: Option<usize>,
    /// Propagate top-left values across Excel merged cells so every row in a group gets its label.
    pub fill_merged: bool,
    /// When true, each column is one label (transposed); otherwise each row is one label.
    pub by_columns: bool,
}

impl Default for DataSource {
    fn default() -> Self {
        Self {
            path: String::new(),
            sheet: String::new(),
            header: true,
            header_row: None,
            fill_merged: true,
            by_columns: false,
        }
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
    /// Parallel to `rows`: true when the cell's value was inherited from a merged range.
    pub merged: Vec<Vec<bool>>,
    /// Number of merged regions found in the worksheet (0 for CSV/ODS or unmerged sheets).
    pub merged_count: usize,
    /// 1-based sheet row (or 1-based column when `by_columns`) used as the header, if `header` is true.
    #[cfg_attr(not(test), allow(dead_code))]
    pub header_index: Option<usize>,
    /// Row number ("2", "3", …) or column letter ("A", "B", …) in the spreadsheet, per record.
    pub row_labels: Vec<String>,
    pub mtime: Option<SystemTime>,
}

impl Table {
    pub fn record(&self, i: usize) -> Option<Arc<Record>> {
        let values = self.rows.get(i)?.clone();
        Some(Arc::new(Record { headers: self.headers.clone(), values }))
    }

    pub fn is_merged(&self, row: usize, col: usize) -> bool {
        self.merged.get(row).and_then(|r| r.get(col)).copied().unwrap_or(false)
    }
}

/// Inclusive 0-based rectangle in sheet coordinates: `((r0, c0), (r1, c1))`.
type MergeRect = ((usize, usize), (usize, usize));

pub fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn load(src: &DataSource) -> Result<Table> {
    let path = Path::new(&src.path);
    // Stat before reading: a save that lands during the read leaves an older mtime, so it reloads again.
    let modified = mtime(&src.path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let (sheets, sheet, grid, first, merges) = if matches!(ext.as_str(), "csv" | "tsv" | "txt") {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        (vec![], String::new(), parse_csv(&decode(&bytes)), (0, 0), vec![])
    } else {
        read_sheet(path, &src.sheet)?
    };
    Ok(build(
        grid,
        first,
        &merges,
        src.header,
        src.header_row,
        src.fill_merged,
        src.by_columns,
        sheets,
        sheet,
        modified,
    ))
}

/// (sheet names, loaded sheet, cells, (row, column) of the first cell, merged cell ranges).
type Sheet = (Vec<String>, String, Vec<Vec<String>>, (usize, usize), Vec<MergeRect>);

fn read_sheet(path: &Path, want: &str) -> Result<Sheet> {
    use calamine::{Reader, Sheets, open_workbook_auto};
    let mut wb = open_workbook_auto(path).map_err(|e| anyhow!("{e}"))?;
    let sheets = wb.sheet_names();
    let name = match sheets.iter().find(|s| *s == want) {
        Some(s) => s.clone(),
        None => sheets.first().cloned().ok_or_else(|| anyhow!("the file has no sheets"))?,
    };
    let range = wb.worksheet_range(&name).map_err(|e| anyhow!("{e}"))?;
    let first = range.start().map_or((0, 0), |(r, c)| (r as usize, c as usize));
    let grid = range.rows().map(|r| r.iter().map(cell).collect()).collect();
    let dims = match &mut wb {
        Sheets::Xlsx(x) => x.merge_cells_by_sheet_name(&name).unwrap_or_default(),
        Sheets::Xls(x) => x.merge_cells_by_sheet_name(&name).unwrap_or_default(),
        _ => vec![],
    };
    let merges = dims
        .into_iter()
        .map(|d| ((d.start.0 as usize, d.start.1 as usize), (d.end.0 as usize, d.end.1 as usize)))
        .collect();
    Ok((sheets, name, grid, first, merges))
}

fn cell(d: &calamine::Data) -> String {
    use calamine::Data;
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => number(*f),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Data::DateTime(dt) => excel_date(dt),
        Data::DateTimeIso(s) => iso_date(s),
        Data::DurationIso(s) => iso_duration(s),
        Data::Error(e) => e.to_string(), // already "#DIV/0!" etc.
    }
}

/// Excel stores 1001 as 1001.0 and 0.1+0.2 as 0.30000000000000004; print what Excel shows
/// (whole numbers exactly, others to Excel's 15 significant digits).
fn number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 9e18 {
        return format!("{}", f as i64);
    }
    let r: f64 = format!("{f:.14e}").parse().unwrap_or(f);
    format!("{r}")
}

/// Excel date/time (1900 or 1904 epoch) as ISO date, plus the time if it has one.
fn excel_date(dt: &calamine::ExcelDateTime) -> String {
    let serial = dt.as_f64();
    if !dt.is_datetime() {
        return clock((serial * 86_400.0).round() as i64); // duration: [h]:mm
    }
    let (y, mo, d, h, mi, s, _) = dt.to_ymd_hms_milli();
    let time = if s == 0 { format!("{h:02}:{mi:02}") } else { format!("{h:02}:{mi:02}:{s:02}") };
    match (serial.trunc() == 0.0, (h, mi, s)) {
        (true, _) => time,
        (_, (0, 0, 0)) => format!("{y:04}-{mo:02}-{d:02}"),
        _ => format!("{y:04}-{mo:02}-{d:02} {time}"),
    }
}

/// Seconds as h:mm (h:mm:ss if it has seconds); hours are not wrapped at 24.
fn clock(secs: i64) -> String {
    let (sign, t) = (if secs < 0 { "-" } else { "" }, secs.unsigned_abs());
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if s == 0 { format!("{sign}{h}:{m:02}") } else { format!("{sign}{h}:{m:02}:{s:02}") }
}

/// ODS date-value ("2023-03-15" or "2023-03-15T12:30:00") in the same form as `excel_date`.
fn iso_date(s: &str) -> String {
    let Some((d, t)) = s.split_once('T') else { return s.to_string() };
    let t = t.get(..8).unwrap_or(t); // hh:mm:ss without fraction or zone
    match t.strip_suffix(":00") {
        Some("00:00") => d.to_string(),
        Some(hm) if hm.len() == 5 => format!("{d} {hm}"),
        _ => format!("{d} {t}"),
    }
}

/// ODS time-value ("PT36H30M00S") as h:mm like an Excel duration; other forms verbatim.
fn iso_duration(s: &str) -> String {
    let (neg, body) = s.strip_prefix('-').map_or((false, s), |b| (true, b));
    let Some(body) = body.strip_prefix("PT") else { return s.to_string() };
    let (mut secs, mut num) = (0.0f64, String::new());
    for c in body.chars() {
        let unit = match c {
            'H' => 3600.0,
            'M' => 60.0,
            'S' => 1.0,
            _ => {
                num.push(c);
                continue;
            }
        };
        let Ok(v) = num.parse::<f64>() else { return s.to_string() };
        secs += v * unit;
        num.clear();
    }
    if !num.is_empty() {
        return s.to_string();
    }
    let secs = secs.round() as i64;
    clock(if neg { -secs } else { secs })
}

/// Windows-1252 0x80..0x9F (the rest of the byte range is Latin-1).
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// CSV bytes as text: UTF-8 (BOM optional), UTF-16 with BOM (Excel "Unicode Text"),
/// anything else as Windows-1252 (Excel's legacy "CSV" on Windows).
fn decode(b: &[u8]) -> String {
    let utf16 = |unit: fn([u8; 2]) -> u16| -> String {
        let units = b[2..].chunks_exact(2).map(|p| unit([p[0], p[1]]));
        char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
    };
    match b {
        [0xFF, 0xFE, ..] => utf16(u16::from_le_bytes),
        [0xFE, 0xFF, ..] => utf16(u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => b.iter().map(|&c| if (0x80..0xA0).contains(&c) { CP1252[c as usize - 0x80] } else { c as char }).collect(),
        },
    }
}

/// RFC 4180-style CSV (quotes, doubled quotes, newlines in quotes; CRLF, LF or CR line
/// ends). The delimiter (`,` `;` or tab) is whichever appears most outside quotes in the
/// first non-empty line.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let count = |d: char| first.split('"').step_by(2).map(|s| s.matches(d).count()).sum::<usize>();
    let delim = [',', ';', '\t'].into_iter().max_by_key(|d| count(*d)).unwrap_or(',');
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

/// True when `s` looks like a numeric literal rather than a text header name.
fn looks_numeric(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.replace(',', ".").parse::<f64>().is_ok()
}

/// Pick which entry of `records` holds the headers:
/// - Explicit `header_row` selects the entry with that 1-based sheet index (fallback: 0).
/// - `None` (Auto) picks row 0 unless row 0 is a sparse banner/title (≤ 2 filled cells or less than
///   half as wide as a header-like text row below it) with data rows underneath.
fn pick_header_pos(records: &[(usize, Vec<String>, Vec<bool>)], header_row: Option<usize>) -> usize {
    if records.is_empty() {
        return 0;
    }
    if let Some(want) = header_row
        && let Some(pos) = records.iter().position(|(n, _, _)| *n == want)
    {
        return pos;
    }
    let stats = |r: &[String]| -> (usize, usize) {
        let mut filled = 0;
        let mut text = 0;
        for c in r {
            let t = c.trim();
            if !t.is_empty() {
                filled += 1;
                if !looks_numeric(t) {
                    text += 1;
                }
            }
        }
        (filled, text)
    };
    let (f0, _) = stats(&records[0].1);
    let scan_end = records.len().saturating_sub(1).min(10);
    for (k, (_, cells, _)) in records.iter().enumerate().take(scan_end).skip(1) {
        let (fk, tk) = stats(cells);
        // Header row should have ≥3 filled cells, be mostly text, and at least 2× wider than a sparse banner above it.
        if fk >= 3 && tk * 5 >= fk * 3 && f0 * 2 <= fk {
            return k;
        }
    }
    0
}

#[allow(clippy::too_many_arguments)]
fn build(
    mut grid: Vec<Vec<String>>,
    (first_row, mut first_col): (usize, usize),
    merges: &[MergeRect],
    header: bool,
    header_row: Option<usize>,
    fill_merged: bool,
    by_columns: bool,
    sheets: Vec<String>,
    sheet: String,
    mtime: Option<SystemTime>,
) -> Table {
    let rows_len = grid.len();
    let max_cols = grid.iter().map(Vec::len).max().unwrap_or(0);
    for r in &mut grid {
        r.resize(max_cols, String::new());
    }
    let mut merged_grid: Vec<Vec<bool>> = vec![vec![false; max_cols]; rows_len];
    if fill_merged && rows_len > 0 && max_cols > 0 {
        for &((r0, c0), (r1, c1)) in merges {
            if r0 < first_row || c0 < first_col {
                continue;
            }
            let (lr0, lc0) = (r0 - first_row, c0 - first_col);
            if lr0 >= rows_len || lc0 >= max_cols {
                continue;
            }
            let val = grid[lr0][lc0].clone();
            if val.trim().is_empty() {
                continue;
            }
            let lr1 = r1.saturating_sub(first_row).min(rows_len - 1);
            let lc1 = c1.saturating_sub(first_col).min(max_cols - 1);
            for r in lr0..=lr1 {
                for c in lc0..=lc1 {
                    if (r, c) != (lr0, lc0) && grid[r][c].trim().is_empty() {
                        grid[r][c] = val.clone();
                        merged_grid[r][c] = true;
                    }
                }
            }
        }
    }

    let mut non_empty_rows: Vec<(usize, Vec<String>, Vec<bool>)> = grid
        .into_iter()
        .zip(merged_grid)
        .enumerate()
        .map(|(i, (r, m))| (first_row + i + 1, r, m))
        .filter(|(_, r, _)| r.iter().any(|c| !c.trim().is_empty()))
        .collect();

    // Trim completely empty leading & trailing columns while preserving sheet column letters.
    let min_col = non_empty_rows
        .iter()
        .filter_map(|(_, r, _)| r.iter().position(|c| !c.trim().is_empty()))
        .min()
        .unwrap_or(0);
    let max_col = non_empty_rows
        .iter()
        .filter_map(|(_, r, _)| r.iter().rposition(|c| !c.trim().is_empty()))
        .max()
        .map_or(0, |p| p + 1);
    let width = max_col.saturating_sub(min_col);
    first_col += min_col;
    for (_, r, m) in &mut non_empty_rows {
        if min_col > 0 && min_col <= r.len() {
            r.drain(..min_col);
            m.drain(..min_col);
        }
        r.truncate(width);
        m.truncate(width);
        r.resize(width, String::new());
        m.resize(width, false);
    }

    let sanitize = |s: String, fallback: String| -> String {
        let s = s.replace(['{', '}'], "").split_whitespace().collect::<Vec<_>>().join(" ");
        if s.is_empty() { fallback } else { s }
    };

    type BuiltRow = (String, Vec<String>, Vec<bool>);
    let (records, mut headers, header_index): (Vec<BuiltRow>, Vec<String>, Option<usize>) = if by_columns {
        let row_nums: Vec<usize> = non_empty_rows.iter().map(|(n, _, _)| *n).collect();
        let mut cols: Vec<(usize, Vec<String>, Vec<bool>)> = (0..width)
            .map(|c| {
                let col: Vec<String> = non_empty_rows.iter().map(|(_, r, _)| r[c].clone()).collect();
                let mcol: Vec<bool> = non_empty_rows.iter().map(|(_, _, m)| m[c]).collect();
                (first_col + c + 1, col, mcol)
            })
            .filter(|(_, col, _)| col.iter().any(|v| !v.trim().is_empty()))
            .collect();
        if header && !cols.is_empty() {
            let pos = pick_header_pos(&cols, header_row);
            let (hdr_n, h, _) = cols.remove(pos);
            cols.drain(..pos);
            let headers = h
                .into_iter()
                .enumerate()
                .map(|(i, s)| sanitize(s, format!("Row {}", row_nums[i])))
                .collect();
            let recs = cols.into_iter().map(|(n, c, m)| (col_letter(n - 1), c, m)).collect();
            (recs, headers, Some(hdr_n))
        } else {
            let headers = row_nums.iter().map(|n| format!("Row {n}")).collect();
            let recs = cols.into_iter().map(|(n, c, m)| (col_letter(n - 1), c, m)).collect();
            (recs, headers, None)
        }
    } else if header && !non_empty_rows.is_empty() {
        let pos = pick_header_pos(&non_empty_rows, header_row);
        let (hdr_n, h, _) = non_empty_rows.remove(pos);
        // Skip banner/title rows above the header row.
        non_empty_rows.drain(..pos);
        let headers = h
            .into_iter()
            .enumerate()
            .map(|(i, s)| sanitize(s, col_letter(first_col + i)))
            .collect();
        let recs = non_empty_rows.into_iter().map(|(n, r, m)| (n.to_string(), r, m)).collect();
        (recs, headers, Some(hdr_n))
    } else {
        let headers = (0..width).map(|i| col_letter(first_col + i)).collect();
        let recs = non_empty_rows.into_iter().map(|(n, r, m)| (n.to_string(), r, m)).collect();
        (recs, headers, None)
    };

    // Duplicate names get " 2", " 3"… so every field can be addressed.
    for i in 1..headers.len() {
        let mut k = 2;
        let base = headers[i].clone();
        while headers[..i].contains(&headers[i]) {
            headers[i] = format!("{base} {k}");
            k += 1;
        }
    }
    let mut row_labels = Vec::with_capacity(records.len());
    let mut rows = Vec::with_capacity(records.len());
    let mut merged = Vec::with_capacity(records.len());
    for (lbl, r, m) in records {
        row_labels.push(lbl);
        rows.push(r);
        merged.push(m);
    }
    Table {
        sheets,
        sheet,
        headers: Arc::new(headers),
        rows,
        merged,
        merged_count: merges.len(),
        header_index,
        row_labels,
        mtime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_simple(grid: Vec<Vec<String>>, first: (usize, usize), header: bool, by_columns: bool) -> Table {
        build(grid, first, &[], header, None, true, by_columns, vec![], String::new(), None)
    }

    #[test]
    fn csv_quotes_delimiters_and_headers() {
        let g = parse_csv("Part;Value;Note\r\nR1;10k;\"say \"\"hi\"\"; ok\"\n\nC1;100n;\"two\nlines\"\n");
        let t = build_simple(g, (0, 0), true, false);
        assert_eq!(*t.headers, vec!["Part", "Value", "Note"]);
        assert_eq!(t.rows[0], vec!["R1", "10k", "say \"hi\"; ok"]);
        assert_eq!(t.rows[1][2], "two\nlines");
        assert_eq!(t.row_labels, vec!["2", "4"]);
        let r = t.record(1).unwrap();
        assert_eq!(r.get("value"), Some("100n"));
        assert_eq!(r.get("Missing"), None);
    }

    #[test]
    fn letters_dupes_and_numbers() {
        assert_eq!((col_letter(0), col_letter(25), col_letter(26), col_letter(701)), ("A".into(), "Z".into(), "AA".into(), "ZZ".into()));
        let t = build_simple(vec![vec!["X".into(), "".into(), "X".into()], vec!["1".into(), "2".into(), "3".into()]], (4, 0), true, false);
        assert_eq!(*t.headers, vec!["X", "B", "X 2"]);
        assert_eq!(t.row_labels, vec!["6"]);
        assert_eq!((number(1001.0), number(0.1 + 0.2), number(2.5)), ("1001".into(), "0.3".into(), "2.5".into()));
        let dt = |v, ty, is_1904| excel_date(&calamine::ExcelDateTime::new(v, ty, is_1904));
        use calamine::ExcelDateTimeType::DateTime;
        assert_eq!(dt(45000.0, DateTime, false), "2023-03-15");
        assert_eq!(dt(45000.5, DateTime, false), "2023-03-15 12:00");
    }

    #[test]
    fn numbers_keep_precision_without_noise() {
        assert_eq!(number(123456789.123), "123456789.123"); // was 123456789.1229999959
        assert_eq!(number(1.5e-12), "0.0000000000015"); // was 0
        assert_eq!(number(-1e-12), "-0.000000000001"); // was -0
        assert_eq!(number(1234567890123456.0), "1234567890123456");
        assert_eq!(number(4006381333931.0), "4006381333931");
        assert_eq!(number(-5.0), "-5");
        assert_eq!(number(10.0 * 1.19), "11.9");
    }

    #[test]
    fn dates_times_and_errors() {
        use calamine::{CellErrorType, Data, ExcelDateTime, ExcelDateTimeType::*};
        let dt = |v, ty, is_1904| excel_date(&ExcelDateTime::new(v, ty, is_1904));
        assert_eq!(dt(45000.0 - 1462.0, DateTime, true), "2023-03-15"); // 1904 epoch
        assert_eq!(dt(45000.999_999_999_99, DateTime, false), "2023-03-16"); // rounds to midnight, not 24:00
        assert_eq!(dt(0.75, DateTime, false), "18:00");
        assert_eq!(dt(45000.5 + 15.0 / 86400.0, DateTime, false), "2023-03-15 12:00:15");
        assert_eq!(dt(1.5, TimeDelta, false), "36:00"); // whole days were dropped
        assert_eq!(dt(0.25, TimeDelta, false), "6:00");
        assert_eq!(cell(&Data::Error(CellErrorType::Div0)), "#DIV/0!"); // was ##DIV/0!
        assert_eq!(iso_date("2023-03-15"), "2023-03-15");
        assert_eq!(iso_date("2023-03-15T00:00:00"), "2023-03-15");
        assert_eq!(iso_date("2023-03-15T12:30:00.5"), "2023-03-15 12:30");
        assert_eq!(iso_date("2023-03-15T12:30:15"), "2023-03-15 12:30:15");
        assert_eq!(iso_duration("PT36H30M00S"), "36:30");
        assert_eq!(iso_duration("PT12H05M30S"), "12:05:30");
        assert_eq!(iso_duration("-PT1H"), "-1:00");
        assert_eq!(iso_duration("P1DT2H"), "P1DT2H");
    }

    #[test]
    fn csv_encodings_line_ends_and_delimiters() {
        assert_eq!(decode(b"\xEF\xBB\xBFPart;Qty"), "Part;Qty");
        assert_eq!(decode(b"M\xfcller;\x80 5"), "Müller;€ 5"); // Windows-1252, was M\u{fffd}ller
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("Grüße\t😀".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode(&utf16), "Grüße\t😀");
        assert_eq!(parse_csv("A;B\r1;2\r3;4"), vec![vec!["A", "B"], vec!["1", "2"], vec!["3", "4"]]); // CR-only (classic Mac)
        assert_eq!(parse_csv("A,B\r\n\"x\r\ny\",2\r\n")[1][0], "x\ny");
        assert_eq!(parse_csv("\"Name; full\",Qty\n\"a;b\",1")[1], vec!["a;b", "1"]); // ';' inside quotes doesn't count
        assert_eq!(parse_csv("\n\nPart;Qty\nR1;5")[3], vec!["R1", "5"]); // leading blank lines
    }

    #[test]
    fn column_letters_follow_the_sheet_offset() {
        let g = || vec![vec!["".to_string(), "Qty".into()], vec!["R1".into(), "5".into()]];
        assert_eq!(*build_simple(g(), (2, 2), true, false).headers, vec!["C", "Qty"]);
        assert_eq!(*build_simple(g(), (2, 2), false, false).headers, vec!["C", "D"]);
    }

    #[test]
    fn by_columns_transposes_sheet_with_or_without_header() {
        let g = || parse_csv("Field;Tube 1;Tube 2\nConc;10 mM;20 mM\n;extra;note\n");
        let with_hdr = build_simple(g(), (0, 0), true, true);
        assert_eq!(*with_hdr.headers, vec!["Field", "Conc", "Row 3"]);
        assert_eq!(with_hdr.row_labels, vec!["B", "C"]);
        assert_eq!(with_hdr.rows[0], vec!["Tube 1", "10 mM", "extra"]);
        assert_eq!(with_hdr.rows[1], vec!["Tube 2", "20 mM", "note"]);
        assert_eq!(with_hdr.record(1).unwrap().get("Conc"), Some("20 mM"));

        let no_hdr = build_simple(g(), (1, 2), false, true);
        assert_eq!(*no_hdr.headers, vec!["Row 2", "Row 3", "Row 4"]);
        assert_eq!(no_hdr.row_labels, vec!["C", "D", "E"]);
        assert_eq!(no_hdr.rows[0], vec!["Field", "Conc", ""]);
    }

    #[test]
    fn fills_merged_cells_and_auto_detects_header_below_banner() {
        let s = |v: &str| v.to_string();
        // Mirrors lab sheets: empty col A, banner on row 2 (H2:I2), real header on row 3, merged groups in B & C.
        let grid = vec![
            vec![s(""), s(""), s(""), s(""), s("60 ng/uL")],
            vec![s(""), s("Cell line"), s("Condition"), s("Sample ID"), s("RNA")],
            vec![s(""), s("ADAR1"), s("NT (+Dox)"), s("Sample 1.1"), s("311.92")],
            vec![s(""), s(""), s(""), s("Sample 1.2"), s("186.32")],
            vec![s(""), s(""), s("TRPM1\nNeg"), s("Sample 2.1"), s("326.76")],
        ];
        // Sheet starts at row 1 (Excel row 2), col 0 (Excel col A).
        let merges = vec![((3, 1), (5, 1)), ((3, 2), (4, 2))];
        let t = build(grid, (1, 0), &merges, true, None, true, false, vec![], String::new(), None);
        assert_eq!(t.header_index, Some(3));
        assert_eq!(*t.headers, vec!["Cell line", "Condition", "Sample ID", "RNA"]);
        assert_eq!(t.row_labels, vec!["4", "5", "6"]);
        assert_eq!(t.rows[0], vec!["ADAR1", "NT (+Dox)", "Sample 1.1", "311.92"]);
        assert_eq!(t.rows[1], vec!["ADAR1", "NT (+Dox)", "Sample 1.2", "186.32"]);
        assert_eq!(t.rows[2], vec!["ADAR1", "TRPM1\nNeg", "Sample 2.1", "326.76"]);
        assert!(!t.is_merged(0, 0));
        assert!(t.is_merged(1, 0));
        assert!(t.is_merged(1, 1));
        assert!(!t.is_merged(2, 1));
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
