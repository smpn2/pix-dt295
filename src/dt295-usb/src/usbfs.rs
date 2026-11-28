//! usbfs backend via `nusb` (Linux and other unix).
//!
//! Same contract as the WinUSB backend, including its timeouts: `Duration::ZERO` is
//! "no timeout" for the device transfers and "only poll" for [`ReadQueue::next`].
//!
//! nusb endpoints are exclusive `&mut` objects, so each one is opened on first use
//! and kept in a per-address slot, locked for the duration of a transfer. A transfer
//! waits in slices of [`ABORT_POLL`], so [`Device::abort_pipe`] -- which cannot take
//! the slot while a read on another thread holds it -- only bumps a counter that the
//! waiter notices within one slice, and then cancels its own transfer.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, Completion, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient,
    TransferError,
};
use nusb::{Endpoint, ErrorKind, Interface, MaybeFuture};

use crate::common::{check_control_dir, check_read_queue, is_in};
use crate::{Error, Result, Setup};

/// How long a blocked transfer can take to notice [`Device::abort_pipe`].
const ABORT_POLL: Duration = Duration::from_millis(20);

/// "No timeout" for nusb calls that add their timeout to `Instant::now()`.
const FOREVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

fn map_error(e: nusb::Error) -> Error {
    match e.kind() {
        ErrorKind::Disconnected | ErrorKind::NotFound => Error::NoDevice,
        ErrorKind::Busy | ErrorKind::PermissionDenied => Error::Access,
        _ => Error::Io(e.os_error().map_or_default(|c| c as i32)),
    }
}

fn map_transfer_error(e: TransferError) -> Error {
    match e {
        TransferError::Cancelled => Error::Cancelled,
        TransferError::Stall => Error::Stall,
        TransferError::Disconnected => Error::NoDevice,
        TransferError::InvalidArgument => Error::Param,
        TransferError::Fault => Error::Io(0), // nusb keeps no errno for a fault
        TransferError::Unknown(c) => Error::Io(c as i32),
    }
}

/// nusb cancels a control transfer only when its timeout expires.
fn map_control_error(e: TransferError) -> Error {
    match e {
        TransferError::Cancelled => Error::Timeout,
        e => map_transfer_error(e),
    }
}

/// `bmRequestType` bits 6..5 and 4..0, as nusb wants them.
fn split_request_type(rt: u8) -> Result<(ControlType, Recipient)> {
    let control_type = match (rt >> 5) & 3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        2 => ControlType::Vendor,
        _ => return Err(Error::Param),
    };
    let recipient = match rt & 0x1F {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        3 => Recipient::Other,
        _ => return Err(Error::Param),
    };
    Ok((control_type, recipient))
}

fn control_timeout(t: Duration) -> Duration {
    if t.is_zero() { FOREVER } else { t.min(FOREVER) }
}

