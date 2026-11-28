//! The device handle and its public API.
//!
//! Concurrency: [`Device`] owns an `Arc<Shared>`: the USB handle, the device state in
//! one mutex ([`State`]: protocol counters, session and link keys, the tuned service,
//! the PAT counters, the PSI keeper, ...), and lock-free live values ([`Live`]). While a
//! stream runs, its worker thread (which holds its own `Arc`) does all device I/O; the
//! host only stops it and reads. A worker that does not stop is left running with its
//! reference, so nothing is freed under it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use dt295_core::Model;
use dt295_core::bonini::DeviceSel;
use dt295_core::fe1::{self, FrontEnd, Port};
use dt295_core::proto::{FW_STALL_LIMIT, Io, Proto, REG_FW_STATE};

use crate::raw::{RawTune, RawWorker};
use crate::s1::{KA_FIRST, KA_INTERVAL, Link, S1};
use crate::ts::psi::PsiKeeper;
use crate::{Error, PsiRecord, Result, Status, System, TsSink, fw};

/// What the host and the stream worker share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub usb: dt295_usb::Device,
    st: Mutex<State>,
    /// The first tuner's live values (its System1 stream's).
    pub live: Arc<LiveInner>,
    /// The host's callback for each link key the device hands out.
    journal: Mutex<Journal>,
    /// A [`crate::Tuner2`] exists.
    pub tuner2: AtomicBool,
    /// The second tuner streams (System2).
    pub s2_streaming: AtomicBool,
}

/// Called with each link key the device hands out ([`Device::set_link_key_journal`]).
pub type KeyJournal = Box<dyn FnMut([u8; 16]) + Send>;

/// The journal slot of [`Shared`] (a closure prints as whether it is set).
#[derive(Default)]
struct Journal(Option<KeyJournal>);

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "Journal(set)"
        } else {
            "Journal(none)"
        })
    }
}

/// The device state behind the mutex.
#[derive(Debug)]
pub(crate) struct State {
    pub proto: Proto,
    pub link: Link,
    pub psi: PsiKeeper,
    /// The B-CAS card session (stage 2) is open: once per boot, by the first stream of
    /// either tuner.
    pub card_open: bool,
    /// Counts the boots ([`Device::start`]); 0 = none yet.
    pub boot_gen: u64,
}

impl Shared {
    fn new(usb: dt295_usb::Device) -> Self {
        let st = State {
            proto: Proto::default(),
            link: Link::default(),
            psi: PsiKeeper::new(),
            card_open: false,
            boot_gen: 0,
        };
        Shared {
            usb,
            st: Mutex::new(st),
            live: Arc::new(LiveInner::new()),
            journal: Mutex::new(Journal::default()),
            tuner2: AtomicBool::new(false),
            s2_streaming: AtomicBool::new(false),
        }
    }

    /// The state (a panic elsewhere does not lock the device out: the state stays usable).
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The state if nobody holds it now.
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, State>> {
        crate::s2::try_lock_state(&self.st)
    }

    /// Hand a link key the device just gave out to the host's journal, if it set one.
    pub(crate) fn note_derived_key(&self, key: [u8; 16]) {
        if let Some(journal) = self
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .as_mut()
        {
            journal(key);
        }
    }

