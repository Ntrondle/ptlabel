//! Offline timing harness and output fingerprints for rendering + job encoding (no hardware).
//! `cargo test --release -- --ignored --nocapture perf_`

use crate::data::Record;
use crate::job::{Pager, PrintSettings};
use crate::label::render::{PREVIEW_SCALE, Renderer, preview_rgba, preview_smooth};
use crate::label::{
    Align, BarKind, BarcodeBlock, Block, DataMatrixBlock, DmShape, ImageBlock, Label, QrBlock, SequenceBlock,
    TextBlock, Vars,
};
use crate::proto::raster::encode_page;
use crate::proto::{Caps, PageOpts, Tape};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::hint::black_box;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const CAPS: Caps = Caps { half_cut: true, cut_every: true };

/// Deterministic 1200x900 grey logo with a soft-edged alpha disc, written once per process.
fn test_image() -> &'static str {
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("ptlabel-perf-{}.png", std::process::id()));
        image::RgbaImage::from_fn(1200, 900, |x, y| {
            let (dx, dy) = (x as f32 - 600.0, y as f32 - 450.0);
            let r = (dx * dx + dy * dy).sqrt();
            let l = ((x * 255) / 1200) as u8 ^ if (x / 60 + y / 60) % 2 == 0 { 0 } else { 0x80 };
            image::Rgba([l, l, l, if r < 400.0 { 255 } else if r < 440.0 { 128 } else { 0 }])
        })
        .save(&path)
        .unwrap();
        path.to_string_lossy().into_owned()
    })
}

fn text(t: &str) -> TextBlock {
    TextBlock { text: t.into(), ..Default::default() }
}

fn label(blocks: Vec<Block>) -> Label {
    Label { blocks, ..Default::default() }
}

fn asset() -> Label {
    Label {
        blocks: vec![
            Block::Qr(QrBlock { data: "https://inventory.local/item/{n:04}".into(), ..Default::default() }),
            Block::Text(TextBlock { bold: true, align: Align::Left, ..text("ASSET {n:04}\nLab bench · {Part}") }),
            Block::Barcode(BarcodeBlock { data: "A{n:04}".into(), ..Default::default() }),
        ],
        gap_mm: 2.5,
        frame: true,
        ..Default::default()
    }
}

fn img_block(dither: bool, invert: bool) -> Block {
    Block::Image(ImageBlock { path: test_image().into(), dither, invert, ..Default::default() })
}

/// (name, label, tape mm, options)
fn cases() -> Vec<(&'static str, Label, u8, PageOpts)> {
    let o = PageOpts::default();
    let hi = PageOpts { high_res: true, ..o.clone() };
    let bar = |kind, data: &str, module| Block::Barcode(BarcodeBlock { kind, data: data.into(), module, show_text: true });
    vec![
        ("text", label(vec![Block::Text(text("Hello World"))]), 24, o.clone()),
        (
            "text3",
            label(vec![Block::Text(TextBlock {
                italic: true,
                underline: true,
                align: Align::Right,
                line_spacing: 1.2,
                ..text("Line one\nSecond line {Part}\n3")
            })]),
            12,
            o.clone(),
        ),
        ("text_pt", label(vec![Block::Text(TextBlock { size_pt: Some(10.0), ..text("Fixed 10 pt") })]), 18, hi.clone()),
        ("qr", label(vec![Block::Qr(QrBlock { data: "https://inventory.local/item/0042".into(), ..Default::default() })]), 24, o.clone()),
        ("dm", label(vec![Block::DataMatrix(DataMatrixBlock { data: "PTL-0042".into(), shape: DmShape::Auto })]), 9, o.clone()),
        ("code128", label(vec![bar(BarKind::Code128, "PTL-0042", 2)]), 12, o.clone()),
        ("code39", label(vec![bar(BarKind::Code39, "abc-123", 1)]), 6, o.clone()),
        ("ean13", label(vec![bar(BarKind::Ean13, "5901234123457", 2)]), 18, hi.clone()),
        ("image", label(vec![img_block(false, false), Block::Text(text("Logo"))]), 24, o.clone()),
        ("image_dither", label(vec![img_block(true, true)]), 12, o.clone()),
        ("asset", asset(), 24, o.clone()),
        (
            "asset_inv_fixed",
            Label { invert: true, fixed_length: true, length_mm: 90.0, align: Align::Right, ..asset() },
            18,
            hi,
        ),
        (
            "seq_spacer",
            label(vec![
                Block::Sequence(SequenceBlock { prefix: "BOX-".into(), ..Default::default() }),
                Block::Spacer { mm: 3.0 },
                Block::Qr(QrBlock { data: "{seq}".into(), ..Default::default() }),
            ]),
            12,
            o,
        ),
    ]
}

fn vars() -> Vars {
    let row = Record { headers: Arc::new(vec!["Part".into()]), values: vec!["R1 10k".into()] };
    Vars { n: 42, index: 3, seqs: vec!["BOX-004".into()], row: Some(Arc::new(row)) }
}

