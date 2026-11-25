//! Label rendering. Everything is drawn into a grayscale coverage canvas at
//! `s` x 180 dpi in both axes:
//! - print: s = 2 (360 dpi), reduced to a 1-bit 180x180 or 180x360 (high-res) bitmap;
//! - preview: s = 4 (720 dpi), kept antialiased and tinted with the cassette colours.

use super::{Align, BarKind, BarcodeBlock, Block, DataMatrixBlock, DmShape, Ec, ImageBlock, Label, QrBlock, TextBlock, Vars, expand};
use crate::proto::raster::{feed_dpmm, margin_dots, min_lines};
use crate::proto::{Bitmap, PageOpts, Tape, media};
use anyhow::{Result, anyhow, bail};
use cosmic_text::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, UnderlineStyle,
    Weight, Wrap,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Supersampling factor for printing (canvas = 360 dpi).
pub const SS: usize = 2;
/// Scale used for the on-screen preview (canvas = 720 dpi).
pub const PREVIEW_SCALE: usize = 4;
/// Ink threshold on the averaged 0..255 coverage.
const THRESH: u32 = 110;
/// Rasterised glyphs kept between renders. cosmic-text never evicts them and every
/// font/size/subpixel position adds one (~50-200 KB at preview scale).
const GLYPH_CACHE_MAX: usize = 32 << 20;

/// Canvas pixels per mm at scale `s`.
pub fn dpmm(s: usize) -> f32 {
    180.0 * s as f32 / 25.4
}

/// Round to a whole number of print dots (a multiple of `s` canvas pixels).
/// Capped at 1000 mm, the longest label the printer takes (spec 2.3.4), so absurd
/// sizes from a hand-edited file can't overflow or allocate gigabytes.
fn snap(v: f32, s: usize) -> usize {
    let v = v.round().clamp(0.0, 1000.0 * dpmm(s)) as usize;
    v.div_ceil(s) * s
}

fn mm_px(mm: f32, s: usize) -> usize {
    snap(mm * dpmm(s), s)
}

#[derive(Clone)]
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![0; w * h] }
    }

    pub fn fill(&mut self, x: i64, y: i64, w: i64, h: i64, a: u8) {
        // Fast paths for the per-pixel glyph callback: max(p, 0) is a no-op; 1x1 needs no clipping loop.
        if a == 0 {
            return;
        }
        if w == 1 && h == 1 {
            if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                let p = &mut self.px[y as usize * self.w + x as usize];
                *p = (*p).max(a);
            }
            return;
        }
        let x0 = (x.max(0) as usize).min(self.w);
        let y0 = (y.max(0) as usize).min(self.h);
        let x1 = ((x + w).max(0) as usize).min(self.w);
        let y1 = ((y + h).max(0) as usize).min(self.h);
        for yy in y0..y1 {
            let row = &mut self.px[yy * self.w..(yy + 1) * self.w];
            for p in &mut row[x0..x1] {
                *p = (*p).max(a);
            }
        }
    }

    pub fn blit(&mut self, src: &Canvas, x: usize, y: usize) {
        // Row slices clipped once (no per-pixel bounds checks).
        let n = src.w.min(self.w.saturating_sub(x));
        if n == 0 {
            return;
        }
        for sy in 0..src.h.min(self.h.saturating_sub(y)) {
            let d = &mut self.px[(y + sy) * self.w + x..][..n];
            for (d, s) in d.iter_mut().zip(&src.px[sy * src.w..][..n]) {
                *d = (*d).max(*s);
            }
        }
    }
}

pub struct Rendered {
    pub bitmap: Bitmap,
    /// Content longer than the fixed length.
    pub overflow: bool,
    pub warnings: Vec<String>,
}

pub struct RenderedCanvas {
    pub canvas: Canvas,
    pub overflow: bool,
    pub warnings: Vec<String>,
}

