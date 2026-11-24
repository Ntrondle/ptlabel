//! PT-P710BT over Bluetooth Classic SPP. On macOS the RFCOMM channel is opened directly
//! (rfcomm.rs); elsewhere through the serial device of the paired printer.

use super::Transport;
use crate::proto::raster::{abort_sequence, job_header};
use crate::proto::status::{self, ST_COMPLETED, ST_ERROR, ST_TURNED_OFF, Status};
use anyhow::{Result, anyhow, bail};
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

/// The byte stream to the printer. Reads wait briefly and fail with TimedOut when nothing came;
/// Ok(0) means the link is gone.
trait Port: Read + Write + Send {}
impl<T: Read + Write + Send> Port for T {}

pub struct BtTransport {
    /// Device path / identifier (`/dev/cu.<name>`, `bt:<name>`, `COM<n>`, or Windows USB path).
    path: String,
    usb: bool,
    port: Option<Box<dyn Port>>,
    buf: Vec<u8>,
}

#[cfg(target_os = "macos")]
fn connect(path: &str, usb: bool) -> Result<Box<dyn Port>> {
    if usb {
        bail!("USB printing is currently only supported on Windows");
    }
    let name = path.strip_prefix("bt:").unwrap_or(path);
    let name = name.rsplit('/').next().unwrap_or(name);
    let name = name.trim_start_matches("cu.").trim_start_matches("tty.");
    Ok(Box::new(super::rfcomm::Rfcomm::open(name)?))
}

#[cfg(target_os = "windows")]
fn connect(path: &str, usb: bool) -> Result<Box<dyn Port>> {
    if usb {
        return Ok(Box::new(super::usb_win::UsbPrint::open(path)?));
    }
    if super::is_com_port(path) {
        use anyhow::Context;
        let p = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(100))
            .open()
            .with_context(|| format!("open {path} (is the printer on and paired?)"))?;
        return Ok(Box::new(p));
    }
    let name = path.strip_prefix("bt:").unwrap_or(path);
    Ok(Box::new(super::bt_win::Rfcomm::open(name)?))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn connect(path: &str, usb: bool) -> Result<Box<dyn Port>> {
    if usb {
        bail!("USB printing is currently only supported on Windows");
    }
    use anyhow::Context;
    let p = serialport::new(path, 115_200)
        .timeout(Duration::from_millis(100))
        .open()
        .with_context(|| format!("open {path} (is the printer on and paired?)"))?;
    Ok(Box::new(p))
}

impl BtTransport {
    pub fn new(path: String) -> Self {
        Self { path, usb: false, port: None, buf: Vec::new() }
    }

    pub fn new_usb(path: String) -> Self {
        Self { path, usb: true, port: None, buf: Vec::new() }
    }

