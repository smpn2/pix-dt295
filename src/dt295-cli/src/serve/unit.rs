//! One served unit: a thread that holds the PIX-DT295 open for good, the way the
//! BonDriver does inside TVTest. It boots the firmware once, keeps the secure
//! session alive while idle, and for each request tunes and streams to the client's
//! connection until the client goes away (a recorder kills `dt295 tune`: the socket
//! closes and the stream is stopped properly), a new request takes the unit over, the
//! unit is unplugged, or the daemon stops. Unplugged, it waits for the unit to come
//! back and opens it again. A B-CAS card pulled or put in during a stream leaves it
//! running scrambled: the stream is started over on a booted device ([`CardWatch`]), as
//! is a stream whose firmware stopped acking.
//!
//! A unit streams on one path, System1 or raw-TS: the configured one, or (a unit opened
//! for a request) the one its first request asked for. A request for the other path is
//! refused while the unit is open.
//!
//! A two-tuner model's second tuner streams on a thread of its own ([`super::tuner2`]); this
//! one lends it the tuner while the unit is open, and boots the unit again when its
//! firmware faulted while no stream runs here. On the raw-TS path the two tuners stream
//! the same way, each through its own TS capture of the chip ([`super::raw`]).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dt295::ipc::{Conn, peer_closed};
use dt295::state::{State, key_journal};
use dt295::{CardWatch, Device, Live, System, TsSink};
use tracing::{info, warn};

use super::conf::Conf;
use super::proto::{Mode, Reply, Request, Tuning};
use super::tuner2::Slot;
use crate::tuner::stopping;

/// A request and the connection its TS goes to.
pub struct Job {
    pub req: Request,
    pub conn: Conn,
}

/// What a tuner's thread tells the daemon: whether it is busy (`--idle-exit` waits for
/// it), and the C/N of its stream (`status`).
#[derive(Debug, Default)]
pub struct Info {
    /// A request is being served: tuning, or streaming.
    busy: AtomicBool,
    /// The running stream's device values and the C/N of its tune.
    stream: Mutex<Option<(Live, Option<i32>)>>,
}

impl Info {
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// The running stream's C/N (0.001 dB): its latest, else its tune's.
    pub fn cn_mdb(&self) -> Option<i32> {
        let stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
        let (live, tuned) = stream.as_ref()?;
        live.cn_mdb().or(*tuned)
    }

    pub(super) fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Release);
    }

    pub(super) fn set_stream(&self, stream: Option<(Live, Option<i32>)>) {
        *self.stream.lock().unwrap_or_else(PoisonError::into_inner) = stream;
    }
}

/// Idle cadence: the keepalive check (it only sends when one is due).
const TICK: Duration = Duration::from_secs(1);
/// How often to check that the unit is still plugged in.
const PRESENCE_EVERY: Duration = Duration::from_secs(5);
/// How long to wait before trying to open (or boot) the unit again.
const OPEN_RETRY: Duration = Duration::from_secs(5);
/// The streaming loop's cadence (client gone, new request, link key to save).
pub(super) const POLL: Duration = Duration::from_millis(200);
/// TS buffered for a slow client, in the worker's reads (up to 64 KiB each): seconds
/// of TS. Beyond it the TS is dropped (and counted), so a client that stops reading
/// never stalls the USB reads.
const QUEUE_READS: usize = 256;
/// A stream that ran this long without a byte marks the device for a fresh boot
/// before the next tune (the remux can stop emitting TS after many bring-ups).
const SILENT: Duration = Duration::from_secs(10);

/// What a unit's thread starts with: its serial number, its path (`fixed`: the
/// configuration's, never changed), and where it lends a two-tuner model's second tuner.
pub struct Spec {
    pub serial: String,
    pub mode: Mode,
    pub fixed: bool,
    pub slot: Arc<Slot>,
}

