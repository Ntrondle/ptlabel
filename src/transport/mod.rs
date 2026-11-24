pub mod bt;
#[cfg(target_os = "windows")]
mod bt_win;
pub mod discovery;
pub mod net;
#[cfg(target_os = "macos")]
mod rfcomm;
pub mod snmp;
#[cfg(target_os = "windows")]
mod usb_win;

use crate::proto::{Caps, Status};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Link {
    /// macOS serial device or Windows `bt:<name>` / `COM<n>` for a paired Bluetooth SPP printer
    Bluetooth(String),
    /// USB printer interface path (Windows `\\?\usb#...` via usbprint.sys)
    Usb(String),
    /// Host/IP of a Wi-Fi printer (raw 9100 + SNMP)
    Network(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrinterId {
    pub model: String,
    pub link: Link,
}

impl PrinterId {
    pub fn display(&self) -> String {
        match &self.link {
            Link::Bluetooth(p) => format!(
                "{} · Bluetooth ({})",
                self.model,
                p.trim_start_matches("/dev/cu.").trim_start_matches("bt:")
            ),
            Link::Usb(_) => format!("{} · USB", self.model),
            Link::Network(h) => format!("{} · Wi-Fi {h}", self.model),
        }
    }

    pub fn caps(&self) -> Caps {
        let m = self.model.to_ascii_uppercase();
        let full = m.contains("E550") || m.contains("P750") || m.contains("P900") || m.contains("P950");
        Caps { half_cut: full, cut_every: full }
    }

    /// Parse a CLI target: `/dev/...`, `bt:<name-or-MAC>`, `usb` / `usb:<model>` / `\\?\usb...`,
    /// Windows `COM<n>`, or a Wi-Fi host/IP.
    pub fn from_target(t: &str) -> Self {
        if t.starts_with("/dev/") {
            let name = t.trim_start_matches("/dev/cu.").trim_start_matches("/dev/tty.");
            Self { model: discovery::model_from_name(name), link: Link::Bluetooth(t.into()) }
        } else if let Some(name) = t.strip_prefix("bt:").or_else(|| t.strip_prefix("BT:")) {
            Self { model: discovery::model_from_name(name), link: Link::Bluetooth(format!("bt:{name}")) }
        } else if t.eq_ignore_ascii_case("usb")
            || t.starts_with("usb:")
            || t.starts_with("USB:")
            || t.to_ascii_lowercase().starts_with(r"\\?\usb")
        {
            Self { model: "PT-P710BT".into(), link: Link::Usb(t.into()) }
        } else if is_com_port(t) {
            Self { model: "PT-P710BT".into(), link: Link::Bluetooth(t.to_ascii_uppercase()) }
        } else {
            Self { model: "PT-E550W".into(), link: Link::Network(t.into()) }
        }
    }

    pub fn open(&self) -> Box<dyn Transport> {
        match &self.link {
            Link::Bluetooth(p) => Box::new(bt::BtTransport::new(p.clone())),
            Link::Usb(p) => Box::new(bt::BtTransport::new_usb(p.clone())),
            Link::Network(h) => Box::new(net::NetTransport::new(h.clone())),
        }
    }
}

/// A connection to one printer. Implementations differ in how status is obtained:
/// Bluetooth replies on the same channel; Wi-Fi uses SNMP because port 9100 is send-only.
pub trait Transport: Send {
    fn status(&mut self) -> Result<Status>;
    /// Start a job: sends invalidate + initialize, returns fresh status.
    fn begin(&mut self) -> Result<Status>;
    /// Send one encoded page. `label_mm` is used for timeouts.
    fn send_page(&mut self, data: &[u8], label_mm: f32) -> Result<()>;
    /// Finish the job and wait until the printer is done.
    fn finish(&mut self) -> Result<()>;
    /// Best-effort cancel.
    fn abort(&mut self);
    /// Release the link (Bluetooth: closes the RFCOMM channel).
    fn close(&mut self) {}
    /// Idle poll interval.
    fn poll_interval(&self) -> std::time::Duration;
}

/// True for Windows COM ports such as `COM3`, `com12`, or `\\.\COM12`.
pub(crate) fn is_com_port(s: &str) -> bool {
    let rest = s.strip_prefix(r"\\.\").unwrap_or(s);
    rest.len() >= 4
        && rest[..3].eq_ignore_ascii_case("COM")
        && rest[3..].bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets_for_macos_windows_and_wifi() {
        let mac = PrinterId::from_target("/dev/cu.PT-P710BT9475");
        assert_eq!(mac.link, Link::Bluetooth("/dev/cu.PT-P710BT9475".into()));
        assert_eq!(mac.display(), "PT-P710BT · Bluetooth (PT-P710BT9475)");

        let win = PrinterId::from_target("bt:PT-P710BT9475");
        assert_eq!(win.link, Link::Bluetooth("bt:PT-P710BT9475".into()));
        assert_eq!(win.display(), "PT-P710BT · Bluetooth (PT-P710BT9475)");

        let com = PrinterId::from_target("com5");
        assert_eq!(com.link, Link::Bluetooth("COM5".into()));
        assert!(is_com_port(r"\\.\COM12"));
        assert!(!is_com_port("com"));
        assert!(!is_com_port("company.local"));

        let usb = PrinterId::from_target("usb");
        assert_eq!(usb.link, Link::Usb("usb".into()));
        assert_eq!(usb.display(), "PT-P710BT · USB");

        let net = PrinterId::from_target("192.168.1.160");
        assert_eq!(net.link, Link::Network("192.168.1.160".into()));
    }
}
