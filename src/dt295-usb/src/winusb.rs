//! WinUSB backend.
//!
//! The device must be bound to WinUSB (`dt295 driver install`, or Zadig). Timeouts are
//! whole milliseconds, rounded up. `Duration::ZERO` means "no
//! timeout" for the device transfers (the WinUSB / `WaitForSingleObject` convention)
//! and "only poll" for [`ReadQueue::next`].

use std::cell::UnsafeCell;
use std::fmt;
use std::mem::{offset_of, size_of};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{self, null, null_mut};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tracing::debug;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::Usb::{
    AUTO_CLEAR_STALL, GUID_DEVINTERFACE_USB_DEVICE, MAXIMUM_TRANSFER_SIZE, PIPE_TRANSFER_TIMEOUT,
    RAW_IO, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_POLICY, WINUSB_SETUP_PACKET, WinUsb_AbortPipe,
    WinUsb_ControlTransfer, WinUsb_Free, WinUsb_GetOverlappedResult, WinUsb_GetPipePolicy,
    WinUsb_Initialize, WinUsb_ReadPipe, WinUsb_ResetPipe, WinUsb_SetPipePolicy, WinUsb_WritePipe,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BAD_COMMAND, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
    ERROR_GEN_FAILURE, ERROR_IO_PENDING, ERROR_NOT_ENOUGH_MEMORY, ERROR_OPERATION_ABORTED,
    ERROR_SEM_TIMEOUT, FALSE, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, TRUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, WaitForSingleObject,
};
use windows_sys::core::BOOL;

use crate::common::{check_control_dir, check_read_queue, is_in};
use crate::{Error, Result, Setup};

/// A Win32 error as a transport error.
fn map_win_error(e: WIN32_ERROR) -> Error {
    match e {
        ERROR_SEM_TIMEOUT | WAIT_TIMEOUT => Error::Timeout,
        ERROR_GEN_FAILURE => Error::Stall,
        ERROR_DEVICE_NOT_CONNECTED | ERROR_FILE_NOT_FOUND | ERROR_BAD_COMMAND => Error::NoDevice,
        ERROR_ACCESS_DENIED => Error::Access,
        ERROR_OPERATION_ABORTED => Error::Cancelled,
        ERROR_NOT_ENOUGH_MEMORY => Error::NoMem,
        e => Error::Io(e as i32),
    }
}

fn last_error() -> WIN32_ERROR {
    // SAFETY: no preconditions.
    unsafe { GetLastError() }
}

/// `Ok` for a successful Win32 call, else its mapped last error.
fn check(ok: BOOL) -> Result<()> {
    (ok != 0).ok_or_else(|| map_win_error(last_error()))
}

/// Whole milliseconds, rounded up so that a non-zero timeout never becomes 0 (which
/// WinUSB reads as "no timeout"); saturates at `u32::MAX` (= `INFINITE`).
fn timeout_ms(t: Duration) -> u32 {
    u32::try_from(t.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX)
}

/// A manual-reset, initially non-signalled event (`CreateEventW(NULL, TRUE, FALSE, NULL)`).
fn new_event() -> Result<OwnedHandle> {
    // SAFETY: null security attributes and name are allowed.
    let h = unsafe { CreateEventW(null(), TRUE, FALSE, null()) };
    if h.is_null() {
        return Err(Error::NoMem);
    }
    // SAFETY: `h` is a fresh event handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// An `OVERLAPPED` with its own event. Boxed, so it never moves while an I/O is
/// posted, and in an `UnsafeCell`, because the kernel writes it meanwhile.
struct Overlapped {
    ov: Box<UnsafeCell<OVERLAPPED>>,
    event: OwnedHandle,
}

impl Overlapped {
    fn new() -> Result<Self> {
        let event = new_event()?;
        let ov = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..Default::default()
        };
        Ok(Self {
            ov: Box::new(UnsafeCell::new(ov)),
            event,
        })
    }

    fn as_ptr(&self) -> *const OVERLAPPED {
        self.ov.get()
    }

    fn event(&self) -> HANDLE {
        self.event.as_raw_handle()
    }

    /// Clear it and its event for the next I/O; returns the pointer to post with.
    ///
    /// # Safety
    /// No I/O may be in flight on it.
    unsafe fn reset(&self) -> *const OVERLAPPED {
        let ov = OVERLAPPED {
            hEvent: self.event(),
            ..Default::default()
        };
        // SAFETY: nothing is in flight (caller), so nobody else accesses the OVERLAPPED.
        unsafe { self.ov.get().write(ov) };
        // SAFETY: a valid event handle.
        unsafe { ResetEvent(self.event()) };
        self.as_ptr()
    }
}

