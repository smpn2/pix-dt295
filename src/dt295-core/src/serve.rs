//! What `dt295 serve` and its clients (`dt295 tune`, the BonDriver) say over the socket.
//! The client sends one line and the daemon answers one line. After the `ok` of a
//! `tune`, the TS follows until either side closes; a `status` ends with its answer:
//!
//! ```text
//! > tune unit=AB12345678 ter=557142857 sid=1024
//! > tune unit=AB12345678 sat=1318000000 tsid=40f1 sid=101
//! > tune unit=AB12345678 sat=1318000000 tsid=40f1 sid=929 if-free
//! > tune ter=557142857 mode=raw-ts client=4242-1 if-free
//! > status unit=AB12345678
//! < ok unit=AB12345678 cn=33500
//! < error unit AB12345678 is not connected
//! < busy unit AB12345678 is streaming
//! ```
//!
//! Frequencies are in Hz, `tsid` in hex, `sid` in decimal or `0x` hex; `tsid` is
//! optional (default: the frequency's first TS, as with `dt295 capture`), and so is
//! `sid` (default: every service of the TS). Without `unit` the daemon takes the only
//! unit connected. `mode` asks for the System1 path (`system1`: the vendor firmware and
//! the unit's B-CAS card) or the blob-free one (`raw-ts`: the scrambled TS); without it
//! the unit runs as configured. `if-free` asks for the tuner only if no stream of another
//! client runs on it (a request without it takes the tuner over); `busy` is the answer
//! when one does (an older client takes it for `error`). `client` names the client, so
//! that its own stream is replaced at once on a channel change.
//!
//! The `ok` gives the unit (its USB serial number) and, when known, the C/N (0.001 dB):
//! of the tune, and of a `status` the stream's latest.

use crate::chan::NO_TSID;

/// The longest request line a daemon reads.
pub const MAX_LINE: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tuning {
    /// ISDB-T: the channel centre frequency.
    Terrestrial { hz: u32 },
    /// ISDB-S: the IF frequency and the broadcast transport_stream_id.
    Satellite { hz: u32, tsid: u16 },
}

/// How a unit streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The vendor firmware's System1 path: descrambled by the unit's B-CAS card.
    System1,
    /// The blob-free raw-TS path: the scrambled TS, descrambled by the host.
    RawTs,
}

impl Mode {
    pub const fn word(self) -> &'static str {
        match self {
            Self::System1 => "system1",
            Self::RawTs => "raw-ts",
        }
    }
}

/// A `tune` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The USB serial number of the unit; empty = the only unit connected.
    pub unit: String,
    pub tuning: Tuning,
    pub sid: Option<u16>,
    /// The path to stream on; `None` = the unit's own.
    pub mode: Option<Mode>,
    /// Who asks (no blanks): a request of the client whose stream runs replaces it, also
    /// with `if_free`.
    pub client: Option<String>,
    /// Only if the unit is free: never take it over from another client's stream.
    pub if_free: bool,
}

/// A `status` request: the unit's latest C/N.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRequest {
    /// The USB serial number of the unit; empty = the only unit connected.
    pub unit: String,
}

/// Any request line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    Tune(Request),
    Status(StatusRequest),
}

impl Asked {
    pub fn parse(line: &str) -> Result<Self, String> {
        match line.split_whitespace().next() {
            Some("tune") => Request::parse(line).map(Self::Tune),
            Some("status") => StatusRequest::parse(line).map(Self::Status),
            _ => Err("expected a tune or status request".into()),
        }
    }

    /// The unit asked for; empty = the only one.
    pub fn unit(&self) -> &str {
        match self {
            Self::Tune(r) => &r.unit,
            Self::Status(s) => &s.unit,
        }
    }

    /// The request line, without its newline.
    pub fn to_line(&self) -> String {
        match self {
            Self::Tune(r) => r.to_line(),
            Self::Status(s) => s.to_line(),
        }
    }
}

