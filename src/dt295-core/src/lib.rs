//! PIX-DT295 hardware primitives and shared types.
//!
//! This crate holds the low-level pieces the rest of the driver builds on: the
//! register / command protocol over USB ([`proto`]), the type-9 I2C transport
//! ([`i2c`]), the System1 front end ([`fe1`]), the channel tables and tuning spaces
//! ([`chan`]), the ini rules of the Win32 profile API ([`profile`], from `dt295-chdef`)
//! and the BonDriver ini's `[Tuner]` ([`bonini`]), the C/N conversions ([`signal`]), the firmware
//! download helpers ([`fwdl`]), what `dt295 serve` and its clients say ([`serve`]) and
//! where they meet ([`ipc`]), the driver error type ([`error`]), and the shared
//! [`System`] / [`Status`] / [`TsSink`] types.
//!
//! Logging uses `tracing`; the host installs the subscriber.

pub mod bonini;
pub mod chan;
pub mod error;
pub mod fe1;
pub mod fwdl;
pub mod i2c;
pub mod ipc;
pub mod proto;
pub mod serve;
pub mod signal;

pub use dt295_chdef::profile;
pub use error::{Error, Result};

/// The TS bulk endpoint (EP 0x81) of the System1 and raw-TS data paths.
pub const EP_TS: u8 = 0x81;

/// Broadcast system of a tune.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    /// ISDB-T (terrestrial, CATV pass-through).
    Terrestrial,
    /// ISDB-S (BS / CS110).
    Satellite,
}

/// Demodulator status read at tune time ([`fe1::read_status`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub locked: bool,
    /// C/N in 0.001 dB.
    pub cn_mdb: i32,
    /// The raw C/N register value (for diagnostics).
    pub raw: u32,
}

/// Receives the TS of a running stream, on the stream worker thread, in 188-byte
/// packet multiples. Must not block for long (the device buffers only ~100 ms).
pub type TsSink = Box<dyn FnMut(&[u8]) + Send + 'static>;