    fn kind(&self) -> &'static str {
        if self.usb { "USB" } else { "Bluetooth" }
    }

    fn port(&mut self) -> Result<&mut Box<dyn Port>> {
        if self.port.is_none() {
            self.port = Some(connect(&self.path, self.usb)?);
            self.buf.clear();
        }
        Ok(self.port.as_mut().unwrap())
    }

    /// Close the link; the next call reconnects.
    fn drop_port(&mut self) {
        self.port = None;
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let kind = self.kind();
        // No-progress timeout: a long page over slow SPP may take longer than 30 s in total.
        let mut deadline = Instant::now() + Duration::from_secs(30);
        let mut off = 0;
        while off < data.len() {
            let end = (off + 1024).min(data.len());
            let r = self.port()?.write(&data[off..end]);
            match r {
                Ok(0) => {
                    self.drop_port();
                    bail!("{kind} write failed: link closed");
                }
                Ok(n) => {
                    off += n;
                    deadline = Instant::now() + Duration::from_secs(30);
                }
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock) => {
                    if Instant::now() > deadline {
                        self.drop_port();
                        bail!("{kind} write timed out");
                    }
                }
                Err(e) => {
                    self.drop_port();
                    return Err(anyhow!(e).context(format!("{kind} write failed")));
                }
            }
        }
        Ok(())
    }

    /// Read the next 32-byte status frame (reply or unsolicited notification).
    fn read_frame(&mut self, timeout: Duration) -> Result<Option<Status>> {
        let kind = self.kind();
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
                // Raw tty (VMIN=1): read() only returns 0 at end of file, i.e. the link hung up.
                Ok(0) => {
                    self.drop_port();
                    bail!("{kind} link closed");
                }
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
                Err(e) => {
                    self.drop_port();
                    return Err(anyhow!(e).context(format!("{kind} read failed")));
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
        let kind = self.kind();
        self.write_all(&status::REQUEST)?;
        match self.read_frame(Duration::from_secs(4))? {
            Some(s) if s.status_type() == ST_TURNED_OFF => {
                self.drop_port();
                bail!("printer turned off")
            }
            Some(s) => Ok(s),
            None => {
                self.drop_port(); // force reconnect next time
                bail!("no status reply over {kind}")
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
                ST_TURNED_OFF => {
                    self.drop_port(); // the link is gone; reopen on the next call
                    bail!("printer turned off")
                }
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
        self.drop_port();
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(3)
    }
}

impl Drop for BtTransport {
    fn drop(&mut self) {
        self.drop_port();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Scripted serial port: `rx` chunks are returned by successive reads (empty → timeout);
    /// each status request written moves the next `replies` entry into `rx`.
    #[derive(Default)]
    struct Mock {
        rx: VecDeque<Vec<u8>>,
        replies: VecDeque<Vec<Vec<u8>>>,
        tx: Arc<Mutex<Vec<u8>>>,
        eof: bool,
        write_zero: bool,
    }

    impl Read for Mock {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.eof {
                return Ok(0);
            }
            let Some(mut c) = self.rx.pop_front() else {
                std::thread::sleep(Duration::from_millis(2));
                return Err(ErrorKind::TimedOut.into());
            };
            let n = c.len().min(b.len());
            b[..n].copy_from_slice(&c[..n]);
            if n < c.len() {
                self.rx.push_front(c.split_off(n));
            }
            Ok(n)
        }
    }

    impl Write for Mock {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.write_zero {
                return Ok(0);
            }
            self.tx.lock().unwrap().extend_from_slice(b);
            if b.ends_with(&status::REQUEST) {
                self.rx.extend(self.replies.pop_front().unwrap_or_default());
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn frame(width: u8, status_type: u8) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[..5].copy_from_slice(&[0x80, 0x20, 0x42, 0x30, 0x71]);
        b[10] = width;
        b[11] = 0x01;
        b[18] = status_type;
        b
    }

    /// No printer has this name, so a reconnect attempt would fail without touching any device.
    fn transport(m: Mock) -> BtTransport {
        let mut t = BtTransport::new("/nonexistent/cu.PT-test".into());
        t.port = Some(Box::new(m));
        t
    }

    #[test]
    fn status_drops_stale_frame_and_reassembles_split_reply() {
        let reply = frame(12, 0x00);
        let m = Mock {
            rx: VecDeque::from([frame(6, 0x06)]),
            replies: VecDeque::from([vec![reply[..10].to_vec(), reply[10..].to_vec()]]),
            ..Default::default()
        };
        let tx = m.tx.clone();
        let mut t = transport(m);
        assert_eq!(t.status().unwrap().width(), 12);
        assert_eq!(*tx.lock().unwrap(), status::REQUEST);
        assert!(t.port.is_some());
    }

    #[test]
    fn turned_off_drops_port() {
        let m = Mock { replies: VecDeque::from([vec![frame(12, ST_TURNED_OFF)]]), ..Default::default() };
        let mut t = transport(m);
        assert!(t.status().is_err());
        assert!(t.port.is_none());

        let m = Mock { rx: VecDeque::from([frame(12, 0x06), frame(12, ST_TURNED_OFF)]), ..Default::default() };
        let mut t = transport(m);
        assert!(t.send_page(&[0x1A], 10.0).is_err());
        assert!(t.port.is_none());
    }

    #[test]
    fn hung_up_link_fails_fast() {
        let start = Instant::now();
        let mut t = transport(Mock { eof: true, ..Default::default() });
        assert!(format!("{:#}", t.status().unwrap_err()).contains("closed"));
        assert!(t.port.is_none());
        let mut t = transport(Mock { write_zero: true, ..Default::default() });
        assert!(t.status().is_err());
        assert!(t.port.is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