/// Start the thread of a unit (its first tuner).
pub fn spawn(
    spec: Spec,
    conf: Arc<Conf>,
    info: Arc<Info>,
) -> std::io::Result<(mpsc::Sender<Job>, JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name(format!("unit {}", spec.serial))
        .spawn(move || run(spec, &conf, &info, &rx))?;
    Ok((tx, thread))
}

fn run(spec: Spec, conf: &Conf, info: &Info, jobs: &Receiver<Job>) {
    let serial = spec.serial.as_str();
    let span = tracing::info_span!("unit", serial);
    let _entered = span.enter();
    let mut unit = Unit::new(
        serial,
        spec.mode == Mode::RawTs,
        spec.fixed,
        conf,
        info,
        spec.slot,
    );
    let mut next = None;
    // the client of the last request served
    let mut last: Option<String> = None;
    while !stopping() {
        unit.open_if_due();
        let job = match next.take() {
            Some(job) => job,
            None => match jobs.recv_timeout(TICK) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    unit.idle();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        let job = yield_to_queued(job, jobs, &format!("unit {serial}"), last.as_deref());
        last.clone_from(&job.req.client);
        let busy = Busy::new(info);
        next = unit.serve(job, jobs);
        drop(busy);
    }
    unit.close();
}

/// Whether `asked` comes from the client whose stream `running` is (it replaces its own
/// stream, also `if-free`).
fn same_client(asked: &Request, running: &Request) -> bool {
    asked.client.is_some() && asked.client == running.client
}

/// What came for a tuner while `running` streams on it.
pub(super) enum Queued {
    Nothing,
    /// A request that takes the tuner over.
    Takeover(Job),
    /// The daemon stops.
    Stop,
}

/// Take the next request for a tuner (`name`: "unit X", "unit X tuner 2") while `running`
/// streams on it: an `if-free` one of another client (a background job, a client that
/// does not cut another's stream off) is refused (`busy`); any other takes the tuner over.
pub(super) fn queued(jobs: &Receiver<Job>, running: &Request, name: &str) -> Queued {
    match jobs.try_recv() {
        Ok(mut job) if job.req.if_free && !same_client(&job.req, running) => {
            info!("{}: refused, the tuner is streaming", job.req.to_line());
            let _ = reply(&mut job.conn, &Reply::Busy(format!("{name} is streaming")));
            Queued::Nothing
        }
        Ok(job) => Queued::Takeover(job),
        Err(TryRecvError::Disconnected) => Queued::Stop,
        Err(TryRecvError::Empty) => Queued::Nothing,
    }
}

/// Of a background request (`if-free`: a logo run, a channel scan) and one queued
/// right behind it, the one that takes the tuner over goes first and the background one
/// is refused: started, it would only be taken over seconds later. Two background
/// requests: that of the client the tuner served last (`last`) goes (a TVTest changing
/// its channel goes before another one's retry), else the first; the other is refused.
/// `name`: the tuner's, as in [`queued`].
pub(super) fn yield_to_queued(
    job: Job,
    jobs: &Receiver<Job>,
    name: &str,
    last: Option<&str>,
) -> Job {
    if !job.req.if_free {
        return job;
    }
    let Ok(other) = jobs.try_recv() else {
        return job;
    };
    let (go, mut refused) = if other_goes(&job.req, &other.req, last) {
        (other, job)
    } else {
        (job, other)
    };
    info!(
        "{}: refused, another request came with it",
        refused.req.to_line()
    );
    let why = format!("{name} is taken by another request");
    let _ = reply(&mut refused.conn, &Reply::Busy(why));
    go
}

/// Of a background request `first` and `other` queued behind it, whether `other` goes
/// ([`yield_to_queued`]).
fn other_goes(first: &Request, other: &Request, last: Option<&str>) -> bool {
    let of_last = |r: &Request| last.is_some() && r.client.as_deref() == last;
    !other.if_free || (of_last(other) && !of_last(first))
}

struct Unit<'a> {
    serial: &'a str,
    conf: &'a Conf,
    info: &'a Info,
    /// Blob-free raw-TS mode: no vendor firmware, the device streams the scrambled TS
    /// (the host descrambles). Chosen per unit in the configuration, or by the first
    /// request.
    raw: bool,
    /// The configuration chose the mode: no request changes it.
    fixed: bool,
    dev: Option<Device>,
    state_path: PathBuf,
    /// What the state file holds (the journal also writes keys to it directly).
    state: State,
    /// The state file is there but could not be read at the open: it is left alone
    /// (nothing is saved) until the next open reads it, as a write would lose its link
    /// keys and PSI.
    state_unreadable: bool,
    /// The journal wrote a key to the state file.
    pending_written: Arc<AtomicBool>,
    /// The last stream delivered nothing: boot again before the next tune.
    reboot: bool,
    /// When to start a stream over for the B-CAS card (per opened device).
    watch: Option<CardWatch>,
    next_open: Instant,
    absent_logged: bool,
    last_presence: Instant,
    /// Where a two-tuner model's second tuner is lent to its thread.
    slot: Arc<Slot>,
}

