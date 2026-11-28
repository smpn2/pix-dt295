//! A served two-tuner model's second tuner: a thread that streams it to the clients that ask
//! for `tuner=2`, beside the unit's thread ([`super::unit`]), which streams the first
//! tuner and keeps the unit: it opens and boots it, keeps the secure session alive and
//! boots it again after a fault. The unit's thread lends this one the second tuner
//! ([`Slot`]) while the unit is open, and takes it back before it closes the unit.
//!
//! A stream ends when its client goes away, a new request takes the tuner over, the
//! unit is closed (unplugged, the daemon stops) or the tuner is taken back. One that
//! delivers nothing for a while (the unit booted again under it, the firmware stopped)
//! is started over on a fresh tune. The tuner streams on the path the unit was opened on;
//! on the raw-TS path ([`super::raw`]) no firmware, the TS of the demod's output B through
//! the chip's second TS capture.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dt295::{System, Tuner2};
use tracing::{info, warn};

use super::proto::{Mode, Reply, Request, Tuning};
use super::raw;
use super::unit::{
    Busy, Info, Job, POLL, Pipe, Queued, Streaming, present, queued, reply, write_ts,
    yield_to_queued,
};
use crate::tuner::stopping;

/// How long a stream may deliver nothing before it is started over.
const SILENT: Duration = Duration::from_secs(10);
/// How long the unit's thread waits for the tuner to come back.
const GIVE_BACK: Duration = Duration::from_secs(10);
/// How long a request waits for the unit to be opened (the daemon just started, the unit
/// was plugged in again).
const LEND_WAIT: Duration = Duration::from_secs(15);

/// Where the unit's thread lends the second tuner out.
#[derive(Default)]
pub struct Slot {
    held: Mutex<Held>,
    back: Condvar,
    /// The path the unit was opened on is raw-TS (set with the tuner).
    raw: AtomicBool,
    /// The unit's thread wants the tuner back: a stream ends and returns it.
    recall: AtomicBool,
    /// The unit is open and has no second tuner (a one-tuner model).
    none: AtomicBool,
}

/// What the slot holds, under one lock.
#[derive(Default)]
struct Held {
    tuner: Option<Tuner2>,
    /// The tuner is out with a stream (not in `tuner`).
    out: bool,
    /// Counts the unit's openings (each [`Slot::lend`]) and the takings back: a tuner that
    /// comes back from an earlier one (a stream that outlasted [`GIVE_BACK`], of a unit
    /// since closed) is dropped, not put back over the unit's current tuner.
    generation: u64,
}

impl Slot {
    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lend `tuner` out (the unit is open, on the raw-TS path if `raw`, else booted).
    pub fn lend(&self, tuner: Tuner2, raw: bool) {
        self.raw.store(raw, Ordering::Release);
        self.recall.store(false, Ordering::Release);
        self.none.store(false, Ordering::Release);
        let old = {
            let mut h = Self::lock(&self.held);
            h.generation += 1;
            h.out = false;
            h.tuner.replace(tuner)
        };
        drop(old);
    }

    /// The unit is open and has no second tuner: requests fail at once.
    pub fn none(&self) {
        self.none.store(true, Ordering::Release);
    }

    /// Take the tuner back (the unit closes): its stream ends and returns it first. The
    /// tuner holds the unit's USB device, which closes only once it is dropped.
    /// One still out after [`GIVE_BACK`] is dropped when it comes back.
    pub fn take_back(&self) -> Option<Tuner2> {
        self.recall.store(true, Ordering::Release);
        let mut h = Self::lock(&self.held);
        let t0 = Instant::now();
        while h.out && t0.elapsed() < GIVE_BACK {
            h = self
                .back
                .wait_timeout(h, Duration::from_millis(100))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        h.generation += 1;
        h.out = false;
        h.tuner.take()
    }

    fn borrow(&self) -> Option<Lent<'_>> {
        let mut h = Self::lock(&self.held);
        let t = h.tuner.take()?;
        h.out = true;
        Some(Lent {
            slot: self,
            tuner: Some(t),
            generation: h.generation,
        })
    }

