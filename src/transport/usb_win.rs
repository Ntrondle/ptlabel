//! Brother P-touch printers over USB on Windows via the built-in USB printer class
//! driver (`usbprint.sys`, `GUID_DEVINTERFACE_USBPRINT`).
//!
//! Works out of the box without Zadig/WinUSB: enumerates connected `VID_04F9` P-touch
//! interfaces via SetupAPI and talks the same bidirectional 32-byte-status + raster
//! protocol over overlapped `ReadFile`/`WriteFile`.

use super::{Link, PrinterId};
use anyhow::{Context, Result, bail};
use std::io::{self, ErrorKind, Read, Write};
use std::time::Duration;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
    GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
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

/// Overlapped Win32 handle to a `usbprint.sys` device interface.
pub struct UsbPrint {
    handle: HANDLE,
    read_ev: HANDLE,
    write_ev: HANDLE,
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
            return Err(err).context("can't open USB printer (is it powered on with Editor Lite off?)");
        }

        let read_ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if read_ev.is_null() {
            let err = io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(err).context("CreateEventW failed");
        }
        let write_ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if write_ev.is_null() {
            let err = io::Error::last_os_error();
            unsafe {
                CloseHandle(read_ev);
                CloseHandle(handle);
            }
            return Err(err).context("CreateEventW failed");
        }

        Ok(Self { handle, read_ev, write_ev })
    }
}

fn wait_overlapped(handle: HANDLE, ov: &mut OVERLAPPED, timeout_ms: u32) -> io::Result<usize> {
    let mut transferred: u32 = 0;
    match unsafe { WaitForSingleObject(ov.hEvent, timeout_ms) } {
        WAIT_OBJECT_0 => {
            if unsafe { GetOverlappedResult(handle, ov, &mut transferred, 0) } != 0 {
                Ok(transferred as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        }
        WAIT_TIMEOUT => {
            unsafe { CancelIoEx(handle, ov) };
            // Must wait for the canceled IRP to finish before `ov` leaves the stack.
            if unsafe { GetOverlappedResult(handle, ov, &mut transferred, 1) } != 0 {
                Ok(transferred as usize)
            } else {
                let err = unsafe { GetLastError() };
                if err == ERROR_OPERATION_ABORTED {
                    Err(ErrorKind::TimedOut.into())
                } else {
                    Err(io::Error::from_raw_os_error(err as i32))
                }
            }
        }
        _ => {
            let err = io::Error::last_os_error();
            unsafe {
                CancelIoEx(handle, ov);
                let _ = GetOverlappedResult(handle, ov, &mut transferred, 1);
            }
            Err(err)
        }
    }
}

impl Read for UsbPrint {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.read_ev;
        let mut n: u32 = 0;
        let ok = unsafe { ReadFile(self.handle, buf.as_mut_ptr(), len, &mut n, &mut ov) };
        let res = if ok != 0 {
            Ok(n as usize)
        } else {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                wait_overlapped(self.handle, &mut ov, 100)
            } else {
                Err(io::Error::from_raw_os_error(err as i32))
            }
        };
        // usbprint.sys can complete a Bulk-IN read with 0 bytes (ZLP) when idle; treat as timeout
        // rather than EOF so BtTransport keeps waiting instead of dropping the handle.
        match res {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(5));
                Err(ErrorKind::TimedOut.into())
            }
            other => other,
        }
    }
}

impl Write for UsbPrint {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.write_ev;
        let mut n: u32 = 0;
        let ok = unsafe { WriteFile(self.handle, buf.as_ptr(), len, &mut n, &mut ov) };
        if ok != 0 {
            Ok(n as usize)
        } else {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                wait_overlapped(self.handle, &mut ov, 20_000)
            } else {
                Err(io::Error::from_raw_os_error(err as i32))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UsbPrint {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.write_ev);
            CloseHandle(self.read_ev);
            CloseHandle(self.handle);
        }
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
