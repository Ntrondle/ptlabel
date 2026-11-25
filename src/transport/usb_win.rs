//! Brother P-touch printers over USB on Windows via the built-in USB printer class
//! driver (`usbprint.sys`, `GUID_DEVINTERFACE_USBPRINT`).
//!
//! Works out of the box without Zadig/WinUSB: enumerates connected `VID_04F9` P-touch
//! interfaces via SetupAPI and talks the same bidirectional 32-byte-status + raster
//! protocol over overlapped `ReadFile`/`WriteFile`. `PTLABEL_USB_TRACE=1` logs the transfers.

use super::{Link, PrinterId};
use anyhow::{Context, Result, bail};
use std::io::{self, ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::Duration;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::core::GUID;

/// `GUID_DEVINTERFACE_USBPRINT`: `{28d78fad-5a12-11d1-ae5b-0000f803a8c2}`.
const GUID_DEVINTERFACE_USBPRINT: GUID = GUID::from_u128(0x28d78fad_5a12_11d1_ae5b_0000f803a8c2);

/// `IOCTL_USBPRINT_GET_1284_ID` (`CTL_CODE(FILE_DEVICE_UNKNOWN, 13, METHOD_BUFFERED, FILE_ANY_ACCESS)`).
const IOCTL_USBPRINT_GET_1284_ID: u32 = 0x220034;

const BROTHER_VID: u16 = 0x04F9;

/// Known Brother P-touch USB Product IDs (VID 0x04F9). Unknown P-touch models still work via
/// `IOCTL_USBPRINT_GET_1284_ID` (`MDL:PT-...`).
fn model_from_pid(pid: u16) -> Option<&'static str> {
    match pid {
        0x205E => Some("PT-H500"),
        0x205F => Some("PT-E500"),
        0x2060 => Some("PT-E550W"),
        0x2061 => Some("PT-P700"),
        0x2062 => Some("PT-P750W"),
        0x2074 => Some("PT-D600"),
        0x2085 => Some("PT-P900W"),
        0x2086 => Some("PT-P950NW"),
        0x20AF => Some("PT-P710BT"),
        0x20C7 => Some("PT-P910BT"),
        0x20E0 | 0x20E1 => Some("PT-D610BT"),
        0x20E2 => Some("PT-D460BT"),
        _ => None,
    }
}

fn parse_usb_vid_pid(path: &str) -> Option<(u16, u16)> {
    let low = path.to_ascii_lowercase();
    let hex4 = |tag: &str| -> Option<u16> {
        let i = low.find(tag)? + tag.len();
        u16::from_str_radix(low.get(i..i + 4)?, 16).ok()
    };
    Some((hex4("vid_")?, hex4("pid_")?))
}

fn parse_1284_model(raw: &[u8]) -> Option<String> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s = String::from_utf8_lossy(&raw[..end]);
    for part in s.split(';') {
        let p = part.trim();
        let val = p
            .strip_prefix("MDL:")
            .or_else(|| p.strip_prefix("MODEL:"))
            .or_else(|| p.strip_prefix("mdl:"))
            .or_else(|| p.strip_prefix("model:"));
        if let Some(v) = val {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Query IEEE 1284 device ID string from an unknown Brother USB printer so we can recognize
/// newer P-touch models while skipping Brother laser/inkjet printers (HL-/MFC-/DCP-).
fn query_1284_model(path: &str) -> Option<String> {
    let wide = to_wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut out = [0u8; 512];
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_USBPRINT_GET_1284_ID,
            std::ptr::null(),
            0,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 || returned == 0 {
        return None;
    }
    parse_1284_model(&out[..returned as usize])
}

/// Enumerate connected Brother P-touch USB printers via `GUID_DEVINTERFACE_USBPRINT`.
pub fn scan_usb() -> Vec<PrinterId> {
    let dev_info = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_USBPRINT,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if dev_info == INVALID_HANDLE_VALUE as isize {
        return vec![];
    }

    let mut out = Vec::new();
    let mut index: u32 = 0;
    loop {
        let mut iface: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        iface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe {
            SetupDiEnumDeviceInterfaces(dev_info, std::ptr::null(), &GUID_DEVINTERFACE_USBPRINT, index, &mut iface)
        } == 0
        {
            break;
        }
        index += 1;

        let mut required: u32 = 0;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface,
                std::ptr::null_mut(),
                0,
                &mut required,
                std::ptr::null_mut(),
            );
        }
        if required < std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32
            || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER
        {
            continue;
        }

        // 4-byte aligned buffer for SP_DEVICE_INTERFACE_DETAIL_DATA_W + trailing UTF-16 path.
        let mut buf = vec![0u32; (required as usize).div_ceil(4)];
        let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        unsafe {
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }
        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(dev_info, &iface, detail, required, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if ok == 0 {
            continue;
        }

        let path_offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        let max_u16 = (required as usize).saturating_sub(path_offset) / 2;
        let path = unsafe {
            let ptr = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
            let slice = std::slice::from_raw_parts(ptr, max_u16);
            let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
            String::from_utf16_lossy(&slice[..len])
        };

        let Some((vid, pid)) = parse_usb_vid_pid(&path) else { continue };
        if vid != BROTHER_VID {
            continue;
        }

        let model = if let Some(m) = model_from_pid(pid) {
            m.to_string()
        } else if let Some(mdl) = query_1284_model(&path) {
            if !mdl.to_ascii_uppercase().contains("PT-") {
                continue;
            }
            super::discovery::model_from_name(&mdl)
        } else {
            continue;
        };

        out.push(PrinterId { model, link: Link::Usb(path) });
    }

    unsafe { SetupDiDestroyDeviceInfoList(dev_info) };
    out.sort_by_key(|a| a.display());
    out.dedup();
    out
}

/// Bulk-IN read size: a whole high-speed packet, so a 32-byte status always fits.
const READ_LEN: usize = 512;
/// Largest single `WriteFile` (BtTransport writes 1 KiB chunks).
const WRITE_LEN: usize = 4096;

/// `PTLABEL_USB_TRACE=1`: log USB transfers to stderr (e.g. `ptlabel status usb` in a terminal).
fn trace(msg: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("PTLABEL_USB_TRACE").is_some()) {
        eprintln!("usb: {}", msg());
    }
}

