//! The C ABI between the C++ shim (`cpp/bonshim.cpp`) and the Rust [`Driver`], and
//! the DLL's one export, `CreateBonDriver`.
//!
//! The shim's `CBonShim : IBonDriver2` forwards each of its 17 methods to the
//! matching `BonOps` function with the opaque `ctx` (a `Box<Driver>`), so the host
//! sees a genuine MSVC class (vtable, RTTI for `dynamic_cast`, `__thiscall` on x86)
//! while all logic is here. Every function catches panics: a panic must never
//! unwind into (or abort) the host, so it becomes FALSE / 0 / NULL /
//! `WAIT_ABANDONED`, logged by the panic hook.

use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::ptr;
use std::sync::{Mutex, PoisonError};

use crate::driver::{Driver, WAIT_ABANDONED};

#[allow(clippy::upper_case_acronyms)]
type BOOL = i32;
#[allow(clippy::upper_case_acronyms)]
type DWORD = u32;
#[allow(clippy::upper_case_acronyms)]
type BYTE = u8;
#[allow(clippy::upper_case_acronyms)]
type LPCWSTR = *const u16;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

/// The function table the shim calls (field for field `struct BonOps` in
/// `cpp/bonshim.cpp`; C calling convention on x86 and x64).
#[repr(C)]
#[derive(Debug)]
pub struct BonOps {
    // IBonDriver
    pub open_tuner: unsafe extern "C" fn(*mut c_void) -> BOOL,
    pub close_tuner: unsafe extern "C" fn(*mut c_void),
    pub set_channel_byte: unsafe extern "C" fn(*mut c_void, BYTE) -> BOOL,
    pub get_signal_level: unsafe extern "C" fn(*mut c_void) -> f32,
    pub wait_ts_stream: unsafe extern "C" fn(*mut c_void, DWORD) -> DWORD,
    pub get_ready_count: unsafe extern "C" fn(*mut c_void) -> DWORD,
    pub get_ts_stream_copy:
        unsafe extern "C" fn(*mut c_void, *mut BYTE, *mut DWORD, *mut DWORD) -> BOOL,
    pub get_ts_stream_ptr:
        unsafe extern "C" fn(*mut c_void, *mut *mut BYTE, *mut DWORD, *mut DWORD) -> BOOL,
    pub purge_ts_stream: unsafe extern "C" fn(*mut c_void),
    pub release: unsafe extern "C" fn(*mut c_void),
    // IBonDriver2
    pub get_tuner_name: unsafe extern "C" fn(*mut c_void) -> LPCWSTR,
    pub is_tuner_opening: unsafe extern "C" fn(*mut c_void) -> BOOL,
    pub enum_tuning_space: unsafe extern "C" fn(*mut c_void, DWORD) -> LPCWSTR,
    pub enum_channel_name: unsafe extern "C" fn(*mut c_void, DWORD, DWORD) -> LPCWSTR,
    pub set_channel: unsafe extern "C" fn(*mut c_void, DWORD, DWORD) -> BOOL,
    pub get_cur_space: unsafe extern "C" fn(*mut c_void) -> DWORD,
    pub get_cur_channel: unsafe extern "C" fn(*mut c_void) -> DWORD,
}

// The shim static_asserts the same size.
const _: () = assert!(size_of::<BonOps>() == 17 * size_of::<usize>());

/// The one table every shim object points to.
pub static OPS: BonOps = BonOps {
    open_tuner,
    close_tuner,
    set_channel_byte,
    get_signal_level,
    wait_ts_stream,
    get_ready_count,
    get_ts_stream_copy,
    get_ts_stream_ptr,
    purge_ts_stream,
    release,
    get_tuner_name,
    is_tuner_opening,
    enum_tuning_space,
    enum_channel_name,
    set_channel,
    get_cur_space,
    get_cur_channel,
};

unsafe extern "C" {
    /// `cpp/bonshim.cpp`: a new `CBonShim` (as `IBonDriver *`) forwarding to `ops`
    /// with `ctx`; null if out of memory. Never throws.
    fn bonshim_new(ops: *const BonOps, ctx: *mut c_void) -> *mut c_void;
}

