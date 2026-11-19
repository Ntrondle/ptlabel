use serde::{Deserialize, Serialize};

pub const HEAD_PINS: usize = 128;
pub const LINE_BYTES: usize = HEAD_PINS / 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaKind {
    Laminated,
    NonLaminated,
    HeatShrink21,
    HeatShrink31,
    Other(u8),
}

impl MediaKind {
    /// Status byte 11 / print-info n2. `None` = no media.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0x00 => None,
            0x01 => Some(Self::Laminated),
            0x03 => Some(Self::NonLaminated),
            0x11 => Some(Self::HeatShrink21),
            0x17 => Some(Self::HeatShrink31),
            c => Some(Self::Other(c)),
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Laminated => 0x01,
            Self::NonLaminated => 0x03,
            Self::HeatShrink21 => 0x11,
            Self::HeatShrink31 => 0x17,
            Self::Other(c) => c,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Laminated => "laminated",
            Self::NonLaminated => "non-laminated",
            Self::HeatShrink21 => "heat-shrink 2:1",
            Self::HeatShrink31 => "heat-shrink 3:1",
            Self::Other(0xFF) => "incompatible",
            Self::Other(_) => "unknown media",
        }
    }
}

/// Geometry of one loaded cassette (from the spec's page-size tables).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tape {
    /// Width byte exactly as the printer reports it (mm). Sent back in `ESC i z`.
    pub width_code: u8,
    pub kind: MediaKind,
    /// Nominal size shown to the user.
    pub nominal_mm: f32,
    /// Physical tape width in 180-dpi dots (for the preview).
    pub tape_dots: u16,
    /// Printable pins (centred on the 128-pin head).
    pub print_pins: u16,
    pub tape_color: u8,
    pub text_color: u8,
}

// (status width byte, nominal mm, tape dots, print pins)
const TZE: [(u8, f32, u16, u16); 6] = [
    (4, 3.5, 24, 24),
    (6, 6.0, 42, 32),
    (9, 9.0, 64, 50),
    (12, 12.0, 84, 70),
    (18, 18.0, 128, 112),
    (24, 24.0, 170, 128),
];
const HS21: [(f32, u16, u16); 5] = [
    (5.8, 40, 28),
    (8.8, 62, 48),
    (11.7, 82, 66),
    (17.7, 126, 106),
    (23.6, 168, 128),
];
const HS31: [(f32, u16, u16); 4] = [(5.2, 36, 20), (9.0, 64, 44), (11.2, 80, 50), (21.0, 148, 120)];

/// TZe widths offered when designing offline.
pub const TZE_WIDTHS: [u8; 6] = [4, 6, 9, 12, 18, 24];

impl Tape {
    /// Standard TZe cassette (black on white) for offline design.
    pub fn tze(width_code: u8) -> Option<Self> {
        Self::from_status(width_code, 0x01, 0x01, 0x08)
    }

    pub fn from_status(width_code: u8, media: u8, tape_color: u8, text_color: u8) -> Option<Self> {
        let kind = MediaKind::from_code(media)?;
        if width_code == 0 {
            return None;
        }
        let nearest = |table: &[(f32, u16, u16)]| {
            *table
                .iter()
                .min_by(|a, b| {
                    (a.0 - width_code as f32).abs().total_cmp(&(b.0 - width_code as f32).abs())
                })
                .unwrap()
        };
        let (nominal_mm, tape_dots, print_pins) = match kind {
            MediaKind::HeatShrink21 => nearest(&HS21),
            MediaKind::HeatShrink31 => nearest(&HS31),
            _ => {
                let &(_, mm, dots, pins) = TZE.iter().find(|t| t.0 == width_code)?;
                (mm, dots, pins)
            }
        };
        Some(Self { width_code, kind, nominal_mm, tape_dots, print_pins, tape_color, text_color })
    }

    /// First head pin of the print area (areas are symmetric on the head).
    pub fn pin_offset(&self) -> usize {
        (HEAD_PINS - self.print_pins as usize) / 2
    }

