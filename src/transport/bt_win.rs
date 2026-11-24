//! PT-P710BT Bluetooth Classic SPP on Windows via Winsock RFCOMM (`AF_BTH` / `BTHPROTO_RFCOMM`).
//!
//! Opens the RFCOMM channel directly by paired device name or MAC address, without requiring
//! the user to create or look up a legacy Bluetooth COM port in Control Panel.

use super::{Link, PrinterId};
use anyhow::{Result, bail};
use std::io::{self, ErrorKind, Read, Write};
use std::sync::OnceLock;
use windows_sys::Win32::Devices::Bluetooth::{
    AF_BTH, BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS, BTHPROTO_RFCOMM,
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindNextDevice, SOCKADDR_BTH,
};
use windows_sys::Win32::Networking::WinSock::{
    FD_SET, FIONBIO, INVALID_SOCKET, SO_ERROR, SO_RCVTIMEO, SO_SNDTIMEO, SOCK_STREAM, SOCKET,
    SOCKET_ERROR, SOL_SOCKET, TIMEVAL, WSADATA, WSAETIMEDOUT, WSAEWOULDBLOCK, WSAGetLastError,
    WSAStartup, closesocket, connect, getsockopt, ioctlsocket, recv, select, send, setsockopt,
    socket,
};
use windows_sys::core::GUID;

/// Serial Port Profile (SPP) service class UUID: `{00001101-0000-1000-8000-00805F9B34FB}`.
const SPP_UUID: GUID = GUID {
    data1: 0x0000_1101,
    data2: 0x0000,
    data3: 0x1000,
    data4: [0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB],
};

fn ensure_wsa() -> Result<()> {
    static WSA: OnceLock<Result<(), i32>> = OnceLock::new();
    let res = *WSA.get_or_init(|| {
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        let r = unsafe { WSAStartup(0x0202, &mut data) };
        if r == 0 { Ok(()) } else { Err(r) }
    });
    res.map_err(|e| anyhow::anyhow!("WSAStartup failed ({e})"))
}

/// Enumerate paired Bluetooth devices as `(name, bt_addr)`.
fn paired_devices() -> Vec<(String, u64)> {
    let params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: 1,
        fReturnRemembered: 1,
        fReturnUnknown: 0,
        fReturnConnected: 1,
        fIssueInquiry: 0,
        cTimeoutMultiplier: 0,
        hRadio: std::ptr::null_mut(),
    };
    let mut info: BLUETOOTH_DEVICE_INFO = unsafe { std::mem::zeroed() };
    info.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32;

    let handle = unsafe { BluetoothFindFirstDevice(&params, &mut info) };
    if handle.is_null() {
        return vec![];
    }
    let mut out = Vec::new();
    loop {
        let len = info.szName.iter().position(|&c| c == 0).unwrap_or(info.szName.len());
        let name = String::from_utf16_lossy(&info.szName[..len]).trim().to_string();
        let addr = unsafe { info.Address.Anonymous.ullLong };
        if !name.is_empty() && addr != 0 {
            out.push((name, addr));
        }
        if unsafe { BluetoothFindNextDevice(handle, &mut info) } == 0 {
            break;
        }
    }
    unsafe { BluetoothFindDeviceClose(handle) };
    out
}

/// Paired P-touch Bluetooth printers on Windows.
pub fn scan_paired() -> Vec<PrinterId> {
    let mut v: Vec<PrinterId> = paired_devices()
        .into_iter()
        .filter(|(name, _)| name.to_ascii_uppercase().starts_with("PT-"))
        .map(|(name, _)| PrinterId {
            model: super::discovery::model_from_name(&name),
            link: Link::Bluetooth(format!("bt:{name}")),
        })
        .collect();
    v.sort_by_key(|a| a.display());
    v.dedup();
    v
}

