//! The blob-free raw-TS stream worker of a tuner ([`crate::Device`] for the first, or the one
//! [`crate::Device::set_tuner`] picked; [`crate::Tuner2`] for a two-tuner model's second).
//!
//! A worker runs [`dt295_rawfw::rawstream::run_cold_stream`] on its own thread. It takes the
//! device I/O one operation at a time (a pump call, an I2C transfer, a capture start), so the
//! two tuners' workers stream at once: the first tuner's TS through the first capture and EP
//! 0x81, the second's (demod output B) through the second capture and EP 0x86.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::error;

use dt295_core::fe1::FrontEnd;
use dt295_core::proto::Io;
use dt295_rawfw::IoAccess;
use dt295_rawfw::fjcore::Capture;

use crate::device::{LiveInner, Shared};
use crate::{Error, RawCaptureStats, Result, TsSink};

impl IoAccess for Shared {
    fn with_io<R>(&self, f: impl FnOnce(&Io) -> R) -> R {
        let mut st = self.lock();
        f(&Io::new(&self.usb, &mut st.proto))
    }

    fn usb(&self) -> &dt295_usb::Device {
        &self.usb
    }
}

/// Whether our monitor is on the chip: the unit was brought up for raw-TS since the plug
/// ([`crate::Device::cold_monitor_installed`]).
pub(crate) fn monitor_installed(sh: &Shared) -> bool {
    // A read error defaults to "installed" (skip the bring-up), not "cold": re-running the
    // once-per-plug full bring-up on a chip that is actually up faults it (needs a re-plug),
    // whereas skipping it when the chip really is cold just fails this tune cleanly and the
    // host retries. So the safe default on an unreadable probe is to not bring up again.
    IoAccess::with_io(sh, |io| {
        dt295_rawfw::frmon_host::monitor_installed(io).unwrap_or(true)
    })
}

/// A tune for a raw-TS stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawTune {
    pub terrestrial: bool,
    pub freq_hz: u32,
    pub tsid: Option<u16>,
    /// Do the once-per-plug bring-up if the chip is still cold.
    pub full: bool,
    /// A safety cap on the stream's length.
    pub duration: Duration,
}

/// A running raw-TS stream worker, forwarding TS to the sink until `stop` is cleared. Its
/// thread returns the sink and the capture result.
pub(crate) struct RawWorker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<(TsSink, Result<RawCaptureStats>)>,
    /// A stop already waited the whole time for it: the next one does not wait again.
    wedged: bool,
    /// The endpoint its TS comes in on (a last-resort nudge to a wedged worker).
    ep: u8,
}

impl fmt::Debug for RawWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawWorker")
            .field("stop", &self.stop.load(Ordering::Relaxed))
            .field("finished", &self.handle.is_finished())
            .field("wedged", &self.wedged)
            .field("ep", &self.ep)
            .finish()
    }
}

impl RawWorker {
    /// Start a worker that streams `fe`'s TS into `sink`, publishing its C/N to `live`.
    pub(crate) fn spawn(
        sh: Arc<Shared>,
        live: Arc<LiveInner>,
        fe: FrontEnd,
        tune: RawTune,
        sink: TsSink,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(true));
        let worker_stop = Arc::clone(&stop);
        // The caller's span (the daemon's unit, say) goes with the worker, so its log lines
        // say which unit they are about.
        let span = tracing::Span::current();
        let handle = thread::Builder::new()
            .name(format!("dt295-raw-tuner{}", fe.tuner() + 1))
            .spawn(move || {
                let _span = span.enter();
                let mut sink = sink;
                let cn_report = |mdb: i32| live.publish_cn(mdb);
                let result = dt295_rawfw::rawstream::run_cold_stream(
                    &*sh,
                    fe,
                    tune.terrestrial,
                    tune.freq_hz,
                    tune.tsid,
                    tune.full,
                    tune.duration,
                    Some(&worker_stop),
                    &mut sink,
                    Some(&cn_report),
                );
                (sink, result)
            })
            .map_err(Error::from)?;
        Ok(RawWorker {
            stop,
            handle,
            wedged: false,
            ep: Capture::of(fe).ep(),
        })
    }

    /// Whether its thread still runs.
    pub(crate) fn running(&self) -> bool {
        !self.handle.is_finished()
    }
}

/// Stop the worker in `slot` and return what it captured (or its fault); `live` is the C/N it
/// published. The stream loop polls the stop flag every pass (~1 ms, the EP read timeout) and
/// exits on its own, so no EP abort is needed to unstick it. A worker still in the
/// (uninterruptible) cold bring-up finishes that first and then sees the flag; this waits it
/// out rather than abort its EP mid-bring-up, which would fault the tune. A genuinely wedged
/// worker gets one EP nudge as a last resort; one that still will not exit stays in `slot`
/// with its own device reference (nothing is freed under it), a re-plug is the reset, and a
/// later stop gives that error at once if it still runs. Ok with nothing to stop.
pub(crate) fn stop(
    slot: &mut Option<RawWorker>,
    sh: &Shared,
    live: &LiveInner,
) -> Result<RawCaptureStats> {
    let Some(mut w) = slot.take() else {
        return Ok(RawCaptureStats::default());
    };
    if w.wedged && !w.handle.is_finished() {
        *slot = Some(w);
        return Err(Error::State("the raw-TS stream worker did not exit".into()));
    }
    w.stop.store(false, Ordering::Relaxed);
    let t0 = Instant::now();
    let mut nudged = false;
    loop {
        if w.handle.is_finished() {
            // The worker has exited: no more C/N samples, so clear the live value now (a
            // stopped stream must not report its last reading for the freshness window).
            live.reset_cn();
            return match w.handle.join() {
                Ok((_, r)) => r,
                Err(_) => {
                    error!("raw-TS worker panicked");
                    Err(Error::State("the raw-TS stream worker panicked".into()))
                }
            };
        }
        if !nudged && t0.elapsed() >= Duration::from_secs(25) {
            nudged = true;
            let _ = sh.usb.abort_pipe(w.ep);
        }
        if t0.elapsed() >= Duration::from_secs(35) {
            error!(
                "raw-TS worker did not exit within 35 s; leaving it running -- re-plug the device"
            );
            w.wedged = true;
            *slot = Some(w);
            return Err(Error::State("the raw-TS stream worker did not exit".into()));
        }
        thread::sleep(Duration::from_millis(10));
    }
}