/// One overlapped transfer. The OVERLAPPED and buffer live on the heap and are only freed once
/// the driver is done with them: a transfer that never completes is leaked, never freed under it.
struct Xfer {
    ov: ManuallyDrop<Box<OVERLAPPED>>,
    buf: ManuallyDrop<Vec<u8>>,
    pending: bool,
}

impl Xfer {
    fn new(len: usize) -> io::Result<Self> {
        let ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if ev.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut ov: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        ov.hEvent = ev;
        Ok(Self { ov: ManuallyDrop::new(ov), buf: ManuallyDrop::new(vec![0; len]), pending: false })
    }

    /// Mark the transfer started by `ReadFile`/`WriteFile` (`ok` = its return value) as in flight.
    /// Completed or queued, the event reports the result either way.
    fn started(&mut self, ok: i32) -> io::Result<()> {
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }
        self.pending = true;
        Ok(())
    }

    /// Wait up to `ms` for the transfer: `None` while it is still running, else its result.
    fn wait(&mut self, handle: HANDLE, ms: u32) -> Option<io::Result<usize>> {
        match unsafe { WaitForSingleObject(self.ov.hEvent, ms) } {
            WAIT_TIMEOUT => None,
            WAIT_OBJECT_0 => {
                self.pending = false;
                let mut n: u32 = 0;
                Some(if unsafe { GetOverlappedResult(handle, &**self.ov, &mut n, 0) } != 0 {
                    Ok(n as usize)
                } else {
                    Err(io::Error::last_os_error())
                })
            }
            _ => Some(Err(io::Error::last_os_error())),
        }
    }

    /// Cancel and give the driver `ms` to let go; `false` if the transfer is still pending.
    fn cancel(&mut self, handle: HANDLE, ms: u32) -> bool {
        if self.pending {
            unsafe { CancelIoEx(handle, &**self.ov) };
            let _ = self.wait(handle, ms);
        }
        !self.pending
    }
}

impl Drop for Xfer {
    fn drop(&mut self) {
        if self.pending {
            // The driver may still fill the buffer and signal the event: leak both.
            trace(|| "transfer still pending at close, leaking its buffer".into());
            return;
        }
        unsafe {
            CloseHandle(self.ov.hEvent);
            ManuallyDrop::drop(&mut self.ov);
            ManuallyDrop::drop(&mut self.buf);
        }
    }
}

/// Overlapped Win32 handle to a `usbprint.sys` device interface.
pub struct UsbPrint {
    handle: HANDLE,
    rd: Xfer,
    wr: Xfer,
    /// Bytes read but not handed out yet.
    rx: Vec<u8>,
}

// SAFETY: Win32 file and event handles are safe to transfer across threads.
unsafe impl Send for UsbPrint {}

impl UsbPrint {
    pub fn open(target: &str) -> Result<Self> {
        let resolved = if target.eq_ignore_ascii_case("usb") {
            match scan_usb().into_iter().next() {
                Some(PrinterId { link: Link::Usb(p), .. }) => p,
                _ => bail!("no Brother P-touch USB printer found (is it on with Editor Lite off?)"),
            }
        } else if let Some(q) = target.strip_prefix("usb:").or_else(|| target.strip_prefix("USB:")) {
            if q.starts_with(r"\\?\") {
                q.to_string()
            } else {
                let low = q.to_ascii_lowercase();
                let found = scan_usb().into_iter().find(|p| {
                    low.is_empty()
                        || p.model.to_ascii_lowercase().contains(&low)
                        || matches!(&p.link, Link::Usb(s) if s.to_ascii_lowercase().contains(&low))
                });
                match found {
                    Some(PrinterId { link: Link::Usb(p), .. }) => p,
                    _ => bail!("USB printer {q:?} not found (is it plugged in and powered on?)"),
                }
            }
        } else {
            target.to_string()
        };

        trace(|| format!("open {resolved}"));
        let wide = to_wide(&resolved);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let err = io::Error::last_os_error();
            trace(|| format!("open failed: {err}"));
            return Err(err).context("can't open USB printer (is it powered on with Editor Lite off?)");
        }