pub struct Renderer {
    fs: FontSystem,
    cache: SwashCache,
    metrics: HashMap<(String, bool, bool), (f32, f32)>,
    images: HashMap<String, Arc<image::GrayAlphaImage>>,
    /// Per path: (source it was made from, target height, inverted, Lanczos-resized grey image).
    resized: HashMap<String, (Arc<image::GrayAlphaImage>, u32, bool, Arc<image::GrayImage>)>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

fn attrs<'a>(font: &'a str, bold: bool, italic: bool) -> Attrs<'a> {
    let fam = if font.is_empty() { Family::SansSerif } else { Family::Name(font) };
    Attrs::new()
        .family(fam)
        .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if italic { Style::Italic } else { Style::Normal })
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            fs: FontSystem::new(),
            cache: SwashCache::new(),
            metrics: HashMap::new(),
            images: HashMap::new(),
            resized: HashMap::new(),
        }
    }

    /// Installed font families, sorted, without hidden system faces.
    pub fn families(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .fs
            .db()
            .faces()
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .filter(|n| !n.starts_with('.'))
            .collect();
        v.sort_by_key(|a| a.to_lowercase());
        v.dedup();
        v
    }

    /// Raw font bytes + collection index for a clean system bold sans-serif face (used by egui UI).
    pub fn bold_ui_font(&self) -> Option<(Vec<u8>, u32)> {
        use cosmic_text::fontdb::{Family as DbFamily, Query, Stretch, Style as DbStyle, Weight as DbWeight};
        let db = self.fs.db();
        let id = db.query(&Query {
            families: &[
                DbFamily::Name("Helvetica Neue"),
                DbFamily::Name("Segoe UI"),
                DbFamily::Name("Arial"),
                DbFamily::SansSerif,
            ],
            weight: DbWeight::BOLD,
            stretch: Stretch::Normal,
            style: DbStyle::Normal,
        })?;
        db.with_face_data(id, |data, index| (data.to_vec(), index))
    }

    /// Bytes held by rasterised glyphs in the swash cache.
    fn glyph_cache_bytes(&self) -> usize {
        self.cache.image_cache.values().flatten().map(|i| i.data.len()).sum()
    }

    /// Print bitmap.
    pub fn render(&mut self, label: &Label, tape: &Tape, vars: &Vars, opts: &PageOpts) -> Rendered {
        let r = self.render_canvas(label, tape, vars, opts, SS);
        Rendered { bitmap: reduce(&r.canvas, opts.high_res), overflow: r.overflow, warnings: r.warnings }
    }

    /// Antialiased coverage canvas at `s` x 180 dpi (same layout as the print).
    pub fn render_canvas(&mut self, label: &Label, tape: &Tape, vars: &Vars, opts: &PageOpts, s: usize) -> RenderedCanvas {
        if self.glyph_cache_bytes() > GLYPH_CACHE_MAX {
            self.cache.image_cache.clear(); // re-rasterising is deterministic: output unchanged
        }
        let pins = tape.print_pins as usize;
        let h = pins * s;
        let mut warnings = Vec::new();

        let frame_t = if label.frame { 2 * s } else { 0 };
        let pad_v = if label.frame { 2 * s } else { 0 };
        let inner_h = h.saturating_sub(2 * (frame_t + pad_v)).max(s);
        let pad_h = if label.frame { frame_t + mm_px(1.2, s) } else { 0 };
        let gap = mm_px(label.gap_mm, s);

        struct Part {
            canvas: Canvas,
            /// Set for auto-sized Text/Sequence blocks that can shrink to fit a fixed label length.
            auto_text: Option<(TextBlock, String)>,
        }

        let mut parts: Vec<Part> = Vec::new();
        let mut seq_idx = 0usize;
        for b in &label.blocks {
            let (r, auto_text) = match b {
                Block::Text(t) => {
                    let text = expand(&t.text, vars);
                    let c = self.text_block(t, &text, inner_h, s, 1.0);
                    let auto = t.size_pt.is_none().then(|| (t.clone(), text));
                    (Ok(c), auto)
                }
                Block::Qr(q) => (self.qr_block(q, &expand(&q.data, vars), inner_h, s), None),
                Block::DataMatrix(d) => (self.dm_block(d, &expand(&d.data, vars), inner_h, s), None),
                Block::Sequence(q) => {
                    let idx = seq_idx;
                    seq_idx += 1;
                    if label.sequence_in_text(idx) {
                        continue;
                    }
                    let text = vars.seqs.get(idx).cloned().unwrap_or_else(|| q.format(vars.index, vars));
                    let c = self.text_block(&q.style, &text, inner_h, s, 1.0);
                    let auto = q.style.size_pt.is_none().then(|| (q.style.clone(), text));
                    (Ok(c), auto)
                }
                Block::Barcode(bc) => (self.barcode_block(bc, &expand(&bc.data, vars), inner_h, s), None),
                Block::Image(i) => (self.image_block(i, inner_h, s), None),
                Block::Spacer { mm } => (Ok(Canvas::new(mm_px(*mm, s), inner_h)), None),
            };
            match r {
                Ok(c) if c.w > 0 => parts.push(Part { canvas: c, auto_text }),
                Ok(_) => {}
                Err(e) => warnings.push(format!("{}: {e}", b.kind_name())),
            }
        }

        let measure = |parts: &[Part]| -> usize {
            let cw: usize = parts.iter().map(|p| p.canvas.w).sum::<usize>() + gap * parts.len().saturating_sub(1);
            cw + 2 * pad_h
        };
        let mut natural = measure(&parts);

        // Minimum raster length (spec 2.3.4) in canvas pixels.
        let min_px = if opts.high_res { min_lines(true) * s / 2 } else { min_lines(false) * s };
        let margin_mm_eff = margin_dots(opts.margin_mm, opts.high_res) as f32 / feed_dpmm(opts.high_res);
        let mut total_w = natural.max(min_px);
        let mut overflow = false;
        if label.fixed_length {
            let target = snap((label.length_mm - 2.0 * margin_mm_eff).max(0.0) * dpmm(s), s);
            if natural > target {
                // Shrink auto-sized text blocks proportionally so the content fits the fixed length.
                let fixed_w: usize = parts
                    .iter()
                    .filter(|p| p.auto_text.is_none())
                    .map(|p| p.canvas.w)
                    .sum::<usize>()
                    + gap * parts.len().saturating_sub(1)
                    + 2 * pad_h;
                let avail_text = target.saturating_sub(fixed_w);
                let mut auto_w: usize = parts.iter().filter(|p| p.auto_text.is_some()).map(|p| p.canvas.w).sum();
                if auto_w > 0 && avail_text > 0 {
                    let mut scale = (avail_text as f32 / auto_w as f32).clamp(0.05, 1.0);
                    for pass in 0..4 {
                        for p in &mut parts {
                            if let Some((tb, txt)) = &p.auto_text {
                                p.canvas = self.text_block(tb, txt, inner_h, s, scale);
                            }
                        }
                        natural = measure(&parts);
                        if natural <= target || scale <= 0.05 || pass == 3 {
                            break;
                        }
                        auto_w = parts.iter().filter(|p| p.auto_text.is_some()).map(|p| p.canvas.w).sum();
                        if auto_w == 0 {
                            break;
                        }
                        scale = (scale * (avail_text as f32 / auto_w as f32) * 0.98).clamp(0.05, scale * 0.95);
                    }
                }
            }
            if natural > target {
                overflow = true;
                total_w = natural.max(min_px);
            } else {
                total_w = target.max(min_px);
            }
        }
        let total_w = total_w.div_ceil(s) * s;

        let mut canvas = Canvas::new(total_w, h);
        let slack = total_w - natural.min(total_w);
        let x0 = pad_h
            + match label.align {
                Align::Left => 0,
                Align::Center => (slack / 2) / s * s,
                Align::Right => slack,
            };
        let mut x = x0;
        for p in &parts {
            canvas.blit(&p.canvas, x, frame_t + pad_v);
            x += p.canvas.w + gap;
        }
        if label.frame {
            let (w, t) = (total_w as i64, frame_t as i64);
            canvas.fill(0, 0, w, t, 255);
            canvas.fill(0, h as i64 - t, w, t, 255);
            canvas.fill(0, 0, t, h as i64, 255);
            canvas.fill(w - t, 0, t, h as i64, 255);
        }
        if label.invert {
            for p in &mut canvas.px {
                *p = 255 - *p;
            }
        }
        RenderedCanvas { canvas, overflow, warnings }
    }

    fn font_metrics(&mut self, font: &str, bold: bool, italic: bool) -> (f32, f32) {
        let key = (font.to_string(), bold, italic);
        if let Some(m) = self.metrics.get(&key) {
            return *m;
        }
        let a = attrs(font, bold, italic);
        let mut buf = Buffer::new(&mut self.fs, Metrics::new(100.0, 120.0));
        buf.set_text("Hg", &a, Shaping::Advanced, None);
        buf.shape_until_scroll(&mut self.fs, false);
        let id = buf
            .layout_runs()
            .next()
            .and_then(|r| r.glyphs.first().map(|g| g.font_id));
        let m = id
            .and_then(|id| self.fs.get_font(id, a.weight))
            .map(|f| {
                let m = f.as_swash().metrics(&[]);
                let upem = m.units_per_em.max(1) as f32;
                (m.ascent.abs() / upem, m.descent.abs() / upem)
            })
            .unwrap_or((0.8, 0.2));
        self.metrics.insert(key, m);
        m
    }

    /// Multi-line text, auto-fit to `h` unless a size is set. `scale` (< 1.0) shrinks auto-sized
    /// text to fit a fixed label length while keeping aspect ratio and vertical centering.
    fn text_block(&mut self, t: &TextBlock, text: &str, h: usize, s: usize, scale: f32) -> Canvas {
        let (asc, desc) = self.font_metrics(&t.font, t.bold, t.italic);
        let em_box = asc + desc;
        // Ignore stray blank lines at the ends and trailing spaces (they shrink/offset auto-fit).
        let mut lines: Vec<&str> = text.split('\n').map(|l| l.trim_end()).collect();
        while lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        while lines.len() > 1 && lines[0].is_empty() {
            lines.remove(0);
        }
        let n = lines.len() as f32;
        let spacing = t.line_spacing.clamp(0.5, 3.0);
        let size = match t.size_pt {
            Some(pt) => pt.max(1.0) * 180.0 * s as f32 / 72.0,
            None => (h as f32 / (em_box * (1.0 + (n - 1.0) * spacing)) * scale).max(1.0),
        };
        let style = LineStyle { underline: t.underline, align: t.align, spacing, italic: t.italic };
        if t.rotation != super::Rotation::R0 {
            return self.turned_lines(t, &lines, style, size, (asc, desc), h, s, scale);
        }
        self.draw_lines(&lines, &attrs(&t.font, t.bold, t.italic), style, size, (asc, desc), h, s)
    }

    /// Rotated text: drawn upright into its own buffer, then turned. Sideways (90°/270°)
    /// the lines run across the tape, so auto-fit sizes the longest line to `h` and the
    /// block's length along the tape follows the stacked lines.
    #[allow(clippy::too_many_arguments)]
    fn turned_lines(
        &mut self,
        t: &TextBlock,
        lines: &[&str],
        st: LineStyle,
        size: f32,
        (asc, desc): (f32, f32),
        h: usize,
        s: usize,
        scale: f32,
    ) -> Canvas {
        let a = attrs(&t.font, t.bold, t.italic);
        let q = t.rotation.quarters();
        if q % 2 == 0 {
            return turn(&self.draw_lines(lines, &a, st, size, (asc, desc), h, s), q, h, s);
        }
        let mut size = size;
        if t.size_pt.is_none() {
            // draw_lines' buffer is max_w + italic overhang + s wide, rounded up to a dot.
            let unit_w = self.max_line_w(lines, &a) + if t.italic { 0.15 } else { 0.0 };
            if unit_w <= 0.0 {
                return Canvas::new(0, h);
            }
            size = (h.saturating_sub(2 * s) as f32 / unit_w * scale).max(1.0);
        }
        let n = lines.len() as f32;
        loop {
            let stack = snap((asc + desc) * size * (1.0 + (n - 1.0) * st.spacing), s).max(s);
            let c = self.draw_lines(lines, &a, st, size, (asc, desc), stack, s);
            // Auto-fit never crops: shrink again should the drawn line come out longer than h.
            if t.size_pt.is_some() || c.w <= h || size < 1.0 {
                return turn(&c, q, h, s);
            }
            size *= h as f32 / c.w as f32 * 0.97;
        }
    }

    /// Widest line at a 1 px font size (advances scale linearly with the size).
    fn max_line_w(&mut self, lines: &[&str], a: &Attrs) -> f32 {
        const REF: f32 = 100.0;
        let mut max_w = 0f32;
        for line in lines {
            let mut buf = Buffer::new(&mut self.fs, Metrics::new(REF, REF));
            buf.set_wrap(Wrap::None);
            buf.set_size(None, None);
            buf.set_text(line, a, Shaping::Advanced, None);
            buf.shape_until_scroll(&mut self.fs, false);
            if let Some(r) = buf.layout_runs().next() {
                max_w = max_w.max(r.line_w);
            }
        }
        max_w / REF
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_lines(
        &mut self,
        lines: &[&str],
        a: &Attrs,
        st: LineStyle,
        size: f32,
        (asc, desc): (f32, f32),
        h: usize,
        s: usize,
    ) -> Canvas {
        let em_box = asc + desc;
        let line_h = em_box * size * st.spacing;
        let block_h = em_box * size + (lines.len() as f32 - 1.0) * line_h;
        let y0 = (h as f32 - block_h) / 2.0;
        let a = if st.underline { a.clone().underline(UnderlineStyle::Single) } else { a.clone() };

        let mut shaped = Vec::with_capacity(lines.len());
        let mut max_w = 0f32;
        for line in lines {
            let mut buf = Buffer::new(&mut self.fs, Metrics::new(size, em_box * size));
            buf.set_wrap(Wrap::None);
            buf.set_size(None, None);
            buf.set_text(line, &a, Shaping::Advanced, None);
            buf.shape_until_scroll(&mut self.fs, false);
            let (w, base) = buf
                .layout_runs()
                .next()
                .map(|r| (r.line_w, r.line_y))
                .unwrap_or((0.0, asc * size));
            max_w = max_w.max(w);
            shaped.push((buf, w, base));
        }
        if max_w <= 0.0 {
            return Canvas::new(0, h);
        }
        let overhang = if st.italic { size * 0.15 } else { 0.0 };
        let mut c = Canvas::new(snap(max_w + overhang + s as f32, s), h);
        for (i, (buf, w, base)) in shaped.iter_mut().enumerate() {
            let xo = match st.align {
                Align::Left => 0.0,
                Align::Center => (max_w - *w) / 2.0,
                Align::Right => max_w - *w,
            }
            .round() as i64
                + 1;
            let yo = (y0 + i as f32 * line_h + asc * size - *base).round() as i64;
            buf.draw(&mut self.fs, &mut self.cache, Color::rgb(0, 0, 0), |x, y, w, hh, col| {
                c.fill(x as i64 + xo, y as i64 + yo, w as i64, hh as i64, col.a());
            });
        }
        c
    }

    /// Single line of text fitted into `h` (barcode human-readable line).
    fn caption(&mut self, text: &str, h: usize, s: usize) -> Canvas {
        let (asc, desc) = self.font_metrics("", false, false);
        let size = h as f32 / (asc + desc);
        let st = LineStyle { underline: false, align: Align::Center, spacing: 1.0, italic: false };
        self.draw_lines(&[text], &attrs("", false, false), st, size, (asc, desc), h, s)
    }

    fn qr_block(&mut self, q: &QrBlock, data: &str, h: usize, s: usize) -> Result<Canvas> {
        if data.is_empty() {
            return Ok(Canvas::new(0, h));
        }
        let ec = match q.ec {
            Ec::L => qrcode::EcLevel::L,
            Ec::M => qrcode::EcLevel::M,
            Ec::Q => qrcode::EcLevel::Q,
            Ec::H => qrcode::EcLevel::H,
        };
        let code = qrcode::QrCode::with_error_correction_level(data.as_bytes(), ec)
            .map_err(|e| anyhow!("{e}"))?;
        let mw = code.width();
        let dots = (h / s) / mw;
        if dots == 0 {
            bail!("too much data for this tape ({mw} modules > {} dots); shorten it or lower EC", h / s);
        }
        let m = dots * s;
        let size = mw * m;
        let y0 = ((h - size) / (2 * s)) * s;
        // 4-module quiet zone left and right (ISO/IEC 18004), like the barcode's; the
        // unprinted tape edge gives it vertically.
        let qz = 4 * m;
        let mut c = Canvas::new(size + 2 * qz, h);
        for (i, col) in code.to_colors().iter().enumerate() {
            if *col == qrcode::Color::Dark {
                let (mx, my) = (i % mw, i / mw);
                c.fill((qz + mx * m) as i64, (y0 + my * m) as i64, m as i64, m as i64, 255);
            }
        }
        Ok(c)
    }

    fn dm_block(&mut self, d: &DataMatrixBlock, data: &str, h: usize, s: usize) -> Result<Canvas> {
        use datamatrix::{DataMatrix, SymbolList};
        if data.is_empty() {
            return Ok(Canvas::new(0, h));
        }
        let dots = h / s;
        let enc = |l: SymbolList| DataMatrix::encode_str(data, l).ok().map(|c| c.bitmap());
        let square = || enc(SymbolList::default().enforce_square());
        let rect = || enc(SymbolList::default().enforce_rectangular());
        let cands: Vec<_> = match d.shape {
            DmShape::Square => vec![square()],
            DmShape::Rectangle => vec![rect()],
            DmShape::Auto => vec![square(), rect()],
        }
        .into_iter()
        .flatten()
        .collect();
        if cands.is_empty() {
            bail!(if d.shape == DmShape::Rectangle {
                "too much data for a rectangular symbol; use Square or Auto"
            } else {
                "too much data for Data Matrix"
            });
        }
        // A square with modules >= 4 dots (0.56 mm) scans fine and is compact; otherwise
        // (narrow tape) take whichever gives the larger module, then the narrower one.
        let square_ok = cands.iter().find(|b| b.width() == b.height() && dots / b.height() >= 4);
        let best = square_ok.or_else(|| {
            cands
                .iter()
                .filter(|b| dots / b.height() > 0)
                .max_by_key(|b| (dots / b.height(), std::cmp::Reverse(b.width() * (dots / b.height()))))
        });
        let Some(bm) = best else {
            let mh = cands.iter().map(|b| b.height()).min().unwrap_or(0);
            bail!("too much data for this tape ({mh} modules > {dots} dots); shorten it");
        };
        let m = (dots / bm.height()) * s;
        let (w, hh) = (bm.width() * m, bm.height() * m);
        let y0 = ((h - hh) / (2 * s)) * s;
        // 1-module quiet zone left and right (ISO/IEC 16022).
        let mut c = Canvas::new(w + 2 * m, h);
        for (x, y) in bm.pixels() {
            c.fill((m + x * m) as i64, (y0 + y * m) as i64, m as i64, m as i64, 255);
        }
        Ok(c)
    }

    fn barcode_block(&mut self, b: &BarcodeBlock, data: &str, h: usize, s: usize) -> Result<Canvas> {
        use barcoders::sym::{code39::Code39, code128::Code128, ean8::EAN8, ean13::EAN13};
        if data.is_empty() {
            return Ok(Canvas::new(0, h));
        }
        let err = |e: barcoders::error::Error| anyhow!("{e}");
        let (bars, text): (Vec<u8>, String) = match b.kind {
            BarKind::Code128 => {
                if let Some(c) = data.chars().find(|c| !c.is_ascii() && !matches!(c, 'À' | 'Ɓ' | 'Ć')) {
                    bail!("Code 128 can't encode '{c}' (ASCII only)");
                }
                let d = if data.starts_with(['À', 'Ɓ', 'Ć']) {
                    data.to_string()
                } else if data.len() >= 4 && data.len() % 2 == 0 && data.bytes().all(|c| c.is_ascii_digit()) {
                    format!("Ć{data}")
                } else {
                    format!("Ɓ{data}")
                };
                (Code128::new(d).map_err(err)?.encode(), data.trim_start_matches(['À', 'Ɓ', 'Ć']).to_string())
            }
            BarKind::Code39 => {
                let d = data.to_uppercase();
                if let Some(c) = d.chars().find(|c| !c.is_ascii_alphanumeric() && !" -.$/+%".contains(*c)) {
                    bail!("Code 39 can't encode '{c}' (A–Z, 0–9, space and - . $ / + %)");
                }
                (Code39::new(&d).map_err(err)?.encode(), d)
            }
            BarKind::Ean13 => {
                let d = gtin(data, 13, "EAN-13")?;
                (EAN13::new(&d).map_err(err)?.encode(), d)
            }
            // UPC-A is the EAN-13 with a leading 0.
            BarKind::UpcA => {
                let d = gtin(data, 12, "UPC-A")?;
                (EAN13::new(format!("0{d}")).map_err(err)?.encode(), d)
            }
            BarKind::Ean8 => {
                let d = gtin(data, 8, "EAN-8")?;
                (EAN8::new(&d).map_err(err)?.encode(), d)
            }
        };
        let mw = b.module.clamp(1, 6) as usize * s;
        let quiet = 8 * mw;
        let w = bars.len() * mw + 2 * quiet;
        let text_h = if b.show_text { snap(h as f32 * 0.26, s) } else { 0 };
        let bar_h = h - text_h;
        let mut c = Canvas::new(w, h);
        for (i, &bit) in bars.iter().enumerate() {
            if bit == 1 {
                c.fill((quiet + i * mw) as i64, 0, mw as i64, (bar_h - if b.show_text { s } else { 0 }) as i64, 255);
            }
        }
        if b.show_text {
            let cap = self.caption(&text, text_h, s);
            if cap.w > w {
                // caption wider than the bars: widen canvas
                let mut wide = Canvas::new(snap(cap.w as f32, s), h);
                wide.blit(&c, (cap.w - w) / 2 / s * s, 0);
                wide.blit(&cap, 0, bar_h);
                return Ok(wide);
            }
            c.blit(&cap, (w - cap.w) / 2, bar_h);
        }
        Ok(c)
    }

    fn image_block(&mut self, ib: &ImageBlock, h: usize, s: usize) -> Result<Canvas> {
        if ib.path.is_empty() {
            return Ok(Canvas::new(0, h));
        }
        let img = match self.images.get(&ib.path) {
            Some(i) => i.clone(),
            None => {
                let i = Arc::new(image::open(&ib.path)?.to_luma_alpha8());
                self.images.insert(ib.path.clone(), i.clone());
                i
            }
        };
        // Threshold/dither at the 180-dpi print grid so preview and print match.
        let th = (h / s) as u32;
        let tw = ((img.width() as f32 / img.height().max(1) as f32) * th as f32).round().max(1.0) as u32;
        // The resize dominates (tens of ms for a photo) and only depends on the source and `th`,
        // which is the same for print and preview: reuse it across renders.
        let small = match self.resized.get(&ib.path) {
            Some((src, sth, inv, small)) if Arc::ptr_eq(src, &img) && *sth == th && *inv == ib.invert => small.clone(),
            _ => {
                let gray = image::GrayImage::from_fn(img.width(), img.height(), |x, y| {
                    let p = img.get_pixel(x, y);
                    let (l, a) = (p[0] as u32, p[1] as u32);
                    // Invert before compositing on white so transparent areas never print.
                    let l = if ib.invert { 255 - l } else { l };
                    image::Luma([(255 - ((255 - l) * a) / 255) as u8])
                });
                let small = Arc::new(image::imageops::resize(&gray, tw, th, image::imageops::FilterType::Lanczos3));
                self.resized.insert(ib.path.clone(), (img.clone(), th, ib.invert, small.clone()));
                small
            }
        };
        let (w, hh) = (tw as usize, th as usize);
        let mut ink: Vec<f32> = small
            .pixels()
            .map(|p| 255.0 - p[0] as f32)
            .collect();
        let mut bits = vec![false; w * hh];
        let t = 255.0 - ib.threshold as f32;
        for y in 0..hh {
            for x in 0..w {
                let i = y * w + x;
                let on = ink[i] >= t;
                bits[i] = on;
                if ib.dither {
                    let e = ink[i] - if on { 255.0 } else { 0.0 };
                    let mut add = |dx: isize, dy: usize, f: f32| {
                        let nx = x as isize + dx;
                        if nx >= 0 && (nx as usize) < w && y + dy < hh {
                            ink[(y + dy) * w + nx as usize] += e * f;
                        }
                    };
                    add(1, 0, 7.0 / 16.0);
                    add(-1, 1, 3.0 / 16.0);
                    add(0, 1, 5.0 / 16.0);
                    add(1, 1, 1.0 / 16.0);
                }
            }
        }
        let mut c = Canvas::new(w * s, h);
        let y0 = (h - hh * s) / 2;
        for y in 0..hh {
            for x in 0..w {
                if bits[y * w + x] {
                    c.fill((x * s) as i64, (y0 + y * s) as i64, s as i64, s as i64, 255);
                }
            }
        }
        Ok(c)
    }
}