    /// Device I/O from the host thread (no worker may run).
    pub(crate) fn with_io<R>(&self, f: impl FnOnce(&mut Io<'_>) -> R) -> R {
        debug_assert!(
            !self.live.streaming(),
            "host device I/O while a stream worker runs"
        );
        let mut st = self.lock();
        f(&mut Io::new(&self.usb, &mut st.proto))
    }
}

/// The live values the stream worker publishes (atomics; no lock).
#[derive(Debug)]
pub(crate) struct LiveInner {
    epoch: Instant,
    /// A stream worker runs (and should keep running).
    streaming: AtomicBool,
    /// The host gave up on a worker that did not stop: it must not store state any more.
    abandoned: AtomicBool,
    /// Bytes the worker has handed to the sink since it started.
    delivered: AtomicU64,
    cn_mdb: AtomicI32,
    /// ms since `epoch` + 1 of the C/N sample; 0 = none this stream.
    cn_at: AtomicU64,
    /// B-CAS card events (a card pulled or inserted) a stream worker saw.
    card_events: AtomicU32,
    /// The B-CAS state of the last one ([`u8::MAX`]: none / unreadable).
    card_state: AtomicU8,
    /// Whether the stream's ES come out descrambled: [`ES_UNKNOWN`], [`ES_CLEAR`] or
    /// [`ES_SCRAMBLED`].
    es: AtomicU8,
    /// The worker's reads fail with "no device": the unit was unplugged.
    lost: AtomicBool,
}

const ES_UNKNOWN: u8 = 0;
const ES_CLEAR: u8 = 1;
const ES_SCRAMBLED: u8 = 2;

impl LiveInner {
    pub(crate) fn new() -> Self {
        LiveInner {
            epoch: Instant::now(),
            streaming: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
            cn_mdb: AtomicI32::new(0),
            cn_at: AtomicU64::new(0),
            card_events: AtomicU32::new(0),
            card_state: AtomicU8::new(u8::MAX),
            es: AtomicU8::new(ES_UNKNOWN),
            lost: AtomicBool::new(false),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    pub(crate) fn streaming(&self) -> bool {
        self.streaming.load(Ordering::Acquire)
    }
    pub(crate) fn set_streaming(&self, on: bool) {
        self.streaming.store(on, Ordering::Release);
    }
    pub(crate) fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }
    pub(crate) fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
    }
    pub(crate) fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Acquire)
    }
    pub(crate) fn set_delivered(&self, n: u64) {
        self.delivered.store(n, Ordering::Release);
    }
    /// A new worker: nothing delivered, no live C/N until it samples one, the ES not
    /// judged yet.
    pub(crate) fn reset_stream(&self) {
        self.delivered.store(0, Ordering::Release);
        self.cn_at.store(0, Ordering::Release);
        self.es.store(ES_UNKNOWN, Ordering::Release);
    }
    /// Drop any live C/N sample: a stream stopped, so the next `GetSignalLevel` falls back to
    /// the tune-time level rather than report the ended stream's last reading for the 3 s
    /// freshness window that `cn` otherwise keeps live.
    pub(crate) fn reset_cn(&self) {
        self.cn_at.store(0, Ordering::Release);
    }
    pub(crate) fn note_card_event(&self, state: u8) {
        self.set_card_state(state);
        self.card_events.fetch_add(1, Ordering::AcqRel);
    }
    /// The B-CAS state last read: an event, stage 2, a stream's first seconds
    /// ([`u8::MAX`]: none).
    pub(crate) fn card(&self) -> u8 {
        self.card_state.load(Ordering::Relaxed)
    }

    pub(crate) fn set_card_state(&self, state: u8) {
        self.card_state.store(state, Ordering::Relaxed);
    }
    pub(crate) fn set_es_clear(&self, clear: Option<bool>) {
        let v = match clear {
            None => ES_UNKNOWN,
            Some(true) => ES_CLEAR,
            Some(false) => ES_SCRAMBLED,
        };
        self.es.store(v, Ordering::Release);
    }
    fn es_clear(&self) -> Option<bool> {
        match self.es.load(Ordering::Acquire) {
            ES_CLEAR => Some(true),
            ES_SCRAMBLED => Some(false),
            _ => None,
        }
    }
    pub(crate) fn set_lost(&self) {
        self.lost.store(true, Ordering::Release);
    }
    pub(crate) fn publish_cn(&self, mdb: i32) {
        self.cn_mdb.store(mdb, Ordering::Relaxed);
        self.cn_at.store(self.now_ms(), Ordering::Release);
    }
    fn cn(&self) -> Option<i32> {
        // Freshness alone, not `streaming()`: the raw-TS worker samples the C/N over the aux
        // through `with_io`, which keeps `streaming()` false, so a recent sample is the
        // signal that a worker is publishing. A sample older than 3 s (the stream stopped)
        // reads as no live value, and the host falls back to the tune-time level.
        let at = self.cn_at.load(Ordering::Acquire);
        if at == 0 || self.now_ms().saturating_sub(at) > 3000 {
            return None;
        }
        Some(self.cn_mdb.load(Ordering::Relaxed))
    }
}

/// An opened PIX-DT295.
///
/// Not `Sync`: callers serialize access (the BonDriver holds it in a `Mutex`). While a
/// stream runs, its worker thread owns the device I/O; the host must not call
/// anything but [`Device::stream_stop`] and the read-only accessors meanwhile.
pub struct Device {
    sh: Arc<Shared>,
    /// The firmware reached idle / ready.
    fw_booted: bool,
    s1: S1,
    /// The blob-free raw-TS stream worker, while one runs (it takes the device I/O one
    /// operation at a time, beside the second tuner's; the host only stops it).
    raw: Option<RawWorker>,
    /// The front end the tunes and streams use ([`Device::set_tuner`]).
    fe: FrontEnd,
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("fw_booted", &self.fw_booted)
            .field("streaming", &self.sh.live.streaming())
            .field("s1", &self.s1)
            .finish_non_exhaustive()
    }
}