        let xfers = Xfer::new(READ_LEN).and_then(|rd| Ok((rd, Xfer::new(WRITE_LEN)?)));
        let (rd, wr) = match xfers {
            Ok(x) => x,
            Err(e) => {
                unsafe { CloseHandle(handle) };
                return Err(e).context("CreateEventW failed");
            }
        };
        Ok(Self { handle, rd, wr, rx: Vec::new() })
    }
}

impl Read for UsbPrint {
    /// Waits up to ~100 ms for data. A read still pending then stays queued and is collected by
    /// the next call instead of being cancelled: usbprint.sys may not finish a cancelled Bulk-IN
    /// read before data arrives, and waiting for that blocked the printer worker for good
    /// (the UI stuck at "Connecting…"). Keeping it queued also can't drop a reply mid-cancel.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.rx.is_empty() {
            if !self.rd.pending {
                let x = &mut self.rd;
                let ok = unsafe {
                    ReadFile(self.handle, x.buf.as_mut_ptr(), x.buf.len() as u32, std::ptr::null_mut(), &mut **x.ov)
                };
                x.started(ok).inspect_err(|e| trace(|| format!("ReadFile failed: {e}")))?;
            }
            let n = match self.rd.wait(self.handle, 100) {
                None => return Err(ErrorKind::TimedOut.into()),
                Some(r) => r.inspect_err(|e| trace(|| format!("read failed: {e}")))?,
            };
            // usbprint.sys can complete a Bulk-IN read with 0 bytes (ZLP) when idle; treat as a
            // timeout rather than EOF so BtTransport keeps waiting instead of dropping the handle.
            if n == 0 {
                std::thread::sleep(Duration::from_millis(5));
                return Err(ErrorKind::TimedOut.into());
            }
            let data = &self.rd.buf[..n];
            trace(|| format!("read {n}: {data:02X?}"));
            self.rx.extend_from_slice(data);
        }
        let n = self.rx.len().min(out.len());
        out[..n].copy_from_slice(&self.rx[..n]);
        self.rx.drain(..n);
        Ok(n)
    }
}

impl Write for UsbPrint {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.wr.pending {
            return Err(io::Error::other("an earlier USB write never completed"));
        }
        let len = data.len().min(WRITE_LEN);
        self.wr.buf[..len].copy_from_slice(&data[..len]);
        let x = &mut self.wr;
        let ok = unsafe { WriteFile(self.handle, x.buf.as_ptr(), len as u32, std::ptr::null_mut(), &mut **x.ov) };
        x.started(ok).inspect_err(|e| trace(|| format!("WriteFile failed: {e}")))?;
        if let Some(r) = x.wait(self.handle, 20_000) {
            trace(|| format!("write {len}: {r:?}"));
            return r;
        }
        // Printer not taking data: cancel, keeping whatever part of it already went out.
        if !x.cancel(self.handle, 1_000) {
            trace(|| "write timed out and won't cancel".into());
            return Err(io::Error::other("USB write neither completed nor cancelled"));
        }
        let sent = x.ov.InternalHigh;
        trace(|| format!("write {len}: timed out after {sent} bytes"));
        if sent == 0 { Err(ErrorKind::TimedOut.into()) } else { Ok(sent) }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UsbPrint {
    fn drop(&mut self) {
        // Transfers that won't let go keep their buffers (leaked in Xfer::drop).
        self.rd.cancel(self.handle, 500);
        self.wr.cancel(self.handle, 500);
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vid_pid_and_1284_model() {
        let p = r"\\?\usb#vid_04f9&pid_20af#000b1z234567#{28d78fad-5a12-11d1-ae5b-0000f803a8c2}";
        assert_eq!(parse_usb_vid_pid(p), Some((0x04F9, 0x20AF)));
        assert_eq!(model_from_pid(0x20AF), Some("PT-P710BT"));
        assert_eq!(model_from_pid(0x2060), Some("PT-E550W"));
        assert_eq!(model_from_pid(0x9999), None);

        let id = b"MFG:Brother;CMD:PT-CBP;MDL:PT-P710BT;CLS:PRINTER;\0";
        assert_eq!(parse_1284_model(id).as_deref(), Some("PT-P710BT"));
        assert_eq!(parse_1284_model(b"MFG:Brother;MDL:HL-L2350DW;").as_deref(), Some("HL-L2350DW"));
    }
}