impl<'a> Unit<'a> {
    fn new(
        serial: &'a str,
        raw: bool,
        fixed: bool,
        conf: &'a Conf,
        info: &'a Info,
        slot: Arc<Slot>,
    ) -> Self {
        Self {
            serial,
            conf,
            info,
            raw,
            fixed,
            dev: None,
            state_path: conf.state_file(serial),
            state: State::default(),
            state_unreadable: false,
            pending_written: Arc::new(AtomicBool::new(false)),
            reboot: false,
            watch: None,
            next_open: Instant::now(),
            absent_logged: false,
            last_presence: Instant::now(),
            slot,
        }
    }

    /// Lend a two-tuner model's second tuner to its thread (the unit is open, and booted on
    /// the System1 path), or tell it there is none.
    fn lend_tuner2(&self, dev: &Device) {
        if dev.model().tuners() < 2 {
            return self.slot.none();
        }
        match dev.tuner2() {
            Ok(t2) => self.slot.lend(t2, self.raw),
            Err(e) => warn!("the second tuner: {e}"),
        }
    }

    /// Close the unit: take the second tuner back first (it holds the USB device too).
    fn drop_device(&mut self, dev: Device) {
        drop(self.slot.take_back());
        drop(dev);
    }

    fn open_if_due(&mut self) {
        if self.dev.is_none() && Instant::now() >= self.next_open {
            self.open();
        }
    }

    /// Open the unit, hand it the state file's link keys and PSI, and boot it.
    fn open(&mut self) {
        self.next_open = Instant::now() + OPEN_RETRY;
        if self.raw {
            return self.open_raw();
        }
        let mut dev = match Device::open_serial(self.serial) {
            Ok(dev) => dev,
            Err(dt295::Error::NoDevice) => {
                if !self.absent_logged {
                    info!("not connected; waiting for it");
                    self.absent_logged = true;
                }
                return;
            }
            Err(e) => {
                warn!("cannot open: {e}");
                return;
            }
        };
        self.absent_logged = false;
        // the System1 path; before start: the front-end power-on depends on it
        dev.set_trc_firmware(Some(self.conf.trc_firmware()));
        (self.state, self.state_unreadable) = match State::load_settled(&self.state_path) {
            Ok(st) => (st.unwrap_or_default(), false),
            Err(e) => {
                warn!(
                    "cannot read {} ({e}); leaving it as it is, nothing is saved until it reads",
                    self.state_path.display()
                );
                (State::default(), true)
            }
        };
        let keys = self.state.prior_keys();
        if !keys.is_empty() {
            dev.set_prior_link_keys(&keys);
        }
        dev.set_link_key_journal(key_journal(
            self.state_path.clone(),
            Arc::clone(&self.pending_written),
        ));
        // what aged out leaves the file now, not only at the next change of the PSI
        if self.state.import_psi(&mut dev)
            && !self.state_unreadable
            && let Err(e) = self.state.save(&self.state_path)
        {
            warn!("cannot save {}: {e}", self.state_path.display());
        }
        let t0 = Instant::now();
        match boot(&mut dev, &self.conf.idle_firmware()) {
            Ok(()) => {
                info!("opened and booted ({} ms)", t0.elapsed().as_millis());
                self.watch = Some(CardWatch::new(&dev.live()));
                self.lend_tuner2(&dev);
                self.dev = Some(dev);
                self.reboot = false;
                self.last_presence = Instant::now();
            }
            Err(e) => warn!("cannot boot: {e}"),
        }
    }

