use super::media::{HEAD_PINS, LINE_BYTES, Tape};
use super::packbits;
use serde::{Deserialize, Serialize};

/// 1-bit label image. `w` = raster lines (feed direction), `h` = print-area pins.
/// Row 0 is the top edge of the label as you read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

impl Bitmap {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![0; w * h] }
    }
    #[inline]
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.px[y * self.w + x] != 0
    }
}

/// What the connected model can do (spec: half cut + `ESC i A` are E550W/P750W only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub half_cut: bool,
    pub cut_every: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageOpts {
    pub auto_cut: bool,
    pub half_cut: bool,
    /// Chain printing: don't feed + cut after the last label (saves ~25 mm of tape).
    pub chain: bool,
    pub cut_every: u8,
    pub mirror: bool,
    pub high_res: bool,
    pub special_tape: bool,
    pub margin_mm: f32,
}

impl Default for PageOpts {
    fn default() -> Self {
        Self {
            auto_cut: true,
            half_cut: false,
            chain: false,
            cut_every: 1,
            mirror: false,
            high_res: false,
            special_tape: false,
            margin_mm: 2.0,
        }
    }
}

/// Dots along the feed direction per mm.
pub fn feed_dpmm(high_res: bool) -> f32 {
    if high_res { 360.0 / 25.4 } else { 180.0 / 25.4 }
}

pub fn margin_dots(margin_mm: f32, high_res: bool) -> u16 {
    let (min, max) = if high_res { (28.0, 1800.0) } else { (14.0, 900.0) };
    (margin_mm * feed_dpmm(high_res)).round().clamp(min, max) as u16
}

/// Minimum raster length in dots (spec 2.3.4).
pub fn min_lines(high_res: bool) -> usize {
    if high_res { 60 } else { 31 }
}

/// Invalidate (100 x NUL) + initialize. Sent once per job.
pub fn job_header() -> Vec<u8> {
    let mut v = vec![0u8; 100];
    v.extend_from_slice(&[0x1B, 0x40]);
    v
}

/// Cancel: return the printer to the receiving state and clear its buffer.
pub fn abort_sequence() -> Vec<u8> {
    job_header()
}

/// One raster line: byte 0 MSB = pin 127, byte 15 LSB = pin 0; label top -> high pins.
pub fn raster_line(bm: &Bitmap, x: usize, pin_offset: usize) -> [u8; LINE_BYTES] {
    let mut line = [0u8; LINE_BYTES];
    for y in 0..bm.h {
        if bm.get(x, y) {
            let pin = pin_offset + (bm.h - 1 - y);
            if pin < HEAD_PINS {
                line[LINE_BYTES - 1 - pin / 8] |= 1 << (pin % 8);
            }
        }
    }
    line
}

