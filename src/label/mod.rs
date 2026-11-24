pub mod render;

use crate::data::{DataSource, Record};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

/// Text direction, clockwise. 90°/270° run the lines across the tape (top to bottom /
/// bottom to top), which suits narrow tape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub const ALL: [Rotation; 4] = [Self::R0, Self::R90, Self::R180, Self::R270];
    /// Clockwise quarter turns (0..=3).
    pub fn quarters(self) -> u8 {
        self as u8
    }
    pub fn degrees(self) -> u16 {
        self.quarters() as u16 * 90
    }
    pub fn from_degrees(d: u16) -> Option<Self> {
        (d % 90 == 0).then(|| Self::ALL[(d / 90 % 4) as usize])
    }
    /// Next clockwise step (270° wraps to 0°).
    pub fn next(self) -> Self {
        Self::ALL[(self.quarters() as usize + 1) % 4]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextBlock {
    pub text: String,
    /// Font family; empty = system sans-serif.
    pub font: String,
    /// `None` = auto-fit to the tape height.
    pub size_pt: Option<f32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub align: Align,
    /// Baseline-to-baseline distance as a multiple of the font's line box.
    pub line_spacing: f32,
    /// Missing in files saved before rotation existed: upright.
    #[serde(default)]
    pub rotation: Rotation,
}

impl Default for TextBlock {
    fn default() -> Self {
        Self {
            text: String::new(),
            font: String::new(),
            size_pt: None,
            bold: false,
            italic: false,
            underline: false,
            align: Align::Center,
            line_spacing: 1.0,
            rotation: Rotation::R0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Ec {
    L,
    #[default]
    M,
    Q,
    H,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QrBlock {
    pub data: String,
    pub ec: Ec,
}

/// Data Matrix symbol shape. Rectangles (8/12/16 modules high) give bigger
/// modules on narrow tape; Auto picks whichever prints larger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DmShape {
    #[default]
    Auto,
    Square,
    Rectangle,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DataMatrixBlock {
    pub data: String,
    pub shape: DmShape,
}

/// Numbered labels: prefix + zero-padded number + suffix, one value per label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SequenceBlock {
    pub prefix: String,
    pub start: i64,
    pub step: i64,
    /// How many labels the sequence prints (ignored when spreadsheet rows drive the job).
    pub count: u32,
    /// Zero-pad the number to this many digits (0 = no padding).
    pub digits: u8,
    pub suffix: String,
    /// Font and style (its `text` is unused).
    pub style: TextBlock,
}

impl Default for SequenceBlock {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            start: 1,
            step: 1,
            count: 10,
            digits: 3,
            suffix: String::new(),
            style: TextBlock { text: String::new(), bold: true, ..Default::default() },
        }
    }
}

impl SequenceBlock {
    pub fn number(&self, i: u64) -> i64 {
        self.start.saturating_add((i as i64).saturating_mul(self.step))
    }

    /// The value printed on label `i` (prefix/suffix may use placeholders).
    pub fn format(&self, i: u64, v: &Vars) -> String {
        let n = self.number(i);
        let w = self.digits as usize;
        let num = if n < 0 { format!("-{:0w$}", n.unsigned_abs()) } else { format!("{n:0w$}") };
        format!("{}{num}{}", expand(&self.prefix, v), expand(&self.suffix, v))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BarKind {
    #[default]
    Code128,
    Code39,
    Ean13,
    Ean8,
    UpcA,
}

impl BarKind {
    pub const ALL: [BarKind; 5] = [Self::Code128, Self::Code39, Self::Ean13, Self::Ean8, Self::UpcA];
    pub fn name(self) -> &'static str {
        match self {
            Self::Code128 => "Code 128",
            Self::Code39 => "Code 39",
            Self::Ean13 => "EAN-13",
            Self::Ean8 => "EAN-8",
            Self::UpcA => "UPC-A",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarcodeBlock {
    pub kind: BarKind,
    pub data: String,
    pub show_text: bool,
    /// Narrow bar width in dots (1 dot = 0.14 mm).
    pub module: u8,
}

impl Default for BarcodeBlock {
    fn default() -> Self {
        Self { kind: BarKind::Code128, data: String::new(), show_text: true, module: 2 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageBlock {
    pub path: String,
    pub threshold: u8,
    pub dither: bool,
    pub invert: bool,
}

impl Default for ImageBlock {
    fn default() -> Self {
        Self { path: String::new(), threshold: 128, dither: false, invert: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Block {
    Text(TextBlock),
    Qr(QrBlock),
    DataMatrix(DataMatrixBlock),
    Barcode(BarcodeBlock),
    Sequence(SequenceBlock),
    Image(ImageBlock),
    Spacer { mm: f32 },
}

impl Block {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Block::Text(_) => "Text",
            Block::Qr(_) => "QR code",
            Block::DataMatrix(_) => "Data Matrix",
            Block::Sequence(_) => "Sequence",
            Block::Barcode(_) => "Barcode",
            Block::Image(_) => "Image",
            Block::Spacer { .. } => "Spacer",
        }
    }

    fn templates(&self) -> Vec<&str> {
        match self {
            Block::Text(t) => vec![&t.text],
            Block::Qr(q) => vec![&q.data],
            Block::DataMatrix(d) => vec![&d.data],
            Block::Sequence(q) => vec![&q.prefix, &q.suffix],
            Block::Barcode(b) => vec![&b.data],
            _ => vec![],
        }
    }
}

/// A label = blocks laid out left to right along the tape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Label {
    pub blocks: Vec<Block>,
    pub gap_mm: f32,
    pub frame: bool,
    pub invert: bool,
    pub fixed_length: bool,
    /// Total label length incl. margins when `fixed_length`.
    pub length_mm: f32,
    pub align: Align,
    /// Spreadsheet whose selected rows fill `{Column}` placeholders.
    pub data: Option<DataSource>,
}

impl Default for Label {
    fn default() -> Self {
        Self {
            blocks: vec![Block::Text(TextBlock { text: "Label".into(), ..Default::default() })],
            gap_mm: 2.0,
            frame: false,
            invert: false,
            fixed_length: false,
            length_mm: 50.0,
            align: Align::Center,
            data: None,
        }
    }
}

impl Label {
    /// True when labels differ from page to page (placeholders or a sequence).
    pub fn uses_vars(&self) -> bool {
        self.sequence().is_some() || self.blocks.iter().flat_map(|b| b.templates()).any(|t| t.contains('{'))
    }
    pub fn sequence(&self) -> Option<&SequenceBlock> {
        self.sequences().next()
    }
    pub fn sequences(&self) -> impl Iterator<Item = &SequenceBlock> {
        self.blocks.iter().filter_map(|b| if let Block::Sequence(q) = b { Some(q) } else { None })
    }
    /// True when a `Text` block references the `index`-th sequence (`{seq}` / `{seq1}` for 0,
    /// `{seq2}` for 1, `{seq3}` for 2, …). In that case the sequence block doesn't render a
    /// duplicate of itself on the label.
    pub fn sequence_in_text(&self, index: usize) -> bool {
        let ph = sequence_placeholder(index);
        self.blocks.iter().any(|b| match b {
            Block::Text(t) => t.text.contains(&ph) || (index == 0 && t.text.contains("{seq1}")),
            _ => false,
        })
    }
    pub fn uses(&self, key: &str) -> bool {
        // `{key}` or `{key:fmt}`, not `{keyword}`.
        let (exact, fmt) = (format!("{{{key}}}"), format!("{{{key}:"));
        self.blocks.iter().flat_map(|b| b.templates()).any(|t| t.contains(&exact) || t.contains(&fmt))
    }
}

/// Placeholder for the `index`-th sequence block: `{seq}`, `{seq2}`, `{seq3}`, …
pub fn sequence_placeholder(index: usize) -> String {
    if index == 0 { "{seq}".into() } else { format!("{{seq{}}}", index + 1) }
}

/// Per-label substitution values.
#[derive(Clone, Debug, Default, PartialEq, Hash)]
pub struct Vars {
    pub n: i64,
    /// Record index: which sequence value / spreadsheet row this label shows.
    pub index: u64,
    /// Values of the label's sequence blocks in order (`{seq}` / `{seq1}`, `{seq2}`, `{seq3}`, …).
    pub seqs: Vec<String>,
    pub row: Option<Arc<Record>>,
}

/// Expand `{n}`, `{n:03}`, `{seq}`, `{seq2}`, …, `{date}`, `{time}` and `{Column}` (spreadsheet);
/// unknown keys stay verbatim.
pub fn expand(s: &str, v: &Vars) -> String {
    if !s.contains('{') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let key = &after[..j];
        // A stray `{` before a placeholder is literal text.
        if let Some(k) = key.rfind('{') {
            out.push_str(&rest[i..i + 1 + k]);
            rest = &after[k..];
            continue;
        }
        let now = chrono::Local::now();
        let rep = match key {
            "n" => Some(v.n.to_string()),
            "date" => Some(now.format("%Y-%m-%d").to_string()),
            "time" => Some(now.format("%H:%M").to_string()),
            "seq" => Some(v.seqs.first().cloned().unwrap_or_default()),
            k if k.starts_with("seq")
                && k.len() > 3
                && !k[3..].starts_with('0')
                && k[3..].bytes().all(|b| b.is_ascii_digit()) =>
            {
                k[3..]
                    .parse::<usize>()
                    .ok()
                    .and_then(|idx| v.seqs.get(idx - 1).cloned())
                    .or_else(|| v.row.as_ref().and_then(|r| r.get(k)).map(String::from))
            }
            // Width capped: i64 has at most 20 chars, and a typo must not allocate gigabytes.
            k if k.starts_with("n:") => k[2..]
                .parse::<usize>()
                .ok()
                .filter(|&w| w <= 32)
                .map(|w| format!("{:0w$}", v.n, w = w)),
            k => v.row.as_ref().and_then(|r| r.get(k)).map(String::from),
        };
        match rep {
            Some(r) => out.push_str(&r),
            None => {
                out.push('{');
                out.push_str(key);
                out.push('}');
            }
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_values() {
        let q = SequenceBlock { prefix: "A-".into(), start: 8, step: 2, digits: 3, suffix: "/{n}".into(), ..Default::default() };
        let v = Vars { n: 5, ..Default::default() };
        assert_eq!(q.format(0, &v), "A-008/5");
        assert_eq!(q.format(2, &v), "A-012/5");
        let neg = SequenceBlock { start: -1, digits: 2, ..Default::default() };
        assert_eq!(neg.format(0, &v), "-01");
    }

    #[test]
    fn expands() {
        let row = Record { headers: Arc::new(vec!["Part".into(), "Bin".into()]), values: vec!["R1".into(), "A-12".into()] };
        let v = Vars {
            n: 7,
            seqs: vec!["S-01".into(), "B-02".into(), "C-03".into()],
            row: Some(Arc::new(row)),
            ..Default::default()
        };
        assert_eq!(expand("Box {n:03} / {bin} / {Part} {seq}", &v), "Box 007 / A-12 / R1 S-01");
        assert_eq!(expand("{seq}|{seq1}|{seq2}|{seq3}|{seq4}|{seq0}", &v), "S-01|S-01|B-02|C-03|{seq4}|{seq0}");
        assert_eq!(expand("{n}{x}{", &v), "7{x}{");
        assert_eq!(expand("plain", &v), "plain");
    }

    #[test]
    fn sequence_in_text_tracks_each_sequence() {
        let label = Label {
            blocks: vec![
                Block::Text(TextBlock { text: "A {seq} C {seq3}".into(), ..Default::default() }),
                Block::Qr(QrBlock { data: "{seq2}".into(), ..Default::default() }),
                Block::Sequence(SequenceBlock::default()),
                Block::Sequence(SequenceBlock::default()),
                Block::Sequence(SequenceBlock::default()),
            ],
            ..Default::default()
        };
        assert!(label.sequence_in_text(0));
        assert!(!label.sequence_in_text(1), "QR alone must not hide the sequence block");
        assert!(label.sequence_in_text(2));

        let alias = Label {
            blocks: vec![Block::Text(TextBlock { text: "{seq1}".into(), ..Default::default() })],
            ..Default::default()
        };
        assert!(alias.sequence_in_text(0));
        assert!(!alias.sequence_in_text(1));
        assert_eq!(sequence_placeholder(0), "{seq}");
        assert_eq!(sequence_placeholder(1), "{seq2}");
        assert_eq!(sequence_placeholder(2), "{seq3}");
    }

    #[test]
    fn rotation_is_optional_in_files() {
        let t: TextBlock = serde_json::from_str(r#"{"text": "A", "bold": true, "align": "Left"}"#).unwrap();
        assert_eq!(t.rotation, Rotation::R0);
        let l: Label = serde_json::from_str(include_str!("../../examples/asset-tag.ptlabel")).unwrap();
        assert!(matches!(&l.blocks[1], Block::Text(t) if t.rotation == Rotation::R0 && t.bold));
        let r = TextBlock { rotation: Rotation::R270, ..Default::default() };
        let back: TextBlock = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(Rotation::from_degrees(450), Some(Rotation::R90));
        assert_eq!(Rotation::from_degrees(45), None);
        assert_eq!(Rotation::R270.next(), Rotation::R0);
    }

    #[test]
    fn expand_edge_cases() {
        let v = Vars { n: -5, ..Default::default() };
        assert_eq!(expand("{n:0}|{n:00}|{n:3}|{n}", &v), "-5|-5|-05|-5");
        assert_eq!(expand("{n:99999999999}", &v), "{n:99999999999}");
        assert_eq!(expand("{{n}} {a {n}", &v), "{-5} {a -5");
        assert_eq!(expand("ä{ö}{n}ü{", &v), "ä{ö}-5ü{");
        assert_eq!(expand("}{n", &v), "}{n");
    }

    #[test]
    fn uses_matches_whole_keys() {
        let mk = |t: &str| Label { blocks: vec![Block::Text(TextBlock { text: t.into(), ..Default::default() })], ..Default::default() };
        assert!(mk("{n:03}").uses("n") && mk("{n}").uses("n"));
        assert!(!mk("{name}").uses("n"));
        assert!(!mk("{PartNo}").uses("Part") && mk("{PartNo}").uses("PartNo"));
    }
}