/// A cheap, clonable view of live values the stream worker publishes, readable
/// from any thread without locking the [`Device`] (e.g. for `GetSignalLevel`).
#[derive(Debug, Clone)]
pub struct Live {
    inner: Arc<LiveInner>,
}

impl Live {
    pub(crate) fn new(inner: Arc<LiveInner>) -> Self {
        Live { inner }
    }

    /// C/N (0.001 dB) sampled by the running stream's worker within the last 3 s.
    pub fn cn_mdb(&self) -> Option<i32> {
        self.inner.cn()
    }
    /// Whether a stream is running.
    pub fn is_streaming(&self) -> bool {
        self.inner.streaming()
    }
    /// Bytes the device delivered in the current (or last) stream, including what the
    /// stream held back before the tuned service's PAT.
    pub fn delivered(&self) -> u64 {
        self.inner.delivered()
    }
    /// B-CAS card events (a card pulled or inserted) seen while streaming, counted from
    /// the open. The stream keeps running across one, but with its ES undescrambled
    /// until the stream is started again on a re-initialized device ([`CardWatch`]).
    ///
    /// [`CardWatch`]: crate::CardWatch
    pub fn card_events(&self) -> u32 {
        self.inner.card_events.load(Ordering::Acquire)
    }
    /// The firmware's B-CAS state as last read: at a card event, after B-CAS stage 2,
    /// a few seconds into a stream. 0 = no card in, 1 = a card in (just inserted, or
    /// right after stage 2), 3 = working, 5 = a card in that did not work (a bad
    /// contact); 2 and 4 were not seen.
    pub fn card_state(&self) -> Option<u8> {
        match self.inner.card_state.load(Ordering::Relaxed) {
            u8::MAX => None,
            s => Some(s),
        }
    }
    /// Whether the running stream carries its video / audio, judged every 2 s by the PES
    /// packets on the PIDs its PMT lists: `Some(false)` for two windows in a row without
    /// any (the device drops them without a B-CAS card session); `None` until known, or
    /// for a stream without video / audio (data broadcasting, PSI only).
    pub fn es_clear(&self) -> Option<bool> {
        self.inner.es_clear()
    }
    /// The stream worker lost the device (unplugged): close it and open it again.
    pub fn device_lost(&self) -> bool {
        self.inner.lost.load(Ordering::Acquire)
    }
}

/// Boots in this process, for [`State::boot_gen`] (unique, so a tune of the second tuner
/// tells a later boot from the one it was on).
static BOOTS: AtomicU64 = AtomicU64::new(0);

/// Attempts and pause of [`Device::open_sel`].
pub const OPEN_ATTEMPTS: usize = 5;
pub const OPEN_RETRY_PAUSE: Duration = Duration::from_millis(400);

impl Device {
    /// Open the `index`-th PIX-DT295 bound to WinUSB (no device I/O beyond the open).
    ///
    /// The index counts only the devices this process can open (one in use elsewhere
    /// is skipped); with several units, [`Device::open_serial`] picks a fixed one.
    pub fn open(index: usize) -> Result<Self> {
        Self::with_usb(dt295_usb::Device::open(
            dt295_usb::VID,
            &dt295_usb::PIDS,
            index,
        ))
    }

    /// Open the unit `sel` names ([`Device::open`] or [`Device::open_serial`]), trying
    /// again for a moment ([`OPEN_ATTEMPTS`] x [`OPEN_RETRY_PAUSE`]) while it is there but
    /// cannot be opened: after a close (or a crash) the WinUSB handle can linger, and a
    /// fresh open fails until the OS releases it. A unit that is not there
    /// ([`Error::NoDevice`]) fails at once.
    pub fn open_sel(sel: &DeviceSel) -> Result<Self> {
        let open = || match sel {
            DeviceSel::Index(n) => Self::open(*n),
            DeviceSel::Serial(s) => Self::open_serial(s),
        };
        let mut opened = open();
        for _ in 1..OPEN_ATTEMPTS {
            if matches!(opened, Ok(_) | Err(Error::NoDevice)) {
                break;
            }
            thread::sleep(OPEN_RETRY_PAUSE);
            opened = open();
        }
        opened
    }