/// The live instance: the shim object handed to the host and its driver.
struct Instance {
    shim: *mut c_void,
    ctx: *mut c_void,
}

// SAFETY: the two pointers are only compared and returned here; the objects they
// point to are used through the shim, whose driver is `Send + Sync`.
unsafe impl Send for Instance {}

/// One instance per process: `CreateBonDriver` returns the
/// existing one until it is released.
static INSTANCE: Mutex<Option<Instance>> = Mutex::new(None);

/// Run `f`, turning a panic into `on_panic` (the panic hook has logged it).
fn guard<R>(on_panic: R, f: impl FnOnce() -> R) -> R {
    panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(on_panic)
}

/// The driver behind a shim's `ctx`.
///
/// # Safety
/// `ctx` is null or the pointer `CreateBonDriver` gave the shim, not yet released.
unsafe fn driver<'a>(ctx: *mut c_void) -> Option<&'a Driver> {
    // SAFETY: per the contract, a non-null ctx is a live `Box<Driver>` allocation.
    unsafe { ctx.cast::<Driver>().as_ref() }
}

/// The host's entry point (looked up with `GetProcAddress`): the `IBonDriver`
/// object, the same one until it is released. Null on failure.
#[unsafe(no_mangle)]
pub extern "C" fn CreateBonDriver() -> *mut c_void {
    guard(ptr::null_mut(), || {
        let mut instance = INSTANCE.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = instance.as_ref() {
            return i.shim;
        }
        let ctx = Box::into_raw(Box::new(Driver::new())).cast::<c_void>();
        // SAFETY: OPS is 'static and `ctx` a live driver; the shim owns `ctx` from
        // here and gives it back exactly once, to `release`.
        let shim = unsafe { bonshim_new(&OPS, ctx) };
        if shim.is_null() {
            // SAFETY: `ctx` came from Box::into_raw above and was handed to no one.
            drop(unsafe { Box::from_raw(ctx.cast::<Driver>()) });
            return ptr::null_mut();
        }
        *instance = Some(Instance { shim, ctx });
        shim
    })
}

// ---- the BonOps functions ----
//
// Contract of each (the shim keeps it): `ctx` is the pointer `bonshim_new` was given,
// and `release` has not been called yet.

unsafe extern "C" fn open_tuner(ctx: *mut c_void) -> BOOL {
    guard(FALSE, || {
        // SAFETY: the shim passes back its own live ctx (see above).
        let Some(d) = (unsafe { driver(ctx) }) else {
            return FALSE;
        };
        d.open_tuner().into()
    })
}

unsafe extern "C" fn close_tuner(ctx: *mut c_void) {
    guard((), || {
        // SAFETY: the shim passes back its own live ctx.
        if let Some(d) = unsafe { driver(ctx) } {
            d.close_tuner();
        }
    });
}

unsafe extern "C" fn set_channel_byte(ctx: *mut c_void, ch: BYTE) -> BOOL {
    guard(FALSE, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return FALSE;
        };
        d.set_channel_byte(ch).into()
    })
}

unsafe extern "C" fn get_signal_level(ctx: *mut c_void) -> f32 {
    guard(0.0, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return 0.0;
        };
        d.signal_level()
    })
}

unsafe extern "C" fn wait_ts_stream(ctx: *mut c_void, timeout_ms: DWORD) -> DWORD {
    guard(WAIT_ABANDONED, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return WAIT_ABANDONED;
        };
        d.wait_ts_stream(timeout_ms)
    })
}

unsafe extern "C" fn get_ready_count(ctx: *mut c_void) -> DWORD {
    guard(0, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return 0;
        };
        d.ready_count()
    })
}