    /// Put `t`, lent in `generation`, back; or drop it, if the unit was closed (and maybe
    /// opened again) since.
    fn give_back(&self, t: Tuner2, generation: u64) {
        let stale = {
            let mut h = Self::lock(&self.held);
            if h.generation == generation {
                h.tuner = Some(t);
                h.out = false;
                None
            } else {
                Some(t)
            }
        };
        self.back.notify_all();
        drop(stale);
    }

    fn recalled(&self) -> bool {
        self.recall.load(Ordering::Acquire)
    }

    fn mode(&self) -> Mode {
        if self.raw.load(Ordering::Acquire) {
            Mode::RawTs
        } else {
            Mode::System1
        }
    }
}

/// The tuner, lent to a stream: it goes back to the [`Slot`] once dropped, also as a panic
/// unwinds the thread (its stream stopped first), so the unit's thread gets it back and a
/// thread started again finds it there.
struct Lent<'a> {
    slot: &'a Slot,
    tuner: Option<Tuner2>,
    /// The slot's generation it was lent in.
    generation: u64,
}

impl Deref for Lent<'_> {
    type Target = Tuner2;

    fn deref(&self) -> &Tuner2 {
        self.tuner.as_ref().expect("lent until dropped")
    }
}

impl DerefMut for Lent<'_> {
    fn deref_mut(&mut self) -> &mut Tuner2 {
        self.tuner.as_mut().expect("lent until dropped")
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        let Some(mut t) = self.tuner.take() else {
            return;
        };
        if thread::panicking() {
            let _ = t.stream_stop();
            let _ = t.stream_stop_raw();
        }
        self.slot.give_back(t, self.generation);
    }
}

/// Start the thread of unit `serial`'s second tuner.
pub fn spawn(
    serial: String,
    slot: Arc<Slot>,
    info: Arc<Info>,
) -> std::io::Result<(mpsc::Sender<Job>, JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name(format!("unit {serial} tuner 2"))
        .spawn(move || run(&serial, &slot, &info, &rx))?;
    Ok((tx, thread))
}