    /// Open the unit cold and hold it: no vendor firmware, no boot, no secure session,
    /// no state. The first tune (of either tuner) does the blob-free cold bring-up; a later
    /// one retunes. A two-tuner model's second tuner is lent to its thread.
    fn open_raw(&mut self) {
        let dev = match Device::open_serial(self.serial) {
            Ok(dev) => dev,
            Err(dt295::Error::NoDevice) => {
                if !self.absent_logged {
                    info!("not connected; waiting for it");
                    self.absent_logged = true;
                }
                return;
            }
            Err(e) => {
                warn!("cannot open: {e}");
                return;
            }
        };
        self.absent_logged = false;
        self.watch = None;
        self.lend_tuner2(&dev);
        self.dev = Some(dev);
        self.last_presence = Instant::now();
        info!("opened (raw-TS, cold; no firmware)");
    }

    /// Keep the session alive, notice an unplug, save what changed.
    fn idle(&mut self) {
        let Some(mut dev) = self.dev.take() else {
            return;
        };
        if self.raw {
            // no secure session to keep alive and no state to save; just notice an unplug
            if self.last_presence.elapsed() >= PRESENCE_EVERY {
                self.last_presence = Instant::now();
                if !self.present() {
                    info!("disconnected");
                    self.next_open = Instant::now();
                    return self.drop_device(dev);
                }
            }
            self.dev = Some(dev);
            return;
        }
        dev.idle_tick();
        if self.last_presence.elapsed() >= PRESENCE_EVERY {
            self.last_presence = Instant::now();
            if !self.present() {
                info!("disconnected");
                self.save(&mut dev);
                self.next_open = Instant::now();
                return self.drop_device(dev);
            }
            // the firmware faulted while no stream ran here (under the second tuner's,
            // say): boot again, and the second tuner's stream starts over
            if dev.needs_recovery() {
                warn!("the firmware faulted; booting the device again");
                if let Err(e) = boot(&mut dev, &self.conf.idle_firmware()) {
                    warn!("cannot boot: {e}");
                }
            }
        }
        self.save(&mut dev);
        self.dev = Some(dev);
    }

    fn present(&self) -> bool {
        present(self.serial)
    }

    /// Write the state file if a stream proved a link key or (not streaming) the PSI
    /// cache changed.
    fn save(&mut self, dev: &mut Device) {
        if self.pending_written.swap(false, Ordering::Relaxed) {
            // the journal wrote an unproved key to the file: keep it in our copy
            if let Ok(Some(file)) = State::load(&self.state_path) {
                self.state.pending_keys = file.pending_keys;
            }
        }
        let mut changed = self
            .state
            .take_link_keys(dev.link_key(), dev.unproved_link_key());
        if !dev.live().is_streaming() && dev.psi_changed() {
            self.state.psi = dev.psi_export();
            changed = true;
        }
        if changed
            && !self.state_unreadable
            && let Err(e) = self.state.save(&self.state_path)
        {
            warn!("cannot save {}: {e}", self.state_path.display());
        }
    }

    fn mode(&self) -> Mode {
        if self.raw { Mode::RawTs } else { Mode::System1 }
    }

