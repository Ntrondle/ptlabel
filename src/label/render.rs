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

/// Canvas pixels per mm at scale `s`.
pub fn dpmm(s: usize) -> f32 {
    180.0 * s as f32 / 25.4
}

/// Round to a whole number of print dots (a multiple of `s` canvas pixels).
fn snap(v: f32, s: usize) -> usize {
    let v = v.round().max(0.0) as usize;
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
        let x0 = x.max(0) as usize;
        let y0 = y.max(0) as usize;
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
        for sy in 0..src.h {
            let dy = y + sy;
            if dy >= self.h {
                break;
            }
            for sx in 0..src.w {
                let dx = x + sx;
                if dx >= self.w {
                    break;
                }
                let d = &mut self.px[dy * self.w + dx];
                *d = (*d).max(src.px[sy * src.w + sx]);
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

    /// Print bitmap.
    pub fn render(&mut self, label: &Label, tape: &Tape, vars: &Vars, opts: &PageOpts) -> Rendered {
        let r = self.render_canvas(label, tape, vars, opts, SS);
        Rendered { bitmap: reduce(&r.canvas, opts.high_res), overflow: r.overflow, warnings: r.warnings }
    }

    /// Antialiased coverage canvas at `s` x 180 dpi (same layout as the print).
    pub fn render_canvas(&mut self, label: &Label, tape: &Tape, vars: &Vars, opts: &PageOpts, s: usize) -> RenderedCanvas {
        let pins = tape.print_pins as usize;
        let h = pins * s;
        let mut warnings = Vec::new();

        let frame_t = if label.frame { 2 * s } else { 0 };
        let pad_v = if label.frame { 2 * s } else { 0 };
        let inner_h = h.saturating_sub(2 * (frame_t + pad_v)).max(s);
        let pad_h = if label.frame { frame_t + mm_px(1.2, s) } else { 0 };
        let gap = mm_px(label.gap_mm, s);

        let mut parts: Vec<Canvas> = Vec::new();
        for b in &label.blocks {
            let r = match b {
                Block::Text(t) => Ok(self.text_block(t, &expand(&t.text, vars), inner_h, s)),
                Block::Qr(q) => self.qr_block(q, &expand(&q.data, vars), inner_h, s),
                Block::DataMatrix(d) => self.dm_block(d, &expand(&d.data, vars), inner_h, s),
                Block::Sequence(q) => Ok(self.text_block(&q.style, &q.format(vars.index, vars), inner_h, s)),
                Block::Barcode(bc) => self.barcode_block(bc, &expand(&bc.data, vars), inner_h, s),
                Block::Image(i) => self.image_block(i, inner_h, s),
                Block::Spacer { mm } => Ok(Canvas::new(mm_px(*mm, s), inner_h)),
            };
            match r {
                Ok(c) if c.w > 0 => parts.push(c),
                Ok(_) => {}
                Err(e) => warnings.push(format!("{}: {e}", b.kind_name())),
            }
        }

        let content_w: usize =
            parts.iter().map(|c| c.w).sum::<usize>() + gap * parts.len().saturating_sub(1);
        let natural = content_w + 2 * pad_h;

        // Minimum raster length (spec 2.3.4) in canvas pixels.
        let min_px = if opts.high_res { min_lines(true) * s / 2 } else { min_lines(false) * s };
        let margin_mm_eff = margin_dots(opts.margin_mm, opts.high_res) as f32 / feed_dpmm(opts.high_res);
        let mut total_w = natural.max(min_px);
        let mut overflow = false;
        if label.fixed_length {
            let target = snap((label.length_mm - 2.0 * margin_mm_eff).max(0.0) * dpmm(s), s);
            if natural > target {
                overflow = true;
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
        for c in &parts {
            canvas.blit(c, x, frame_t + pad_v);
            x += c.w + gap;
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

    /// Multi-line text, auto-fit to `h` unless a size is set.
    fn text_block(&mut self, t: &TextBlock, text: &str, h: usize, s: usize) -> Canvas {
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
            None => h as f32 / (em_box * (1.0 + (n - 1.0) * spacing)),
        };
        let style = LineStyle { underline: t.underline, align: t.align, spacing, italic: t.italic };
        self.draw_lines(&lines, &attrs(&t.font, t.bold, t.italic), style, size, (asc, desc), h, s)
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
            .map_err(|e| anyhow!("{e:?}"))?;
        let mw = code.width();
        let dots = (h / s) / mw;
        if dots == 0 {
            bail!("too much data for this tape ({mw} modules > {} dots); shorten it or lower EC", h / s);
        }
        let m = dots * s;
        let size = mw * m;
        let y0 = ((h - size) / (2 * s)) * s;
        let mut c = Canvas::new(size, h);
        for (i, col) in code.to_colors().iter().enumerate() {
            if *col == qrcode::Color::Dark {
                let (mx, my) = (i % mw, i / mw);
                c.fill((mx * m) as i64, (y0 + my * m) as i64, m as i64, m as i64, 255);
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
        let mut c = Canvas::new(w, h);
        for (x, y) in bm.pixels() {
            c.fill((x * m) as i64, (y0 + y * m) as i64, m as i64, m as i64, 255);
        }
        Ok(c)
    }

    fn barcode_block(&mut self, b: &BarcodeBlock, data: &str, h: usize, s: usize) -> Result<Canvas> {
        use barcoders::sym::{code39::Code39, code128::Code128, ean8::EAN8, ean13::EAN13};
        if data.is_empty() {
            return Ok(Canvas::new(0, h));
        }
        let err = |e: barcoders::error::Error| anyhow!("{e}");
        let bars: Vec<u8> = match b.kind {
            BarKind::Code128 => {
                let d = if data.starts_with(['À', 'Ɓ', 'Ć']) {
                    data.to_string()
                } else if data.len() >= 4 && data.len() % 2 == 0 && data.bytes().all(|c| c.is_ascii_digit()) {
                    format!("Ć{data}")
                } else {
                    format!("Ɓ{data}")
                };
                Code128::new(d).map_err(err)?.encode()
            }
            BarKind::Code39 => Code39::new(data.to_uppercase()).map_err(err)?.encode(),
            BarKind::Ean13 | BarKind::UpcA => EAN13::new(data).map_err(err)?.encode(),
            BarKind::Ean8 => EAN8::new(data).map_err(err)?.encode(),
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
            let cap = self.caption(data.trim_start_matches(['À', 'Ɓ', 'Ć']), text_h, s);
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
        let gray = image::GrayImage::from_fn(img.width(), img.height(), |x, y| {
            let p = img.get_pixel(x, y);
            let (l, a) = (p[0] as u32, p[1] as u32);
            image::Luma([(255 - ((255 - l) * a) / 255) as u8])
        });
        let small = image::imageops::resize(&gray, tw, th, image::imageops::FilterType::Lanczos3);
        let (w, hh) = (tw as usize, th as usize);
        let mut ink: Vec<f32> = small
            .pixels()
            .map(|p| if ib.invert { p[0] as f32 } else { 255.0 - p[0] as f32 })
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

#[derive(Clone, Copy)]
struct LineStyle {
    underline: bool,
    align: Align,
    spacing: f32,
    italic: bool,
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
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let a = if y >= top && y < top + c.h && x >= margin && x < margin + c.w {
                c.px[(y - top) * c.w + (x - margin)] as u32
            } else {
                0
            };
            let o = (y * w + x) * 4;
            for k in 0..3 {
                rgba[o + k] = ((bg[k] * (255 - a) + ink[k] * a) / 255) as u8;
            }
            rgba[o + 3] = 255;
        }
    }
    (w, h, rgba)
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
}