/// `None` = wait forever.
fn deadline(t: Duration) -> Option<Instant> {
    if t.is_zero() {
        None
    } else {
        Instant::now().checked_add(t)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A bulk or interrupt endpoint of either direction.
#[derive(Debug)]
enum AnyEp {
    BulkIn(Endpoint<Bulk, In>),
    BulkOut(Endpoint<Bulk, Out>),
    InterruptIn(Endpoint<Interrupt, In>),
    InterruptOut(Endpoint<Interrupt, Out>),
}

macro_rules! each_ep {
    ($ep:expr, $e:ident => $body:expr) => {
        match $ep {
            AnyEp::BulkIn($e) => $body,
            AnyEp::BulkOut($e) => $body,
            AnyEp::InterruptIn($e) => $body,
            AnyEp::InterruptOut($e) => $body,
        }
    };
}

impl AnyEp {
    fn open(intf: &Interface, addr: u8) -> Result<Self> {
        let ty = intf
            .descriptor()
            .and_then(|d| d.endpoints().find(|e| e.address() == addr))
            .map(|e| e.transfer_type())
            .ok_or(Error::Param)?;
        Ok(match (ty, is_in(addr)) {
            (TransferType::Bulk, true) => Self::BulkIn(intf.endpoint(addr).map_err(map_error)?),
            (TransferType::Bulk, false) => Self::BulkOut(intf.endpoint(addr).map_err(map_error)?),
            (TransferType::Interrupt, true) => {
                Self::InterruptIn(intf.endpoint(addr).map_err(map_error)?)
            }
            (TransferType::Interrupt, false) => {
                Self::InterruptOut(intf.endpoint(addr).map_err(map_error)?)
            }
            _ => return Err(Error::Param),
        })
    }

    fn max_packet_size(&self) -> usize {
        each_ep!(self, e => e.max_packet_size())
    }

    fn pending(&self) -> usize {
        each_ep!(self, e => e.pending())
    }

    fn submit(&mut self, buf: Buffer) {
        each_ep!(self, e => e.submit(buf))
    }

    fn wait_next_complete(&mut self, timeout: Duration) -> Option<Completion> {
        each_ep!(self, e => e.wait_next_complete(timeout))
    }

    fn cancel_all(&mut self) {
        each_ep!(self, e => e.cancel_all())
    }

    fn clear_halt(&mut self) -> std::result::Result<(), nusb::Error> {
        each_ep!(self, e => e.clear_halt().wait())
    }

    /// The completion of a transfer that has just been cancelled.
    fn reap(&mut self) -> Completion {
        loop {
            if let Some(c) = self.wait_next_complete(Duration::from_secs(1)) {
                return c;
            }
        }
    }
}

/// Why we cancelled a transfer.
#[derive(Clone, Copy)]
enum Stop {
    Timeout,
    Abort,
}

/// Wait for the oldest pending transfer on `e`, cancelling it at `deadline` or when
/// `aborts` moves on from `epoch`.
fn wait_transfer(
    e: &mut AnyEp,
    deadline: Option<Instant>,
    aborts: &AtomicU64,
    epoch: u64,
) -> (Completion, Option<Stop>) {
    loop {
        let slice = deadline.map_or(ABORT_POLL, |d| {
            d.saturating_duration_since(Instant::now()).min(ABORT_POLL)
        });
        if let Some(c) = e.wait_next_complete(slice) {
            return (c, None);
        }
        let stop = if aborts.load(Ordering::Acquire) != epoch {
            Stop::Abort
        } else if deadline.is_some_and(|d| Instant::now() >= d) {
            Stop::Timeout
        } else {
            continue;
        };
        e.cancel_all();
        return (e.reap(), Some(stop));
    }
}

/// A finished transfer's byte count: what arrived before a timeout or an abort is a
/// short transfer, not lost; only "nothing at all" is the error.
fn transferred(c: &Completion, stop: Option<Stop>) -> Result<usize> {
    match c.status {
        Ok(()) => Ok(c.actual_len),
        Err(TransferError::Cancelled) if c.actual_len > 0 => Ok(c.actual_len),
        Err(TransferError::Cancelled) if matches!(stop, Some(Stop::Timeout)) => Err(Error::Timeout),
        Err(e) => Err(map_transfer_error(e)),
    }
}

#[derive(Debug, Default)]
struct EpSlot {
    /// Opened on first use; taken by a [`ReadQueue`] for its lifetime.
    ep: Mutex<Option<AnyEp>>,
    /// Bumped by `abort_pipe`: transfers started before the bump are cancelled.
    aborts: AtomicU64,
}

/// An opened device: interface 0, the control-transfer lock and the endpoint slots
/// (indexed by endpoint number, IN in the upper half).
pub(crate) struct Device {
    intf: Interface,
    ctl_lock: Mutex<()>,
    eps: [EpSlot; 32],
    serial: Option<String>,
    pid: u16,
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("interface", &self.intf)
            .finish_non_exhaustive()
    }
}

/// The present devices of vendor `vid` with a product id in `pids`.
fn matching(vid: u16, pids: &[u16]) -> Result<Vec<nusb::DeviceInfo>> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(map_error)?
        .filter(|d| d.vendor_id() == vid && pids.contains(&d.product_id()))
        .collect())
}

pub(crate) fn present_serials(vid: u16, pids: &[u16]) -> Result<Vec<String>> {
    Ok(matching(vid, pids)?
        .into_iter()
        .filter_map(|d| d.serial_number().map(str::to_owned))
        .collect())
}