/// `GetTsStream(BYTE *pDst, ...)`: copies up to one chunk (`TS_CHUNK` bytes, the
/// buffer size the BonDriver interface assumes) into the host's buffer.
unsafe extern "C" fn get_ts_stream_copy(
    ctx: *mut c_void,
    dst: *mut BYTE,
    size: *mut DWORD,
    remain: *mut DWORD,
) -> BOOL {
    guard(FALSE, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return FALSE;
        };
        if dst.is_null() || size.is_null() {
            return FALSE;
        }
        let n = d.pop_ts_with(|a, b| {
            // SAFETY: `dst` is the host's buffer of at least one chunk, and
            // a.len() + b.len() <= TS_CHUNK; our ring and the host buffer are
            // distinct allocations.
            unsafe {
                ptr::copy_nonoverlapping(a.as_ptr(), dst, a.len());
                ptr::copy_nonoverlapping(b.as_ptr(), dst.add(a.len()), b.len());
            }
        });
        // SAFETY: `size` is non-null and points to the host's DWORD.
        unsafe { size.write(n as DWORD) };
        if !remain.is_null() {
            // SAFETY: non-null, the host's DWORD.
            unsafe { remain.write(d.ready_count()) };
        }
        TRUE
    })
}

/// `GetTsStream(BYTE **ppDst, ...)`: a pointer into the driver's own buffer (valid
/// until the next call); with no data TRUE, size 0 and a null pointer.
unsafe extern "C" fn get_ts_stream_ptr(
    ctx: *mut c_void,
    dst: *mut *mut BYTE,
    size: *mut DWORD,
    remain: *mut DWORD,
) -> BOOL {
    guard(FALSE, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return FALSE;
        };
        if dst.is_null() || size.is_null() {
            return FALSE;
        }
        let (p, n) = d.ts_stream_ptr();
        // SAFETY: `dst` and `size` are non-null and point to the host's variables.
        unsafe {
            dst.write(p);
            size.write(n as DWORD);
        }
        if !remain.is_null() {
            // SAFETY: non-null, the host's DWORD.
            unsafe { remain.write(d.ready_count()) };
        }
        TRUE
    })
}

unsafe extern "C" fn purge_ts_stream(ctx: *mut c_void) {
    guard((), || {
        // SAFETY: the shim passes back its own live ctx.
        if let Some(d) = unsafe { driver(ctx) } {
            d.purge_ts_stream();
        }
    });
}

/// `Release`: drops the driver (closes the tuner, joins the client's threads, flushes
/// and closes the log); the shim deletes itself afterwards.
unsafe extern "C" fn release(ctx: *mut c_void) {
    guard((), || {
        if ctx.is_null() {
            return;
        }
        {
            let mut instance = INSTANCE.lock().unwrap_or_else(PoisonError::into_inner);
            if instance.as_ref().is_some_and(|i| i.ctx == ctx) {
                *instance = None;
            }
        }
        // SAFETY: `ctx` came from Box::into_raw in CreateBonDriver; the shim calls
        // release once and never uses ctx again.
        drop(unsafe { Box::from_raw(ctx.cast::<Driver>()) });
    });
}

unsafe extern "C" fn get_tuner_name(ctx: *mut c_void) -> LPCWSTR {
    guard(ptr::null(), || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return ptr::null();
        };
        d.tuner_name().as_ptr()
    })
}

unsafe extern "C" fn is_tuner_opening(ctx: *mut c_void) -> BOOL {
    guard(FALSE, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return FALSE;
        };
        d.is_tuner_opening().into()
    })
}

unsafe extern "C" fn enum_tuning_space(ctx: *mut c_void, space: DWORD) -> LPCWSTR {
    guard(ptr::null(), || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return ptr::null();
        };
        d.enum_tuning_space(space)
            .map_or(ptr::null(), <[u16]>::as_ptr)
    })
}

unsafe extern "C" fn enum_channel_name(ctx: *mut c_void, space: DWORD, channel: DWORD) -> LPCWSTR {
    guard(ptr::null(), || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return ptr::null();
        };
        d.enum_channel_name(space, channel)
            .map_or(ptr::null(), <[u16]>::as_ptr)
    })
}

unsafe extern "C" fn set_channel(ctx: *mut c_void, space: DWORD, channel: DWORD) -> BOOL {
    guard(FALSE, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return FALSE;
        };
        d.set_channel(space, channel).into()
    })
}

unsafe extern "C" fn get_cur_space(ctx: *mut c_void) -> DWORD {
    guard(crate::driver::NO_SELECTION, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return crate::driver::NO_SELECTION;
        };
        d.cur_space()
    })
}