/// Parse `AA:BB:CC:DD:EE:FF` or `AA-BB-CC-DD-EE-FF` into a Windows `BTH_ADDR` (`u64`).
fn parse_bt_addr(s: &str) -> Option<u64> {
    let hex: String = s.chars().filter(|c| *c != ':' && *c != '-').collect();
    if hex.len() != 12 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

fn find_addr(name: &str) -> Result<u64> {
    if let Some(addr) = parse_bt_addr(name) {
        return Ok(addr);
    }
    let devs = paired_devices();
    for (dev_name, addr) in &devs {
        if dev_name == name || dev_name.replace(' ', "-") == name {
            return Ok(*addr);
        }
    }
    let lower = name.to_ascii_lowercase();
    for (dev_name, addr) in &devs {
        if dev_name.to_ascii_lowercase() == lower {
            return Ok(*addr);
        }
    }
    bail!("{name} isn't paired with this PC (Settings › Bluetooth & devices)")
}

/// Open Windows Winsock RFCOMM channel to a paired Bluetooth SPP printer.
pub struct Rfcomm {
    sock: SOCKET,
}

// SAFETY: a Winsock SOCKET is an OS handle safe to transfer across threads.
unsafe impl Send for Rfcomm {}

impl Rfcomm {
    pub fn open(name: &str) -> Result<Self> {
        ensure_wsa()?;
        let bt_addr = find_addr(name)?;

        let sock = unsafe { socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32) };
        if sock == INVALID_SOCKET {
            let err = unsafe { WSAGetLastError() };
            bail!("Bluetooth socket failed (WSA {err})");
        }
        let this = Self { sock };

        let mut addr: SOCKADDR_BTH = unsafe { std::mem::zeroed() };
        addr.addressFamily = AF_BTH;
        addr.btAddr = bt_addr;
        addr.serviceClassId = SPP_UUID;
        addr.port = 0; // let Windows resolve the SPP RFCOMM channel via SDP

        // Non-blocking connect with a 15 s timeout, matching macOS rfcomm.rs.
        let mut nonblocking: u32 = 1;
        unsafe { ioctlsocket(this.sock, FIONBIO, &mut nonblocking) };

        let r = unsafe {
            connect(
                this.sock,
                (&addr as *const SOCKADDR_BTH).cast(),
                std::mem::size_of::<SOCKADDR_BTH>() as i32,
            )
        };
        if r == SOCKET_ERROR {
            let err = unsafe { WSAGetLastError() };
            if err != WSAEWOULDBLOCK {
                bail!("can't open Bluetooth channel to {name} (WSA {err}). Is the printer on?");
            }
            let mut wfds: FD_SET = unsafe { std::mem::zeroed() };
            wfds.fd_count = 1;
            wfds.fd_array[0] = this.sock;
            let mut efds: FD_SET = unsafe { std::mem::zeroed() };
            efds.fd_count = 1;
            efds.fd_array[0] = this.sock;
            let tv = TIMEVAL { tv_sec: 15, tv_usec: 0 };
            let sel = unsafe { select(0, std::ptr::null_mut(), &mut wfds, &mut efds, &tv) };
            if sel == 0 {
                bail!("Bluetooth: the printer didn't answer. Is it on?");
            }
            if sel == SOCKET_ERROR {
                let e = unsafe { WSAGetLastError() };
                bail!("Bluetooth connect failed (WSA {e}). Is the printer on?");
            }
            let mut so_err: i32 = 0;
            let mut len: i32 = std::mem::size_of::<i32>() as i32;
            unsafe {
                getsockopt(
                    this.sock,
                    SOL_SOCKET,
                    SO_ERROR,
                    (&mut so_err as *mut i32).cast(),
                    &mut len,
                );
            }
            if so_err != 0 || efds.fd_count > 0 {
                bail!("can't open Bluetooth channel to {name} (WSA {so_err}). Is the printer on?");
            }
        }

        // Back to blocking mode with short read timeout (100 ms) so BtTransport can poll/cancel.
        let mut blocking: u32 = 0;
        unsafe { ioctlsocket(this.sock, FIONBIO, &mut blocking) };

        let rcv_ms: u32 = 100;
        let snd_ms: u32 = 20_000;
        unsafe {
            setsockopt(
                this.sock,
                SOL_SOCKET,
                SO_RCVTIMEO,
                (&rcv_ms as *const u32).cast(),
                std::mem::size_of::<u32>() as i32,
            );
            setsockopt(
                this.sock,
                SOL_SOCKET,
                SO_SNDTIMEO,
                (&snd_ms as *const u32).cast(),
                std::mem::size_of::<u32>() as i32,
            );
        }

        Ok(this)
    }
}

impl Read for Rfcomm {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        let n = unsafe { recv(self.sock, buf.as_mut_ptr(), len, 0) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = unsafe { WSAGetLastError() };
        if err == WSAETIMEDOUT || err == WSAEWOULDBLOCK {
            Err(ErrorKind::TimedOut.into())
        } else {
            Err(io::Error::from_raw_os_error(err))
        }
    }
}

impl Write for Rfcomm {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        let n = unsafe { send(self.sock, buf.as_ptr(), len, 0) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = unsafe { WSAGetLastError() };
        if err == WSAETIMEDOUT || err == WSAEWOULDBLOCK {
            Err(ErrorKind::TimedOut.into())
        } else {
            Err(io::Error::from_raw_os_error(err))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Rfcomm {
    fn drop(&mut self) {
        unsafe {
            closesocket(self.sock);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bt_mac_addresses() {
        assert_eq!(parse_bt_addr("AA:BB:CC:DD:EE:FF"), Some(0xAA_BB_CC_DD_EE_FF));
        assert_eq!(parse_bt_addr("01-23-45-67-89-ab"), Some(0x01_23_45_67_89_AB));
        assert_eq!(parse_bt_addr("PT-P710BT9475"), None);
        assert_eq!(parse_bt_addr("AA:BB:CC"), None);
    }
}