/// A SetupDi device information set, destroyed on drop.
struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        // SAFETY: a valid set from SetupDiGetClassDevsW, destroyed once.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

impl DevInfo {
    /// The present USB device interfaces (`GUID_DEVINTERFACE_USB_DEVICE`).
    fn usb_devices() -> Result<Self> {
        // SAFETY: a valid GUID; null enumerator and parent window are allowed.
        let di = unsafe {
            SetupDiGetClassDevsW(
                &GUID_DEVINTERFACE_USB_DEVICE,
                null(),
                null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        };
        if di == INVALID_HANDLE_VALUE as HDEVINFO {
            return Err(Error::Io(last_error() as i32));
        }
        Ok(Self(di))
    }

    /// Device paths (without the terminating NUL) of every interface in the set.
    fn paths(&self) -> Vec<Vec<u16>> {
        let mut paths = Vec::new();
        for i in 0u32.. {
            let mut ifd = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: a valid set and a properly sized output struct.
            let ok = unsafe {
                SetupDiEnumDeviceInterfaces(
                    self.0,
                    null(),
                    &GUID_DEVINTERFACE_USB_DEVICE,
                    i,
                    &mut ifd,
                )
            };
            if ok == 0 {
                break;
            }
            paths.extend(self.path(&ifd));
        }
        paths
    }

    /// The interface's device path (`SP_DEVICE_INTERFACE_DETAIL_DATA_W::DevicePath`).
    fn path(&self, ifd: &SP_DEVICE_INTERFACE_DATA) -> Option<Vec<u16>> {
        const HDR: usize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        const PATH: usize = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        let mut need = 0u32;
        // SAFETY: size query: no output buffer, `need` receives the required size.
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(self.0, ifd, null_mut(), 0, &mut need, null_mut())
        };
        let need_bytes = need as usize;
        if need_bytes < HDR {
            return None;
        }
        // u32 storage: the detail struct is 4-byte aligned (on x64; packed on x86).
        let mut buf = vec![0u32; need_bytes.div_ceil(4)];
        let det = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: `buf` holds at least `need >= HDR` suitably aligned bytes. cbSize is the
        // fixed header size (8 on x64, 6 on x86), as SetupAPI requires.
        unsafe { (*det).cbSize = HDR as u32 };
        // SAFETY: `det` points to `need` writable bytes.
        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(self.0, ifd, det, need, null_mut(), null_mut())
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: DevicePath, a NUL-terminated UTF-16 string, lies between offset PATH
        // (even, in a 4-aligned buffer) and the end of the `need` bytes SetupAPI filled.
        let wide = unsafe {
            std::slice::from_raw_parts(
                det.cast::<u8>().add(PATH).cast::<u16>(),
                (need_bytes - PATH) / 2,
            )
        };
        let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        Some(wide[..len].to_vec())
    }
}

/// The USB serial number in a device interface path: the device instance id's last
/// part (`\\?\usb#vid_06b8&pid_103b#cd23456789#{guid}` -> `CD23456789`), which Windows
/// takes from iSerialNumber; upper case, as Device Manager shows the instance id.
/// `None` for an id Windows generated (`5&2c0c0c5d&0&2`: no usable serial).
fn path_serial(path: &str) -> Option<String> {
    let s = path.split('#').nth(2)?;
    (!s.is_empty() && !s.contains('&')).then(|| s.to_ascii_uppercase())
}