    /// Open the PIX-DT295 with this USB serial number (e.g. `CD23456789`, as
    /// [`crate::list`] shows it; case-insensitive).
    pub fn open_serial(serial: &str) -> Result<Self> {
        Self::with_usb(dt295_usb::Device::open_serial(
            dt295_usb::VID,
            &dt295_usb::PIDS,
            serial,
        ))
    }

    /// Start a blob-free COLD raw-TS stream worker: host board bring-up, host-face tune,
    /// capture arm and ring->EP feed, with no resident and no vendor firmware (the unit cores
    /// and the aux core run only our code; see [`dt295_rawfw::rawstream::run_cold_stream`]).
    /// The worker passes scrambled TS into the sink until stopped; the host descrambles it with a real
    /// card. A channel change is a stop + a fresh start on the new frequency. On a two-tuner
    /// model the second tuner streams beside it ([`crate::Tuner2::stream_start_cold`]).
    pub fn stream_start_cold(
        &mut self,
        terrestrial: bool,
        freq_hz: u32,
        tsid: Option<u16>,
        full: bool,
        duration: Duration,
        sink: TsSink,
    ) -> Result<()> {
        if self.raw.is_some() {
            return Err(Error::State("a raw-TS stream is already running".into()));
        }
        let tune = RawTune {
            terrestrial,
            freq_hz,
            tsid,
            full,
            duration,
        };
        let live = Arc::clone(&self.sh.live);
        self.raw = Some(RawWorker::spawn(
            Arc::clone(&self.sh),
            live,
            self.fe,
            tune,
            sink,
        )?);
        Ok(())
    }

    /// Whether a raw-TS worker is running (its thread has not exited). False once the
    /// worker ended on its own -- a capture fault or an unplug -- so the host can reap it
    /// with [`Device::stream_stop_raw`] and learn why.
    pub fn raw_running(&self) -> bool {
        self.raw.as_ref().is_some_and(RawWorker::running)
    }

    /// Whether a raw-TS worker slot is occupied (running, or ended but not yet reaped).
    pub fn raw_present(&self) -> bool {
        self.raw.is_some()
    }

    /// Stop the raw-TS worker and return what it captured (or its fault). A worker still in
    /// the cold bring-up finishes it first; one that will not exit is left running with its
    /// own device reference (a re-plug is the reset), and a later stop gives that error at
    /// once. No-op if none is running.
    pub fn stream_stop_raw(&mut self) -> Result<crate::RawCaptureStats> {
        crate::raw::stop(&mut self.raw, &self.sh, &self.sh.live)
    }

    /// Read one register of an I2C slave (e.g. the demod at 7-bit 0x10) over the host face
    /// of the Fujitsu I2C controller (0x00FD8FE0, the 0xBC window). Blob-free: no aux core,
    /// no monitor, no vendor firmware. See [`dt295_rawfw::fjcore`].
    pub fn fj_demod_read(&self, slave: u8, reg: u8) -> Result<u8> {
        self.sh
            .with_io(|io| dt295_rawfw::fjcore::demod_read(io, slave, reg))
    }

    /// Experimental blob-free COLD read: run the host-side board bring-up (external
    /// memory, the INITIALIZE clock/strap chain, the 0xF10114 I2C-block enable), set up
    /// the host-face I2C, and read one demod register with step diagnostics. Verifies on
    /// our own hardware whether the demod I2C can be powered from cold with no resident
    /// and no aux core. `restore` puts the board registers back so a re-plug is not
    /// needed between runs. Returns the value and the report. See [`dt295_rawfw::fjcore`].
    pub fn fj_demod_read_cold(
        &self,
        slave: u8,
        reg: u8,
        restore: bool,
    ) -> (Result<u8>, Vec<String>) {
        match self.sh.with_io(|io| {
            let (res, log) = dt295_rawfw::fjcore::demod_read_cold(io, slave, reg, restore);
            Ok((res, log))
        }) {
            Ok(pair) => pair,
            Err(e) => (Err(e), Vec::new()),
        }
    }

    /// Whether our blob-free monitor BODY is already present on the chip: the cold bring-up
    /// has run and persists (across worker restarts and a reopen without a re-plug), so a
    /// later tune only needs a retune + rearm, not the once-per-plug bootstrap (a second
    /// bootstrap faults). A pure host-window read, safe on a cold device (it then reads the
    /// stock boot0 body and returns false). The second tuner's raw-TS stream may run.
    pub fn cold_monitor_installed(&self) -> bool {
        crate::raw::monitor_installed(&self.sh)
    }