/// Encode one page (control codes + TIFF raster + print command).
pub fn encode_page(
    out: &mut Vec<u8>,
    bm: &Bitmap,
    tape: &Tape,
    opts: &PageOpts,
    caps: Caps,
    page_index: u32,
    last: bool,
) {
    assert_eq!(bm.h, tape.print_pins as usize, "bitmap height must match tape print area");
    let lines = bm.w as u32;

    // Switch dynamic command mode -> raster
    out.extend_from_slice(&[0x1B, 0x69, 0x61, 0x01]);
    // Print information: valid = recover | width | kind
    let n = lines.to_le_bytes();
    out.extend_from_slice(&[
        0x1B,
        0x69,
        0x7A,
        0x80 | 0x04 | 0x02,
        tape.kind.code(),
        tape.width_code,
        0x00,
        n[0],
        n[1],
        n[2],
        n[3],
        if page_index == 0 { 0 } else { 1 },
        0x00,
    ]);
    // Various mode: bit6 auto cut, bit7 mirror
    let mut m = 0u8;
    if opts.auto_cut {
        m |= 0x40;
    }
    if opts.mirror {
        m |= 0x80;
    }
    out.extend_from_slice(&[0x1B, 0x69, 0x4D, m]);
    // Cut every N labels (not on P710BT)
    if caps.cut_every && opts.auto_cut {
        out.extend_from_slice(&[0x1B, 0x69, 0x41, opts.cut_every.clamp(1, 99)]);
    }
    // Advanced mode: bit2 half cut, bit3 no-chain, bit4 special tape, bit6 high-res
    let mut k = 0u8;
    if opts.half_cut && caps.half_cut {
        k |= 0x04;
    }
    if !opts.chain {
        k |= 0x08;
    }
    if opts.special_tape {
        k |= 0x10;
    }
    if opts.high_res {
        k |= 0x40;
    }
    out.extend_from_slice(&[0x1B, 0x69, 0x4B, k]);
    // Margin (feed) amount
    let md = margin_dots(opts.margin_mm, opts.high_res).to_le_bytes();
    out.extend_from_slice(&[0x1B, 0x69, 0x64, md[0], md[1]]);
    // TIFF compression
    out.extend_from_slice(&[0x4D, 0x02]);

    let off = tape.pin_offset();
    for x in 0..bm.w {
        let line = raster_line(bm, x, off);
        if line.iter().all(|&b| b == 0) {
            out.push(0x5A);
        } else {
            let c = packbits::encode(&line);
            out.push(0x47);
            out.extend_from_slice(&(c.len() as u16).to_le_bytes());
            out.extend_from_slice(&c);
        }
    }
    out.push(if last { 0x1A } else { 0x0C });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_bit_layout() {
        let tape = Tape::tze(24).unwrap();
        let mut bm = Bitmap::new(1, 128);
        bm.px[0] = 1; // top row -> pin 127 -> byte 0 MSB
        bm.px[127] = 1; // bottom row -> pin 0 -> byte 15 LSB
        assert_eq!(
            raster_line(&bm, 0, tape.pin_offset()),
            [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]
        );

        let tape = Tape::tze(12).unwrap(); // 70 pins at offset 29
        let mut bm = Bitmap::new(1, 70);
        bm.px[69] = 1; // bottom row -> pin 29 -> byte 12 bit 5
        assert_eq!(
            raster_line(&bm, 0, tape.pin_offset()),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0]
        );
    }

    #[test]
    fn page_control_codes() {
        let tape = Tape::tze(24).unwrap();
        let bm = Bitmap::new(682, 128);
        let mut out = Vec::new();
        let caps = Caps { half_cut: true, cut_every: true };
        let opts = PageOpts { half_cut: true, ..Default::default() };
        encode_page(&mut out, &bm, &tape, &opts, caps, 0, true);
        assert_eq!(&out[..4], &[0x1B, 0x69, 0x61, 0x01]);
        // Same print-info layout as the spec example (24 mm, 0x2AA lines), plus kind flag
        assert_eq!(
            &out[4..17],
            &[0x1B, 0x69, 0x7A, 0x86, 0x01, 0x18, 0x00, 0xAA, 0x02, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(&out[17..21], &[0x1B, 0x69, 0x4D, 0x40]);
        assert_eq!(&out[21..25], &[0x1B, 0x69, 0x41, 0x01]);
        assert_eq!(&out[25..29], &[0x1B, 0x69, 0x4B, 0x0C]);
        assert_eq!(&out[29..34], &[0x1B, 0x69, 0x64, 0x0E, 0x00]); // 2 mm = 14 dots
        assert_eq!(&out[34..36], &[0x4D, 0x02]);
        assert!(out[36..36 + 682].iter().all(|&b| b == 0x5A));
        assert_eq!(*out.last().unwrap(), 0x1A);
    }

    #[test]
    fn p710bt_skips_unsupported() {
        let tape = Tape::tze(12).unwrap();
        let bm = Bitmap::new(40, 70);
        let mut out = Vec::new();
        let opts = PageOpts { half_cut: true, chain: true, high_res: true, ..Default::default() };
        encode_page(&mut out, &bm, &tape, &opts, Caps::default(), 1, false);
        assert!(!out.windows(3).any(|w| w == [0x1B, 0x69, 0x41]));
        let k = out.windows(3).position(|w| w == [0x1B, 0x69, 0x4B]).unwrap();
        assert_eq!(out[k + 3], 0x40); // hi-res only: no half cut, chain on
        let z = out.windows(3).position(|w| w == [0x1B, 0x69, 0x7A]).unwrap();
        assert_eq!(out[z + 11], 1); // not the first page
        assert_eq!(*out.last().unwrap(), 0x0C);
    }

    /// Decode the raster section (after `M 02`) back to 16-byte lines; returns (lines, terminator).
    fn decode_raster(page: &[u8]) -> (Vec<[u8; LINE_BYTES]>, u8) {
        let mut i = page.windows(2).position(|w| w == [0x4D, 0x02]).unwrap() + 2;
        let mut lines = Vec::new();
        loop {
            match page[i] {
                0x5A => {
                    lines.push([0; LINE_BYTES]);
                    i += 1;
                }
                0x47 => {
                    let k = u16::from_le_bytes([page[i + 1], page[i + 2]]) as usize;
                    assert!(k <= LINE_BYTES + 1, "G length {k}");
                    let d = packbits::decode(&page[i + 3..i + 3 + k]);
                    lines.push(d.try_into().unwrap());
                    i += 3 + k;
                }
                t => {
                    assert_eq!(i + 1, page.len(), "trailing bytes after {t:02X}");
                    return (lines, t);
                }
            }
        }
    }

    #[test]
    fn raster_lines_roundtrip() {
        let tape = Tape::tze(12).unwrap();
        let mut bm = Bitmap::new(64, 70);
        for x in 0..bm.w {
            for y in 0..bm.h {
                bm.px[y * bm.w + x] = ((x * 7 + y * 3) % 11 < x % 5) as u8;
            }
        }
        let mut out = Vec::new();
        encode_page(&mut out, &bm, &tape, &PageOpts::default(), Caps::default(), 0, true);
        let z = out.windows(3).position(|w| w == [0x1B, 0x69, 0x7A]).unwrap();
        assert_eq!(u32::from_le_bytes(out[z + 7..z + 11].try_into().unwrap()), 64);
        let (lines, term) = decode_raster(&out);
        assert_eq!(term, 0x1A);
        assert_eq!(lines.len(), 64);
        for (x, l) in lines.iter().enumerate() {
            let bits = u128::from_be_bytes(*l);
            // Pins outside the 12 mm print area (0..29, 99..128) stay blank.
            assert_eq!(bits & ((1 << 29) - 1), 0, "line {x} low margin");
            assert_eq!(bits >> 99, 0, "line {x} high margin");
            for y in 0..bm.h {
                let pin = 29 + (bm.h - 1 - y);
                assert_eq!((bits >> pin) & 1 != 0, bm.get(x, y), "line {x} row {y}");
            }
        }
    }

    #[test]
    fn margins_and_lengths_follow_spec() {
        // 2.3.3: 2..127 mm = 14..900 dots at 180 dpi, 28..1800 dots at 180x360.
        assert_eq!(margin_dots(2.0, false), 14);
        assert_eq!(margin_dots(2.0, true), 28);
        assert_eq!(margin_dots(127.0, false), 900);
        assert_eq!(margin_dots(127.0, true), 1800);
        assert_eq!(margin_dots(0.0, true), 28);
        assert_eq!(margin_dots(500.0, true), 1800);
        // 2.3.4: minimum 4.4 mm (31 dots) / 4.2 mm (60 dots at 360).
        assert_eq!((min_lines(false), min_lines(true)), (31, 60));

        let tape = Tape::tze(24).unwrap();
        let bm = Bitmap::new(120, 128);
        let mut out = Vec::new();
        let opts = PageOpts { high_res: true, ..Default::default() };
        encode_page(&mut out, &bm, &tape, &opts, Caps::default(), 0, true);
        let k = out.windows(3).position(|w| w == [0x1B, 0x69, 0x4B]).unwrap();
        assert_eq!(out[k + 3], 0x48); // no-chain + high-res
        let d = out.windows(3).position(|w| w == [0x1B, 0x69, 0x64]).unwrap();
        assert_eq!(&out[d + 3..d + 5], &[28, 0]);
    }

    #[test]
    fn heat_shrink_print_info_and_area() {
        let tape = Tape::from_status(18, 0x11, 0x70, 0x08).unwrap(); // HS 17.7 mm: 106 pins at 11
        let mut bm = Bitmap::new(31, 106);
        bm.px[0] = 1; // top row -> pin 116
        bm.px[105 * 31] = 1; // bottom row -> pin 11
        let mut out = Vec::new();
        encode_page(&mut out, &bm, &tape, &PageOpts::default(), Caps::default(), 3, false);
        assert_eq!(&out[4..13], &[0x1B, 0x69, 0x7A, 0x86, 0x11, 18, 0, 31, 0]);
        assert_eq!(out[15], 1); // page 4 is an "other" page
        let (lines, term) = decode_raster(&out);
        assert_eq!(term, 0x0C);
        assert_eq!(u128::from_be_bytes(lines[0]), (1 << 116) | (1 << 11));
        assert!(lines[1..].iter().all(|l| *l == [0; LINE_BYTES]));
    }
}