/// The vendor and product id in a device interface path: the device instance id's
/// first part (`\\?\usb#vid_06b8&pid_103b#cd23456789#{guid}` -> (0x06B8, 0x103B)).
/// `None` for an interface of a composite device (`...&pid_c22d&mi_00#...`).
fn path_ids(path: &str) -> Option<(u16, u16)> {
    let ids = path.split('#').nth(1)?.to_ascii_lowercase();
    let (vid, pid) = ids.split_once('&')?;
    let hex = |h: &str| (h.len() == 4).then(|| u16::from_str_radix(h, 16).ok())?;
    Some((
        hex(vid.strip_prefix("vid_")?)?,
        hex(pid.strip_prefix("pid_")?)?,
    ))
}

/// A present device of the vendor and a product id asked for.
struct Matching {
    /// The device interface path.
    path: Vec<u16>,
    /// The path as text.
    name: String,
    pid: u16,
}

/// The present devices of vendor `vid` with a product id in `pids`.
fn matching_paths(vid: u16, pids: &[u16]) -> Result<Vec<Matching>> {
    Ok(DevInfo::usb_devices()?
        .paths()
        .into_iter()
        .filter_map(|path| {
            let name = String::from_utf16_lossy(&path);
            match path_ids(&name) {
                Some((v, pid)) if v == vid && pids.contains(&pid) => {
                    Some(Matching { path, name, pid })
                }
                _ => None,
            }
        })
        .collect())
}

pub(crate) fn list(vid: u16, pids: &[u16]) -> Result<Vec<crate::Listed>> {
    Ok(matching_paths(vid, pids)?
        .into_iter()
        .map(|m| crate::Listed {
            pid: m.pid,
            serial: path_serial(&m.name),
            // closed again at once
            available: Device::open_path(&m.path, None, m.pid).is_ok(),
            location: m.name,
        })
        .collect())
}

pub(crate) fn present_serials(vid: u16, pids: &[u16]) -> Result<Vec<String>> {
    Ok(matching_paths(vid, pids)?
        .into_iter()
        .filter_map(|m| path_serial(&m.name))
        .collect())
}

/// An opened device: the file handle, its WinUSB interface 0 handle, and the lock
/// that serializes control transfers.
#[derive(Debug)]
pub(crate) struct Device {
    wusb: WINUSB_INTERFACE_HANDLE,
    file: OwnedHandle,
    ctl_lock: Mutex<()>,
    serial: Option<String>,
    pid: u16,
}

// SAFETY: WinUSB and file handles may be used from any thread; every transfer brings
// its own OVERLAPPED / event (or is synchronous), control transfers are serialized by
// `ctl_lock`, and the handle values never change after `open`.
unsafe impl Send for Device {}
// SAFETY: as for `Send` -- concurrent WinUSB calls on one handle are thread-safe.
unsafe impl Sync for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: `wusb` is from WinUsb_Initialize and freed once. Nothing is in flight:
        // every transfer completes before its method returns, and a ReadQueue borrows
        // the device. `file` is closed afterwards by its own drop.
        unsafe { WinUsb_Free(self.wusb) };
    }
}

impl Device {
    pub(crate) fn open(vid: u16, pids: &[u16], index: usize) -> Result<Self> {
        let mut found = 0;
        let mut ret = Err(Error::NoDevice);
        for m in matching_paths(vid, pids)? {
            // Try to WinUSB-open each matching device; skip ones that fail (e.g. units
            // bound to the vendor driver). `index` counts only the WinUSB-openable
            // matches, so multiple units can coexist.
            match Self::open_path(&m.path, path_serial(&m.name), m.pid) {
                Ok(dev) if found == index => return Ok(dev),
                Ok(_) => found += 1,
                Err(e) => {
                    debug!(path = %m.name, "not WinUSB-openable: {}", map_win_error(e));
                    ret = Err(Error::Access); // a match existed but wasn't WinUSB
                }
            }
        }
        ret
    }