    /// Experimental blob-free COLD tune: host board bring-up + host-face I2C, then run
    /// the front-end init and tune ([`dt295_core::fe1`]) over the host face and read the
    /// lock / C/N. No resident, no aux core, no vendor firmware. Returns whether it
    /// locked and the step report. See [`dt295_rawfw::fjcore::cold_tune`].
    pub fn fj_cold_tune(
        &self,
        terrestrial: bool,
        freq_hz: u32,
        tsid: Option<u16>,
        restore: bool,
    ) -> (Result<bool>, Vec<String>) {
        match self.sh.with_io(|io| {
            Ok(dt295_rawfw::fjcore::cold_tune(
                io,
                self.fe,
                terrestrial,
                freq_hz,
                tsid,
                restore,
            ))
        }) {
            Ok(pair) => pair,
            Err(e) => (Err(e), Vec::new()),
        }
    }

    /// Experimental blob-free COLD capture arm (wall C): host board bring-up + host-face
    /// tune, install our monitor, arm the capture engine, and report whether the TS ring
    /// fills -- all with no resident, no aux core, no vendor firmware. Returns whether it
    /// locked and the ring advanced, plus the step report. See
    /// [`dt295_rawfw::fjcore::cold_capture`].
    pub fn fj_cold_capture(
        &self,
        terrestrial: bool,
        freq_hz: u32,
        tsid: Option<u16>,
    ) -> (Result<bool>, Vec<String>) {
        match self.sh.with_io(|io| {
            Ok(dt295_rawfw::fjcore::cold_capture(
                io,
                self.fe,
                terrestrial,
                freq_hz,
                tsid,
            ))
        }) {
            Ok(pair) => pair,
            Err(e) => (Err(e), Vec::new()),
        }
    }

    /// Experimental blob-free COLD stream (end to end): host board bring-up, host-face
    /// tune, monitor, capture arm, then pump the ring to EP 0x81 for `secs` and count
    /// bytes + TS sync -- no resident, no aux core, no vendor firmware. Optionally writes
    /// the captured TS to `out_path`. See [`dt295_rawfw::fjcore::cold_stream`].
    pub fn fj_cold_stream(
        &self,
        terrestrial: bool,
        freq_hz: u32,
        tsid: Option<u16>,
        secs: u64,
        out_path: Option<&str>,
    ) -> (Result<(u64, f32)>, Vec<String>) {
        match self.sh.with_io(|io| {
            Ok(dt295_rawfw::fjcore::cold_stream(
                io,
                self.fe,
                terrestrial,
                freq_hz,
                tsid,
                secs,
                out_path,
            ))
        }) {
            Ok(pair) => pair,
            Err(e) => (Err(e), Vec::new()),
        }
    }

    /// Read `len` bytes from chip address `addr` through the stock 0xBC register
    /// window (24-bit addresses; no banking). A bring-up / analysis helper.
    pub fn reg_read(&self, addr: u32, len: usize) -> Result<Vec<u8>> {
        self.sh.with_io(|io| {
            let mut buf = vec![0u8; len];
            io.read_block(addr, &mut buf)?;
            Ok(buf)
        })
    }

    /// Write `data` to chip address `addr` through the stock 0xBC register window.
    /// A bring-up / analysis helper; RAM and registers reset on a re-plug.
    pub fn reg_write(&self, addr: u32, data: &[u8]) -> Result<()> {
        self.sh.with_io(|io| io.write_block(addr, data))
    }

    fn with_usb(usb: dt295_usb::Result<dt295_usb::Device>) -> Result<Self> {
        let usb = usb.map_err(|e| {
            error!("usbx_open: {e}");
            match e {
                dt295_usb::Error::Access => Error::Usb(e),
                _ => Error::NoDevice,
            }
        })?;
        let mut dev = Device {
            sh: Arc::new(Shared::new(usb)),
            fw_booted: false,
            s1: S1::new(),
            raw: None,
            fe: FrontEnd::DT295,
        };
        dev.fe = dev.front_end(0).unwrap_or(FrontEnd::DT295);
        dev.s1.fe = dev.fe;
        Ok(dev)
    }

    /// The front end of tuner `n` (0-based) of this unit, its TS out of output A.
    fn front_end(&self, n: usize) -> Option<FrontEnd> {
        match self.model().tuners() {
            1 => (n == 0).then_some(FrontEnd::DT295),
            _ => FrontEnd::dt195w(n, Port::A),
        }
    }

