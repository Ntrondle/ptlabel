//! The PT-P710BT's serial (SPP) channel, opened directly with IOBluetooth.
//!
//! Not through /dev/cu.PT-…: bluetoothd's serial bridge behind that file connects RFCOMM only
//! once per pairing. After the printer has been off, opening the file never reaches the printer
//! again. And Connect in the Bluetooth menu gives a paired serial device to BrailleService (macOS
//! takes it for a braille display), so the only way back was to forget and re-pair the printer.

use anyhow::{Context, Result, bail};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, DefinedClass, Message, define_class, msg_send};
use objc2_foundation::NSObjectProtocol;
use objc2_io_bluetooth::{
    IOBluetoothDevice, IOBluetoothRFCOMMChannel, IOBluetoothRFCOMMChannelDelegate, IOBluetoothSDPUUID,
};
use std::ffi::{c_int, c_void};
use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Service class UUID of the Serial Port Profile.
const SERIAL_PORT: u16 = 0x1101;

#[derive(Default)]
struct State {
    rx: Vec<u8>,
    opened: Option<c_int>,
    closed: bool,
    written: Option<c_int>,
    sdp: Option<c_int>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Delegate has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "PTLabelRfcommDelegate"]
    #[ivars = Mutex<State>]
    struct Delegate;

    impl Delegate {
        #[unsafe(method(sdpQueryComplete:status:))]
        fn sdp_query_complete(&self, _device: *mut AnyObject, status: c_int) {
            self.state().sdp = Some(status);
        }
    }

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl IOBluetoothRFCOMMChannelDelegate for Delegate {
        #[unsafe(method(rfcommChannelData:data:length:))]
        fn data(&self, _channel: *mut AnyObject, data: *mut c_void, len: usize) {
            if !data.is_null() && len > 0 {
                // SAFETY: IOBluetooth hands over `len` readable bytes for the duration of the call.
                let b = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
                self.state().rx.extend_from_slice(b);
            }
        }

        #[unsafe(method(rfcommChannelOpenComplete:status:))]
        fn open_complete(&self, _channel: *mut AnyObject, status: c_int) {
            self.state().opened = Some(status);
        }

        #[unsafe(method(rfcommChannelClosed:))]
        fn closed(&self, _channel: *mut AnyObject) {
            self.state().closed = true;
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:))]
        fn write_complete(&self, _channel: *mut AnyObject, _refcon: *mut c_void, status: c_int) {
            self.state().written = Some(status);
        }
    }
);

impl Delegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(Mutex::new(State::default()));
        // SAFETY: plain NSObject init.
        unsafe { msg_send![super(this), init] }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.ivars().lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Run this thread's run loop for up to `d`: IOBluetooth delivers its callbacks there. With
/// nothing scheduled on it (the callbacks may go to the main thread), it returns at once: sleep.
fn pump(d: Duration) {
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
    }
    const FINISHED: i32 = 1; // kCFRunLoopRunFinished: no sources or timers
    // SAFETY: plain CoreFoundation call with its own constant.
    if unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, d.as_secs_f64(), 1) } == FINISHED {
        std::thread::sleep(d);
    }
}

/// An open RFCOMM channel to the printer. Read/write behave like the tty they replace: read waits
/// up to 100 ms (TimedOut if nothing came), Ok(0) once the link has closed.
pub struct Rfcomm {
    device: Retained<IOBluetoothDevice>,
    channel: Retained<IOBluetoothRFCOMMChannel>,
    delegate: Retained<Delegate>,
}

// SAFETY: a channel is opened, used and closed on the printer's worker thread. BtTransport opens
// it lazily, after the transport has moved to that thread (PrinterHandle::spawn_with).
unsafe impl Send for Rfcomm {}

impl Rfcomm {
    /// Connect to the paired device called `name` (e.g. PT-P710BT9475).
    pub fn open(name: &str) -> Result<Self> {
        let device = find(name)?;
        let delegate = Delegate::new();
        let id = channel_id(&device, &delegate)?;
        let mut channel = None;
        let obj: &AnyObject = &delegate;
        // SAFETY: the delegate outlives the channel: Drop clears it before letting go.
        let r = unsafe { device.openRFCOMMChannelAsync_withChannelID_delegate(Some(&mut channel), id, Some(obj)) };
        let channel = match channel {
            Some(c) if r == 0 => c,
            _ => bail!("{}", open_failed(r)),
        };
        let this = Self { device, channel, delegate };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let s = this.delegate.state();
            match s.opened {
                Some(0) => break,
                Some(e) => bail!("{}", open_failed(e)),
                None if s.closed => bail!("{}", open_failed(-1)),
                None => {}
            }
            drop(s);
            if Instant::now() > deadline {
                bail!("Bluetooth: the printer didn't answer. Is it on?");
            }
            pump(Duration::from_millis(50));
        }
        Ok(this)
    }
}