    pub(crate) fn open_serial(vid: u16, pids: &[u16], serial: &str) -> Result<Self> {
        let m = matching_paths(vid, pids)?
            .into_iter()
            .find(|m| path_serial(&m.name).is_some_and(|s| s.eq_ignore_ascii_case(serial)))
            .ok_or(Error::NoDevice)?;
        Self::open_path(&m.path, path_serial(&m.name), m.pid).map_err(|e| {
            debug!(path = %m.name, "not WinUSB-openable: {}", map_win_error(e));
            match map_win_error(e) {
                Error::NoDevice => Error::NoDevice, // unplugged in between
                _ => Error::Access,
            }
        })
    }

    pub(crate) fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }

    pub(crate) fn pid(&self) -> u16 {
        self.pid
    }

    /// `CreateFileW` (overlapped, as WinUSB requires) + `WinUsb_Initialize`.
    fn open_path(
        path: &[u16],
        serial: Option<String>,
        pid: u16,
    ) -> std::result::Result<Self, WIN32_ERROR> {
        let wide: Vec<u16> = path.iter().copied().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated path; null security attributes and template are allowed.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        // SAFETY: `h` is a valid handle that nothing else owns.
        let file = unsafe { OwnedHandle::from_raw_handle(h) };
        let mut wusb = null_mut();
        // SAFETY: an open device handle; `wusb` receives the interface handle.
        if unsafe { WinUsb_Initialize(file.as_raw_handle(), &mut wusb) } == 0 {
            return Err(last_error());
        }
        Ok(Self {
            wusb,
            file,
            ctl_lock: Mutex::new(()),
            serial,
            pid,
        })
    }

    fn set_policy<T: Copy>(&self, ep: u8, policy: WINUSB_PIPE_POLICY, value: T) -> bool {
        // SAFETY: `value` is a live `T` of the length passed (u8 / u32 per the policy).
        let ok = unsafe {
            WinUsb_SetPipePolicy(
                self.wusb,
                ep,
                policy,
                size_of::<T>() as u32,
                ptr::from_ref(&value).cast(),
            )
        };
        ok != 0
    }

    fn max_transfer_size(&self, ep: u8) -> Option<u32> {
        let mut max = 0u32;
        let mut len = size_of::<u32>() as u32;
        // SAFETY: `max` and `len` are live and `len` holds the size of `max`.
        let ok = unsafe {
            WinUsb_GetPipePolicy(
                self.wusb,
                ep,
                MAXIMUM_TRANSFER_SIZE,
                &mut len,
                ptr::from_mut(&mut max).cast(),
            )
        };
        (ok != 0).then_some(max)
    }

    /// Wait for an overlapped WinUSB operation with timeout; cancel it on timeout.
    /// Returns only once the operation is over (the buffer and `ov` are free again).
    fn wait_ov(&self, ov: &Overlapped, started: bool, timeout: Duration) -> Result<usize> {
        if !started {
            let e = last_error();
            if e != ERROR_IO_PENDING {
                return Err(map_win_error(e)); // never started: nothing in flight
            }
        }
        let ms = match timeout_ms(timeout) {
            0 => INFINITE,
            ms => ms,
        };
        // SAFETY: a valid event handle.
        let w = unsafe { WaitForSingleObject(ov.event(), ms) };
        let mut n = 0u32;
        if w != WAIT_OBJECT_0 {
            // Timed out (or, never seen, the wait itself failed): cancel and reap.
            let err = if w == WAIT_FAILED {
                last_error()
            } else {
                WAIT_TIMEOUT
            };
            // SAFETY: `ov` belongs to an I/O on `self.file`.
            unsafe { CancelIoEx(self.file.as_raw_handle(), ov.as_ptr()) };
            // A transfer that already moved bytes when the timeout hit returns them as
            // a short transfer instead of dropping them (a TS burst rarely fills a
            // 64 KiB read).
            // SAFETY: bWait = TRUE blocks until the cancelled I/O has completed.
            let ok = unsafe { WinUsb_GetOverlappedResult(self.wusb, ov.as_ptr(), &mut n, TRUE) };
            if ok != 0 && n > 0 {
                return Ok(n as usize);
            }
            return Err(map_win_error(err));
        }
        // SAFETY: the event is signalled, so the I/O has completed.
        check(unsafe { WinUsb_GetOverlappedResult(self.wusb, ov.as_ptr(), &mut n, FALSE) })?;
        Ok(n as usize)
    }

    /// `usbx_control`: the buffer is written only for an IN request.
    fn control(&self, s: Setup, data: *mut u8, len: usize, timeout: Duration) -> Result<usize> {
        let len = u16::try_from(len).map_err(|_| Error::Param)?;
        let ov = Overlapped::new()?;
        let sp = WINUSB_SETUP_PACKET {
            RequestType: s.request_type,
            Request: s.request,
            Value: s.value,
            Index: s.index,
            Length: len,
        };
        let _lock = self.ctl_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut n = 0u32;
        // SAFETY: `data` is valid for `len` bytes (the caller's slice) and, like `ov` and
        // `n`, outlives the transfer: wait_ov returns only once it is over.
        let ok = unsafe {
            WinUsb_ControlTransfer(self.wusb, sp, data, u32::from(len), &mut n, ov.as_ptr())
        };
        self.wait_ov(&ov, ok != 0, timeout)
    }

    pub(crate) fn control_in(&self, s: Setup, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        check_control_dir(s.request_type, true)?;
        self.control(s, buf.as_mut_ptr(), buf.len(), timeout)
    }

    pub(crate) fn control_out(&self, s: Setup, data: &[u8], timeout: Duration) -> Result<usize> {
        check_control_dir(s.request_type, false)?;
        // WinUSB only reads the buffer of a host-to-device request, but the kernel still
        // probes it for write access: a constant payload (`&[8, 0, 0, 0]`, promoted to
        // read-only static memory) fails with ERROR_NOACCESS (998). Send a copy.
        let mut buf = data.to_vec();
        self.control(s, buf.as_mut_ptr(), buf.len(), timeout)
    }

    /// `xfer_pipe`, OUT branch: overlapped write with timeout.
    pub(crate) fn bulk_out(&self, ep: u8, data: &[u8], timeout: Duration) -> Result<usize> {
        if is_in(ep) {
            return Err(Error::Param);
        }
        let len = u32::try_from(data.len()).map_err(|_| Error::Param)?;
        let ov = Overlapped::new()?;
        let mut n = 0u32;
        // SAFETY: `data`, `ov` and `n` outlive the transfer (wait_ov returns only once it
        // is over); WinUSB only reads `data`.
        let ok =
            unsafe { WinUsb_WritePipe(self.wusb, ep, data.as_ptr(), len, &mut n, ov.as_ptr()) };
        self.wait_ov(&ov, ok != 0, timeout)
    }

    /// `xfer_pipe`, IN branch.
    pub(crate) fn read(&self, ep: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if !is_in(ep) {
            return Err(Error::Param);
        }
        let len = u32::try_from(buf.len()).map_err(|_| Error::Param)?;
        // Bulk/interrupt IN: WinUSB's own per-pipe transfer timeout and a synchronous
        // read (as the vendor stack does). WinUSB then returns the bytes that
        // arrived within the timeout -- a TS burst is usually smaller than the 64 KiB
        // buffer, and the overlapped+CancelIoEx path dropped those partial reads, so
        // EP 0x81 delivered nothing. (Policy failures are ignored.)
        self.set_policy(ep, AUTO_CLEAR_STALL, 1u8);
        self.set_policy(ep, PIPE_TRANSFER_TIMEOUT, timeout_ms(timeout));
        let mut n = 0u32;
        // SAFETY: synchronous (no OVERLAPPED): `buf` is valid for `len` bytes throughout.
        check(unsafe { WinUsb_ReadPipe(self.wusb, ep, buf.as_mut_ptr(), len, &mut n, null()) })?;
        Ok(n as usize)
    }

    pub(crate) fn clear_halt(&self, ep: u8) -> Result<()> {
        // SAFETY: a valid interface handle; an unknown pipe id just fails.
        check(unsafe { WinUsb_ResetPipe(self.wusb, ep) })
    }

    pub(crate) fn abort_pipe(&self, ep: u8) -> Result<()> {
        // Cancels pending WinUSB transfers on the pipe; a synchronous WinUsb_ReadPipe
        // blocked on another thread then fails with ERROR_OPERATION_ABORTED.
        // SAFETY: a valid interface handle; an unknown pipe id just fails.
        check(unsafe { WinUsb_AbortPipe(self.wusb, ep) })
    }

    /// `usbx_rq_open`.
    pub(crate) fn read_queue(&self, ep: u8, n: usize, size: usize) -> Result<ReadQueue<'_>> {
        check_read_queue(ep, n, size)?;
        let size32 = u32::try_from(size).map_err(|_| Error::Param)?;
        let slots = (0..n)
            .map(|_| Slot::new(size))
            .collect::<Result<Box<[_]>>>()?;
        // No WinUSB timeout: requests stay posted until data arrives.
        self.set_policy(ep, AUTO_CLEAR_STALL, 1u8);
        self.set_policy(ep, PIPE_TRANSFER_TIMEOUT, 0u32);
        // RAW_IO: without it WinUSB hands the queued reads to the host controller one
        // at a time, the next from the previous one's completion routine, so a system
        // DPC latency spike leaves NO read posted and the device's small TS buffer
        // overflows. Hardware 2026-10-18 (CS ショップチャンネル, 10.5 Mbit/s, 67 min): 5
        // bursts of ~100 continuity errors across every PID, each in a minute where the
        // reads paused 125-203 ms (79 ms otherwise). With RAW_IO all n reads sit in the
        // controller. Needs size % max-packet == 0 and size <= MAXIMUM_TRANSFER_SIZE;
        // dropping the queue turns it off again for the synchronous reads (other sizes).
        let raw = self.max_transfer_size(ep).is_some_and(|max| size32 <= max)
            && self.set_policy(ep, RAW_IO, 1u8);
        let mut q = ReadQueue {
            dev: self,
            ep,
            size: size32,
            slots,
            head: 0,
            advance: false,
            raw,
        };
        for slot in &mut q.slots {
            slot.post(self.wusb, ep, size32);
        }
        Ok(q)
    }
}