fn run(serial: &str, slot: &Slot, info: &Info, jobs: &Receiver<Job>) {
    let span = tracing::info_span!("unit", serial, tuner = 2);
    let _entered = span.enter();
    let name = format!("unit {serial} tuner 2");
    let mut next = None;
    // the client of the last request served
    let mut last: Option<String> = None;
    while !stopping() {
        let job = match next.take() {
            Some(job) => job,
            None => match jobs.recv_timeout(Duration::from_secs(1)) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        let job = yield_to_queued(job, jobs, &name, last.as_deref());
        last.clone_from(&job.req.client);
        let busy = Busy::new(info);
        next = serve(serial, slot, info, job, jobs);
        drop(busy);
    }
}

/// Serve one request; returns a request that took the tuner over meanwhile.
#[allow(clippy::too_many_lines)]
fn serve(
    serial: &str,
    slot: &Slot,
    info: &Info,
    mut job: Job,
    jobs: &Receiver<Job>,
) -> Option<Job> {
    // the unit may be opening still (the daemon just started): wait for it a moment
    let t0 = Instant::now();
    let mut t2 = loop {
        if let Some(t2) = slot.borrow() {
            break t2;
        }
        if slot.none.load(Ordering::Acquire) {
            let why = format!("unit {serial} has no second tuner");
            warn!("{}: {why}", job.req.to_line());
            let _ = reply(&mut job.conn, &Reply::Error(why));
            return None;
        }
        if t0.elapsed() >= LEND_WAIT || stopping() {
            let why = format!("unit {serial} is not connected");
            warn!("{}: {why}", job.req.to_line());
            let _ = reply(&mut job.conn, &Reply::Error(why));
            return None;
        }
        thread::sleep(Duration::from_millis(200));
    };
    let have = slot.mode();
    if let Some(want) = job.req.mode.filter(|&m| m != have) {
        let why = format!(
            "unit {serial} streams {} (the path it was opened on), not {}",
            have.word(),
            want.word()
        );
        warn!("{}: {why}", job.req.to_line());
        let _ = reply(&mut job.conn, &Reply::Error(why));
        return None;
    }
    if have == Mode::RawTs {
        let tuner = raw::Tuner {
            serial,
            number: 2,
            info,
        };
        let ended = raw::serve(&mut *t2, job, jobs, &tuner, &|| present(serial), &|| {
            slot.recalled()
        });
        // an unplug: the unit's thread notices it too, and takes the tuner back (once it is
        // back here, as `t2` is dropped)
        return ended.takeover;
    }
    let Job { req, mut conn } = job;
    let name = format!("unit {serial} tuner 2");
    let t0 = Instant::now();
    let (pipe, queue) = Pipe::new();
    let cn_mdb = match start(&mut t2, &req, &pipe) {
        Ok(cn) => cn,
        Err(why) => {
            warn!("{}: {why}", req.to_line());
            let _ = reply(&mut conn, &Reply::Error(why));
            return None;
        }
    };
    info!(
        "{}: streaming ({} ms)",
        req.to_line(),
        t0.elapsed().as_millis()
    );
    let live = t2.live();
    info.set_stream(Some((live.clone(), Some(cn_mdb))));
    let streaming = Streaming::new(info, &pipe, &conn);
    let ok = Reply::Ok {
        unit: Some(serial.to_owned()),
        cn_mdb: Some(cn_mdb),
    };
    if reply(&mut conn, &ok).is_err() {
        pipe.closed.store(true, Ordering::Release); // the client is gone already
    }
    let writer = {
        let pipe = Arc::clone(&pipe);
        thread::Builder::new()
            .name(format!("{name} writer"))
            .spawn(move || write_ts(&pipe, &queue, conn))
    };
    if writer.is_err() {
        pipe.closed.store(true, Ordering::Release);
    }

    let t_stream = Instant::now();
    let mut takeover = None;
    let mut last = (0, Instant::now());
    let end = loop {
        if stopping() {
            break "the daemon stops";
        }
        if slot.recalled() {
            break "the unit closes";
        }
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
        if live.device_lost() {
            break "the unit was unplugged";
        }
        // nothing for a while: the unit booted again under it, or the firmware stopped
        let got = live.delivered();
        if got != last.0 {
            last = (got, Instant::now());
        } else if last.1.elapsed() >= SILENT {
            warn!(
                "{}: no TS for {SILENT:?}; starting the stream over",
                req.to_line()
            );
            let _ = t2.stream_stop();
            if let Err(why) = start(&mut t2, &req, &pipe) {
                warn!("{}: {why}", req.to_line());
                break "the stream did not start again";
            }
            last = (live.delivered(), Instant::now());
        }
        thread::sleep(POLL);
    };
    if let Err(e) = t2.stream_stop() {
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
    drop(t2); // back to the slot
    takeover
}

/// Tune, wait for lock (up to ~3 s more), and start streaming into the pipe. Returns the
/// C/N of the lock (0.001 dB).
fn start(t2: &mut Tuner2, req: &Request, pipe: &Arc<Pipe>) -> Result<i32, String> {
    let sys = match req.tuning {
        Tuning::Terrestrial { hz } => {
            t2.tune_t(hz).map_err(|e| format!("tune: {e}"))?;
            System::Terrestrial
        }
        Tuning::Satellite { hz, tsid } => {
            t2.tune_s(hz, tsid).map_err(|e| format!("tune: {e}"))?;
            System::Satellite
        }
    };
    let mut lock = Err(String::new());
    for _ in 0..30 {
        lock = match t2.read_status(sys) {
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
    t2.set_service_id(req.sid);
    t2.stream_start(pipe.sink())
        .map_err(|e| format!("stream start: {e}"))?;
    Ok(cn_mdb)
}