fn open_failed(code: c_int) -> String {
    format!(
        "can't open the printer's Bluetooth channel ({code:#x}). Is it on? If you clicked Connect \
         in the macOS Bluetooth menu, macOS lends it to its braille-display service for about a \
         minute; PTLabel retries by itself."
    )
}

/// The paired device whose name matches `name`. /dev/cu.<name> turns spaces into dashes.
fn find(name: &str) -> Result<Retained<IOBluetoothDevice>> {
    // SAFETY: plain IOBluetooth class method; returns nil when nothing is paired (or access to
    // Bluetooth is denied).
    if let Some(all) = unsafe { IOBluetoothDevice::pairedDevices() } {
        for i in 0..all.count() {
            let obj = all.objectAtIndex(i);
            let Some(d) = obj.downcast_ref::<IOBluetoothDevice>() else { continue };
            // SAFETY: plain getter.
            let Some(n) = (unsafe { d.nameOrAddress() }) else { continue };
            let n = n.to_string();
            if n == name || n.replace(' ', "-") == name {
                return Ok(d.retain());
            }
        }
    }
    bail!(
        "{name} isn't paired with this Mac, or PTLabel may not use Bluetooth \
         (System Settings › Privacy & Security › Bluetooth)"
    )
}

/// The printer's SPP channel number, from its SDP record (cached by macOS after pairing; asked
/// for again if missing).
fn channel_id(device: &IOBluetoothDevice, delegate: &Delegate) -> Result<u8> {
    // SAFETY: plain IOBluetooth calls on live objects; `id` outlives the out-pointer use.
    let uuid = unsafe { IOBluetoothSDPUUID::uuid16(SERIAL_PORT) }.context("SPP UUID")?;
    let lookup = || {
        let rec = unsafe { device.getServiceRecordForUUID(Some(&uuid)) }?;
        let mut id = 0u8;
        (unsafe { rec.getRFCOMMChannelID(&mut id) } == 0).then_some(id)
    };
    if let Some(id) = lookup() {
        return Ok(id);
    }
    let obj: &AnyObject = delegate;
    let r = unsafe { device.performSDPQuery(Some(obj)) };
    if r != 0 {
        bail!("Bluetooth: can't ask the printer for its services ({r:#x}). Is it on?");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while delegate.state().sdp.is_none() && Instant::now() < deadline {
        pump(Duration::from_millis(50));
    }
    lookup().context("the printer offers no Bluetooth serial port (or didn't answer). Is it on?")
}

impl Read for Rfcomm {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            {
                let mut s = self.delegate.state();
                if !s.rx.is_empty() {
                    let n = s.rx.len().min(buf.len());
                    buf[..n].copy_from_slice(&s.rx[..n]);
                    s.rx.drain(..n);
                    return Ok(n);
                }
                if s.closed {
                    return Ok(0);
                }
            }
            if Instant::now() >= deadline {
                return Err(ErrorKind::TimedOut.into());
            }
            pump(Duration::from_millis(10));
        }
    }
}

impl Write for Rfcomm {
    /// Sends up to one MTU and waits until the printer has it. A write that never completes is an
    /// error, not a timeout: retrying could send the chunk twice.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.delegate.state().closed {
            return Ok(0);
        }
        // SAFETY: plain getter.
        let mtu = usize::from(unsafe { self.channel.getMTU() }).max(1);
        let n = buf.len().min(mtu);
        let mut chunk = buf[..n].to_vec();
        self.delegate.state().written = None;
        // SAFETY: `chunk` stays alive until the write completes (or is leaked below).
        let r = unsafe { self.channel.writeAsync_length_refcon(chunk.as_mut_ptr().cast(), n as u16, std::ptr::null_mut()) };
        if r != 0 {
            return Err(io::Error::other(format!("RFCOMM write failed ({r:#x})")));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let s = self.delegate.state();
            match s.written {
                Some(0) => return Ok(n),
                Some(e) => return Err(io::Error::other(format!("RFCOMM write failed ({e:#x})"))),
                None if s.closed => return Ok(0),
                None => {}
            }
            drop(s);
            if Instant::now() > deadline {
                std::mem::forget(chunk); // IOBluetooth may still read it
                return Err(io::Error::other("RFCOMM write stalled"));
            }
            pump(Duration::from_millis(5));
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Rfcomm {
    fn drop(&mut self) {
        // SAFETY: detach the delegate first, so no callback reaches it once it is released.
        unsafe {
            self.channel.setDelegate(None);
            self.channel.closeChannel();
            self.device.closeConnection();
        }
    }
}