pub(crate) fn list(vid: u16, pids: &[u16]) -> Result<Vec<crate::Listed>> {
    Ok(matching(vid, pids)?
        .into_iter()
        .map(|d| crate::Listed {
            pid: d.product_id(),
            serial: d.serial_number().map(str::to_owned),
            location: format!("bus {} address {}", d.bus_id(), d.device_address()),
            available: is_free(&d),
        })
        .collect())
}

/// No other program holds the unit: its interface can be claimed (and is released at
/// once). Opening the usbfs node alone succeeds for a unit `dt295 serve` holds.
fn is_free(info: &nusb::DeviceInfo) -> bool {
    info.open()
        .wait()
        .is_ok_and(|d| d.claim_interface(0).wait().is_ok())
}

/// The `index`-th of `units` that `free` passes. When there are not that many:
/// [`Error::Access`] if there are that many units but one was skipped (held: it may be
/// free in a moment, as WinUSB reports a unit it cannot open), else [`Error::NoDevice`].
fn nth_free<T>(units: Vec<T>, mut free: impl FnMut(&T) -> bool, index: usize) -> Result<T> {
    let enough = index < units.len();
    let mut skipped = false;
    let found = units
        .into_iter()
        .filter(|u| {
            let f = free(u);
            skipped |= !f;
            f
        })
        .nth(index);
    found.ok_or(if skipped && enough {
        Error::Access
    } else {
        Error::NoDevice
    })
}

impl Device {
    /// The `index`-th free unit, as WinUSB counts them (a held unit is skipped).
    pub(crate) fn open(vid: u16, pids: &[u16], index: usize) -> Result<Self> {
        let info = nth_free(matching(vid, pids)?, is_free, index)?;
        Self::open_info(&info)
    }

    pub(crate) fn open_serial(vid: u16, pids: &[u16], serial: &str) -> Result<Self> {
        let info = matching(vid, pids)?
            .into_iter()
            .find(|d| {
                d.serial_number()
                    .is_some_and(|s| s.eq_ignore_ascii_case(serial))
            })
            .ok_or(Error::NoDevice)?;
        Self::open_info(&info)
    }