/// EAN/UPC digits with the check digit appended (`n - 1` digits) or verified (`n`).
fn gtin(data: &str, n: usize, name: &str) -> Result<String> {
    if !data.bytes().all(|c| c.is_ascii_digit()) || !(n - 1..=n).contains(&data.len()) {
        bail!("{name} needs {} digits, or {n} with the check digit", n - 1);
    }
    // GS1 mod 10: weights 3, 1, 3, ... from the rightmost data digit.
    let sum: u32 = data.bytes().take(n - 1).rev().enumerate().map(|(i, c)| (c - b'0') as u32 * if i % 2 == 0 { 3 } else { 1 }).sum();
    let check = char::from(b'0' + ((10 - sum % 10) % 10) as u8);
    match data.chars().nth(n - 1) {
        Some(c) if c != check => bail!("{name} check digit is {check}, not {c}"),
        Some(_) => Ok(data.to_string()),
        None => Ok(format!("{data}{check}")),
    }
}

#[derive(Clone, Copy)]
struct LineStyle {
    underline: bool,
    align: Align,
    spacing: f32,
    italic: bool,
}

/// `c` turned clockwise by `q` quarter turns and centred across a canvas `h` high
/// (cropped evenly if longer). Quarter turns are lossless, so antialiasing is kept.
fn turn(c: &Canvas, q: u8, h: usize, s: usize) -> Canvas {
    if c.w == 0 || c.h == 0 {
        return Canvas::new(0, h);
    }
    let (w2, h2) = if q % 2 == 1 { (c.h, c.w) } else { (c.w, c.h) };
    let top = (h as i64 - h2 as i64) / (2 * s as i64) * s as i64;
    let mut out = Canvas::new(w2, h);
    for y in 0..c.h {
        for x in 0..c.w {
            let (dx, dy) = match q % 4 {
                1 => (c.h - 1 - y, x),
                2 => (c.w - 1 - x, c.h - 1 - y),
                3 => (y, c.w - 1 - x),
                _ => (x, y),
            };
            let dy = dy as i64 + top;
            if dy >= 0 && (dy as usize) < h {
                out.px[dy as usize * w2 + dx] = c.px[y * c.w + x];
            }
        }
    }
    out
}

