//! The BonDriver object, behind the C++ shim's `IBonDriver2` methods.
//!
//! The unit belongs to `dt295 serve` ([`client`]): `OpenTuner` makes sure the daemon of
//! the DLL's folder answers (starting it if none does), `SetChannel` asks it for the
//! channel's stream, and the TS it sends goes into the FIFO `GetTsStream` reads. In raw-TS
//! mode `OpenTuner` also primes the unit ([`Driver::open_tuner`]).
//!
//! Threads: TVTest calls `SetChannel` and `GetTsStream` on its streaming thread and
//! `OpenTuner` / `CloseTuner` / `Release` on another; the client's reader and supervisor
//! run their own. What the host polls (`GetSignalLevel`, `GetCurSpace`,
//! `IsTunerOpening`, the FIFO) is readable without waiting for a `SetChannel`.
//!
//! [`client`]: crate::client

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dt295::chan::{self, Channel, Space};
use dt295::serve::{Mode, Tuning};
use dt295::{System, TsSink, profile};
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};

use crate::client::{Client, SERVER_EXE, Settings, Want};
use crate::config::{Config, ModulePaths};
use crate::fifo::{TS_CHUNK, TS_FIFO_SIZE, TsFifo};
use crate::logging::{self, LogSession};
use crate::sys;

/// `GetCurSpace` / `GetCurChannel` before a tune.
pub const NO_SELECTION: u32 = 0xFFFF_FFFF;
/// `WaitTsStream` results (the Win32 wait codes).
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 0x102;
pub const WAIT_ABANDONED: u32 = 0x80;
/// A `WaitTsStream` timeout meaning "forever" (as is `0`).
pub const INFINITE: u32 = 0xFFFF_FFFF;

/// `GetTunerName`.
pub static TUNER_NAME: [u16; 10] = ascii_wide("PIX-DT295");

const fn ascii_wide<const N: usize>(s: &str) -> [u16; N] {
    let b = s.as_bytes();
    let mut out = [0u16; N];
    let mut i = 0;
    while i < b.len() {
        out[i] = b[i] as u16;
        i += 1;
    }
    assert!(b.len() < N, "room for the NUL");
    out
}

fn wide_z(s: &str) -> Box<[u16]> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic caught at the FFI boundary may have poisoned a lock; the driver keeps
    // working with the state as it was (each update is self-contained).
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// NUL-terminated UTF-16 names of a space and its channels: built once, so the
/// pointers `EnumTuningSpace` / `EnumChannelName` return stay valid (and stable)
/// for the driver's lifetime.
#[derive(Debug)]
struct SpaceNames {
    name: Box<[u16]>,
    channels: Vec<Box<[u16]>>,
}

/// What the host polls without waiting (`GetCurSpace`, `IsTunerOpening`, the FIFO).
#[derive(Debug)]
struct Shared {
    fifo: TsFifo,
    cur_space: AtomicU32,
    cur_channel: AtomicU32,
    opening: AtomicBool,
}

impl Shared {
    /// TS lost because the host did not read the FIFO in time (logged per stream;
    /// separates host-side drops from the TS's own continuity errors).
    fn log_fifo_drops(&self) {
        let dropped = self.fifo.take_dropped();
        if dropped > 0 {
            warn!("TS FIFO overflow: {dropped} bytes dropped (the host did not read in time)");
        }
    }
}

/// One BonDriver instance.
#[derive(Debug)]
pub struct Driver {
    settings: Settings,
    spaces: Vec<Space>,
    /// TVTest's channel-scan file, for the raw-TS prime.
    ch2_file: Option<PathBuf>,
    sh: Arc<Shared>,
    names: Vec<SpaceNames>,
    /// What `SetChannel(BYTE)` tunes for UHF 13-62ch (index `n - 13`): the first channel
    /// at its frequency, of whichever space ([`uhf_channels`]).
    uhf: Vec<Option<(u32, u32)>>,
    /// The buffer behind `GetTsStream(BYTE **)`.
    out: Mutex<Box<[u8]>>,
    /// The connection to `dt295 serve` while the tuner is open.
    client: Mutex<Option<Arc<Client>>>,
    log: Mutex<Option<LogSession>>,
}