    pub(crate) fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }

    pub(crate) fn pid(&self) -> u16 {
        self.pid
    }

    fn open_info(info: &nusb::DeviceInfo) -> Result<Self> {
        let dev = info.open().wait().map_err(map_error)?;
        // The libusb driver selected configuration 1 ("may already be configured");
        // only do it when another one is active, as re-selecting resets the endpoints.
        if dev
            .active_configuration()
            .map(|c| c.configuration_value())
            .ok()
            != Some(1)
        {
            let _ = dev.set_configuration(1).wait();
        }
        let intf = dev
            .detach_and_claim_interface(0)
            .wait()
            .map_err(map_error)?;
        Ok(Self {
            intf,
            ctl_lock: Mutex::new(()),
            eps: std::array::from_fn(|_| EpSlot::default()),
            serial: info.serial_number().map(str::to_owned),
            pid: info.product_id(),
        })
    }

    fn slot(&self, ep: u8) -> &EpSlot {
        &self.eps[usize::from(ep & 0x0F) | (usize::from(ep >> 7) << 4)]
    }

    /// Run `f` on the (lazily opened) endpoint `ep`, holding its slot, with the abort
    /// counter and its value now.
    fn with_ep<R>(
        &self,
        ep: u8,
        f: impl FnOnce(&mut AnyEp, &AtomicU64, u64) -> Result<R>,
    ) -> Result<R> {
        let slot = self.slot(ep);
        let mut guard = lock(&slot.ep);
        let e = match guard.take() {
            Some(e) => e,
            None => AnyEp::open(&self.intf, ep)?,
        };
        let e = guard.insert(e);
        f(e, &slot.aborts, slot.aborts.load(Ordering::Acquire))
    }

    pub(crate) fn control_in(&self, s: Setup, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        check_control_dir(s.request_type, true)?;
        let (control_type, recipient) = split_request_type(s.request_type)?;
        let length = u16::try_from(buf.len()).map_err(|_| Error::Param)?;
        let req = ControlIn {
            control_type,
            recipient,
            request: s.request,
            value: s.value,
            index: s.index,
            length,
        };
        let _lock = lock(&self.ctl_lock);
        let data = self
            .intf
            .control_in(req, control_timeout(timeout))
            .wait()
            .map_err(map_control_error)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    pub(crate) fn control_out(&self, s: Setup, data: &[u8], timeout: Duration) -> Result<usize> {
        check_control_dir(s.request_type, false)?;
        let (control_type, recipient) = split_request_type(s.request_type)?;
        u16::try_from(data.len()).map_err(|_| Error::Param)?;
        let req = ControlOut {
            control_type,
            recipient,
            request: s.request,
            value: s.value,
            index: s.index,
            data,
        };
        let _lock = lock(&self.ctl_lock);
        self.intf
            .control_out(req, control_timeout(timeout))
            .wait()
            .map_err(map_control_error)?;
        Ok(data.len())
    }

    pub(crate) fn bulk_out(&self, ep: u8, data: &[u8], timeout: Duration) -> Result<usize> {
        if is_in(ep) {
            return Err(Error::Param);
        }
        u32::try_from(data.len()).map_err(|_| Error::Param)?;
        let deadline = deadline(timeout);
        self.with_ep(ep, |e, aborts, epoch| {
            e.submit(Buffer::from(data));
            let (c, stop) = wait_transfer(e, deadline, aborts, epoch);
            transferred(&c, stop)
        })
    }

    pub(crate) fn read(&self, ep: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if !is_in(ep) {
            return Err(Error::Param);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        u32::try_from(buf.len()).map_err(|_| Error::Param)?;
        let deadline = deadline(timeout);
        self.with_ep(ep, |e, aborts, epoch| {
            // nusb wants whole packets on IN; a packet that overruns `buf` is cut short.
            e.submit(Buffer::new(
                buf.len().next_multiple_of(e.max_packet_size().max(1)),
            ));
            let (c, stop) = wait_transfer(e, deadline, aborts, epoch);
            let n = transferred(&c, stop)?.min(c.buffer.len()).min(buf.len());
            buf[..n].copy_from_slice(&c.buffer[..n]);
            Ok(n)
        })
    }

    pub(crate) fn clear_halt(&self, ep: u8) -> Result<()> {
        self.with_ep(ep, |e, _, _| e.clear_halt().map_err(map_error))
    }

    pub(crate) fn abort_pipe(&self, ep: u8) -> Result<()> {
        // Whoever has a transfer in flight on `ep` holds the endpoint and cancels it
        // within ABORT_POLL; with nothing in flight there is nothing to do.
        self.slot(ep).aborts.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    pub(crate) fn read_queue(&self, ep: u8, n: usize, size: usize) -> Result<ReadQueue<'_>> {
        check_read_queue(ep, n, size)?;
        u32::try_from(size).map_err(|_| Error::Param)?;
        let slot = self.slot(ep);
        let taken = lock(&slot.ep).take();
        let mut e = match taken {
            Some(e) => e,
            None => AnyEp::open(&self.intf, ep)?,
        };
        // Use heap buffers: usbfs mmap-backed buffers produced corrupt TS on
        // Linux 4.9.44 (32-bit ARM), while heap buffers worked. This is consistent
        // with known usbfs DMA mapping bugs on non-coherent systems; the exact
        // cause in that kernel is unverified (upstream fix: 2bef9aed6f0e).
        // These buffers are allocated here and reused by ReadQueue::next.
        for _ in 0..n {
            e.submit(Buffer::new(size));
        }
        Ok(ReadQueue {
            ep: e,
            aborts: &slot.aborts,
            epoch: slot.aborts.load(Ordering::Acquire),
            done: None,
        })
    }
}

/// `n` reads kept submitted on a bulk IN endpoint. The queue owns the endpoint while
/// it lives; afterwards the next transfer on it opens it again.
#[derive(Debug)]
pub(crate) struct ReadQueue<'d> {
    ep: AnyEp,
    aborts: &'d AtomicU64,
    /// `aborts` as last acted upon.
    epoch: u64,
    /// The buffer handed out by the last `next`; the following one re-submits it.
    done: Option<Buffer>,
}