    /// Use tuner `n` (0-based; the two-tuner models have two) for the tunes and streams from now
    /// on. Its TS goes out of the demod's output A, the one the System1 and raw-TS paths
    /// take, so one stream at a time. An error past the model's tuners, or while a stream
    /// runs.
    pub fn set_tuner(&mut self, n: usize) -> Result<()> {
        let fe = self
            .front_end(n)
            .ok_or(Error::Param("the unit has no such tuner"))?;
        self.no_stream()?;
        self.fe = fe;
        self.s1.fe = fe;
        Ok(())
    }

    /// The tuner the tunes and streams use (0-based).
    pub fn tuner(&self) -> usize {
        self.fe.tuner()
    }

    /// The device's USB serial number (`None` if the OS reports none).
    pub fn serial(&self) -> Option<&str> {
        self.sh.usb.serial()
    }

    /// The model, from the USB product id.
    pub fn model(&self) -> Model {
        // `open` takes only the product ids of `dt295_usb::PIDS`, each a model
        Model::from_pid(self.sh.usb.pid()).unwrap_or(Model::Dt295)
    }

    /// The System1 transcoder firmware (`mb86m01_assp_osec_trc_m_h.bin`). Must be
    /// set before [`Device::start`]: the front-end power-on sequence depends on it.
    pub fn set_trc_firmware(&mut self, path: Option<PathBuf>) {
        self.s1.set_trc(path);
    }

    /// The link key a previous process verified.
    /// Set before [`Device::start`]; a cold boot discards it.
    ///
    /// The device keeps link-encrypting the TS with the first key it handed out after
    /// power-up; a new process on a device that was not re-plugged gets a fresh key the
    /// device never applies. The stream checks both keys against the PAT CRC and uses
    /// the one that works. This is a per-power-cycle USB transport key, not a card or
    /// user secret.
    pub fn set_prior_link_key(&mut self, key: [u8; 16]) {
        self.set_prior_link_keys(&[key]);
    }

    /// Keys that may be the applied one, tried in order when the fresh key fails: the
    /// one a previous process proved, then keys handed out to processes that ended
    /// before proving any (the device applies the first key it hands out after its
    /// secure session lapsed). Dropped by a cold boot.
    pub fn set_prior_link_keys(&mut self, keys: &[[u8; 16]]) {
        self.sh.lock().link.alt = keys.to_vec();
    }

    /// Called with each link key the device hands out, before a stream proves it, so
    /// the host can keep it (a process can end any time, e.g. by SIGKILL).
    pub fn set_link_key_journal(&mut self, journal: KeyJournal) {
        self.sh
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0 = Some(journal);
    }

    fn no_stream(&self) -> Result<()> {
        if self.s1.running() {
            return Err(Error::State("a stream is running (stop it first)".into()));
        }
        Ok(())
    }

    /// Boot the firmware (download from `fw` on a cold device, soft reset otherwise),
    /// authenticate, power on the front end.
    pub fn start(&mut self, fw: &Path) -> Result<()> {
        self.no_stream()?;
        let fe = self.fe;
        let mut g = self.sh.lock();
        let st = &mut *g;
        st.proto.fw_stall = 0; // a fresh (re)start gets to try commands again
        // the boot re-authenticates (a new session): restart the keepalive counter as a
        // fresh process does
        st.proto.ka_seq = 0;
        st.proto.ka_fails = 0;
        if self.sh.s2_streaming.load(Ordering::Acquire) {
            warn!("the unit starts again: the second tuner's stream ends");
        }
        st.boot_gen = 0; // not booted until it is
        let mut io = Io::new(&self.sh.usb, &mut st.proto);
        fw::boot(&mut io, fw, &mut st.link.alt)?;
        self.fw_booted = true;
        // a (re)boot drops the card session; the next stream re-establishes it
        self.s1.cas_ready = false;
        st.card_open = false;
        // The keepalive clock starts with the fresh session, the first keepalive due
        // KA_FIRST after authentication. Not ~3 s: the firmware sometimes answers that
        // with a short challenge (hardware 2026-10-18: "keepalive FAILED" once per open).
        // Not the whole KA_INTERVAL either: a recorder's tuner command (mirakc) often
        // lives less than 12 s, and a run of them that never sent a keepalive let the
        // firmware stop acking about 70 s later (Linux VM 2026-10-24). A stream that
        // starts sooner sends the first one at once (`s1::bringup`).
        let now = Instant::now();
        io.p.ka_last = Some(now.checked_sub(KA_INTERVAL - KA_FIRST).unwrap_or(now));
        // The System1 front end, whose demod init feeds the LSI the TS the stream path
        // needs; otherwise cmd(5,0,2) underruns to 0xFFFF.
        fe1::power_on(&mut io, fe)?;
        st.boot_gen = BOOTS.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(())
    }

