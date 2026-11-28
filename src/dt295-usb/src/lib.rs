//! USB transport for the PIX-DT295 (VID 0x06B8, PID 0x103B) and its siblings, the
//! PIX-DT295W (0x103C), the PIX-DT195 (0x103A) and the PIX-DT195W (0x1040), which have the
//! same USB interface.
//!
//! A thin, blocking API over the one interface the driver uses: vendor control
//! transfers on EP0, bulk OUT (firmware download, EP 0x02), interrupt IN (events,
//! EP 0x83) and bulk IN (the TS, EP 0x81) -- synchronous with a timeout, or as a
//! queue of posted reads ([`ReadQueue`]) so the host keeps receiving while the
//! caller is busy.
//!
//! Backends: WinUSB through `windows-sys` on Windows (with the pipe policies the
//! hardware relies on), and usbfs through `nusb` elsewhere.
//!
//! Semantics the driver relies on (all verified on hardware):
//! * [`Device::read`] returns whatever arrived within the timeout as a short read;
//!   only "nothing at all" is [`Error::Timeout`]. On Windows it is a synchronous
//!   `WinUsb_ReadPipe` with `PIPE_TRANSFER_TIMEOUT` (RAW_IO off) -- the
//!   overlapped+`CancelIoEx` path dropped partial reads.
//! * [`Device::abort_pipe`] makes a read blocked on another thread return
//!   [`Error::Cancelled`] (used to unstick the stream worker on a wedged device).
//! * [`ReadQueue`] keeps `n` reads posted; on WinUSB it turns RAW_IO on for the
//!   queue's lifetime (without it WinUSB passes queued reads to the host
//!   controller one at a time) and off again when dropped.
//! * Control transfers are serialized internally; `Device` is `Send + Sync`.

use std::time::Duration;

mod common;

#[cfg(windows)]
mod winusb;
#[cfg(windows)]
use winusb as backend;

#[cfg(unix)]
mod usbfs;
#[cfg(unix)]
use usbfs as backend;

/// PIXELA's USB vendor id.
pub const VID: u16 = 0x06B8;
/// The PIX-DT295's USB product id.
pub const PID_DT295: u16 = 0x103B;
/// The PIX-DT295W's USB product id.
pub const PID_DT295W: u16 = 0x103C;
/// The PIX-DT195's USB product id (a Mac model).
pub const PID_DT195: u16 = 0x103A;
/// The PIX-DT195W's USB product id (a Mac model).
pub const PID_DT195W: u16 = 0x1040;
/// Every product id this crate opens.
pub const PIDS: [u16; 4] = [PID_DT295, PID_DT295W, PID_DT195, PID_DT195W];

/// Transport errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error (OS error {0})")]
    Io(i32),
    #[error("timeout")]
    Timeout,
    #[error("device not found or disconnected")]
    NoDevice,
    #[error("access denied (is the device bound to WinUSB?)")]
    Access,
    #[error("endpoint stalled")]
    Stall,
    #[error("transfer cancelled")]
    Cancelled,
    #[error("invalid parameter")]
    Param,
    #[error("out of memory")]
    NoMem,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Control transfer setup (bmRequestType, bRequest, wValue, wIndex).
#[derive(Debug, Clone, Copy)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
}

/// A present device as [`list`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The USB product id (one of the ones [`list`] was given).
    pub pid: u16,
    /// The USB serial number (iSerialNumber, e.g. `CD23456789`; upper case on
    /// Windows, where it comes from the device instance id); `None` if the OS has none.
    pub serial: Option<String>,
    /// Where it is (Windows: the device interface path; unix: bus and address).
    pub location: String,
    /// Whether this process could open it just now; `false` = in use by another
    /// process, or (Windows) not bound to WinUSB.
    pub available: bool,
}

/// The present devices of vendor `vid` with a product id in `pids`, in enumeration
/// order. Each one is opened and closed again to fill in [`Listed::available`] (no
/// transfers).
///
/// [`Device::open`]'s index counts only the available ones (on Windows; see there).
pub fn list(vid: u16, pids: &[u16]) -> Result<Vec<Listed>> {
    backend::list(vid, pids)
}