/// 2x canvas -> print bitmap (180x180 or 180x360).
fn reduce(c: &Canvas, high_res: bool) -> Bitmap {
    let sx = if high_res { 1 } else { SS };
    let (w, h) = (c.w / sx, c.h / SS);
    let mut bm = Bitmap::new(w, h);
    let n = (sx * SS) as u32;
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0u32;
            for dy in 0..SS {
                for dx in 0..sx {
                    sum += c.px[(y * SS + dy) * c.w + x * sx + dx] as u32;
                }
            }
            bm.px[y * w + x] = (sum / n >= THRESH) as u8;
        }
    }
    bm
}

/// Physical tape image (cassette colours, margins) from a print bitmap, at 180 dpi.
/// Pixel-exact: used for the CLI `render` output.
pub fn preview_rgba(bm: &Bitmap, tape: &Tape, opts: &PageOpts) -> (usize, usize, Vec<u8>) {
    let xs = if opts.high_res { 2 } else { 1 };
    let content_w = bm.w.div_ceil(xs);
    let margin = margin_dots(opts.margin_mm, false) as usize;
    let w = content_w + 2 * margin;
    let h = tape.tape_dots as usize;
    let top = (h - bm.h) / 2;
    let bg = media::tape_color(tape.tape_color).1;
    let ink = media::text_color(tape.text_color).1;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let on = y >= top
                && y < top + bm.h
                && x >= margin
                && x < margin + content_w
                && (0..xs).any(|d| {
                    let bx = (x - margin) * xs + d;
                    bx < bm.w && bm.get(bx, y - top)
                });
            let c = if on { ink } else { bg };
            rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    (w, h, rgba)
}