    /// The second tuner of a two-tuner model (PIX-DT295W, PIX-DT195W), to tune and stream
    /// beside this one (the first):
    /// one at a time. Its System2 stream works while this `Device` keeps the unit booted
    /// ([`Device::start`]); its raw-TS stream needs no firmware.
    pub fn tuner2(&self) -> Result<crate::Tuner2> {
        if self.model().tuners() < 2 {
            return Err(Error::Param(
                "only the two-tuner models (PIX-DT295W, PIX-DT195W) have a second tuner",
            ));
        }
        if self.sh.tuner2.swap(true, Ordering::AcqRel) {
            return Err(Error::State("the second tuner is in use".into()));
        }
        Ok(crate::Tuner2::new(Arc::clone(&self.sh)))
    }

    /// Open the B-CAS card session again in place, for a card put back in (no stream
    /// may run), and start the stream on the tune as it is: stage 2 of the card session
    /// and up to 4 s for the card to work. Much quicker than [`Device::start`], and the
    /// link key stays. `false`: it did not work (no card, no session yet, or the card did
    /// not come up): start the device again instead.
    pub fn resume_in_place(&mut self, sink: TsSink) -> bool {
        let reopened = self.no_stream().and_then(|()| {
            // no card session yet: the next stream opens one
            Ok(self.s1.cas_ready && self.sh.with_io(crate::s1::reopen_card)?)
        });
        match reopened {
            Ok(true) => self
                .stream_start(sink)
                .inspect_err(|e| warn!("stream start failed ({e})"))
                .is_ok(),
            Ok(false) => {
                info!("the card session did not reopen in place");
                false
            }
            Err(e) => {
                warn!("reopening the card session failed ({e})");
                false
            }
        }
    }

    /// The firmware stopped acking or faulted (0xFFFF): [`Device::start`] again.
    /// (While a stream runs only the ack stall is checked: the worker owns the device.)
    pub fn needs_recovery(&mut self) -> bool {
        let mut g = self.sh.lock();
        if g.proto.fw_stall >= FW_STALL_LIMIT {
            return true;
        }
        if self.s1.running() {
            return false;
        }
        matches!(
            Io::new(&self.sh.usb, &mut g.proto).read16(REG_FW_STATE),
            Ok(0xFFFF)
        )
    }

    /// Keep the secure session alive while nothing streams; call about once a second.
    ///
    /// The firmware faults to 0xFFFF about 60 s after the last SECURE_REALTIME keepalive,
    /// also while idle. This does no device I/O unless a keepalive is due; it is a
    /// no-op while streaming (the worker keeps the session alive then) and before a
    /// session exists.
    pub fn idle_tick(&mut self) {
        self.s1.idle_keepalive(&self.sh);
    }

    /// Tune ISDB-T and wait for lock.
    pub fn tune_t(&mut self, freq_hz: u32) -> Result<()> {
        let fe = self.fe;
        self.tune(System::Terrestrial, 0, |io| fe1::tune_t(io, fe, freq_hz))
    }

    /// Tune ISDB-S (`freq_hz` = IF, `tsid` = the broadcast TSID) and wait for lock.
    pub fn tune_s(&mut self, freq_hz: u32, tsid: u16) -> Result<()> {
        let mut got = None;
        let fe = self.fe;
        self.tune(System::Satellite, tsid, |io| {
            got = fe1::tune_s(io, fe, freq_hz, tsid)?;
            Ok(())
        })?;
        // the PSI keeper serves the TS the demod streams (without a TSID or with one
        // the transponder does not carry: its first)
        if let Some(t) = got.filter(|&t| t != tsid) {
            self.s1.cur_tsid = t;
        }
        Ok(())
    }