    pub fn describe(&self) -> String {
        let w = if self.nominal_mm.fract() == 0.0 {
            format!("{:.0} mm", self.nominal_mm)
        } else {
            format!("{:.1} mm", self.nominal_mm)
        };
        format!(
            "{w} {} · {} on {}",
            self.kind.label(),
            text_color(self.text_color).0,
            tape_color(self.tape_color).0
        )
    }
}

/// Status byte 24.
pub fn tape_color(code: u8) -> (&'static str, [u8; 3]) {
    match code {
        0x01 => ("white", [250, 250, 248]),
        0x02 => ("other", [205, 205, 205]),
        0x03 => ("clear", [226, 232, 236]),
        0x04 => ("red", [214, 48, 49]),
        0x05 => ("blue", [41, 98, 201]),
        0x06 => ("yellow", [250, 214, 45]),
        0x07 => ("green", [56, 168, 82]),
        0x08 => ("black", [28, 28, 30]),
        0x09 => ("clear (white text)", [200, 208, 214]),
        0x20 => ("matte white", [244, 243, 238]),
        0x21 => ("matte clear", [222, 228, 230]),
        0x22 => ("matte silver", [190, 192, 196]),
        0x23 => ("satin gold", [206, 172, 92]),
        0x24 => ("satin silver", [200, 201, 206]),
        0x30 => ("blue (D)", [30, 80, 190]),
        0x31 => ("red (D)", [200, 40, 44]),
        0x40 => ("fluorescent orange", [255, 122, 40]),
        0x41 => ("fluorescent yellow", [232, 255, 60]),
        0x50 => ("berry pink", [220, 92, 150]),
        0x51 => ("light gray", [200, 200, 200]),
        0x52 => ("lime green", [150, 210, 70]),
        0x60 => ("yellow (fabric)", [250, 222, 70]),
        0x61 => ("pink (fabric)", [245, 152, 182]),
        0x62 => ("blue (fabric)", [122, 172, 232]),
        0x70 => ("white heat-shrink", [248, 248, 248]),
        0x90 => ("white flex-ID", [252, 252, 252]),
        0x91 => ("yellow flex-ID", [250, 214, 45]),
        0xF0 => ("cleaning", [220, 220, 220]),
        0xF1 => ("stencil", [238, 238, 238]),
        0xFF => ("incompatible", [180, 180, 180]),
        _ => ("unknown", [235, 235, 235]),
    }
}

/// Status byte 25.
pub fn text_color(code: u8) -> (&'static str, [u8; 3]) {
    match code {
        0x01 => ("white", [255, 255, 255]),
        0x02 => ("other", [60, 60, 60]),
        0x04 => ("red", [205, 30, 36]),
        0x05 => ("blue", [30, 70, 180]),
        0x08 => ("black", [16, 16, 18]),
        0x0A => ("gold", [196, 152, 44]),
        0x62 => ("blue", [40, 90, 200]),
        0xF0 => ("cleaning", [120, 120, 120]),
        0xF1 => ("stencil", [90, 90, 90]),
        0xFF => ("incompatible", [90, 90, 90]),
        _ => ("black", [16, 16, 18]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_areas_are_centred_on_head() {
        for w in TZE_WIDTHS {
            let t = Tape::tze(w).unwrap();
            assert_eq!(t.pin_offset() * 2 + t.print_pins as usize, HEAD_PINS);
        }
        assert_eq!(Tape::tze(12).unwrap().pin_offset(), 29);
        assert_eq!(Tape::tze(4).unwrap().pin_offset(), 52);
        assert_eq!(Tape::tze(24).unwrap().pin_offset(), 0);
    }

    #[test]
    fn heat_shrink_nearest() {
        let t = Tape::from_status(9, 0x17, 0x70, 0x08).unwrap();
        assert_eq!(t.print_pins, 44);
        let t = Tape::from_status(24, 0x11, 0x70, 0x08).unwrap();
        assert_eq!(t.print_pins, 128);
    }
}