/// Smooth (antialiased) physical tape image from a coverage canvas at scale `s`.
pub fn preview_smooth(c: &Canvas, s: usize, tape: &Tape, opts: &PageOpts) -> (usize, usize, Vec<u8>) {
    let margin = margin_dots(opts.margin_mm, false) as usize * s;
    let w = c.w + 2 * margin;
    let h = tape.tape_dots as usize * s;
    let top = (h - c.h) / 2;
    let bg = media::tape_color(tape.tape_color).1.map(|v| v as u32);
    let ink = media::text_color(tape.text_color).1.map(|v| v as u32);
    // Blend once per coverage value, then copy: tape colour everywhere, canvas rows on top.
    let lut: [[u8; 4]; 256] = std::array::from_fn(|a| {
        let a = a as u32;
        let m = |k: usize| ((bg[k] * (255 - a) + ink[k] * a) / 255) as u8;
        [m(0), m(1), m(2), 255]
    });
    let mut rgba = vec![lut[0]; w * h];
    for y in 0..c.h {
        let o = (top + y) * w + margin;
        for (d, &a) in rgba[o..o + c.w].iter_mut().zip(&c.px[y * c.w..(y + 1) * c.w]) {
            *d = lut[a as usize];
        }
    }
    (w, h, rgba.into_flattened())
}

/// Physical length of a rendered label including both margins.
pub fn label_length_mm(bm: &Bitmap, opts: &PageOpts) -> f32 {
    let m = margin_dots(opts.margin_mm, opts.high_res) as f32;
    (bm.w as f32 + 2.0 * m) / feed_dpmm(opts.high_res)
}

/// Same as [`label_length_mm`] for a canvas at scale `s`.
pub fn canvas_length_mm(c: &Canvas, s: usize, opts: &PageOpts) -> f32 {
    let m = margin_dots(opts.margin_mm, opts.high_res) as f32 / feed_dpmm(opts.high_res);
    c.w as f32 / dpmm(s) + 2.0 * m
}

