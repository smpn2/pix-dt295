//! PIX-DT295 user-mode driver core.
//!
//! * [`Device`] -- open, boot + authenticate ([`Device::start`]), tune
//!   ([`Device::tune_t`] / [`Device::tune_s`]), and stream one service or every
//!   service of the TS as descrambled, link-decrypted MPEG-TS
//!   ([`Device::stream_start`]) on the System1 path (trc firmware, EP 0x81).
//! * [`ts`] -- pure TS / PSI logic (CRC, PAT checks, link decryption, the
//!   satellite PSI keeper, stream statistics); no device I/O, unit-tested.
//! * [`state`] -- the host's persistent state file (link key, PSI).
//! * [`chan`] -- the built-in ISDB-T channel tables, the satellite ini line and the
//!   tuning spaces of a BonDriver ini; [`bonini`] -- its `[Tuner]`; [`profile`] -- the
//!   Win32 profile API's reading of an ini file.
//! * [`CardWatch`] -- when a host should start a stream over for the B-CAS card;
//!   [`StallWatch`] -- for a firmware that stopped acking.
//!
//! The hardware primitives (register / command protocol, I2C, front end, firmware
//! download, shared types) live in the `dt295-core` crate and the blob-free raw-TS
//! path in `dt295-rawfw`; both are re-exported here so the public API is unchanged.
//!
//! Logging uses `tracing`; the host installs the subscriber.

pub mod ch2;
pub mod state;
pub mod ts;

mod auth;
mod cas;
mod device;
mod dtvdec;
mod fw;
mod s1;
mod watch;

pub use device::{Device, KeyJournal, Live};
pub use dt295_core::fwdl::{IDLE_FIRMWARE, TRC_FIRMWARE};
pub use dt295_core::{
    Error, Result, Status, System, TsSink, bonini, chan, error, ipc, profile, serve,
};
pub use dt295_rawfw::{RAW_FIRST_TS, RAW_STALL, RAW_STREAM_CAP, RawCaptureStats, raw_stall_due};
pub use dt295_usb::Listed;
pub use watch::{CardWatch, StallWatch, StartOver};

/// The present PIX-DT295 units with their USB serial numbers, in the order
/// [`Device::open`] counts them; each is opened and closed again (no transfers) to
/// tell whether it is free.
pub fn list() -> Result<Vec<Listed>> {
    Ok(dt295_usb::list(dt295_usb::VID, dt295_usb::PID)?)
}

/// The USB serial numbers of the present PIX-DT295 units, without opening any.
pub fn present_serials() -> Result<Vec<String>> {
    Ok(dt295_usb::present_serials(dt295_usb::VID, dt295_usb::PID)?)
}

/// One cached satellite PSI section ([`Device::psi_export`] / [`Device::psi_import`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsiRecord {
    /// 1 PAT, 2 PMT, 3 NIT, 4 SDT.
    pub kind: u8,
    /// An SDT kept from SDT\[other\] (table 0x46).
    pub other: bool,
    /// transport_stream_id (PAT/PMT/SDT) or network_id (NIT).
    pub id: u16,
    /// program_number (PMT) or section_number (NIT/SDT).
    pub sub: u16,
    /// Day (Unix time / 86400) the device last delivered it.
    pub seen_day: u32,
    /// The full section, table_id .. CRC_32.
    pub section: Vec<u8>,
}

/// Sections not delivered for this many days are dropped on import.
pub const PSI_KEEP_DAYS: u32 = 90;

/// Today as a Unix day number (the unit of [`PsiRecord::seen_day`]).
pub fn today() -> u32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs / 86_400) as u32
}