    /// Stream on `mode` (`None`: the unit's own): a unit not open that the configuration
    /// does not fix takes it; `Err` says why not.
    fn take_mode(&mut self, mode: Option<Mode>) -> Result<(), String> {
        let Some(mode) = mode.filter(|&m| m != self.mode()) else {
            return Ok(());
        };
        let (have, want) = (self.mode().word(), mode.word());
        if self.fixed {
            return Err(format!(
                "unit {} streams {have} (as configured), not {want}",
                self.serial
            ));
        }
        if self.dev.is_some() {
            return Err(format!(
                "unit {} streams {have} (the path it was opened on), not {want}",
                self.serial
            ));
        }
        info!("{want} from now on");
        self.raw = mode == Mode::RawTs;
        self.watch = None;
        self.reboot = false;
        Ok(())
    }

    /// Serve one request; returns a request that took the unit over meanwhile.
    fn serve(&mut self, mut job: Job, jobs: &Receiver<Job>) -> Option<Job> {
        if let Err(why) = self.take_mode(job.req.mode) {
            warn!("{}: {why}", job.req.to_line());
            let _ = reply(&mut job.conn, &Reply::Error(why));
            return None;
        }
        if self.raw {
            return self.serve_raw(job, jobs);
        }
        let Job { req, mut conn } = job;
        if self.dev.is_none() {
            self.open();
        }
        let Some(mut dev) = self.dev.take() else {
            let why = format!("unit {} is not connected", self.serial);
            let _ = reply(&mut conn, &Reply::Error(why));
            return None;
        };
        let fw = self.conf.idle_firmware();
        if self.reboot || dev.needs_recovery() {
            info!(
                "{}; booting the device again",
                if self.reboot {
                    "the last stream delivered nothing"
                } else {
                    "the firmware faulted"
                }
            );
            if let Err(e) = boot(&mut dev, &fw) {
                warn!("cannot boot: {e}");
                let _ = reply(
                    &mut conn,
                    &Reply::Error(format!("cannot boot the unit: {e}")),
                );
                self.dev = Some(dev);
                return None;
            }
            self.reboot = false;
        }

        let t0 = Instant::now();
        if let Some(w) = self.watch.as_mut() {
            w.tuned();
        }
        let (pipe, queue) = Pipe::new();
        let mut started = start(&mut dev, &req, pipe.sink());
        if let Err(why) = &started {
            // 45-60 s after a previous session the firmware can fault on the way
            if dev.needs_recovery() && !stopping() {
                warn!("{why}; the firmware faulted, booting the device once more");
                started = boot(&mut dev, &fw)
                    .map_err(|e| format!("cannot boot the unit: {e}"))
                    .and_then(|()| start(&mut dev, &req, pipe.sink()));
            }
        }
        let cn_mdb = match started {
            Ok(cn_mdb) => cn_mdb,
            Err(why) => {
                warn!("{}: {why}", req.to_line());
                let _ = reply(&mut conn, &Reply::Error(why));
                self.dev = Some(dev);
                return None;
            }
        };
        info!(
            "{}: streaming ({} ms)",
            req.to_line(),
            t0.elapsed().as_millis()
        );
        self.info.set_stream(Some((dev.live(), cn_mdb)));
        // to end a write the client never takes (a takeover must not wait for it)
        let streaming = Streaming::new(self.info, &pipe, &conn);
        let ok = Reply::Ok {
            unit: Some(self.serial.to_owned()),
            cn_mdb,
        };
        if reply(&mut conn, &ok).is_err() {
            pipe.closed.store(true, Ordering::Release); // the client is gone already
        }
        let name = format!("unit {}", self.serial);
        let writer = {
            let pipe = Arc::clone(&pipe);
            thread::Builder::new()
                .name(format!("unit {} writer", self.serial))
                .spawn(move || write_ts(&pipe, &queue, conn))
        };
        if writer.is_err() {
            pipe.closed.store(true, Ordering::Release);
        }

        let t_stream = Instant::now();
        let mut takeover = None;
        let mut unplugged = false;
        let end = loop {
            if stopping() {
                break "the daemon stops";
            }
            // a write fails at once, but while nothing is written (the stream holds its
            // TS back until the service's PAT) only the socket tells
            if pipe.closed.load(Ordering::Acquire) || streaming.client_closed() {
                break "the client went away";
            }
            match queued(jobs, &req, &name) {
                Queued::Nothing => {}
                Queued::Takeover(job) => {
                    takeover = Some(job);
                    break "a new request takes the tuner over";
                }
                Queued::Stop => break "the daemon stops",
            }
            if dev.live().device_lost() {
                unplugged = true;
                break "the unit was unplugged";
            }
            if self.last_presence.elapsed() >= PRESENCE_EVERY {
                self.last_presence = Instant::now();
                if !self.present() {
                    unplugged = true;
                    break "the unit was unplugged";
                }
            }
            let stalled = dev.needs_recovery();
            let over = self
                .watch
                .as_mut()
                .and_then(|w| w.poll(&dev.live(), stalled, Instant::now()));
            if let Some(over) = over {
                info!("{}: starting the stream over", over.why);
                if let Err(e) = dev.stream_stop() {
                    warn!("stream stop failed: {e}");
                }
                let again = if over.in_place && dev.resume_in_place(pipe.sink()) {
                    Ok(())
                } else {
                    boot(&mut dev, &fw)
                        .map_err(|e| format!("cannot boot the unit: {e}"))
                        .and_then(|()| start(&mut dev, &req, pipe.sink()).map(drop))
                };
                if let Some(w) = self.watch.as_mut() {
                    w.started_over(Instant::now());
                }
                if let Err(why) = again {
                    warn!("{}: {why}", req.to_line());
                    break "the stream did not start again";
                }
            }
            self.save(&mut dev); // a link key the stream proved, at once
            thread::sleep(POLL);
        };
        let device_bytes = dev.live().delivered();
        if let Err(e) = dev.stream_stop() {
            warn!("stream stop failed: {e}");
        }
        drop(streaming);
        if let Ok(writer) = writer {
            let _ = writer.join();
        }
        let bytes = pipe.bytes.load(Ordering::Relaxed);
        let dropped = pipe.dropped.load(Ordering::Relaxed);
        info!(
            "stream ended after {:.1} s ({end}): {bytes} bytes{}",
            t_stream.elapsed().as_secs_f64(),
            if dropped > 0 {
                format!(", {dropped} dropped (the client did not read in time)")
            } else {
                String::new()
            }
        );
        if device_bytes == 0 && t_stream.elapsed() >= SILENT {
            warn!("the device delivered nothing; the next tune boots it first");
            self.reboot = true;
        } else if bytes == 0 && device_bytes > 0 {
            info!(
                "no PAT listed the service ({device_bytes} bytes held back): off the air, or \
                 not on this TS"
            );
        }
        self.save(&mut dev);
        if unplugged {
            info!("disconnected");
            self.next_open = Instant::now();
            self.drop_device(dev);
        } else {
            self.dev = Some(dev);
        }
        takeover
    }

