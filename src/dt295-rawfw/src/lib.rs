//! PIX-DT295 blob-free raw-TS path.
//!
//! This crate carries the host side of the blob-free raw-TS capture: the blob-free COLD
//! stream worker ([`rawstream`]), our own FR monitor host side ([`frmon_host`]) and the
//! blob-free demod I2C ([`fjcore`]). It is built on the hardware primitives in `dt295-core`.
//!
//! Logging uses `tracing`; the host installs the subscriber.

pub mod fjcore;
pub mod frmon_host;
mod hwpath;
mod prefetch;
mod prio;
pub mod rawstream;

pub use rawstream::{RAW_FIRST_TS, RAW_STALL, RAW_STREAM_CAP, RawCaptureStats, raw_stall_due};

use dt295_core::proto::Io;

/// The device I/O, taken for one operation at a time. The stream worker of each of a
/// two-tuner model's two tuners takes it for a pump call, an I2C transfer, a capture
/// start, and gives it back, so the two streams share the unit.
pub trait IoAccess {
    /// Run `f` with the device I/O, held for its duration only.
    fn with_io<R>(&self, f: impl FnOnce(&Io) -> R) -> R;
    /// The USB device, for the bulk IN reads (they need no lock).
    fn usb(&self) -> &dt295_usb::Device;
}