/// The USB serial numbers of the present devices of vendor `vid` with a product id in
/// `pids`, without opening any (a host that holds one open can poll whether it is still
/// there).
pub fn present_serials(vid: u16, pids: &[u16]) -> Result<Vec<String>> {
    backend::present_serials(vid, pids)
}

/// An opened device (interface 0 claimed).
#[derive(Debug)]
pub struct Device {
    inner: backend::Device,
}

impl Device {
    /// Open the `index`-th (0-based) device of vendor `vid` with a product id in `pids`,
    /// in enumeration order. On Windows the index counts only the devices this process
    /// can open: one in use by another process (or not bound to WinUSB) is skipped, so
    /// with several units the numbers shift -- select by serial number there
    /// ([`Device::open_serial`]). On Linux the index counts the free units the same way.
    /// When there are not that many, [`Error::Access`] if one was skipped (it may be free
    /// in a moment), else [`Error::NoDevice`].
    pub fn open(vid: u16, pids: &[u16], index: usize) -> Result<Self> {
        Ok(Self {
            inner: backend::Device::open(vid, pids, index)?,
        })
    }

    /// Open the device of vendor `vid` with a product id in `pids` whose USB serial
    /// number is `serial` (ASCII case-insensitive). [`Error::NoDevice`] if no such device
    /// is present, [`Error::Access`] if it is in use or not openable.
    pub fn open_serial(vid: u16, pids: &[u16], serial: &str) -> Result<Self> {
        Ok(Self {
            inner: backend::Device::open_serial(vid, pids, serial)?,
        })
    }

    /// The device's USB serial number, as [`Listed::serial`].
    pub fn serial(&self) -> Option<&str> {
        self.inner.serial()
    }

    /// The device's USB product id.
    pub fn pid(&self) -> u16 {
        self.inner.pid()
    }

    /// Control IN; returns the number of bytes received.
    pub fn control_in(&self, setup: Setup, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.inner.control_in(setup, buf, timeout)
    }

    /// Control OUT; returns the number of bytes sent.
    pub fn control_out(&self, setup: Setup, data: &[u8], timeout: Duration) -> Result<usize> {
        self.inner.control_out(setup, data, timeout)
    }

    /// Bulk OUT on `ep`; returns the number of bytes sent.
    pub fn bulk_out(&self, ep: u8, data: &[u8], timeout: Duration) -> Result<usize> {
        self.inner.bulk_out(ep, data, timeout)
    }

    /// Synchronous bulk / interrupt IN on `ep`. A short read is `Ok(n)`;
    /// `Err(Timeout)` only when nothing arrived.
    pub fn read(&self, ep: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.inner.read(ep, buf, timeout)
    }

    /// Clear a halt / reset the pipe.
    pub fn clear_halt(&self, ep: u8) -> Result<()> {
        self.inner.clear_halt(ep)
    }

    /// Cancel every transfer in flight on `ep` (a blocked [`Device::read`] on another
    /// thread returns `Err(Cancelled)`). Safe when nothing is pending.
    pub fn abort_pipe(&self, ep: u8) -> Result<()> {
        self.inner.abort_pipe(ep)
    }

    /// Keep `n` reads of `size` bytes (a multiple of 512) posted on bulk IN `ep`.
    pub fn read_queue(&self, ep: u8, n: usize, size: usize) -> Result<ReadQueue<'_>> {
        Ok(ReadQueue {
            inner: self.inner.read_queue(ep, n, size)?,
        })
    }
}

/// Reads kept posted on a bulk IN endpoint, completed in order. Dropping it
/// cancels and reaps every posted read (and restores the pipe policy).
#[derive(Debug)]
pub struct ReadQueue<'d> {
    inner: backend::ReadQueue<'d>,
}

impl ReadQueue<'_> {
    /// Wait up to `timeout` for the oldest posted read. `Ok(Some(data))` = it
    /// completed (the slice stays valid until the next call, which re-posts that
    /// buffer); `Ok(None)` = still pending (it stays posted); `Err` = it failed (it
    /// is re-posted on the next call).
    pub fn next(&mut self, timeout: Duration) -> Result<Option<&[u8]>> {
        self.inner.next(timeout)
    }

    /// Whether the reads sit in the host controller all at once (WinUSB RAW_IO).
    pub fn raw_io(&self) -> bool {
        self.inner.raw_io()
    }
}