// The FFI hands one `&Driver` to every host thread.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Driver>();
};

impl Driver {
    /// The driver as `CreateBonDriver` makes it: the ini next to the DLL, its log file.
    pub fn new() -> Self {
        logging::init();
        let paths = sys::module_path()
            .map(|p| ModulePaths::from_module_path(&p))
            .unwrap_or_default();
        let ini_path = paths.sibling(".ini");
        let text = std::fs::read(&ini_path)
            .map(|b| profile::decode(&b))
            .unwrap_or_default(); // no ini: all defaults
        let (ini, ini_error) = match profile::parse(&text) {
            Ok(ini) => (ini, None),
            Err(e) => (profile::Ini::new(), Some(e)),
        };
        let cfg = Config::from_ini(&ini, &paths);
        logging::set_level(*cfg.log_level.as_ref().unwrap_or(&LevelFilter::INFO));
        let log = cfg.log_file.as_deref().and_then(logging::open_log_file);
        info!(
            "version {} ({})",
            env!("DT295_VERSION"),
            std::env::consts::ARCH
        );
        if let Err(v) = &cfg.log_level {
            warn!("ini: LogLevel={v} is none of error, warn, info, debug, trace; using info");
        }
        if let Some(e) = ini_error {
            warn!(
                "ini: cannot parse {} ({e}); using the defaults",
                ini_path.display()
            );
        }
        for key in &cfg.retired {
            warn!(
                "ini: [Tuner] {key} is no longer read: the unit belongs to {SERVER_EXE} serve, \
                 whose firmware and state files are those of the DLL's folder"
            );
        }
        match &cfg.channel_file_log {
            Ok(lines) => lines.iter().for_each(|l| info!("ini: {l}")),
            Err(e) => warn!("ini: {e}; no tuning spaces"),
        }
        Self::with_config(cfg, log)
    }

    /// A driver for `cfg`.
    pub fn with_config(cfg: Config, log: Option<LogSession>) -> Self {
        let names = cfg
            .spaces
            .iter()
            .map(|s| SpaceNames {
                name: wide_z(&s.name),
                channels: s.channels.iter().map(|c| wide_z(&c.name)).collect(),
            })
            .collect();
        let uhf = uhf_channels(&cfg.spaces);
        let sh = Arc::new(Shared {
            fifo: TsFifo::new(TS_FIFO_SIZE),
            cur_space: AtomicU32::new(NO_SELECTION),
            cur_channel: AtomicU32::new(NO_SELECTION),
            opening: AtomicBool::new(false),
        });
        Self {
            settings: cfg.settings(),
            spaces: cfg.spaces,
            ch2_file: cfg.ch2_file,
            sh,
            names,
            uhf,
            out: Mutex::new(vec![0; TS_CHUNK].into_boxed_slice()),
            client: Mutex::new(None),
            log: Mutex::new(log),
        }
    }

    fn client(&self) -> Option<Arc<Client>> {
        lock(&self.client).clone()
    }

    // ---- tuner ----

    /// `OpenTuner`: make sure `dt295 serve` answers at the DLL's folder, starting it if
    /// none does. Without a unit the daemon still answers: the first `SetChannel` fails,
    /// and the channel is asked for again once the unit is there. In raw-TS mode the unit
    /// is primed ([`Driver::prime_raw`]).
    pub fn open_tuner(&self) -> bool {
        let mut client = lock(&self.client);
        if client.is_some() {
            return true;
        }
        let sink = {
            let sh = Arc::clone(&self.sh);
            Box::new(move || -> TsSink {
                let sh = Arc::clone(&sh);
                Box::new(move |ts: &[u8]| sh.fifo.push(ts))
            })
        };
        match Client::open(self.settings.clone(), sink) {
            Ok(c) => {
                if self.settings.mode == Mode::RawTs {
                    self.prime_raw(&c);
                }
                *client = Some(Arc::new(c));
                self.sh.opening.store(true, Ordering::Release);
                true
            }
            Err(e) => {
                warn!("OpenTuner: {e}");
                false
            }
        }
    }