/// One request of a [`ReadQueue`]. While `posted`, the kernel owns `buf` and `ov`;
/// both are heap allocations that stay put until the request is reaped.
struct Slot {
    ov: Overlapped,
    buf: Box<[u8]>,
    /// Request in flight: must be reaped before reuse or free.
    posted: bool,
    /// Submit error when not posted.
    err: WIN32_ERROR,
}

impl Slot {
    fn new(size: usize) -> Result<Self> {
        Ok(Self {
            ov: Overlapped::new()?,
            buf: vec![0; size].into_boxed_slice(),
            posted: false,
            err: 0,
        })
    }

    /// `rq_post`.
    fn post(&mut self, wusb: WINUSB_INTERFACE_HANDLE, ep: u8, size: u32) {
        debug_assert!(!self.posted);
        // SAFETY: not posted, so no I/O is in flight on the OVERLAPPED.
        let ov = unsafe { self.ov.reset() };
        // SAFETY: `buf` (`size` bytes) and the OVERLAPPED stay allocated and unmoved until
        // the request is reaped (in `ReadQueue::next` or its drop).
        let ok = unsafe { WinUsb_ReadPipe(wusb, ep, self.buf.as_mut_ptr(), size, null_mut(), ov) };
        if ok != 0 {
            self.posted = true;
        } else {
            self.err = last_error();
            self.posted = self.err == ERROR_IO_PENDING;
        }
    }
}

