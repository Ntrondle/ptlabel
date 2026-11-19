use super::media::Tape;
use anyhow::{Result, bail};

pub const STATUS_LEN: usize = 32;
pub const REQUEST: [u8; 3] = [0x1B, 0x69, 0x53];

pub const ST_COMPLETED: u8 = 0x01;
pub const ST_ERROR: u8 = 0x02;
pub const ST_TURNED_OFF: u8 = 0x04;

/// 32-byte status block (`ESC i S` reply, automatic notification, or SNMP).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub raw: [u8; STATUS_LEN],
}

impl Status {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < STATUS_LEN {
            bail!("status too short ({} bytes)", b.len());
        }
        if b[0] != 0x80 || b[1] != 0x20 || b[2] != 0x42 {
            bail!("bad status header {:02X} {:02X} {:02X}", b[0], b[1], b[2]);
        }
        let mut raw = [0u8; STATUS_LEN];
        raw.copy_from_slice(&b[..STATUS_LEN]);
        Ok(Self { raw })
    }

    /// Find a status frame anywhere in `buf`; returns (status, bytes consumed).
    pub fn find(buf: &[u8]) -> Option<(Self, usize)> {
        let start = buf.windows(3).position(|w| w == [0x80, 0x20, 0x42])?;
        let s = Self::parse(&buf[start..]).ok()?;
        Some((s, start + STATUS_LEN))
    }

    pub fn model_code(&self) -> u8 {
        self.raw[4]
    }
    pub fn err1(&self) -> u8 {
        self.raw[8]
    }
    pub fn err2(&self) -> u8 {
        self.raw[9]
    }
    pub fn width(&self) -> u8 {
        self.raw[10]
    }
    pub fn media(&self) -> u8 {
        self.raw[11]
    }
    pub fn status_type(&self) -> u8 {
        self.raw[18]
    }
    pub fn phase_type(&self) -> u8 {
        self.raw[19]
    }
    pub fn tape_color(&self) -> u8 {
        self.raw[24]
    }
    pub fn text_color(&self) -> u8 {
        self.raw[25]
    }

    pub fn model_name(&self) -> Option<&'static str> {
        match self.model_code() {
            0x66 => Some("PT-E550W"),
            0x68 => Some("PT-P750W"),
            _ => None,
        }
    }

    pub fn is_printing(&self) -> bool {
        self.phase_type() == 0x01
    }

    pub fn errors(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        let (e1, e2) = (self.err1(), self.err2());
        if e1 & 0x01 != 0 {
            v.push("No tape");
        }
        if e1 & 0x04 != 0 {
            v.push("Cutter jam");
        }
        if e1 & 0x08 != 0 {
            v.push("Weak batteries");
        }
        if e1 & 0x40 != 0 {
            v.push("Wrong AC adapter");
        }
        if e2 & 0x01 != 0 {
            v.push("Wrong tape for this job");
        }
        if e2 & 0x10 != 0 {
            v.push("Cover open");
        }
        if e2 & 0x20 != 0 {
            v.push("Overheated");
        }
        if self.media() == 0xFF {
            v.push("Incompatible tape");
        }
        v
    }

    pub fn tape(&self) -> Option<Tape> {
        if self.media() == 0xFF {
            return None;
        }
        Tape::from_status(self.width(), self.media(), self.tape_color(), self.text_color())
    }

    /// Short human summary for the status pill.
    pub fn summary(&self) -> String {
        let errs = self.errors();
        if !errs.is_empty() {
            return errs.join(", ");
        }
        if self.is_printing() {
            return "Printing".into();
        }
        match self.tape() {
            Some(_) => "Ready".into(),
            None => "No tape".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real SNMP reply from the PT-E550W at 192.168.1.160 (12 mm TZe black on white).
    const E550W: [u8; 32] = [
        0x80, 0x20, 0x42, 0x30, 0x66, 0x30, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];

    #[test]
    fn parses_real_e550w_status() {
        use crate::proto::media::MediaKind;
        let s = Status::parse(&E550W).unwrap();
        assert_eq!(s.model_name(), Some("PT-E550W"));
        assert!(s.errors().is_empty());
        let t = s.tape().unwrap();
        assert_eq!(t.width_code, 12);
        assert_eq!(t.print_pins, 70);
        assert_eq!(t.kind, MediaKind::Laminated);
        assert_eq!(t.describe(), "12 mm laminated · black on white");
        assert_eq!(s.summary(), "Ready");
    }

    #[test]
    fn finds_frame_in_noise() {
        let mut buf = vec![0x00, 0x13];
        buf.extend_from_slice(&E550W);
        buf.push(0x99);
        let (s, used) = Status::find(&buf).unwrap();
        assert_eq!(used, 34);
        assert_eq!(s.width(), 12);
    }

    #[test]
    fn errors_decode() {
        let mut b = E550W;
        b[8] = 0x01;
        b[9] = 0x10;
        let s = Status::parse(&b).unwrap();
        assert_eq!(s.errors(), vec!["No tape", "Cover open"]);
    }
}