    fn tune(
        &mut self,
        sys: System,
        tsid: u16,
        f: impl FnOnce(&mut Io<'_>) -> Result<()>,
    ) -> Result<()> {
        if !self.fw_booted {
            return Err(Error::State("the firmware is not booted".into()));
        }
        self.no_stream()?;
        // empty-channel runs start no stream: keep the session alive
        self.s1.idle_keepalive(&self.sh);
        self.s1.cur_system = Some(sys); // the PSI keeper serves satellite TSs only
        self.s1.cur_tsid = tsid; // the PSI keeper serves this TS
        self.sh.with_io(f)
    }

    /// Lock and C/N right after a tune (do not call while streaming).
    pub fn read_status(&mut self, sys: System) -> Result<Status> {
        self.no_stream()?;
        let fe = self.fe;
        self.sh.with_io(|io| fe1::read_status(io, fe, sys))
    }

    /// Service to stream next; `None` = every service of the TS.
    pub fn set_service_id(&mut self, sid: Option<u16>) {
        self.s1.service = sid;
    }

    /// Start streaming into `sink` (System1, EP 0x81): every service of the TS (the
    /// routes follow its PAT and PMTs), or the service [`Device::set_service_id`] set,
    /// single-service (switched to the TS's PSI if it delivers nothing).
    pub fn stream_start(&mut self, sink: TsSink) -> Result<()> {
        self.s1.stream_start(&self.sh, sink)
    }

    /// Stop the stream and return the firmware to idle.
    pub fn stream_stop(&mut self) -> Result<()> {
        self.s1.stream_stop(&self.sh)
    }

    /// The link key a stream's PAT proved the device applies (persist it).
    pub fn link_key(&self) -> Option<[u8; 16]> {
        let st = self.sh.lock();
        st.link.verified.then_some(st.link.key)
    }

    /// The key the device handed out at the last stream start, while no PAT proved it
    /// (keep it with the proved one). A stream that starts with TS left over from the
    /// previous secure session proves that session's key first; a process ended then
    /// has not proved this one, which the device's own TS uses.
    pub fn unproved_link_key(&self) -> Option<[u8; 16]> {
        let st = self.sh.lock();
        st.link
            .fresh
            .filter(|k| !(st.link.verified && st.link.key == *k))
    }

    /// Live values for other threads.
    pub fn live(&self) -> Live {
        Live {
            inner: Arc::clone(&self.sh.live),
        }
    }

    /// Cached satellite PSI for the host's state file; clears [`Device::psi_changed`].
    /// Call while not streaming.
    pub fn psi_export(&mut self) -> Vec<PsiRecord> {
        self.sh.lock().psi.export()
    }

    /// Take one cached section back (false: bad CRC/kind, or unseen for 90 days).
    pub fn psi_import(&mut self, rec: &PsiRecord) -> bool {
        self.sh.lock().psi.import(rec, crate::today())
    }

    /// The PSI cache gained or refreshed sections since the last export/import, or lost
    /// some that went unseen for [`crate::PSI_KEEP_DAYS`] (they age out here too, for a
    /// host that runs for months).
    pub fn psi_changed(&self) -> bool {
        let mut sh = self.sh.lock();
        sh.psi.age(crate::today());
        sh.psi.changed()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.raw.is_some() {
            let _ = self.stream_stop_raw();
        }
        if self.s1.running() {
            let _ = self.s1.stream_stop(&self.sh);
        }
        if self.s1.worker_stuck {
            // The stuck worker still holds its own reference to the shared state and the
            // USB handle, so they stay alive (and the device open) until it exits.
            error!("close: stream worker still running; leaking the device handle");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_cn_needs_a_fresh_sample() {
        let l = LiveInner::new();
        assert_eq!(l.cn(), None); // no sample yet
        l.publish_cn(12_345);
        assert_eq!(l.cn(), Some(12_345)); // a fresh sample shows (the raw path never sets streaming)
        l.reset_stream();
        assert_eq!(l.cn(), None); // a new worker clears it until it samples again

        // a sample older than 3 s does not count
        let Some(epoch) = Instant::now().checked_sub(std::time::Duration::from_secs(10)) else {
            return;
        };
        let old = LiveInner {
            epoch,
            ..LiveInner::new()
        };
        old.publish_cn(7);
        assert_eq!(old.cn(), Some(7));
        old.cn_at.store(1, Ordering::Release); // taken at the epoch, 10 s ago
        assert_eq!(old.cn(), None);
    }

    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}

    #[test]
    fn shared_state_crosses_threads() {
        assert_send_sync::<Shared>();
        assert_send_sync::<Live>();
        assert_send::<Device>(); // the host keeps it in a Mutex shared by its threads
    }
}