    /// Raw-TS: stream a known on-air channel once during `OpenTuner`, so that TVTest's
    /// first `SetChannel` is fast. The one-time cold bring-up of a freshly plugged unit
    /// (board, demod INITIALIZE, a tune: about 2.5 s) runs here instead of in the first
    /// `SetChannel`, during which TVTest's viewer would starve on no TS and never start --
    /// the first channel would stay black until a channel change. The channel is the first
    /// enabled terrestrial service of TVTest's scan (`.ch2`); without one the unit stays as
    /// it is. The chip stays brought up (the daemon tells that from the chip), so the first
    /// `SetChannel` only retunes and rearms. The bring-up time is unchanged: it moves from
    /// the first `SetChannel` to the open.
    fn prime_raw(&self, client: &Client) {
        let Some((space, channel)) = self.first_scanned_terrestrial() else {
            return;
        };
        let ch = &self.spaces[space as usize].channels[channel as usize];
        let t0 = Instant::now();
        let primed = client.set_channel(want(ch), || {});
        client.stop();
        self.purge_ts_stream();
        match primed {
            Ok(_) => info!(
                "raw-TS: primed the unit on {} at the open ({} ms); the first SetChannel will \
                 be fast",
                ch.name,
                t0.elapsed().as_millis()
            ),
            Err(e) => warn!("raw-TS: priming on {} did not stream ({e})", ch.name),
        }
    }

    /// The `(space, channel)` of the first enabled terrestrial service in TVTest's `.ch2`
    /// scan file: a channel TVTest already found on air, so priming on it locks. `None` if
    /// the file is absent or unreadable, or names no terrestrial channel to tune.
    fn first_scanned_terrestrial(&self) -> Option<(u32, u32)> {
        let services = dt295::ch2::read(self.ch2_file.as_ref()?).ok()?;
        first_terrestrial_ch2_channel(&services, |space, channel| {
            self.spaces
                .get(space as usize)
                .and_then(|sp| sp.channels.get(channel as usize))
                .is_some_and(|c| c.sys == System::Terrestrial && c.untunable.is_none())
        })
    }

    /// `CloseTuner`: end the stream (the daemon ends with its last client); the
    /// selection is forgotten.
    pub fn close_tuner(&self) {
        let client = lock(&self.client).take();
        if let Some(c) = client {
            c.close();
            self.sh.log_fifo_drops();
        }
        self.sh.opening.store(false, Ordering::Release);
        self.sh.cur_space.store(NO_SELECTION, Ordering::Release);
        self.sh.cur_channel.store(NO_SELECTION, Ordering::Release);
        self.purge_ts_stream();
    }

    /// `IsTunerOpening`.
    pub fn is_tuner_opening(&self) -> bool {
        self.sh.opening.load(Ordering::Acquire)
    }

    // ---- channels ----

