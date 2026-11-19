pub mod bt;
pub mod discovery;
pub mod net;
pub mod snmp;

use crate::proto::{Caps, Status};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Link {
    /// macOS serial device for a paired Bluetooth SPP printer, e.g. /dev/cu.PT-P710BT6431
    Bluetooth(String),
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
                p.trim_start_matches("/dev/cu.")
            ),
            Link::Network(h) => format!("{} · Wi-Fi {h}", self.model),
        }
    }

    pub fn caps(&self) -> Caps {
        let m = self.model.to_ascii_uppercase();
        let full = m.contains("E550") || m.contains("P750") || m.contains("P900") || m.contains("P950");
        Caps { half_cut: full, cut_every: full }
    }

    /// Parse a CLI target: /dev path, "bt", or a host.
    pub fn from_target(t: &str) -> Self {
        if t.starts_with("/dev/") {
            let name = t.trim_start_matches("/dev/cu.").trim_start_matches("/dev/tty.");
            Self { model: discovery::model_from_name(name), link: Link::Bluetooth(t.into()) }
        } else {
            Self { model: "PT-E550W".into(), link: Link::Network(t.into()) }
        }
    }

    pub fn open(&self) -> Box<dyn Transport> {
        match &self.link {
            Link::Bluetooth(p) => Box::new(bt::BtTransport::new(p.clone())),
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