/// `usbx_rq`: `n` reads kept posted on a bulk IN pipe, completed in order.
pub(crate) struct ReadQueue<'d> {
    dev: &'d Device,
    ep: u8,
    size: u32,
    slots: Box<[Slot]>,
    head: usize,
    /// The head request was handed out (or failed) by the last `next`: re-post it and
    /// move on first (`usbx_rq_next`).
    advance: bool,
    /// RAW_IO turned on for this queue.
    raw: bool,
}

// SAFETY: the queue owns its buffers, OVERLAPPEDs and events outright, and the kernel
// completes posted reads whichever thread then waits for them; `&Device` is Send.
unsafe impl Send for ReadQueue<'_> {}

impl fmt::Debug for ReadQueue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadQueue")
            .field("ep", &format_args!("{:#04x}", self.ep))
            .field("n", &self.slots.len())
            .field("size", &self.size)
            .field("head", &self.head)
            .field("raw_io", &self.raw)
            .finish_non_exhaustive()
    }
}

impl ReadQueue<'_> {
    /// `usbx_rq_next` for the previous result, then `usbx_rq_wait`.
    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<&[u8]>> {
        let wusb = self.dev.wusb;
        if self.advance {
            self.advance = false;
            let slot = &mut self.slots[self.head];
            if !slot.posted {
                slot.post(wusb, self.ep, self.size);
            }
            self.head = (self.head + 1) % self.slots.len();
        }
        let slot = &mut self.slots[self.head];
        if !slot.posted {
            self.advance = true;
            return Err(map_win_error(slot.err));
        }
        // SAFETY: a valid event handle.
        match unsafe { WaitForSingleObject(slot.ov.event(), timeout_ms(timeout)) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(None),
            // Never seen; the read is still posted, so the next call waits for it again.
            _ => return Err(Error::Io(last_error() as i32)),
        }
        slot.posted = false;
        self.advance = true;
        let mut got = 0u32;
        // SAFETY: the event is signalled: the read has completed.
        check(unsafe { WinUsb_GetOverlappedResult(wusb, slot.ov.as_ptr(), &mut got, FALSE) })?;
        Ok(Some(&slot.buf[..got as usize]))
    }

    pub(crate) fn raw_io(&self) -> bool {
        self.raw
    }
}