    /// Serve one raw-TS request of the first tuner ([`super::raw::serve`]); a two-tuner model's
    /// second tuner streams beside it on its own thread.
    fn serve_raw(&mut self, job: Job, jobs: &Receiver<Job>) -> Option<Job> {
        if self.dev.is_none() {
            self.open();
        }
        let Some(mut dev) = self.dev.take() else {
            let Job { mut conn, .. } = job;
            let why = format!("unit {} is not connected", self.serial);
            let _ = reply(&mut conn, &Reply::Error(why));
            return None;
        };
        let tuner = super::raw::Tuner {
            serial: self.serial,
            number: 1,
            info: self.info,
        };
        let ended = super::raw::serve(&mut dev, job, jobs, &tuner, &|| self.present(), &|| false);
        if ended.unplugged {
            info!("disconnected");
            self.next_open = Instant::now();
            self.drop_device(dev);
        } else {
            self.dev = Some(dev);
        }
        ended.takeover
    }

    fn close(&mut self) {
        if let Some(mut dev) = self.dev.take() {
            if !self.raw {
                self.save(&mut dev);
            }
            self.drop_device(dev);
        }
    }
}

impl Drop for Unit<'_> {
    /// As a panic unwinds the thread: take the second tuner back, so its stream ends (its
    /// client asks again, and the threads are started again for it) and the unit closes.
    fn drop(&mut self) {
        if thread::panicking() {
            drop(self.slot.take_back());
        }
    }
}

