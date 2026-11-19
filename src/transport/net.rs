//! PT-E550W in Wi-Fi infrastructure mode: raw TCP 9100 for data (send-only per the
//! spec's network flow), SNMP for the 32-byte status block.

use super::Transport;
use super::snmp::{self, BROTHER_STATUS_OID};
use crate::proto::Status;
use crate::proto::raster::{abort_sequence, job_header};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub struct NetTransport {
    host: String,
    stream: Option<TcpStream>,
    pages: u32,
    total_mm: f32,
}

impl NetTransport {
    pub fn new(host: String) -> Self {
        Self { host, stream: None, pages: 0, total_mm: 0.0 }
    }

    fn snmp_status(&self) -> Result<Status> {
        let v = snmp::get(&self.host, BROTHER_STATUS_OID, Duration::from_millis(1200))?;
        Status::parse(&v)
    }
}

impl Transport for NetTransport {
    fn status(&mut self) -> Result<Status> {
        self.snmp_status()
    }

    fn begin(&mut self) -> Result<Status> {
        let st = self.snmp_status().context("read status via SNMP")?;
        let addr = (self.host.as_str(), 9100)
            .to_socket_addrs()?
            .next()
            .context("resolve printer address")?;
        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .with_context(|| format!("connect {}:9100", self.host))?;
        s.set_nodelay(true)?;
        s.set_write_timeout(Some(Duration::from_secs(60)))?;
        self.stream = Some(s);
        self.pages = 0;
        self.total_mm = 0.0;
        self.stream.as_mut().unwrap().write_all(&job_header())?;
        Ok(st)
    }

    fn send_page(&mut self, data: &[u8], label_mm: f32) -> Result<()> {
        if self.pages > 0 {
            // Catch tape-out / cover-open between labels instead of stalling on a full buffer.
            if let Ok(st) = self.snmp_status() {
                let errs = st.errors();
                if !errs.is_empty() {
                    bail!("printer error: {}", errs.join(", "));
                }
            }
        }
        let s = self.stream.as_mut().context("job not started")?;
        s.write_all(data).context("send to printer")?;
        self.pages += 1;
        self.total_mm += label_mm;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(s) = self.stream.take() {
            let mut s = s;
            s.flush()?;
            let _ = s.shutdown(Shutdown::Write);
        }
        // Wait for the printer to drain its buffer (E550W ~30 mm/s incl. cutting).
        let limit = Duration::from_secs_f32(8.0 + self.total_mm / 15.0 + self.pages as f32 * 1.5);
        let start = Instant::now();
        let mut seen_printing = false;
        while start.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(400));
            let Ok(st) = self.snmp_status() else { continue };
            let errs = st.errors();
            if !errs.is_empty() {
                bail!("printer error: {}", errs.join(", "));
            }
            if st.is_printing() {
                seen_printing = true;
            } else if seen_printing || start.elapsed() > Duration::from_secs(4) {
                return Ok(());
            }
        }
        Ok(())
    }

    fn abort(&mut self) {
        if let Some(mut s) = self.stream.take() {
            let _ = s.write_all(&abort_sequence());
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(2)
    }
}