/// Save a print bitmap as the pixel-exact tape image (CLI `render`, File › Export PNG).
pub fn save_png(bm: &Bitmap, tape: &Tape, opts: &PageOpts, path: &std::path::Path) -> Result<()> {
    let (w, h, rgba) = preview_rgba(bm, tape, opts);
    image::RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or_else(|| anyhow!("image"))?.save(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{QrBlock, TextBlock};

    #[test]
    fn text_fills_print_area() {
        let mut r = Renderer::new();
        let tape = Tape::tze(12).unwrap();
        let label = Label {
            blocks: vec![Block::Text(TextBlock { text: "HELLO".into(), ..Default::default() })],
            ..Default::default()
        };
        let out = r.render(&label, &tape, &Vars::default(), &PageOpts::default());
        assert_eq!(out.bitmap.h, 70);
        assert!(out.bitmap.w > 100, "width {}", out.bitmap.w);
        // ink reaches near the top of the print area (caps fill the ascent)
        let first_row = (0..out.bitmap.h).find(|&y| (0..out.bitmap.w).any(|x| out.bitmap.get(x, y))).unwrap();
        assert!(first_row < 20, "first ink row {first_row}");
    }

    #[test]
    fn datamatrix_fits_and_auto_picks_bigger_modules() {
        use crate::label::{DataMatrixBlock, DmShape};
        let mut r = Renderer::new();
        let tape = Tape::tze(6).unwrap(); // 32 pins
        let render = |r: &mut Renderer, shape| {
            let label = Label {
                blocks: vec![Block::DataMatrix(DataMatrixBlock { data: "PTL-0042".into(), shape })],
                ..Default::default()
            };
            r.render(&label, &tape, &Vars::default(), &PageOpts::default())
        };
        let sq = render(&mut r, DmShape::Square);
        let re = render(&mut r, DmShape::Rectangle);
        let auto = render(&mut r, DmShape::Auto);
        for x in [&sq, &re, &auto] {
            assert!(x.warnings.is_empty(), "{:?}", x.warnings);
            assert!(!x.overflow);
        }
        // Symbol heights in dots (rows containing ink).
        let ink_rows = |b: &Bitmap| (0..b.h).filter(|&y| (0..b.w).any(|x| b.get(x, y))).count();
        let (hs, hr, ha) = (ink_rows(&sq.bitmap), ink_rows(&re.bitmap), ink_rows(&auto.bitmap));
        assert!(hs > 0 && hr > 0 && hs <= 32 && hr <= 32);
        assert_eq!(ha, hs.max(hr), "narrow tape: auto = larger of square/rect ({hs} vs {hr})");

        // Wide tape: the square is big enough, so Auto stays compact.
        let wide = Tape::tze(24).unwrap();
        let mk = |shape| Label {
            blocks: vec![Block::DataMatrix(DataMatrixBlock { data: "PTL-0042".into(), shape })],
            ..Default::default()
        };
        let a = r.render(&mk(DmShape::Auto), &wide, &Vars::default(), &PageOpts::default());
        let s = r.render(&mk(DmShape::Square), &wide, &Vars::default(), &PageOpts::default());
        assert_eq!(a.bitmap.w, s.bitmap.w);
    }

    #[test]
    fn qr_is_crisp_and_fits() {
        let mut r = Renderer::new();
        let tape = Tape::tze(24).unwrap();
        let label = Label {
            blocks: vec![Block::Qr(QrBlock { data: "https://example.com".into(), ..Default::default() })],
            ..Default::default()
        };
        let out = r.render(&label, &tape, &Vars::default(), &PageOpts::default());
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        assert_eq!(out.bitmap.h, 128);
    }

    #[test]
    fn fixed_length_and_hires() {
        let mut r = Renderer::new();
        let tape = Tape::tze(12).unwrap();
        let mut label = Label::default();
        label.fixed_length = true;
        label.length_mm = 40.0;
        let opts = PageOpts::default();
        let out = r.render(&label, &tape, &Vars::default(), &opts);
        assert!((label_length_mm(&out.bitmap, &opts) - 40.0).abs() < 0.5);
        let hi = PageOpts { high_res: true, ..opts };
        let out2 = r.render(&label, &tape, &Vars::default(), &hi);
        assert!((out2.bitmap.w as i64 - 2 * out.bitmap.w as i64).abs() <= 2);
    }

    #[test]
    fn fixed_length_scales_auto_text_to_fit() {
        let mut r = Renderer::new();
        let tape = Tape::tze(12).unwrap();
        let opts = PageOpts::default();
        // Long text that would naturally be > 70 mm on 12 mm tape shrinks to fit a 25 mm tube label.
        let auto_label = Label {
            blocks: vec![Block::Text(TextBlock {
                text: "ADAR1 TRPM1 Negative Cntrl Sample 2.1".into(),
                size_pt: None,
                ..Default::default()
            })],
            fixed_length: true,
            length_mm: 25.0,
            ..Default::default()
        };
        let out = r.render(&auto_label, &tape, &Vars::default(), &opts);
        assert!(!out.overflow, "auto-sized text should shrink to fit fixed length");
        assert!((label_length_mm(&out.bitmap, &opts) - 25.0).abs() < 0.5, "len {}", label_length_mm(&out.bitmap, &opts));
        assert!(out.bitmap.px.iter().any(|&p| p != 0), "scaled text still renders ink");

        // QR + auto-sized text on 40 mm fixed length: QR stays crisp, text shrinks to fit the remaining width.
        let combo = Label {
            blocks: vec![
                Block::Qr(QrBlock { data: "SAMPLE-2.1".into(), ..Default::default() }),
                Block::Text(TextBlock { text: "ADAR1 Circular 100nt".into(), size_pt: None, ..Default::default() }),
            ],
            fixed_length: true,
            length_mm: 40.0,
            ..Default::default()
        };
        let out_combo = r.render(&combo, &tape, &Vars::default(), &opts);
        assert!(!out_combo.overflow, "combo label should scale auto text to fit 40 mm");
        assert!((label_length_mm(&out_combo.bitmap, &opts) - 40.0).abs() < 0.5);

        // Explicit manual font size does NOT silently override the user's pt size; it still overflows.
        let manual = Label {
            blocks: vec![Block::Text(TextBlock {
                text: "ADAR1 TRPM1 Negative Cntrl Sample 2.1".into(),
                size_pt: Some(24.0),
                ..Default::default()
            })],
            fixed_length: true,
            length_mm: 25.0,
            ..Default::default()
        };
        assert!(r.render(&manual, &tape, &Vars::default(), &opts).overflow);
    }

    #[test]
    fn trailing_newline_does_not_shrink_text() {
        let mut r = Renderer::new();
        let tape = Tape::tze(12).unwrap();
        let mk = |t: &str| Label {
            blocks: vec![Block::Text(TextBlock { text: t.into(), ..Default::default() })],
            ..Default::default()
        };
        let o = PageOpts::default();
        let a = r.render(&mk("Inductors"), &tape, &Vars::default(), &o).bitmap;
        let b = r.render(&mk("\nInductors \n"), &tape, &Vars::default(), &o).bitmap;
        assert_eq!(a, b);
    }

    #[test]
    fn fill_clips_instead_of_panicking() {
        let mut c = Canvas::new(4, 3);
        c.fill(6, 1, 1, 1, 255); // glyph pixel right of the canvas (italic overhang)
        c.fill(1, 5, 1, 1, 255);
        c.fill(3, 0, 5, 1, 255);
        assert_eq!(c.px.iter().filter(|&&p| p != 0).count(), 1);
        // Italic overhang beyond the advance width (Times italic "f") used to panic.
        let mut r = Renderer::new();
        let t = TextBlock { text: "jff".into(), font: "Times New Roman".into(), italic: true, ..Default::default() };
        let label = Label { blocks: vec![Block::Text(t)], ..Default::default() };
        r.render(&label, &Tape::tze(12).unwrap(), &Vars::default(), &PageOpts::default());
    }

    #[test]
    fn absurd_sizes_are_capped() {
        let mut r = Renderer::new();
        let mut label = Label { blocks: vec![Block::Spacer { mm: 1e30 }], gap_mm: 1e30, ..Default::default() };
        label.blocks.push(Block::Spacer { mm: 5.0 });
        let out = r.render(&label, &Tape::tze(12).unwrap(), &Vars::default(), &PageOpts::default());
        assert!(out.bitmap.w <= 3 * 7087, "width {}", out.bitmap.w);
    }

    #[test]
    fn gtin_check_digits_and_errors() {
        assert_eq!(gtin("400638133393", 13, "EAN-13").unwrap(), "4006381333931");
        assert_eq!(gtin("4006381333931", 13, "EAN-13").unwrap(), "4006381333931");
        assert_eq!(gtin("03600029145", 12, "UPC-A").unwrap(), "036000291452");
        assert_eq!(gtin("9638507", 8, "EAN-8").unwrap(), "96385074");
        assert!(gtin("4006381333932", 13, "EAN-13").unwrap_err().to_string().contains("check digit is 1"));
        for bad in ["12345", "40063813339a", "４００６３８１３３３９３", ""] {
            assert!(gtin(bad, 13, "EAN-13").is_err(), "{bad}");
        }
    }

    #[test]
    fn barcodes_upca_and_errors() {
        use crate::label::BarcodeBlock;
        let mut r = Renderer::new();
        let bar = |r: &mut Renderer, kind, data: &str| {
            r.barcode_block(&BarcodeBlock { kind, data: data.into(), show_text: false, module: 1 }, data, 64, 1)
        };
        // UPC-A 036000291452 = EAN-13 0036000291452 (identical bars).
        let upc = bar(&mut r, BarKind::UpcA, "036000291452").unwrap();
        let ean = bar(&mut r, BarKind::Ean13, "0036000291452").unwrap();
        assert_eq!(upc.px, ean.px);
        assert_ne!(upc.px, bar(&mut r, BarKind::Ean13, "036000291452").unwrap().px);
        assert!(bar(&mut r, BarKind::Code39, "abc-12").is_ok());
        let e = bar(&mut r, BarKind::Code39, "a*b").err().unwrap().to_string();
        assert!(e.contains("'*'"), "{e}");
        let e = bar(&mut r, BarKind::Code128, "Grüße").err().unwrap().to_string();
        assert!(e.contains("'ü'"), "{e}");
        assert!(bar(&mut r, BarKind::Code128, "A").is_ok());
        assert!(bar(&mut r, BarKind::Ean8, "1234567x").is_err());
        assert!(bar(&mut r, BarKind::UpcA, "03600029145299").is_err());
    }

    #[test]
    fn qr_and_dm_get_quiet_zones() {
        use crate::label::{DataMatrixBlock, DmShape};
        let mut r = Renderer::new();
        for w in [4, 6, 12, 24] {
            let tape = Tape::tze(w).unwrap();
            let h = tape.print_pins as usize;
            let qr = r.qr_block(&QrBlock { data: "HELLO".into(), ..Default::default() }, "HELLO", h, 1).unwrap();
            let m = h / 21; // version 1
            let ink_cols: Vec<usize> = (0..qr.w).filter(|&x| (0..h).any(|y| qr.px[y * qr.w + x] != 0)).collect();
            assert_eq!((ink_cols[0], qr.w - 1 - ink_cols.last().unwrap()), (4 * m, 4 * m), "{w} mm");
            let dm = DataMatrixBlock { data: "PTL".into(), shape: DmShape::Auto };
            let c = r.dm_block(&dm, "PTL", h, 1).unwrap();
            let first = (0..c.w).find(|&x| (0..h).any(|y| c.px[y * c.w + x] != 0)).unwrap();
            assert!(first > 0 && first * 2 <= c.w, "{w} mm: dm quiet {first}");
        }
    }

    #[test]
    fn transparent_image_never_prints_even_inverted() {
        let mut r = Renderer::new();
        // Transparent background with an opaque black square in the middle.
        let img = image::GrayAlphaImage::from_fn(20, 20, |x, y| {
            if (5..15).contains(&x) && (5..15).contains(&y) { image::LumaA([0, 255]) } else { image::LumaA([0, 0]) }
        });
        r.images.insert("mem:test".into(), Arc::new(img));
        let ink = |r: &mut Renderer, invert| {
            let ib = ImageBlock { path: "mem:test".into(), invert, ..Default::default() };
            let c = r.image_block(&ib, 40, 1).unwrap();
            (c.px[0], c.px[20 * c.w + 20])
        };
        assert_eq!(ink(&mut r, false), (0, 255));
        assert_eq!(ink(&mut r, true), (0, 0));
    }

    #[test]
    fn preview_matches_print_geometry() {
        let mut r = Renderer::new();
        let tape = Tape::tze(24).unwrap();
        let label = Label::default();
        let opts = PageOpts::default();
        let print = r.render(&label, &tape, &Vars::default(), &opts);
        let pv = r.render_canvas(&label, &tape, &Vars::default(), &opts, PREVIEW_SCALE);
        assert_eq!(pv.canvas.h, 128 * PREVIEW_SCALE);
        let a = label_length_mm(&print.bitmap, &opts);
        let b = canvas_length_mm(&pv.canvas, PREVIEW_SCALE, &opts);
        assert!((a - b).abs() < 0.5, "print {a} vs preview {b}");
        let (w, h, rgba) = preview_smooth(&pv.canvas, PREVIEW_SCALE, &tape, &opts);
        assert_eq!(h, 170 * PREVIEW_SCALE);
        assert_eq!(rgba.len(), w * h * 4);
    }

    #[test]
    fn rotated_text_runs_across_narrow_tape() {
        use crate::label::Rotation;
        let mut r = Renderer::new();
        let tape = Tape::tze(12).unwrap(); // 70 pins
        let mk = |rotation| Label {
            blocks: vec![Block::Text(TextBlock { text: "HELLO".into(), rotation, ..Default::default() })],
            ..Default::default()
        };
        let (o, v) = (PageOpts::default(), Vars::default());
        // Ink bounding box (x0, x1, y0, y1), inclusive.
        let ink = |c: &Canvas| {
            let on: Vec<(usize, usize)> =
                (0..c.h).flat_map(|y| (0..c.w).map(move |x| (x, y))).filter(|&(x, y)| c.px[y * c.w + x] > 0).collect();
            let xs = on.iter().map(|p| p.0);
            let ys = on.iter().map(|p| p.1);
            (xs.clone().min().unwrap(), xs.max().unwrap(), ys.clone().min().unwrap(), ys.max().unwrap())
        };
        let up = r.render_canvas(&mk(Rotation::R0), &tape, &v, &o, SS).canvas;
        let (ux0, ux1, ..) = ink(&up);
        for rot in [Rotation::R90, Rotation::R270] {
            let out = r.render_canvas(&mk(rot), &tape, &v, &o, SS);
            assert!(out.warnings.is_empty() && !out.overflow);
            let c = out.canvas;
            assert_eq!(c.h, 70 * SS);
            let (x0, x1, y0, y1) = ink(&c);
            assert!(y1 - y0 > 2 * (x1 - x0), "{rot:?} not taller than wide: {}x{}", x1 - x0, y1 - y0);
            // Auto-fit: the line spans the tape height without being cut at either edge.
            assert!(y1 - y0 > c.h * 8 / 10, "{rot:?} line only {} of {} px", y1 - y0, c.h);
            assert!(y0 > 0 && y1 < c.h - 1, "{rot:?} clipped: rows {y0}..={y1} of {}", c.h);
            assert!(x1 - x0 < (ux1 - ux0) / 2, "{rot:?} block not shorter along the tape");
            let bm = r.render(&mk(rot), &tape, &v, &o).bitmap;
            assert_eq!(bm.h, 70);
        }
        // 180°: same box, every pixel turned.
        let flip = r.render_canvas(&mk(Rotation::R180), &tape, &v, &o, SS).canvas;
        assert_eq!((flip.w, flip.h), (up.w, up.h));
        assert!(flip.px.iter().rev().eq(up.px.iter()));
        // Fixed size: the block length follows the line stack, empty text adds nothing.
        let mut l = mk(Rotation::R90);
        if let Block::Text(t) = &mut l.blocks[0] {
            t.size_pt = Some(8.0);
            t.text = "A\nB".into();
        }
        let two = r.render_canvas(&l, &tape, &v, &o, SS).canvas;
        let (x0, x1, ..) = ink(&two);
        assert!(x1 - x0 > 30, "two 8 pt lines side by side: {} px", x1 - x0);
        let empty = Label { blocks: vec![Block::Text(TextBlock { rotation: Rotation::R90, ..Default::default() })], ..Default::default() };
        assert_eq!(r.render_canvas(&empty, &tape, &v, &o, SS).canvas.w, min_lines(false) * SS);
    }

    /// Deterministic pseudo-random coverage canvas.
    fn noise(w: usize, h: usize, seed: u32) -> Canvas {
        let mut s = seed;
        let px = (0..w * h).map(|_| { s = s.wrapping_mul(1_103_515_245).wrapping_add(12345); (s >> 16) as u8 }).collect();
        Canvas { w, h, px }
    }

    #[test]
    fn blit_and_fill_match_per_pixel_reference() {
        let src = noise(37, 23, 1);
        for (x, y) in [(0, 0), (5, 3), (40, 10), (69, 39), (70, 0), (0, 40), (500, 500)] {
            let mut fast = noise(70, 40, 2);
            let mut slow = fast.clone();
            fast.blit(&src, x, y);
            for sy in 0..src.h {
                for sx in 0..src.w {
                    if x + sx < slow.w && y + sy < slow.h {
                        let d = &mut slow.px[(y + sy) * slow.w + x + sx];
                        *d = (*d).max(src.px[sy * src.w + sx]);
                    }
                }
            }
            assert!(fast.px == slow.px, "blit at {x},{y}");
        }
        for (x, y, w, h, a) in [(3, 4, 1, 1, 200), (-1, 0, 1, 1, 255), (69, 39, 1, 1, 9), (70, 5, 1, 1, 255), (2, 2, 9, 7, 0), (-3, -2, 9, 7, 77), (60, 35, 20, 20, 255)] {
            let mut fast = noise(70, 40, 3);
            let mut slow = fast.clone();
            fast.fill(x, y, w, h, a);
            for yy in y.max(0)..(y + h).min(slow.h as i64) {
                for xx in x.max(0)..(x + w).min(slow.w as i64) {
                    let p = &mut slow.px[yy as usize * slow.w + xx as usize];
                    *p = (*p).max(a);
                }
            }
            assert!(fast.px == slow.px, "fill {x},{y} {w}x{h} a={a}");
        }
    }

    #[test]
    fn preview_smooth_matches_per_pixel_blend() {
        let tape = Tape::from_status(12, 0x01, 0x04, 0x02).unwrap(); // coloured tape + ink
        let opts = PageOpts::default();
        let c = noise(300, 70 * 2, 4);
        let (w, h, rgba) = preview_smooth(&c, 2, &tape, &opts);
        let margin = margin_dots(opts.margin_mm, false) as usize * 2;
        let top = (h - c.h) / 2;
        let bg = media::tape_color(tape.tape_color).1.map(|v| v as u32);
        let ink = media::text_color(tape.text_color).1.map(|v| v as u32);
        for y in 0..h {
            for x in 0..w {
                let inside = y >= top && y < top + c.h && x >= margin && x < margin + c.w;
                let a = if inside { c.px[(y - top) * c.w + x - margin] as u32 } else { 0 };
                let o = (y * w + x) * 4;
                for k in 0..3 {
                    assert_eq!(rgba[o + k] as u32, (bg[k] * (255 - a) + ink[k] * a) / 255, "{x},{y}");
                }
                assert_eq!(rgba[o + 3], 255);
            }
        }
    }

    #[test]
    fn glyph_cache_is_bounded_and_transparent() {
        let mut r = Renderer::new();
        let tape = Tape::tze(24).unwrap();
        let o = PageOpts::default();
        let mk = |pt| Label {
            blocks: vec![Block::Text(TextBlock { text: "ABCDEFGHIJKLM".into(), size_pt: Some(pt), ..Default::default() })],
            ..Default::default()
        };
        let first = r.render(&mk(30.0), &tape, &Vars::default(), &o).bitmap;
        let (mut peak, mut prev, mut cleared) = (0, 0, false);
        for i in 0..60 {
            // Like dragging the size control with the preview open.
            r.render_canvas(&mk(30.0 + i as f32 * 0.5), &tape, &Vars::default(), &o, PREVIEW_SCALE);
            let b = r.glyph_cache_bytes();
            cleared |= b < prev;
            (peak, prev) = (peak.max(b), b);
        }
        assert!(cleared, "cache never trimmed (peak {peak} bytes)");
        assert!(peak < GLYPH_CACHE_MAX + GLYPH_CACHE_MAX / 2, "peak {peak} bytes");
        assert_eq!(r.render(&mk(30.0), &tape, &Vars::default(), &o).bitmap, first);
    }

    #[test]
    fn image_resize_cache_is_transparent() {
        let img = |k: u32| {
            Arc::new(image::GrayAlphaImage::from_fn(300, 200, |x, y| image::LumaA([((x * k + y) % 256) as u8, if x < 280 { 255 } else { 90 }])))
        };
        let tape = Tape::tze(12).unwrap();
        let o = PageOpts::default();
        let label = Label {
            blocks: vec![Block::Image(ImageBlock { path: "mem:logo".into(), dither: true, ..Default::default() })],
            ..Default::default()
        };
        let mut r = Renderer::new();
        r.images.insert("mem:logo".into(), img(3));
        let a = r.render(&label, &tape, &Vars::default(), &o).bitmap;
        assert_eq!(r.resized.len(), 1);
        assert_eq!(r.render(&label, &tape, &Vars::default(), &o).bitmap, a, "cached");
        r.resized.clear();
        assert_eq!(r.render(&label, &tape, &Vars::default(), &o).bitmap, a, "recomputed");
        // A different source under the same path (reloaded file) must not hit the old entry.
        r.images.insert("mem:logo".into(), img(7));
        let b = r.render(&label, &tape, &Vars::default(), &o).bitmap;
        let mut fresh = Renderer::new();
        fresh.images.insert("mem:logo".into(), img(7));
        assert_eq!(b, fresh.render(&label, &tape, &Vars::default(), &o).bitmap);
        assert_ne!(a, b);
    }

    #[test]
    fn sequence_hidden_when_used_in_text() {
        use crate::job::{Pager, PrintSettings};
        use crate::label::SequenceBlock;

        let mut r = Renderer::new();
        let tape = Tape::tze(18).unwrap();
        let o = PageOpts::default();
        let s = PrintSettings::default();

        // Single sequence referenced in a non-bold Text block: renders identically to the Text block alone.
        let non_bold = TextBlock { text: "{seq}".into(), size_pt: Some(8.0), bold: false, align: Align::Left, ..Default::default() };
        let seq1 = SequenceBlock { prefix: "adar-".into(), start: 1, step: 1, digits: 3, count: 10, ..Default::default() };
        let with_seq = Label {
            blocks: vec![Block::Text(non_bold.clone()), Block::Sequence(seq1.clone())],
            ..Default::default()
        };
        let plain_text = Label {
            blocks: vec![Block::Text(TextBlock { text: "adar-001".into(), ..non_bold.clone() })],
            ..Default::default()
        };
        let v = Pager::new(&with_seq, &s, vec![]).vars(0);
        assert_eq!(
            r.render(&with_seq, &tape, &v, &o).bitmap,
            r.render(&plain_text, &tape, &Vars::default(), &o).bitmap
        );

        // Sequence referenced ONLY in a QR block still renders its own text next to the QR.
        let seq_qr = Label {
            blocks: vec![Block::Sequence(seq1.clone()), Block::Qr(QrBlock { data: "{seq}".into(), ..Default::default() })],
            ..Default::default()
        };
        let qr_only = Label {
            blocks: vec![Block::Qr(QrBlock { data: "adar-001".into(), ..Default::default() })],
            ..Default::default()
        };
        let v_qr = Pager::new(&seq_qr, &s, vec![]).vars(0);
        assert!(r.render(&seq_qr, &tape, &v_qr, &o).bitmap.w > r.render(&qr_only, &tape, &Vars::default(), &o).bitmap.w);

        // Multiple sequences: only the ones referenced in Text are hidden; unspecified ones render as before.
        let seq2 = SequenceBlock { prefix: "B-".into(), start: 10, step: 1, digits: 2, count: 5, ..Default::default() };
        let partial = Label {
            blocks: vec![
                Block::Text(TextBlock { text: "{seq}".into(), ..non_bold.clone() }),
                Block::Sequence(seq1.clone()),
                Block::Sequence(seq2.clone()),
            ],
            ..Default::default()
        };
        let expected_partial = Label {
            blocks: vec![
                Block::Text(TextBlock { text: "adar-001".into(), ..non_bold.clone() }),
                Block::Sequence(seq2.clone()),
            ],
            ..Default::default()
        };
        let vp = Pager::new(&partial, &s, vec![]).vars(0);
        assert_eq!(
            r.render(&partial, &tape, &vp, &o).bitmap,
            r.render(&expected_partial, &tape, &Pager::new(&expected_partial, &s, vec![]).vars(0), &o).bitmap
        );

        let both = Label {
            blocks: vec![
                Block::Text(TextBlock { text: "{seq} / {seq2}".into(), ..non_bold.clone() }),
                Block::Sequence(seq1),
                Block::Sequence(seq2),
            ],
            ..Default::default()
        };
        let expected_both = Label {
            blocks: vec![Block::Text(TextBlock { text: "adar-001 / B-10".into(), ..non_bold })],
            ..Default::default()
        };
        let vb = Pager::new(&both, &s, vec![]).vars(0);
        assert_eq!(
            r.render(&both, &tape, &vb, &o).bitmap,
            r.render(&expected_both, &tape, &Vars::default(), &o).bitmap
        );
    }
}