/// Whether the unit `serial` is plugged in (true when the list cannot be read).
pub(super) fn present(serial: &str) -> bool {
    dt295::present_serials().map_or(true, |serials| {
        serials.iter().any(|s| s.eq_ignore_ascii_case(serial))
    })
}

/// Boot the firmware; once more if it stopped acking on the way (within a minute of
/// an earlier session it can).
fn boot(dev: &mut Device, fw: &std::path::Path) -> dt295::Result<()> {
    dev.start(fw)?;
    if dev.needs_recovery() {
        warn!("the firmware stopped acking during the start; starting once more");
        dev.start(fw)?;
    }
    Ok(())
}

/// Tune, wait for lock (up to ~3 s more), and start streaming into `sink`. Returns the
/// C/N of the lock (0.001 dB).
fn start(dev: &mut Device, req: &Request, sink: TsSink) -> Result<Option<i32>, String> {
    let sys = match req.tuning {
        Tuning::Terrestrial { hz } => {
            dev.tune_t(hz).map_err(|e| format!("tune: {e}"))?;
            System::Terrestrial
        }
        Tuning::Satellite { hz, tsid } => {
            dev.tune_s(hz, tsid).map_err(|e| format!("tune: {e}"))?;
            System::Satellite
        }
    };
    let mut lock = Err(String::new());
    for _ in 0..30 {
        lock = match dev.read_status(sys) {
            Ok(st) if st.locked => Ok(st.cn_mdb),
            Ok(st) => Err(format!(
                "no lock (C/N {:.1} dB)",
                f64::from(st.cn_mdb) / 1000.0
            )),
            Err(e) => Err(format!("status: {e}")),
        };
        if lock.is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let cn_mdb = lock?;
    dev.set_service_id(req.sid);
    dev.stream_start(sink)
        .map_err(|e| format!("stream start: {e}"))?;
    Ok(Some(cn_mdb))
}

pub(super) fn reply(conn: &mut Conn, r: &Reply) -> std::io::Result<()> {
    conn.write_all(format!("{}\n", r.to_line()).as_bytes())
}

/// The TS between the stream worker and the client: the worker never blocks on it.
pub(super) struct Pipe {
    tx: Mutex<Option<SyncSender<Vec<u8>>>>,
    /// Bytes the device delivered (queued or dropped).
    pub bytes: AtomicU64,
    /// Bytes dropped because the client did not keep up.
    pub dropped: AtomicU64,
    /// The client is gone.
    pub closed: AtomicBool,
}

impl Pipe {
    pub fn new() -> (Arc<Self>, Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::sync_channel(QUEUE_READS);
        let pipe = Self {
            tx: Mutex::new(Some(tx)),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        };
        (Arc::new(pipe), rx)
    }

    pub fn sink(self: &Arc<Self>) -> TsSink {
        let pipe = Arc::clone(self);
        Box::new(move |ts: &[u8]| pipe.push(ts))
    }

    fn push(&self, ts: &[u8]) {
        self.bytes.fetch_add(ts.len() as u64, Ordering::Relaxed);
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let tx = self.tx.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(tx) = tx.as_ref() else {
            return;
        };
        match tx.try_send(ts.to_vec()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(ts.len() as u64, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => self.closed.store(true, Ordering::Release),
        }
    }

    /// No more TS: the writer drains the queue and ends.
    pub fn finish(&self) {
        self.tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// A request served: the tuner's thread is busy (`--idle-exit` waits) until this is
/// dropped, also by a panic.
pub(super) struct Busy<'a>(&'a Info);

impl<'a> Busy<'a> {
    pub(super) fn new(info: &'a Info) -> Self {
        info.set_busy(true);
        Self(info)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.set_busy(false);
    }
}

/// A stream running to a client. Dropped (at the stream's end, or as a panic unwinds
/// the tuner's thread), it ends what the stream left: the writer's queue, the client's
/// connection (so the client asks again, and a thread is started again for it), and
/// the stream's values for `status`.
pub(super) struct Streaming<'a> {
    info: &'a Info,
    pipe: Arc<Pipe>,
    shutter: Option<Conn>,
}

impl<'a> Streaming<'a> {
    pub(super) fn new(info: &'a Info, pipe: &Arc<Pipe>, conn: &Conn) -> Self {
        Self {
            info,
            pipe: Arc::clone(pipe),
            shutter: conn.try_clone().ok(),
        }
    }

    /// Whether the client closed its end (also while nothing is written to it).
    pub(super) fn client_closed(&self) -> bool {
        self.shutter.as_ref().is_some_and(peer_closed)
    }
}

impl Drop for Streaming<'_> {
    fn drop(&mut self) {
        self.info.set_stream(None);
        self.pipe.finish();
        if let Some(conn) = &self.shutter {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// The writer thread: queue to client until the queue ends or the client is gone.
pub(super) fn write_ts(pipe: &Pipe, queue: &Receiver<Vec<u8>>, mut conn: Conn) {
    for chunk in queue {
        if conn.write_all(&chunk).is_err() {
            pipe.closed.store(true, Ordering::Release);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(client: Option<&str>, if_free: bool) -> Request {
        Request {
            unit: String::new(),
            tuner: 1,
            tuning: Tuning::Terrestrial { hz: 557_142_857 },
            sid: None,
            mode: None,
            client: client.map(str::to_owned),
            if_free,
        }
    }

    #[test]
    fn a_stream_unwound_by_a_panic_ends() {
        let path =
            std::env::temp_dir().join(format!("dt295-unwind-test-{}.sock", std::process::id()));
        let target = dt295::ipc::Target::Unix(path.clone());
        let listener = dt295::ipc::listen(&target).expect("listen");
        let mut client = dt295::ipc::connect(&target).expect("connect");
        let conn = loop {
            match dt295::ipc::accept(&listener) {
                Ok(c) => break c,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        let info = Info::default();
        let (pipe, queue) = Pipe::new();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _busy = Busy::new(&info);
            let _streaming = Streaming::new(&info, &pipe, &conn);
            assert!(info.busy());
            panic!("a bug in the stream");
        }));
        assert!(unwound.is_err());
        assert!(!info.busy(), "not busy");
        // the writer's queue ends, and the client sees the end of its stream
        assert!(queue.recv().is_err());
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut client, &mut rest).expect("the end");
        drop(conn);
        dt295::ipc::unlisten(listener);
        let mut lock = path.into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }

    #[test]
    fn which_queued_request_goes() {
        let (a, b) = (Some("1-1"), Some("2-1"));
        // one that takes units over goes before a background one
        assert!(other_goes(&req(a, true), &req(None, false), None));
        // two background ones: the first, unless the other is the last client's
        assert!(!other_goes(&req(a, true), &req(b, true), None));
        assert!(other_goes(&req(a, true), &req(b, true), b));
        assert!(!other_goes(&req(a, true), &req(b, true), a));
        assert!(!other_goes(&req(None, true), &req(None, true), None));
    }
}