impl Request {
    /// The request line, without its newline.
    pub fn to_line(&self) -> String {
        let mut s = "tune".to_owned();
        if !self.unit.is_empty() {
            s.push_str(&format!(" unit={}", self.unit));
        }
        match self.tuning {
            Tuning::Terrestrial { hz } => s.push_str(&format!(" ter={hz}")),
            Tuning::Satellite { hz, tsid: NO_TSID } => s.push_str(&format!(" sat={hz}")),
            Tuning::Satellite { hz, tsid } => s.push_str(&format!(" sat={hz} tsid={tsid:04x}")),
        }
        if let Some(sid) = self.sid {
            s.push_str(&format!(" sid={sid}"));
        }
        if let Some(mode) = self.mode {
            s.push_str(&format!(" mode={}", mode.word()));
        }
        if let Some(client) = &self.client {
            s.push_str(&format!(" client={client}"));
        }
        if self.if_free {
            s.push_str(" if-free");
        }
        s
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split_whitespace();
        if words.next() != Some("tune") {
            return Err("expected a tune request".into());
        }
        let (mut unit, mut ter, mut sat, mut tsid, mut sid) = (None, None, None, None, None);
        let (mut mode, mut client) = (None, None);
        let mut if_free = false;
        for word in words {
            if word == "if-free" {
                if_free = true;
                continue;
            }
            let (key, value) = word
                .split_once('=')
                .ok_or_else(|| format!("{word:?}: expected key=value"))?;
            match key {
                "unit" if !value.is_empty() => unit = Some(value.to_owned()),
                "ter" => ter = Some(parse_u32(value)?),
                "sat" => sat = Some(parse_u32(value)?),
                "tsid" => tsid = Some(parse_hex_u16(value)?),
                "sid" => sid = Some(parse_u16(value)?),
                "mode" if value == "system1" => mode = Some(Mode::System1),
                "mode" if value == "raw-ts" => mode = Some(Mode::RawTs),
                "client" if !value.is_empty() => client = Some(value.to_owned()),
                _ => return Err(format!("{word:?}: unknown")),
            }
        }
        let tuning = match (ter, sat, tsid) {
            (Some(hz), None, None) => Tuning::Terrestrial { hz },
            (None, Some(hz), tsid) => Tuning::Satellite {
                hz,
                tsid: tsid.unwrap_or(NO_TSID),
            },
            _ => return Err("expected ter=, or sat= (with tsid=)".into()),
        };
        Ok(Self {
            unit: unit.unwrap_or_default(),
            tuning,
            sid,
            mode,
            client,
            if_free,
        })
    }
}

impl StatusRequest {
    /// The request line, without its newline.
    pub fn to_line(&self) -> String {
        if self.unit.is_empty() {
            "status".into()
        } else {
            format!("status unit={}", self.unit)
        }
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split_whitespace();
        if words.next() != Some("status") {
            return Err("expected a status request".into());
        }
        let mut unit = None;
        for word in words {
            match word.split_once('=') {
                Some(("unit", value)) if !value.is_empty() => unit = Some(value.to_owned()),
                _ => return Err(format!("{word:?}: unknown")),
            }
        }
        Ok(Self {
            unit: unit.unwrap_or_default(),
        })
    }
}

/// The daemon's answer to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A stream runs (the TS follows), or the status: the unit's serial number and the
    /// C/N (0.001 dB) when the daemon gives them (`ok unit=AB12345678 cn=33500`).
    Ok {
        unit: Option<String>,
        cn_mdb: Option<i32>,
    },
    /// An `if-free` request for a unit that streams for another client.
    Busy(String),
    Error(String),
}

impl Reply {
    /// The reply line, without its newline.
    pub fn to_line(&self) -> String {
        match self {
            Self::Ok { unit, cn_mdb } => {
                let mut s = "ok".to_owned();
                if let Some(unit) = unit {
                    s.push_str(&format!(" unit={unit}"));
                }
                if let Some(cn) = cn_mdb {
                    s.push_str(&format!(" cn={cn}"));
                }
                s
            }
            // one line, whatever the message holds
            Self::Busy(why) => format!("busy {}", why.replace(['\r', '\n'], " ")),
            Self::Error(why) => format!("error {}", why.replace(['\r', '\n'], " ")),
        }
    }

    pub fn parse(line: &str) -> Self {
        let line = line.trim_end();
        let mut words = line.split_whitespace();
        if words.next() == Some("ok") {
            let (mut unit, mut cn_mdb) = (None, None);
            for w in words {
                if let Some(v) = w.strip_prefix("unit=") {
                    unit = Some(v.to_owned());
                } else if let Some(v) = w.strip_prefix("cn=") {
                    cn_mdb = v.parse().ok();
                }
            }
            return Self::Ok { unit, cn_mdb };
        }
        match line.strip_prefix("busy ") {
            Some(why) => Self::Busy(why.to_owned()),
            None => Self::Error(line.trim_prefix("error ").to_owned()),
        }
    }
}

/// A decimal or `0x` hex number.
fn parse_u64_auto(s: &str) -> Result<u64, String> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => s.parse(),
    }
    .map_err(|e| format!("{s:?}: {e}"))
}

