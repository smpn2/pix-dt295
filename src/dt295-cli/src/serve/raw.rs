//! A raw-TS stream to a client, for either tuner of a unit: the first on the unit's thread
//! ([`super::unit`]), a two-tuner model's second on its own ([`super::tuner2`]). The two stream at
//! once: each through its own TS capture of the chip.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use dt295::{RAW_STALL, raw_stall_due};
use tracing::{info, warn};

use super::proto::Reply;
use super::unit::{Info, Job, POLL, Pipe, Queued, Streaming, queued, reply, write_ts};
use crate::rawts::{self, RawTuner};
use crate::tuner::stopping;

/// The tuner a stream runs on, as the replies and the daemon see it.
pub struct Tuner<'a> {
    /// The unit's USB serial number.
    pub serial: &'a str,
    /// 1, or 2 (a two-tuner model's second).
    pub number: u8,
    /// Where the stream's C/N goes (`status`).
    pub info: &'a Info,
}

impl Tuner<'_> {
    /// How the replies and the log name the tuner: "unit X", "unit X tuner 2".
    pub fn name(&self) -> String {
        match self.number {
            1 => format!("unit {}", self.serial),
            n => format!("unit {} tuner {n}", self.serial),
        }
    }
}

/// How a raw-TS stream ended.
pub struct Ended {
    /// A request that took the tuner over meanwhile.
    pub takeover: Option<Job>,
    /// The unit was unplugged.
    pub unplugged: bool,
}

/// Bring the chip up (on a cold plug) or retune, and stream `req`'s scrambled TS to `conn`
/// until the client goes away, a new request takes the tuner over, the tuner is `recalled`,
/// the unit is unplugged (not `present`) or the daemon stops. No firmware, no secure
/// session, no state, no B-CAS (the host descrambles).
#[allow(clippy::too_many_lines)]
pub fn serve(
    t: &mut impl RawTuner,
    job: Job,
    jobs: &Receiver<Job>,
    tuner: &Tuner,
    present: &dyn Fn() -> bool,
    recalled: &dyn Fn() -> bool,
) -> Ended {
    let Job { req, mut conn } = job;
    let name = tuner.name();
    let mut ended = Ended {
        takeover: None,
        unplugged: false,
    };
    let t0 = Instant::now();
    let (pipe, queue) = Pipe::new();
    let full = match rawts::start(t, req.tuning, pipe.sink()) {
        Ok(full) => full,
        Err(why) => {
            warn!("{}: {why}", req.to_line());
            let _ = reply(&mut conn, &Reply::Error(why));
            return ended;
        }
    };
    // the cold bootstrap and the lock wait take seconds; wait (bounded) for the first TS
    if !rawts::wait_first_ts(t, || pipe.bytes.load(Ordering::Relaxed)) {
        let why = if stopping() {
            let _ = t.stream_stop_raw();
            "the daemon stops".to_owned()
        } else {
            rawts::why_no_ts(t, full)
        };
        warn!("{}: {why}", req.to_line());
        let _ = reply(&mut conn, &Reply::Error(why));
        return ended;
    }
    info!(
        "{}: streaming raw-TS ({} ms)",
        req.to_line(),
        t0.elapsed().as_millis()
    );
    let live = t.live();
    let cn_mdb = live.cn_mdb();
    tuner.info.set_stream(Some((live, cn_mdb)));
    let streaming = Streaming::new(tuner.info, &pipe, &conn);
    let ok = Reply::Ok {
        unit: Some(tuner.serial.to_owned()),
        cn_mdb,
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
    let mut stall: Option<(u64, Instant)> = None;
    // Consecutive restarts that delivered no TS (off air / no lock): back off a little each
    // time, up to a few seconds, so a dead channel does not spin re-tuning every ~2 s.
    let mut idle_restarts = 0u32;
    let end = loop {
        if stopping() {
            break "the daemon stops";
        }
        if recalled() {
            break "the unit closes";
        }
        if pipe.closed.load(Ordering::Acquire) || streaming.client_closed() {
            break "the client went away";
        }
        match queued(jobs, &req, &name) {
            Queued::Nothing => {}
            Queued::Takeover(job) => {
                ended.takeover = Some(job);
                break "a new request takes the tuner over";
            }
            Queued::Stop => break "the daemon stops",
        }
        // the worker owns the tuner's I/O while streaming, so never touch it here. It ends
        // on its own on a capture fault or an unplug.
        if !t.raw_running() {
            let stopped = t.stream_stop_raw();
            if !present() {
                ended.unplugged = true;
                break "the unit was unplugged";
            }
            let bytes = match stopped {
                Ok(s) => s.bytes,
                Err(e) => {
                    warn!("raw-TS: the stream worker failed: {e}");
                    0
                }
            };
            if bytes == 0 {
                // No TS flowed (no lock / off air): back off (up to 5 s) so a dead channel
                // does not spin re-tuning and spamming the log every couple of seconds.
                idle_restarts = (idle_restarts + 1).min(5);
                thread::sleep(Duration::from_secs(u64::from(idle_restarts)));
            } else {
                idle_restarts = 0;
            }
            if stopping() {
                break "the daemon stops";
            }
            warn!("raw-TS: the stream worker ended ({bytes} bytes); restarting the channel");
            if let Err(why) = rawts::start(t, req.tuning, pipe.sink()) {
                warn!("{}: {why}", req.to_line());
                break "the worker ended and the restart failed";
            }
            stall = None;
            continue;
        }
        // a halt never faults the worker: the only stall signal is the byte count not
        // advancing. Restart the channel (stop + retune); a transient stall recovers.
        if raw_stall_due(
            &mut stall,
            pipe.bytes.load(Ordering::Relaxed),
            Instant::now(),
        ) {
            warn!("raw-TS: no TS for {RAW_STALL:?} while streaming; restarting the channel");
            if let Err(e) = t.stream_stop_raw() {
                // The stalled worker would not stop; rawts::start below would refuse it. End
                // the stream; a re-plug is the reset.
                warn!("raw-TS: could not stop the stalled worker ({e}); ending");
                break "the stalled worker would not stop";
            }
            if let Err(why) = rawts::start(t, req.tuning, pipe.sink()) {
                warn!("{}: {why}", req.to_line());
                break "the datapath stalled and the restart failed";
            }
            continue;
        }
        thread::sleep(POLL);
    };
    if let Err(e) = t.stream_stop_raw() {
        warn!("raw stream stop failed: {e}");
    }
    drop(streaming);
    if let Ok(writer) = writer {
        let _ = writer.join();
    }
    let bytes = pipe.bytes.load(Ordering::Relaxed);
    let dropped = pipe.dropped.load(Ordering::Relaxed);
    info!(
        "raw stream ended after {:.1} s ({end}): {bytes} bytes{}",
        t_stream.elapsed().as_secs_f64(),
        if dropped > 0 {
            format!(", {dropped} dropped (the client did not read in time)")
        } else {
            String::new()
        }
    );
    ended
}