impl Drop for ReadQueue<'_> {
    /// `usbx_rq_close`: abort, reap every posted request, restore RAW_IO; the
    /// buffers and events are freed afterwards with the slots.
    fn drop(&mut self) {
        let wusb = self.dev.wusb;
        // SAFETY: a valid interface handle.
        unsafe { WinUsb_AbortPipe(wusb, self.ep) };
        for slot in self.slots.iter_mut().filter(|s| s.posted) {
            let mut got = 0u32;
            // SAFETY: bWait = TRUE blocks until the aborted read has completed; the kernel
            // then no longer touches its buffer or OVERLAPPED.
            unsafe { WinUsb_GetOverlappedResult(wusb, slot.ov.as_ptr(), &mut got, TRUE) };
            slot.posted = false;
        }
        if self.raw {
            self.dev.set_policy(self.ep, RAW_IO, 0u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn win_errors_map() {
        let cases = [
            (ERROR_SEM_TIMEOUT, "timeout"),
            (WAIT_TIMEOUT, "timeout"),
            (ERROR_GEN_FAILURE, "endpoint stalled"),
            (
                ERROR_DEVICE_NOT_CONNECTED,
                "device not found or disconnected",
            ),
            (ERROR_FILE_NOT_FOUND, "device not found or disconnected"),
            (ERROR_BAD_COMMAND, "device not found or disconnected"),
            (
                ERROR_ACCESS_DENIED,
                "access denied (is the device bound to WinUSB?)",
            ),
            (ERROR_OPERATION_ABORTED, "transfer cancelled"),
            (ERROR_NOT_ENOUGH_MEMORY, "out of memory"),
            (87, "I/O error (OS error 87)"), // ERROR_INVALID_PARAMETER: not special-cased
        ];
        for (code, text) in cases {
            assert_eq!(map_win_error(code).to_string(), text, "Win32 error {code}");
        }
        assert_matches!(map_win_error(ERROR_IO_PENDING), Error::Io(997));
    }

    #[test]
    fn timeouts_round_up_to_whole_ms() {
        assert_eq!(timeout_ms(Duration::ZERO), 0);
        assert_eq!(
            timeout_ms(Duration::from_nanos(1)),
            1,
            "never 0 = no timeout"
        );
        assert_eq!(timeout_ms(Duration::from_micros(999)), 1);
        assert_eq!(timeout_ms(Duration::from_millis(1)), 1);
        assert_eq!(timeout_ms(Duration::from_micros(1001)), 2);
        assert_eq!(timeout_ms(Duration::from_millis(200)), 200);
        assert_eq!(timeout_ms(Duration::MAX), INFINITE);
    }

    /// Exercises the SetupDi enumeration and the detail-struct parsing on every USB
    /// device present (opens none of them).
    #[test]
    fn usb_device_paths_parse() {
        let paths = DevInfo::usb_devices()
            .expect("SetupDiGetClassDevsW")
            .paths();
        let mut ours = 0;
        for p in &paths {
            let s = String::from_utf16_lossy(p).to_ascii_lowercase();
            assert!(s.starts_with(r"\\?\"), "{s}");
            assert!(s.ends_with("{a5dcbf10-6530-11d2-901f-00c04fb951ed}"), "{s}");
            ours += usize::from(
                path_ids(&s).is_some_and(|(v, p)| v == crate::VID && crate::PIDS.contains(&p)),
            );
        }
        eprintln!("{} USB device interface(s), {ours} of ours", paths.len());
    }

    #[test]
    fn open_absent_device_is_no_device() {
        // No device has VID/PID 0000:0000, so nothing is ever opened.
        assert_matches!(Device::open(0, &[0], 0), Err(Error::NoDevice));
        assert_matches!(
            Device::open_serial(0, &[0], "CD23456789"),
            Err(Error::NoDevice)
        );
        assert_eq!(list(0, &[0]).expect("enumerates"), []);
    }

    #[test]
    fn serial_from_device_path() {
        let guid = "{a5dcbf10-6530-11d2-901f-00c04fb951ed}";
        assert_eq!(
            path_serial(&format!(r"\\?\usb#vid_06b8&pid_103b#cd23456789#{guid}")).as_deref(),
            Some("CD23456789")
        );
        assert_eq!(
            path_serial(&format!(r"\\?\USB#VID_06B8&PID_103B#EF34567890#{guid}")).as_deref(),
            Some("EF34567890")
        );
        // No serial: Windows generated the instance id.
        assert_eq!(
            path_serial(&format!(r"\\?\usb#vid_046d&pid_c52b#5&2c0c0c5d&0&2#{guid}")),
            None
        );
        assert_eq!(path_serial(r"\\?\usb#vid_06b8&pid_103b"), None);
        assert_eq!(
            path_serial(&format!(r"\\?\usb#vid_06b8&pid_103b##{guid}")),
            None
        );
    }

    #[test]
    fn ids_from_device_path() {
        let guid = "{a5dcbf10-6530-11d2-901f-00c04fb951ed}";
        assert_eq!(
            path_ids(&format!(r"\\?\usb#vid_06b8&pid_1040#tk07002372#{guid}")),
            Some((0x06B8, 0x1040))
        );
        assert_eq!(
            path_ids(&format!(r"\\?\USB#VID_06B8&PID_103B#CD23456789#{guid}")),
            Some((0x06B8, 0x103B))
        );
        // an interface of a composite device: the ids, then the interface number
        assert_eq!(
            path_ids(&format!(
                r"\\?\usb#vid_046d&pid_c22d&mi_00#7&242dfaaa&0&0000#{guid}"
            )),
            None
        );
        assert_eq!(path_ids(r"\\?\usb#vid_06b8&pid_103"), None);
        assert_eq!(path_ids(r"\\?\usb"), None);
    }

    #[test]
    fn overlapped_starts_clear() {
        let ov = Overlapped::new().expect("event");
        // SAFETY: nothing was ever posted on it.
        let p = unsafe { ov.reset() };
        // SAFETY: `p` points to the boxed OVERLAPPED, not in use.
        let o = unsafe { &*p };
        assert_eq!(o.hEvent, ov.event());
        assert_eq!(o.Internal, 0);
        // SAFETY: a valid event handle.
        let w = unsafe { WaitForSingleObject(ov.event(), 0) };
        assert_eq!(w, WAIT_TIMEOUT, "event reset");
    }
}