fn parse_u32(s: &str) -> Result<u32, String> {
    u32::try_from(parse_u64_auto(s)?).map_err(|_| format!("{s:?}: out of range"))
}

fn parse_u16(s: &str) -> Result<u16, String> {
    u16::try_from(parse_u64_auto(s)?).map_err(|_| format!("{s:?}: out of range"))
}

/// Hex with or without `0x`.
fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u16::from_str_radix(hex, 16).map_err(|e| format!("{s:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tune(unit: &str, tuning: Tuning) -> Request {
        Request {
            unit: unit.into(),
            tuning,
            sid: None,
            mode: None,
            client: None,
            if_free: false,
        }
    }

    #[test]
    fn requests_round_trip() {
        for r in [
            Request {
                sid: Some(1024),
                ..tune("AB12345678", Tuning::Terrestrial { hz: 557_142_857 })
            },
            Request {
                if_free: true,
                ..tune(
                    "CD23456789",
                    Tuning::Satellite {
                        hz: 1_318_000_000,
                        tsid: 0x40F1,
                    },
                )
            },
            Request {
                sid: Some(101),
                ..tune(
                    "A",
                    Tuning::Satellite {
                        hz: 1_318_000_000,
                        tsid: NO_TSID,
                    },
                )
            },
            Request {
                mode: Some(Mode::RawTs),
                client: Some("4242-1".into()),
                if_free: true,
                ..tune("", Tuning::Terrestrial { hz: 557_142_857 })
            },
            Request {
                mode: Some(Mode::System1),
                ..tune("A", Tuning::Terrestrial { hz: 1 })
            },
        ] {
            assert_eq!(Request::parse(&r.to_line()), Ok(r.clone()));
            assert_eq!(Asked::parse(&r.to_line()), Ok(Asked::Tune(r)));
        }
        assert_eq!(
            Request::parse("tune unit=A sat=1318000000").unwrap().tuning,
            Tuning::Satellite {
                hz: 1_318_000_000,
                tsid: NO_TSID
            },
            "the first TS of the frequency"
        );
        assert_eq!(
            Request::parse("tune unit=A sat=1318000000 tsid=0x40F1 sid=0x65").unwrap(),
            Request {
                sid: Some(101),
                ..tune(
                    "A",
                    Tuning::Satellite {
                        hz: 1_318_000_000,
                        tsid: 0x40F1
                    }
                )
            }
        );
        assert_eq!(
            Request::parse("tune ter=1").unwrap().unit,
            "",
            "without a unit: the only one"
        );
    }

    #[test]
    fn status_round_trip() {
        for unit in ["AB12345678", ""] {
            let s = StatusRequest { unit: unit.into() };
            assert_eq!(Asked::parse(&s.to_line()), Ok(Asked::Status(s)));
        }
        assert_eq!(StatusRequest::parse("status").unwrap().unit, "");
        assert!(Asked::parse("status ter=1").is_err());
        assert!(Asked::parse("status unit=").is_err());
    }

    #[test]
    fn bad_requests() {
        for line in [
            "",
            "play unit=A ter=1",
            "tune unit= ter=1",
            "tune unit=A",
            "tune unit=A ter=1 tsid=1",
            "tune unit=A ter=1 sat=2 tsid=1",
            "tune unit=A ter=x",
            "tune unit=A ter=1 lnb=on",
            "tune unit=A ter=1 mode=raw",
            "tune unit=A ter=1 client=",
            "tune unit=A ter",
        ] {
            assert!(Asked::parse(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn replies() {
        for r in [
            Reply::Ok {
                unit: None,
                cn_mdb: None,
            },
            Reply::Ok {
                unit: Some("AB12345678".into()),
                cn_mdb: Some(33_500),
            },
            Reply::Ok {
                unit: Some("AB12345678".into()),
                cn_mdb: None,
            },
            Reply::Ok {
                unit: None,
                cn_mdb: Some(-120),
            },
            Reply::Busy("unit A is streaming".into()),
            Reply::Error("unit A is not connected".into()),
        ] {
            assert_eq!(Reply::parse(&r.to_line()), r);
        }
        assert_eq!(
            Reply::parse("error two\nlines").to_line(),
            "error two lines"
        );
        assert_eq!(
            Reply::parse("ok\n"),
            Reply::Ok {
                unit: None,
                cn_mdb: None
            },
            "a daemon that gives neither"
        );
        assert!(matches!(Reply::parse("garbage"), Reply::Error(_)));
    }
}
