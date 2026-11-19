//! PT-P710BT over Bluetooth Classic SPP. macOS exposes a paired SPP device as
//! /dev/cu.<name>; opening it brings up the RFCOMM link.

use super::Transport;
use crate::proto::raster::{abort_sequence, job_header};
use crate::proto::status::{self, ST_COMPLETED, ST_ERROR, ST_TURNED_OFF, Status};
use anyhow::{Context, Result, anyhow, bail};
use serialport::SerialPort;
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

pub struct BtTransport {
    path: String,
    port: Option<Box<dyn SerialPort>>,
    buf: Vec<u8>,
}

impl BtTransport {
    pub fn new(path: String) -> Self {
        Self { path, port: None, buf: Vec::new() }
    }

    fn port(&mut self) -> Result<&mut Box<dyn SerialPort>> {
        if self.port.is_none() {
            let p = serialport::new(&self.path, 115_200)
                .timeout(Duration::from_millis(100))
                .open()
                .with_context(|| format!("open {} (is the printer on and paired?)", self.path))?;
            self.port = Some(p);
            self.buf.clear();
        }
        Ok(self.port.as_mut().unwrap())
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut off = 0;
        while off < data.len() {
            let end = (off + 1024).min(data.len());
            let r = self.port()?.write(&data[off..end]);
            match r {
                Ok(n) => off += n,
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock) => {
                    if Instant::now() > deadline {
                        self.port = None;
                        bail!("Bluetooth write timed out");
                    }
                }
                Err(e) => {
                    self.port = None;
                    return Err(anyhow!(e).context("Bluetooth write failed"));
                }
            }
        }
        let r = self.port()?.flush();
        if let Err(e) = r {
            self.port = None;
            return Err(anyhow!(e).context("Bluetooth flush failed"));
        }
        Ok(())
    }

    /// Read the next 32-byte status frame (reply or unsolicited notification).
    fn read_frame(&mut self, timeout: Duration) -> Result<Option<Status>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((s, used)) = Status::find(&self.buf) {
                self.buf.drain(..used);
                return Ok(Some(s));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let mut tmp = [0u8; 64];
            let r = self.port()?.read(&mut tmp);
            match r {
                Ok(0) => {}
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
                Err(e) => {
                    self.port = None;
                    return Err(anyhow!(e).context("Bluetooth read failed"));
                }
            }
        }
    }

    fn drain(&mut self) -> Result<()> {
        while self.read_frame(Duration::from_millis(30))?.is_some() {}
        self.buf.clear();
        Ok(())
    }

    fn request_status(&mut self) -> Result<Status> {
        self.write_all(&status::REQUEST)?;
        match self.read_frame(Duration::from_secs(4))? {
            Some(s) => Ok(s),
            None => {
                self.port = None; // force reconnect next time
                bail!("no status reply over Bluetooth")
            }
        }
    }
}

impl Transport for BtTransport {
    fn status(&mut self) -> Result<Status> {
        self.drain()?;
        self.request_status()
    }

    fn begin(&mut self) -> Result<Status> {
        self.drain()?;
        self.write_all(&job_header())?;
        self.request_status()
    }

    fn send_page(&mut self, data: &[u8], label_mm: f32) -> Result<()> {
        self.write_all(data)?;
        // P710BT prints ~20 mm/s; it reports phase changes then "printing completed".
        let timeout = Duration::from_secs_f32(10.0 + label_mm / 10.0);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Some(s) = self.read_frame(deadline - Instant::now())? else { break };
            match s.status_type() {
                ST_COMPLETED => return Ok(()),
                ST_ERROR => bail!("printer error: {}", s.summary()),
                ST_TURNED_OFF => bail!("printer turned off"),
                _ => {}
            }
        }
        // No completion notice: carry on; the next page/status will surface errors.
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    fn abort(&mut self) {
        let _ = self.write_all(&abort_sequence());
    }

    fn close(&mut self) {
        self.port = None;
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(3)
    }
}
