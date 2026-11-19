//! Finds printers: paired Bluetooth P-touch serial ports in /dev and Wi-Fi models
//! advertising `_pdl-datastream._tcp` over Bonjour.

use super::{Link, PrinterId};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const KNOWN: [&str; 7] =
    ["PT-P710BT", "PT-P300BT", "PT-P910BT", "PT-E550W", "PT-P750W", "PT-P900W", "PT-P950NW"];

pub fn model_from_name(name: &str) -> String {
    let up = name.to_ascii_uppercase();
    for k in KNOWN {
        if up.contains(k) {
            return k.into();
        }
    }
    up.find("PT-")
        .map(|i| {
            up[i..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect()
        })
        .unwrap_or_else(|| name.into())
}

/// Paired P-touch SPP devices, e.g. /dev/cu.PT-P710BT6431.
pub fn scan_serial() -> Vec<PrinterId> {
    let Ok(rd) = std::fs::read_dir("/dev") else { return vec![] };
    let mut v: Vec<PrinterId> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix("cu.")?;
            rest.to_ascii_uppercase().starts_with("PT-").then(|| PrinterId {
                model: model_from_name(rest),
                link: Link::Bluetooth(format!("/dev/{name}")),
            })
        })
        .collect();
    v.sort_by(|a, b| a.display().cmp(&b.display()));
    v
}

/// Background discovery; `on_change` gets the full list whenever it changes.
pub fn spawn(on_change: impl Fn(Vec<PrinterId>) + Send + 'static) {
    std::thread::Builder::new()
        .name("discovery".into())
        .spawn(move || {
            let daemon = ServiceDaemon::new().ok();
            let rx = daemon
                .as_ref()
                .and_then(|d| d.browse("_pdl-datastream._tcp.local.").ok());
            let mut net: BTreeMap<String, PrinterId> = BTreeMap::new();
            let mut last: Vec<PrinterId> = vec![];
            let mut next_scan = Instant::now();
            loop {
                if let Some(rx) = &rx {
                    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) {
                        match ev {
                            ServiceEvent::ServiceResolved(info) => {
                                let full = info.get_fullname().to_string();
                                let inst = full.split("._").next().unwrap_or(&full).replace("\\032", " ");
                                if !inst.to_ascii_uppercase().contains("PT-") {
                                    continue;
                                }
                                if let Some(ip) = info.get_addresses_v4().into_iter().next() {
                                    net.insert(
                                        full,
                                        PrinterId {
                                            model: model_from_name(&inst),
                                            link: Link::Network(ip.to_string()),
                                        },
                                    );
                                }
                            }
                            ServiceEvent::ServiceRemoved(_, full) => {
                                net.remove(&full);
                            }
                            _ => {}
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                }
                if Instant::now() >= next_scan {
                    next_scan = Instant::now() + Duration::from_secs(3);
                    let mut all = scan_serial();
                    for p in net.values() {
                        if !all.contains(p) {
                            all.push(p.clone());
                        }
                    }
                    if all != last {
                        last = all.clone();
                        on_change(all);
                    }
                }
            }
        })
        .expect("spawn discovery");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names() {
        assert_eq!(model_from_name("PT-P710BT6431"), "PT-P710BT");
        assert_eq!(model_from_name("Brother PT-E550W"), "PT-E550W");
        assert_eq!(model_from_name("PT-D610BT-abc"), "PT-D610BT-ABC");
    }
}