    /// `GetTunerName`.
    pub fn tuner_name(&self) -> &'static [u16] {
        &TUNER_NAME
    }

    /// `EnumTuningSpace`: the NUL-terminated name, or `None` past the last space.
    pub fn enum_tuning_space(&self, space: u32) -> Option<&[u16]> {
        self.names.get(space as usize).map(|s| &*s.name)
    }

    /// `EnumChannelName`: the NUL-terminated name, or `None` past the last channel.
    pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<&[u16]> {
        self.names
            .get(space as usize)?
            .channels
            .get(channel as usize)
            .map(|c| &**c)
    }

    /// `GetCurSpace` ([`NO_SELECTION`] until a tune succeeded).
    pub fn cur_space(&self) -> u32 {
        self.sh.cur_space.load(Ordering::Acquire)
    }

    /// `GetCurChannel` ([`NO_SELECTION`] until a tune succeeded).
    pub fn cur_channel(&self) -> u32 {
        self.sh.cur_channel.load(Ordering::Acquire)
    }

    /// `SetChannel(BYTE)` (IBonDriver v1): the UHF physical channel 13-62.
    pub fn set_channel_byte(&self, ch: u8) -> bool {
        let at = usize::from(ch).checked_sub(13);
        match at.and_then(|i| self.uhf.get(i)).copied().flatten() {
            Some((space, channel)) => self.set_channel(space, channel),
            None => false,
        }
    }

    /// `SetChannel(space, channel)`: ask `dt295 serve` for the channel's stream (every
    /// service of the TS, or the channel's ServiceID). True only when a stream runs.
    pub fn set_channel(&self, space: u32, channel: u32) -> bool {
        let Some(ch) = self
            .spaces
            .get(space as usize)
            .and_then(|s| s.channels.get(channel as usize))
        else {
            return false;
        };
        if let Some(why) = &ch.untunable {
            info!(
                "SetChannel({space}, {channel}): {} is no channel to tune ({why})",
                ch.name
            );
            return false;
        }
        let Some(client) = self.client() else {
            return false; // not open
        };
        self.sh.log_fifo_drops();
        let t0 = Instant::now();
        // emptied once the old stream's reader is gone, so none of its TS comes after
        let result = client.set_channel(want(ch), || self.purge_ts_stream());
        let ms = t0.elapsed().as_millis();
        match result {
            Ok(cn_mdb) => {
                info!(
                    "SetChannel({space}, {channel}) {} {:.3} MHz: streaming{} ({ms} ms)",
                    ch.name,
                    f64::from(ch.freq_hz) / 1e6,
                    cn_mdb.map_or_default(|cn| format!(", C/N {:.1} dB", f64::from(cn) / 1000.0))
                );
                self.sh.cur_space.store(space, Ordering::Release);
                self.sh.cur_channel.store(channel, Ordering::Release);
                true
            }
            Err(why) => {
                warn!(
                    "SetChannel({space}, {channel}) {} {:.3} MHz: {why} ({ms} ms)",
                    ch.name,
                    f64::from(ch.freq_hz) / 1e6
                );
                false
            }
        }
    }

    /// `GetSignalLevel`: C/N in dB, as `dt295 serve` last told it (never waits); 0
    /// without a stream.
    pub fn signal_level(&self) -> f32 {
        self.client().map_or(0.0, |c| c.signal_level())
    }

    // ---- TS FIFO ----

    /// `WaitTsStream`: wait for TS data, `timeout_ms` (0 or INFINITE = forever).
    pub fn wait_ts_stream(&self, timeout_ms: u32) -> u32 {
        let timeout = match timeout_ms {
            0 | INFINITE => None,
            ms => Some(Duration::from_millis(u64::from(ms))),
        };
        if self.sh.fifo.wait(timeout) {
            WAIT_OBJECT_0
        } else {
            WAIT_TIMEOUT
        }
    }

    /// `GetReadyCount`: buffered chunks.
    pub fn ready_count(&self) -> u32 {
        self.sh.fifo.ready_count()
    }

    /// `GetTsStream(BYTE *)`: up to one chunk, handed to `copy` in (at most) two
    /// parts. Returns the byte count.
    pub fn pop_ts_with(&self, copy: impl FnOnce(&[u8], &[u8])) -> usize {
        self.sh.fifo.pop_with(TS_CHUNK, copy)
    }

    /// `GetTsStream(BYTE **)`: up to one chunk in the driver's own buffer, which
    /// stays valid (and unchanged) until the next call or the driver's release.
    /// Null and 0 when there is no data.
    pub fn ts_stream_ptr(&self) -> (*mut u8, usize) {
        let mut out = lock(&self.out);
        let n = self.sh.fifo.pop(&mut out);
        let p = if n > 0 {
            out.as_mut_ptr()
        } else {
            std::ptr::null_mut()
        };
        (p, n)
    }

    /// `PurgeTsStream`.
    pub fn purge_ts_stream(&self) {
        self.sh.fifo.purge();
    }
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Driver {
    /// `Release`: everything is stopped and joined here (never in DllMain).
    fn drop(&mut self) {
        self.close_tuner();
        drop(
            self.log
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }
}

/// What to ask `dt295 serve` for a channel: its frequency (and TS), and its ServiceID.
fn want(ch: &Channel) -> Want {
    Want {
        name: ch.name.clone(),
        tuning: match ch.sys {
            System::Terrestrial => Tuning::Terrestrial { hz: ch.freq_hz },
            System::Satellite => Tuning::Satellite {
                hz: ch.freq_hz,
                tsid: ch.tsid,
            },
        },
        sid: ch.sid,
    }
}

/// The `(space, channel)` of the first enabled service in a parsed `.ch2` for which
/// `is_terrestrial` holds (reading and parsing are shared in `dt295::ch2`).
fn first_terrestrial_ch2_channel(
    services: &[dt295::ch2::Service],
    is_terrestrial: impl Fn(u32, u32) -> bool,
) -> Option<(u32, u32)> {
    services
        .iter()
        .find(|s| s.enabled && is_terrestrial(s.space, s.channel))
        .map(|s| (s.space, s.channel))
}

/// For UHF 13-62ch (index `n - 13`), the `(space, channel)` of the first channel to tune
/// within 0.5 MHz of its centre, in space order: the built-in UHF space when it is
/// listed, or the UHF channel of a channel file.
fn uhf_channels(spaces: &[Space]) -> Vec<Option<(u32, u32)>> {
    chan::UHF
        .map(|n| {
            let hz = chan::uhf_hz(n)?;
            spaces.iter().enumerate().find_map(|(si, s)| {
                let ci = s.channels.iter().position(|c| {
                    c.untunable.is_none()
                        && c.sys == System::Terrestrial
                        && c.freq_hz.abs_diff(hz) < 500_000
                })?;
                Some((si as u32, ci as u32))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn driver(ini: &str) -> Driver {
        let paths =
            ModulePaths::from_module_path(OsStr::new(r"C:\nonexistent-dt295\BonDriver_DT295.dll"));
        let mut cfg = Config::from_ini(&profile::parse(ini).unwrap(), &paths);
        cfg.ch2_file = None;
        Driver::with_config(cfg, None)
    }

    fn utf16z(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn tuner_name() {
        let d = driver("");
        assert_eq!(d.tuner_name(), &utf16z("PIX-DT295")[..]);
    }

    #[test]
    fn ch2_first_enabled_terrestrial_channel() {
        // name may contain commas; fields are space,channel,_,type,sid,nid,tsid,enabled.
        let services = dt295::ch2::parse(
            "; TVTest Channel File\r\n\
             ＮＨＫ総合,0,14,1,1,1024,32736,32736,1\r\n\
             Name, with comma,2,40,1,1,101,4,16625,0\r\n\
             big,3,0,1,1,70000,7,1,1\r\n",
        );
        // space 0 is terrestrial here, 2 and 3 are not.
        let terr = |space, _ch| space == 0;
        assert_eq!(
            first_terrestrial_ch2_channel(&services, terr),
            Some((0, 14))
        );
        // nothing terrestrial -> none
        assert_eq!(first_terrestrial_ch2_channel(&services, |_, _| false), None);
        // a disabled terrestrial line (...,0) is skipped; the next enabled one wins
        let text2 = dt295::ch2::parse("A,0,14,1,1,1,1,1,0\r\nB,0,15,1,1,1,1,1,1\r\n");
        assert_eq!(
            first_terrestrial_ch2_channel(&text2, |_, _| true),
            Some((0, 15))
        );
    }

    #[test]
    fn enumerates_spaces_and_channels() {
        let d = driver("[BS]\nBS01/TS0=1049480,0x4010,151\n");
        let names: Vec<_> = (0..)
            .map_while(|s| d.enum_tuning_space(s))
            .map(|w| String::from_utf16_lossy(&w[..w.len() - 1]))
            .collect();
        assert_eq!(names, ["UHF", "CATV", "BS"]);
        assert_eq!(d.enum_channel_name(0, 0), Some(&utf16z("13ch")[..]));
        assert_eq!(d.enum_channel_name(1, 50), Some(&utf16z("C63ch")[..]));
        assert_eq!(d.enum_channel_name(1, 51), None);
        assert_eq!(d.enum_channel_name(2, 0), Some(&utf16z("BS01/TS0")[..]));
        assert_eq!(d.enum_channel_name(3, 0), None);
        assert_eq!(d.enum_tuning_space(NO_SELECTION), None);
        // Stable pointers: the same storage every call.
        let a = d.enum_channel_name(2, 0).unwrap().as_ptr();
        let b = d.enum_channel_name(2, 0).unwrap().as_ptr();
        assert_eq!(a, b);
    }

    #[test]
    fn what_a_channel_asks_for() {
        let d = driver("[BS]\nBS01/TS0=1049480,0x4010,151\nBS03/TS1=1087840,0x4631\n");
        let w = |space: usize, ch: usize| want(&d.spaces[space].channels[ch]);
        assert_eq!(
            w(0, 14),
            Want {
                name: "27ch".into(),
                tuning: Tuning::Terrestrial { hz: 557_142_857 },
                sid: None
            }
        );
        assert_eq!(
            w(2, 0),
            Want {
                name: "BS01/TS0".into(),
                tuning: Tuning::Satellite {
                    hz: 1_049_480_000,
                    tsid: 0x4010
                },
                sid: Some(151)
            }
        );
        assert_eq!(w(2, 1).sid, None, "every service of the TS");
        assert_eq!(
            w(2, 1).tuning,
            Tuning::Satellite {
                hz: 1_087_840_000,
                tsid: 0x4631
            }
        );
    }

    #[test]
    fn closed_tuner_state() {
        let d = driver("");
        assert!(!d.is_tuner_opening());
        assert_eq!(
            (d.cur_space(), d.cur_channel()),
            (NO_SELECTION, NO_SELECTION)
        );
        assert_eq!(d.signal_level(), 0.0);
        // Not open: no tune (and no daemon asked).
        assert!(!d.set_channel(0, 14));
        assert!(!d.set_channel(9, 0));
        assert!(!d.set_channel_byte(27));
        d.close_tuner();
        assert_eq!(d.cur_space(), NO_SELECTION);
    }

    #[test]
    fn set_channel_byte_needs_uhf() {
        let d = driver("[Tuner]\nUHF=0\n");
        assert!(d.uhf.iter().all(Option::is_none), "CATV is no UHF");
        assert!(!d.set_channel_byte(27));
        let d = driver("");
        assert_eq!((d.uhf[0], d.uhf[14]), (Some((0, 0)), Some((0, 14))));
        assert!(!d.set_channel_byte(12));
        assert!(!d.set_channel_byte(63));
        // by frequency, in whichever space (a channel file's: BDA's 473.143 MHz)
        let mut c = chan::uhf_space();
        c.kind = chan::SpaceKind::File;
        c.channels.truncate(3);
        c.channels[1].freq_hz = 479_143_000;
        c.channels[2].untunable = Some("no definition".into());
        let other = Space {
            channels: Vec::new(),
            ..c.clone()
        };
        let u = uhf_channels(&[other, c]);
        assert_eq!(&u[..3], [Some((1, 0)), Some((1, 1)), None]);
    }

    #[test]
    fn ts_reads() {
        let d = driver("");
        // No data: TRUE with size 0 and a null pointer at the FFI.
        assert_eq!(d.ts_stream_ptr(), (std::ptr::null_mut(), 0));
        assert_eq!(d.ready_count(), 0);
        assert_eq!(d.wait_ts_stream(10), WAIT_TIMEOUT);

        let ts: Vec<u8> = (0..TS_CHUNK + 188).map(|i| i as u8).collect();
        d.sh.fifo.push(&ts);
        assert_eq!(d.ready_count(), 2);
        assert_eq!(d.wait_ts_stream(0), WAIT_OBJECT_0);
        let (p, n) = d.ts_stream_ptr();
        assert_eq!(n, TS_CHUNK);
        // SAFETY: `p` points into the driver's TS_CHUNK-byte output buffer, which
        // nothing else touches until the next ts_stream_ptr call.
        let chunk = unsafe { std::slice::from_raw_parts(p, n) };
        assert_eq!(chunk, &ts[..TS_CHUNK]);
        assert_eq!(d.ready_count(), 1);

        let mut rest = Vec::new();
        let n = d.pop_ts_with(|a, b| {
            rest.extend_from_slice(a);
            rest.extend_from_slice(b);
        });
        assert_eq!(n, 188);
        assert_eq!(rest, &ts[TS_CHUNK..]);

        d.sh.fifo.push(&ts[..10]);
        d.purge_ts_stream();
        assert_eq!(d.ready_count(), 0);
        assert_eq!(d.wait_ts_stream(1), WAIT_TIMEOUT);
    }
}