fn h64(v: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// Fingerprints of every output stage. Run before and after a change and diff the lines:
/// they must be identical (bit-exact rendering and job bytes).
#[test]
#[ignore]
fn perf_fingerprints() {
    let mut r = Renderer::new();
    let v = vars();
    for (name, l, mm, o) in cases() {
        let tape = Tape::tze(mm).unwrap();
        let out = r.render(&l, &tape, &v, &o);
        assert!(out.warnings.is_empty(), "{name}: {:?}", out.warnings);
        let mut page = Vec::new();
        encode_page(&mut page, &out.bitmap, &tape, &o, CAPS, 0, true);
        let pv = r.render_canvas(&l, &tape, &v, &o, PREVIEW_SCALE);
        let smooth = preview_smooth(&pv.canvas, PREVIEW_SCALE, &tape, &o);
        let rgba = preview_rgba(&out.bitmap, &tape, &o);
        println!(
            "FP {name:16} {}x{} bm={:016x} page={:016x} canvas={}x{}:{:016x} smooth={:016x} rgba={:016x}",
            out.bitmap.w,
            out.bitmap.h,
            h64(&out.bitmap.px),
            h64(&page),
            pv.canvas.w,
            pv.canvas.h,
            h64(&pv.canvas.px),
            h64(&smooth),
            h64(&rgba)
        );
    }
}

/// Median wall time of `f` (at least 5 runs, about `budget` in total).
fn bench(name: &str, budget: Duration, mut f: impl FnMut()) {
    f(); // warm caches (fonts, glyphs, images)
    let mut t = Vec::new();
    let start = Instant::now();
    while t.len() < 5 || (start.elapsed() < budget && t.len() < 500) {
        let s = Instant::now();
        f();
        t.push(s.elapsed());
    }
    t.sort();
    println!("PERF {name:28} median {:>10.3?}  min {:>10.3?}  n={}", t[t.len() / 2], t[0], t.len());
}

/// What `job::run_job` does per page, minus the transport: render + encode.
fn run_pages(r: &mut Renderer, l: &Label, tape: &Tape, s: &PrintSettings, rows: Vec<Arc<Record>>) -> usize {
    let pager = Pager::new(l, s, rows);
    let mut buf = Vec::new();
    let mut bytes = 0;
    for i in 0..pager.total.unwrap() {
        let out = r.render(l, tape, &pager.vars(i), &s.opts);
        buf.clear();
        encode_page(&mut buf, &out.bitmap, tape, &s.opts, CAPS, i, i + 1 == pager.total.unwrap());
        bytes += buf.len();
    }
    bytes
}

#[test]
#[ignore]
fn perf_timings() {
    let t0 = Instant::now();
    let mut r = Renderer::new();
    println!("PERF {:28} {:>10.3?}", "Renderer::new (font db)", t0.elapsed());
    let v = vars();
    let budget = Duration::from_millis(1500);
    for (name, l, mm, o) in cases() {
        let tape = Tape::tze(mm).unwrap();
        bench(&format!("print {name}"), budget, || {
            black_box(r.render(&l, &tape, &v, &o));
        });
        bench(&format!("preview {name}"), budget, || {
            let pv = r.render_canvas(&l, &tape, &v, &o, PREVIEW_SCALE);
            black_box(preview_smooth(&pv.canvas, PREVIEW_SCALE, &tape, &o));
        });
    }
    let tape = Tape::tze(24).unwrap();
    let o = PageOpts::default();
    let bm = r.render(&asset(), &tape, &v, &o).bitmap;
    bench("encode_page asset 24mm", budget, || {
        let mut buf = Vec::new();
        encode_page(&mut buf, &bm, &tape, &o, CAPS, 0, true);
        black_box(buf);
    });

    let s = PrintSettings::default();
    let tape12 = Tape::tze(12).unwrap();
    let seq = label(vec![
        Block::Sequence(SequenceBlock { prefix: "BOX-".into(), count: 100, ..Default::default() }),
        Block::Qr(QrBlock { data: "{seq}".into(), ..Default::default() }),
    ]);
    bench("job seq x100 12mm", Duration::from_secs(3), || {
        black_box(run_pages(&mut r, &seq, &tape12, &s, vec![]));
    });
    let seq_logo = Label { blocks: [vec![img_block(false, false)], seq.blocks.clone()].concat(), ..seq.clone() };
    bench("job seq+image x100 12mm", Duration::from_secs(3), || {
        black_box(run_pages(&mut r, &seq_logo, &tape12, &s, vec![]));
    });

    let headers = Arc::new(vec!["Part".to_string(), "Value".to_string(), "Bin".to_string()]);
    let rows: Vec<_> = (0..500)
        .map(|i| {
            let values = vec![format!("R{i}"), format!("{}k 1% 0603", i % 97 + 1), format!("B{:04}", i * 7)];
            Arc::new(Record { headers: headers.clone(), values })
        })
        .collect();
    let excel = label(vec![
        Block::Text(TextBlock { bold: true, ..text("{Part}\n{Value}") }),
        Block::Barcode(BarcodeBlock { data: "{Bin}".into(), ..Default::default() }),
    ]);
    bench("job excel x500 12mm", Duration::from_secs(3), || {
        black_box(run_pages(&mut r, &excel, &tape12, &s, rows.clone()));
    });
}