impl ReadQueue<'_> {
    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<&[u8]>> {
        if let Some(buf) = self.done.take() {
            self.ep.submit(buf); // an IN buffer keeps its requested length
        }
        let deadline = Instant::now().checked_add(timeout); // ZERO: one poll
        let c = loop {
            let slice = deadline.map_or(ABORT_POLL, |d| {
                d.saturating_duration_since(Instant::now()).min(ABORT_POLL)
            });
            if let Some(c) = self.ep.wait_next_complete(slice) {
                break c;
            }
            let epoch = self.aborts.load(Ordering::Acquire);
            if epoch != self.epoch {
                // abort_pipe: every posted read completes as cancelled, in turn.
                self.epoch = epoch;
                self.ep.cancel_all();
            } else if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
        };
        let status = match c.status {
            Err(TransferError::Cancelled) if c.actual_len > 0 => Ok(()),
            s => s,
        };
        let buf = self.done.insert(c.buffer);
        match status {
            Ok(()) => Ok(Some(&buf[..])),
            Err(e) => Err(map_transfer_error(e)),
        }
    }

    /// usbfs hands every submitted URB to the host controller at once (what RAW_IO
    /// buys on WinUSB).
    pub(crate) fn raw_io(&self) -> bool {
        true
    }
}

impl Drop for ReadQueue<'_> {
    fn drop(&mut self) {
        self.ep.cancel_all();
        while self.ep.pending() > 0 {
            let _ = self.ep.reap();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn request_type_splits() {
        assert_matches!(
            split_request_type(0xC0),
            Ok((ControlType::Vendor, Recipient::Device))
        );
        assert_matches!(
            split_request_type(0x40),
            Ok((ControlType::Vendor, Recipient::Device))
        );
        assert_matches!(
            split_request_type(0x21),
            Ok((ControlType::Class, Recipient::Interface))
        );
        assert_matches!(
            split_request_type(0x02),
            Ok((ControlType::Standard, Recipient::Endpoint))
        );
        assert_matches!(split_request_type(0x60), Err(Error::Param), "reserved type");
        assert_matches!(
            split_request_type(0x44),
            Err(Error::Param),
            "reserved recipient"
        );
    }

    #[test]
    fn transfer_errors_map() {
        assert_matches!(
            map_transfer_error(TransferError::Cancelled),
            Error::Cancelled
        );
        assert_matches!(map_transfer_error(TransferError::Stall), Error::Stall);
        assert_matches!(
            map_transfer_error(TransferError::Disconnected),
            Error::NoDevice
        );
        assert_matches!(
            map_transfer_error(TransferError::InvalidArgument),
            Error::Param
        );
        assert_matches!(map_transfer_error(TransferError::Unknown(5)), Error::Io(5));
        assert_matches!(map_control_error(TransferError::Cancelled), Error::Timeout);
    }

    #[test]
    fn index_counts_free_units() {
        let free = |u: &&str| !u.starts_with("held");
        assert_matches!(nth_free(vec!["a", "held", "b"], free, 1), Ok("b"));
        assert_matches!(nth_free(vec!["a", "held"], free, 0), Ok("a"));
        // not that many free: in use (a retry may open it), or not there
        assert_matches!(nth_free(vec!["a", "held"], free, 1), Err(Error::Access));
        assert_matches!(nth_free(vec!["a"], free, 1), Err(Error::NoDevice));
        assert_matches!(nth_free(Vec::<&str>::new(), free, 0), Err(Error::NoDevice));
        // not that many units at all: no device, held one or not
        assert_matches!(nth_free(vec!["a", "held"], free, 2), Err(Error::NoDevice));
    }

    #[test]
    fn zero_timeout_is_forever() {
        assert_eq!(control_timeout(Duration::ZERO), FOREVER);
        assert_eq!(control_timeout(Duration::MAX), FOREVER);
        assert!(deadline(Duration::ZERO).is_none());
        assert!(deadline(Duration::from_millis(200)).is_some());
    }
}