unsafe extern "C" fn get_cur_channel(ctx: *mut c_void) -> DWORD {
    guard(crate::driver::NO_SELECTION, || {
        // SAFETY: the shim passes back its own live ctx.
        let Some(d) = (unsafe { driver(ctx) }) else {
            return crate::driver::NO_SELECTION;
        };
        d.cur_channel()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        /// `cpp/bonshim_test.cpp`: `dynamic_cast<IBonDriver2 *>`, a round of vtable
        /// calls on a closed tuner, then `Release`. 1 = all checks passed.
        fn bonshim_test_drive(bon: *mut c_void, name: *mut u16, cap: i32, spaces: *mut u32) -> i32;
    }

    /// The host's view: CreateBonDriver, the C++ object, Release (one test, as the
    /// instance is process-wide).
    #[test]
    fn host_drives_the_shim() {
        let bon = CreateBonDriver();
        assert!(!bon.is_null());
        assert_eq!(CreateBonDriver(), bon, "one instance until released");

        let mut name = [0u16; 32];
        let mut spaces = 0u32;
        // SAFETY: `bon` is the live IBonDriver; `name` holds 32 units.
        let rc = unsafe { bonshim_test_drive(bon, name.as_mut_ptr(), 32, &mut spaces) };
        assert_eq!(rc, 1, "check {} of bonshim_test.cpp failed", -rc);
        let len = name.iter().position(|&c| c == 0).unwrap();
        assert_eq!(String::from_utf16_lossy(&name[..len]), "PIX-DT295");
        assert!(spaces >= 2, "UHF and CATV unless the ini hides them");
        assert!(
            INSTANCE.lock().unwrap().is_none(),
            "Release forgot the instance"
        );

        // After Release a new instance is created.
        let again = CreateBonDriver();
        assert!(!again.is_null());
        // SAFETY: as above.
        let rc = unsafe { bonshim_test_drive(again, name.as_mut_ptr(), 32, &mut spaces) };
        assert_eq!(rc, 1);
    }

    /// The ops directly (as the shim calls them), including a null ctx.
    #[test]
    fn ops_table() {
        let ctx = Box::into_raw(Box::new(Driver::default())).cast::<c_void>();
        // SAFETY: `ctx` is a live driver until `release` below; the out pointers
        // are valid locals.
        unsafe {
            assert_eq!(*(OPS.get_tuner_name)(ctx), u16::from(b'P'));
            assert!(!(OPS.enum_tuning_space)(ctx, 0).is_null());
            assert!((OPS.enum_tuning_space)(ctx, 99).is_null());
            assert!((OPS.enum_channel_name)(ctx, 0, 999).is_null());
            assert_eq!((OPS.set_channel)(ctx, 0, 0), FALSE, "not open");
            let mut p: *mut u8 = ptr::dangling_mut();
            let (mut size, mut remain) = (1, 1);
            assert_eq!(
                (OPS.get_ts_stream_ptr)(ctx, &mut p, &mut size, &mut remain),
                TRUE
            );
            assert!(p.is_null());
            assert_eq!((size, remain), (0, 0));
            assert_eq!(
                (OPS.get_ts_stream_ptr)(ctx, ptr::null_mut(), &mut size, &mut remain),
                FALSE
            );
            assert_eq!(
                (OPS.get_ts_stream_copy)(ctx, ptr::null_mut(), &mut size, ptr::null_mut()),
                FALSE
            );
            assert_eq!((OPS.wait_ts_stream)(ctx, 1), crate::driver::WAIT_TIMEOUT);
            // A null ctx is refused, not dereferenced.
            assert_eq!((OPS.open_tuner)(ptr::null_mut()), FALSE);
            assert_eq!((OPS.wait_ts_stream)(ptr::null_mut(), 1), WAIT_ABANDONED);
            (OPS.release)(ctx);
        }
    }

    #[test]
    fn panics_become_failure_values() {
        assert_eq!(guard(FALSE, || panic!("boom")), FALSE);
        assert_eq!(
            guard(WAIT_ABANDONED, || -> DWORD { panic!("boom") }),
            WAIT_ABANDONED
        );
        assert_eq!(guard(TRUE, || TRUE), TRUE);
    }
}
