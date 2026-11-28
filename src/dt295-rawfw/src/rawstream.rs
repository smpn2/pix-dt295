//! Blob-free COLD raw-TS stream worker.
//!
//! [`run_cold_stream`] brings the chip up from cold, tunes and arms the front end's capture
//! over the host face (no resident and no vendor firmware; see
//! [`crate::fjcore::cold_prepare_stream`]). The on-chip ring goes to the capture's endpoint
//! (EP 0x81, or EP 0x86 for a two-tuner model's second tuner) through a unit's output
//! path ([`crate::hwpath`]), or failing that through our own monitor pump, and the worker
//! reframes the bytes into 188-byte TS packets for the sink, until stopped. The scrambled
//! TS is passed through untouched, to be descrambled by host software with a real card;
//! only the null packets are dropped.
//!
//! The worker takes the device I/O for one operation at a time ([`IoAccess`]): a pump call,
//! an I2C transfer of a tune or a C/N read, the capture start. So the workers of a
//! two-tuner model's two tuners share the unit and stream at once. The bulk IN reads need
//! no lock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::IoAccess;
use crate::fjcore::{Capture, Feed};
use dt295_core::fe1::FrontEnd;
use dt295_core::{Error, Result, TsSink};

/// One EP read buffer: 60 KiB, a multiple of the 512-byte bulk packet (the fallback
/// single-read path).
const BULK_READ_BYTES: usize = 61_440;
/// Reads kept posted on the EP.
///
/// WinUSB: 48 x 64 KiB = 3 MiB, about 0.6 s of a full CS transponder. They complete in
/// hardware (RAW_IO), so the EP keeps draining while the stream thread is held up (a C/N
/// read, the OS scheduling it late).
///
/// usbfs (Linux): few. The stream's end cancels every posted read, about 3 ms each on an
/// EHCI host (measured on a 4.9 kernel: 48 took ~150 ms), and while it runs the controller
/// holds back the control transfers of every device on it: another unit's pump among them.
/// 8 x 64 KiB cancel in ~25 ms and hold ~100 ms of a full CS transponder for the stream
/// thread to reap; past that the on-chip ring holds the TS.
const RQ_N: usize = cfg_select! {
    windows => 48,
    _ => 8,
};
const RQ_SIZE: usize = 0x10000;
/// How long to wait for the next EP chunk; also paces the pump.
const REAP_TIMEOUT: Duration = Duration::from_millis(1);

/// How often the loop pumps the ring to the EP.
const PUMP_EVERY: Duration = Duration::from_millis(2);
/// A full CS transponder, the fastest stream: about 4.9 MB/s (the demod drops the dummy
/// slots, S reg 0x8E bit 1). BS and terrestrial streams are slower.
const FULL_RATE: u64 = 4_900_000;
/// A pump interval this long is reported at once: what the on-chip ring holds of a full CS
/// transponder (4 MiB, ~0.85 s), so the ring has likely overflowed.
const OVERFLOW_PUMP: Duration =
    Duration::from_millis(dt295_frmon::RING_SIZE as u64 * 1000 / FULL_RATE);
/// A pump interval past this is counted as late: half of [`OVERFLOW_PUMP`].
const LATE_PUMP: Duration = Duration::from_millis(dt295_frmon::RING_SIZE as u64 * 500 / FULL_RATE);
/// How often the loop reports losses while it streams (only when there were any, see
/// [`RawCaptureStats::lossy`]: late pumps or damaged packets alone are no loss).
const LOSS_REPORT: Duration = Duration::from_secs(10);

/// What a capture delivered.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawCaptureStats {
    /// TS bytes handed to the sink (a multiple of 188).
    pub bytes: u64,
    /// 188-byte packets emitted.
    pub packets: u64,
    /// Null packets (PID 0x1FFF) dropped before the sink: the demodulator fills a terrestrial
    /// TS with them, nearly half of its packets.
    pub null_packets: u64,
    /// Continuity-counter gaps in the delivered TS: packets lost on the way (the on-chip
    /// ring overflowing while the pump came late), or one after a damaged packet (which is
    /// not checked, so the next packet on its PID shows a gap).
    pub cc_gaps: u64,
    /// Packets the demodulator flagged as damaged (transport_error_indicator): a weak
    /// signal, not a late pump.
    pub error_packets: u64,
    /// Pump intervals longer than `LATE_PUMP` (~0.4 s).
    pub late_pumps: u64,
    /// The longest interval between two pumps.
    pub max_pump_gap: Duration,
}

impl RawCaptureStats {
    /// Whether TS was, or likely was, lost on the way: more continuity gaps than the damaged
    /// packets explain (each leaves up to one gap), or a pump interval long enough to
    /// overflow the ring. A weak signal alone, damaged packets and their gaps, is not.
    fn lossy(&self) -> bool {
        self.cc_gaps > self.error_packets || self.max_pump_gap >= OVERFLOW_PUMP
    }

    /// One line on the losses, for the logs (the pump intervals only when a pump ran: the
    /// unit's output path has none).
    fn loss_line(&self) -> String {
        let packets = format!(
            "{} continuity gaps, {} damaged packets",
            self.cc_gaps, self.error_packets
        );
        if self.late_pumps == 0 && self.max_pump_gap.is_zero() {
            return packets;
        }
        format!(
            "{packets}, {} late pumps (longest interval {} ms)",
            self.late_pumps,
            self.max_pump_gap.as_millis()
        )
    }
}

/// The raw-TS worker's duration cap, effectively unbounded (a channel change or the client
/// going away stops it first). Shared by the BonDriver and the daemon.
pub const RAW_STREAM_CAP: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// How long to wait for the first TS after starting a raw-TS stream: past the cold bring-up
/// (about 1 s before the tune) and the lock wait (up to ~8 s on a satellite). It MUST
/// exceed both, or stopping the wait mid-bring-up aborts the pre-tune EP queue and faults
/// the tune.
pub const RAW_FIRST_TS: Duration = Duration::from_secs(30);
/// How long a running raw-TS stream may deliver no TS before it is a stalled datapath. A
/// halt never faults the worker, so the received-byte count is the only liveness signal.
pub const RAW_STALL: Duration = Duration::from_secs(8);

/// Advance the raw-TS liveness tracker and say whether a stall restart is due. `state` holds
/// the last received-byte count and when it last advanced; `rx` is the current count, `now`
/// the current time. Returns true once the stream has delivered nothing for at least
/// [`RAW_STALL`], clearing `state` so the restart starts from a fresh window. Shared by the
/// BonDriver idle keeper and the daemon's stream loop.
pub fn raw_stall_due(state: &mut Option<(u64, Instant)>, rx: u64, now: Instant) -> bool {
    if state.is_none_or(|(last, _)| rx > last) {
        *state = Some((rx, now));
        false
    } else if state.is_some_and(|(_, since)| now.duration_since(since) >= RAW_STALL) {
        *state = None;
        true
    } else {
        false
    }
}

/// A complete TS packet is 188 bytes; the capture appends a 4-byte suffix, so one record off
/// the ring is 192 bytes and the sink gets its first 188.
const TS_PACKET: usize = 188;
const TS_RECORD: usize = 192;
const TS_SYNC: u8 = 0x47;

/// Reassembles the byte stream off EP 0x81 into 188-byte TS packets, re-synchronizing on the
/// 0x47 sync byte. Our records are 192 bytes (a 188-byte packet followed by a 4-byte suffix
/// we drop).
struct ColdReframer {
    buf: Vec<u8>,
    synced: bool,
}

impl ColdReframer {
    fn new() -> Self {
        ColdReframer {
            buf: Vec::with_capacity(TS_RECORD * 4),
            synced: false,
        }
    }

    /// Scan `buf[start..]` for two in-phase sync bytes one record apart; return the offset of
    /// the first, or None.
    fn sync_at(&self, start: usize) -> Option<usize> {
        let mut off = start;
        while off + TS_RECORD * 2 <= self.buf.len() {
            if self.buf[off] == TS_SYNC && self.buf[off + TS_RECORD] == TS_SYNC {
                return Some(off);
            }
            off += 1;
        }
        None
    }

    /// Feed one bulk chunk; append each complete packet's 188 bytes to `out`. Consumes the
    /// buffer with a single `drain` at the end, advancing a cursor as it goes, so a chunk costs
    /// linear time (a per-packet `drain` would move the whole tail each time: quadratic).
    fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        self.buf.extend_from_slice(data);
        let mut pos = 0usize;
        loop {
            if !self.synced {
                match self.sync_at(pos) {
                    Some(p) => {
                        pos = p;
                        self.synced = true;
                    }
                    None => {
                        // No aligned pair in buf[pos..]; keep only the tail that may still
                        // begin one (the last 2*record-1 bytes).
                        let keep = TS_RECORD * 2 - 1;
                        if self.buf.len() - pos > keep {
                            pos = self.buf.len() - keep;
                        }
                        break;
                    }
                }
            }
            if self.buf.len() - pos < TS_RECORD {
                break;
            }
            if self.buf[pos] != TS_SYNC {
                // Lost alignment: drop one byte and search again.
                self.synced = false;
                pos += 1;
                continue;
            }
            out.extend_from_slice(&self.buf[pos..pos + TS_PACKET]);
            pos += TS_RECORD;
        }
        self.buf.drain(..pos);
    }
}

/// Counts continuity-counter gaps in the TS, per PID: the one sign, on the host, that
/// packets were lost before they reached it.
struct CcCheck {
    /// The last continuity counter seen per PID, or [`CcCheck::NONE`].
    last: Vec<u8>,
}

impl CcCheck {
    const NONE: u8 = 0xff;

    fn new() -> Self {
        CcCheck {
            last: vec![Self::NONE; 0x2000],
        }
    }

    /// Check each 188-byte packet of `ts`; add the gaps and the damaged packets to `stats`.
    fn check(&mut self, ts: &[u8], stats: &mut RawCaptureStats) {
        for p in ts.as_chunks::<TS_PACKET>().0 {
            if p[1] & 0x80 != 0 {
                // transport_error_indicator: the demodulator could not correct it, so even
                // its PID may be wrong. Counted on its own, not checked.
                stats.error_packets += 1;
                continue;
            }
            let pid = (usize::from(p[1] & 0x1f) << 8) | usize::from(p[2]);
            let control = (p[3] >> 4) & 3;
            if pid == usize::from(dt295_core::ts::NULL_PID) || control & 1 == 0 {
                continue; // null packets, and packets without payload: the counter does not count
            }
            let cc = p[3] & 0x0f;
            // An adaptation field's discontinuity_indicator announces a jump.
            let discontinuity = control == 3 && p[4] > 0 && p[5] & 0x80 != 0;
            let prev = self.last[pid];
            // The same counter again is a duplicate packet, which the standard allows once.
            if prev != Self::NONE && !discontinuity && cc != prev && cc != (prev + 1) & 0x0f {
                stats.cc_gaps += 1;
            }
            self.last[pid] = cc;
        }
    }
}

/// Blob-free COLD raw-TS stream: host bring-up + tune + capture arm (no resident and no vendor
/// firmware; [`crate::fjcore::cold_prepare_stream`]), then take the ring of `fe`'s capture to
/// its EP (the unit's output path, or the pump) and reframe to the sink until stopped. `full` does the one-time cold
/// bring-up (the first tune after a re-plug / open); a channel change passes `full` = false
/// and only retunes + rearms. `duration` is a safety cap; the worker runs until `stop`.
#[allow(clippy::too_many_arguments)]
pub fn run_cold_stream<L: IoAccess>(
    l: &L,
    fe: FrontEnd,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    full: bool,
    duration: Duration,
    stop: Option<&AtomicBool>,
    sink: &mut TsSink,
    cn_report: Option<&dyn Fn(i32)>,
) -> Result<RawCaptureStats> {
    let (status, feed) =
        crate::fjcore::cold_prepare_stream(l, fe, terrestrial, freq_hz, tsid, full)?;
    let locked = status.locked;
    if locked && let Some(report) = cn_report {
        // The C/N of the lock, at once: the first periodic read is a second away, and the host
        // may want a level as soon as the TS flows (the daemon's reply, say).
        report(status.cn_mdb);
    }
    info!(
        "raw-TS cold: tuner {} {} at {:.3} MHz",
        fe.tuner() + 1,
        if locked { "locked" } else { "NO LOCK" },
        f64::from(freq_hz) / 1e6
    );
    let cap = Capture::of(fe);
    let streamed = if locked {
        pump_stream(l, fe, terrestrial, feed, duration, stop, sink, cn_report)
    } else {
        // An unlocked demod produces no TS, so end now rather than pump an empty ring for
        // the host's whole first-TS timeout: an off-air / empty channel then reports no
        // stream in the ~2 s of a tune, not ~30 s.
        Ok(RawCaptureStats::default())
    };
    // Stop the engine the arm started, so the next arm starts from a stopped one. Not after
    // an unplug: the device is gone.
    let unplugged = matches!(&streamed, Err(Error::State(why)) if why == UNPLUGGED);
    if !unplugged && let Err(e) = l.with_io(|io| crate::fjcore::stop_capture(io, cap)) {
        warn!("raw-TS cold: capture stop: {e}");
    }
    streamed
}

const UNPLUGGED: &str = "device unplugged during cold stream";

/// The stream loop of [`run_cold_stream`], once the demod has locked: pump the ring to the
/// capture's EP (unless the unit's output path feeds it) and reframe to the sink until `stop`
/// (or `duration`).
#[allow(clippy::too_many_arguments)]
fn pump_stream<L: IoAccess>(
    l: &L,
    fe: FrontEnd,
    terrestrial: bool,
    feed: Feed,
    duration: Duration,
    stop: Option<&AtomicBool>,
    sink: &mut TsSink,
    cn_report: Option<&dyn Fn(i32)>,
) -> Result<RawCaptureStats> {
    let cap = Capture::of(fe);
    let ep = cap.ep();
    let usb = l.usb();
    let _ = usb.abort_pipe(ep);
    // Past the bring-up, from here on a late pump loses TS (the module doc of `prio`).
    match crate::prio::raise_stream_threads() {
        Ok(got) => info!("raw-TS cold: stream thread at {got}"),
        // Not a warning: the shipped unit grants no real-time limits, and a capable host keeps
        // up at the normal priority. One that falls behind shows it in the loss reports below.
        Err(why) => info!("raw-TS cold: the stream keeps the normal priority ({why})"),
    }
    let mut queue = usb.read_queue(ep, RQ_N, RQ_SIZE).ok();
    let mut seq = 0;
    let mut reframer = ColdReframer::new();
    let mut cc = CcCheck::new();
    let mut fallback = vec![0u8; BULK_READ_BYTES];
    let mut out: Vec<u8> = Vec::with_capacity(RQ_SIZE);
    let mut stats = RawCaptureStats::default();
    let start = Instant::now();
    let mut last_cn = Instant::now();
    let mut last_pump = Instant::now() - Duration::from_millis(10);
    // When the previous pump started (the intervals are measured start to start).
    let mut pump_started: Option<Instant> = None;
    // The loss report: the totals at the last one, and the longest pump interval since.
    let mut last_report = Instant::now();
    let mut reported = RawCaptureStats::default();
    let mut window_max_gap = Duration::ZERO;
    // When a long pump interval was last reported (at most once a second).
    let mut last_overflow_warn: Option<Instant> = None;
    const CN_INTERVAL: Duration = Duration::from_millis(1000);
    while start.elapsed() < duration {
        if stop.is_some_and(|s| !s.load(Ordering::Relaxed)) {
            info!("raw-TS cold: stop requested; ending");
            break;
        }
        // Keep the FIFO fed, but cap the pump rate: one pump drains up to 32 KiB, so ~2 ms
        // between pumps (16 MB/s) is well above the ~5 MB/s of a full CS transponder, while
        // cutting the control-transfer traffic of pumping on every (sub-millisecond) loop
        // pass. Each interval is timed: one past what the ring holds (~0.85 s) loses TS.
        if feed == Feed::Pump && last_pump.elapsed() >= PUMP_EVERY {
            let now = Instant::now();
            if let Some(prev) = pump_started {
                let gap = now - prev;
                if gap > LATE_PUMP {
                    stats.late_pumps += 1;
                }
                if gap >= OVERFLOW_PUMP
                    && last_overflow_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
                {
                    warn!(
                        "raw-TS cold: no pump for {} ms: the ring may have overflowed (TS lost)",
                        gap.as_millis()
                    );
                    last_overflow_warn = Some(now);
                }
                stats.max_pump_gap = stats.max_pump_gap.max(gap);
                window_max_gap = window_max_gap.max(gap);
            }
            pump_started = Some(now);
            let _ = l.with_io(|io| crate::frmon_host::call_pump(io, cap.pump(), &mut seq));
            last_pump = Instant::now();
        }
        match queue.as_mut() {
            Some(q) => match q.next(REAP_TIMEOUT) {
                Ok(Some(data)) => {
                    frame_chunk(&mut reframer, &mut cc, sink, data, &mut out, &mut stats)
                }
                Ok(None) => {}
                Err(dt295_usb::Error::Timeout) | Err(dt295_usb::Error::Cancelled) => {}
                Err(dt295_usb::Error::NoDevice) => {
                    return Err(Error::State(UNPLUGGED.into()));
                }
                Err(e) => return Err(Error::Usb(e)),
            },
            None => match usb.read(ep, &mut fallback, REAP_TIMEOUT) {
                Ok(n) if n > 0 => {
                    let got = fallback[..n].to_vec();
                    frame_chunk(&mut reframer, &mut cc, sink, &got, &mut out, &mut stats);
                }
                Ok(_) => {}
                Err(dt295_usb::Error::Timeout) | Err(dt295_usb::Error::Cancelled) => {}
                Err(dt295_usb::Error::NoDevice) => {
                    return Err(Error::State(UNPLUGGED.into()));
                }
                Err(e) => return Err(Error::Usb(e)),
            },
        }
        if let Some(report) = cn_report
            && last_cn.elapsed() >= CN_INTERVAL
        {
            last_cn = Instant::now();
            if let Ok(cn_mdb) = l.with_io(|io| crate::fjcore::cold_cn(io, fe, terrestrial)) {
                report(cn_mdb);
            }
        }
        // Losses are otherwise silent: say so while it streams, but only when there were any.
        if last_report.elapsed() >= LOSS_REPORT {
            let window = RawCaptureStats {
                cc_gaps: stats.cc_gaps - reported.cc_gaps,
                error_packets: stats.error_packets - reported.error_packets,
                late_pumps: stats.late_pumps - reported.late_pumps,
                max_pump_gap: window_max_gap,
                ..RawCaptureStats::default()
            };
            if window.lossy() {
                warn!(
                    "raw-TS cold: in the last {} s: {}",
                    LOSS_REPORT.as_secs(),
                    window.loss_line()
                );
            }
            reported = stats;
            window_max_gap = Duration::ZERO;
            last_report = Instant::now();
        }
    }
    if feed == Feed::Unit
        && let Some(q) = queue.as_mut()
    {
        settle(l, cap, q);
    }
    stats.packets = stats.bytes / TS_PACKET as u64;
    let summary = format!(
        "raw-TS cold: streamed {:.1} s, {} packets ({} null packets dropped): {}",
        start.elapsed().as_secs_f64(),
        stats.packets,
        stats.null_packets,
        stats.loss_line()
    );
    if stats.lossy() {
        warn!("{summary}");
    } else {
        info!("{summary}");
    }
    Ok(stats)
}

/// Longest wait for the endpoint to go quiet at a stream's end, and the quiet that counts.
const SETTLE_MAX: Duration = Duration::from_millis(200);
const SETTLE_QUIET: Duration = Duration::from_millis(20);

/// At the end of a stream on the unit's output path: stop the feeder and read off what it
/// already sent (discarded) before the posted reads are cancelled. On a Linux EHCI host,
/// cancelling reads while the endpoint still sends cost a few packets of another unit's
/// stream (a few CC gaps there when this one stops); the pump's endpoint is idle by then.
fn settle<L: IoAccess>(l: &L, cap: Capture, q: &mut dt295_usb::ReadQueue<'_>) {
    if let Err(e) = l.with_io(|io| crate::hwpath::pause(io, cap)) {
        warn!("raw-TS cold: unit path pause: {e}");
        return;
    }
    let started = Instant::now();
    let mut last = Instant::now();
    while started.elapsed() < SETTLE_MAX && last.elapsed() < SETTLE_QUIET {
        if let Ok(Some(_)) = q.next(REAP_TIMEOUT) {
            last = Instant::now();
        }
    }
}

static RAW_DUMPED: AtomicBool = AtomicBool::new(false);

/// Log the first bulk chunk once (head bytes + the 0x47 stride histogram), for diagnosing a
/// stream whose record framing is off.
fn raw_dump_once(data: &[u8]) {
    if data.len() < 1024 || RAW_DUMPED.swap(true, Ordering::Relaxed) {
        return;
    }
    let head: String = data[..64].iter().map(|b| format!("{b:02x} ")).collect();
    info!("raw-dump: first chunk {} B, head64: {head}", data.len());
    // Distances between consecutive 0x47 bytes over the first ~8 KiB.
    let scan = &data[..data.len().min(8192)];
    let mut prev: Option<usize> = None;
    let mut hist: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();
    let mut total = 0u32;
    for (i, &b) in scan.iter().enumerate() {
        if b == 0x47 {
            if let Some(p) = prev {
                *hist.entry(i - p).or_default() += 1;
                total += 1;
            }
            prev = Some(i);
        }
    }
    let mut top: Vec<(usize, u32)> = hist.into_iter().collect();
    top.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    let summary: String = top
        .iter()
        .take(8)
        .map(|(stride, n)| format!("{stride}:{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    info!(
        "raw-dump: 0x47 gaps over {} B ({total} gaps) top stride:count = {summary}",
        scan.len()
    );
}

/// Reframe one bulk chunk into 188-byte packets, check their continuity and forward them
/// to the sink without the null packets, which carry nothing (the counters are checked
/// first, so a damaged packet is counted whatever PID it reads).
fn frame_chunk(
    reframer: &mut ColdReframer,
    cc: &mut CcCheck,
    sink: &mut TsSink,
    data: &[u8],
    out: &mut Vec<u8>,
    stats: &mut RawCaptureStats,
) {
    raw_dump_once(data);
    out.clear();
    reframer.feed(data, out);
    if !out.is_empty() {
        cc.check(out, stats);
        let kept = dt295_core::ts::drop_null(out);
        stats.null_packets += ((out.len() - kept) / TS_PACKET) as u64;
        out.truncate(kept);
    }
    if !out.is_empty() {
        sink(out);
        stats.bytes += out.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 188-byte TS packet whose byte 1 carries `n`, so a test can tell packets apart.
    fn packet(n: u8) -> Vec<u8> {
        let mut p = vec![0u8; TS_PACKET];
        p[0] = TS_SYNC;
        p[1] = n;
        p
    }

    /// One 192-byte record: a packet plus a 4-byte suffix.
    fn record(n: u8) -> Vec<u8> {
        let mut r = packet(n);
        r.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        r
    }

    fn feed_all(rx: &mut ColdReframer, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        rx.feed(data, &mut out);
        out
    }

    #[test]
    fn emits_each_packet_dropping_the_suffix() {
        let mut rx = ColdReframer::new();
        let mut stream = Vec::new();
        for n in 0..5u8 {
            stream.extend_from_slice(&record(n));
        }
        let out = feed_all(&mut rx, &stream);
        assert_eq!(out.len(), TS_PACKET * 5);
        for (i, pkt) in out.chunks(TS_PACKET).enumerate() {
            assert_eq!(pkt[0], TS_SYNC);
            assert_eq!(pkt[1], i as u8, "packet order preserved, suffix dropped");
        }
    }

    #[test]
    fn reassembles_byte_at_a_time() {
        let mut rx = ColdReframer::new();
        let mut stream = Vec::new();
        for n in 0..6u8 {
            stream.extend_from_slice(&record(n));
        }
        let mut out = Vec::new();
        for b in &stream {
            rx.feed(std::slice::from_ref(b), &mut out);
        }
        assert_eq!(out.len(), TS_PACKET * 6);
    }

    #[test]
    fn resyncs_after_a_garbage_prefix() {
        let mut rx = ColdReframer::new();
        let mut stream = vec![0x11, 0x22, 0x33]; // not sync-aligned
        for n in 0..4u8 {
            stream.extend_from_slice(&record(n));
        }
        let out = feed_all(&mut rx, &stream);
        // The garbage prefix is dropped; the four aligned packets come through.
        assert_eq!(out.len(), TS_PACKET * 4);
        assert_eq!(out[1], 0);
    }

    /// A TS packet on `pid` with continuity counter `cc`, carrying a payload.
    fn ts(pid: u16, cc: u8) -> Vec<u8> {
        let mut p = vec![0xffu8; TS_PACKET];
        p[0] = TS_SYNC;
        p[1] = (pid >> 8) as u8 & 0x1f;
        p[2] = pid as u8;
        p[3] = 0x10 | (cc & 0x0f); // payload only
        p
    }

    fn gaps_of(packets: &[Vec<u8>]) -> RawCaptureStats {
        let mut cc = CcCheck::new();
        let mut stats = RawCaptureStats::default();
        cc.check(&packets.concat(), &mut stats);
        stats
    }

    #[test]
    fn continuity_counts_only_real_gaps() {
        // In order, across the 15 -> 0 wrap, on two interleaved PIDs: no gap.
        let ok: Vec<_> = (0..20u8)
            .flat_map(|n| [ts(0x100, n), ts(0x200, n)])
            .collect();
        assert_eq!(gaps_of(&ok).cc_gaps, 0);
        // A duplicate (the same counter twice) is allowed.
        assert_eq!(
            gaps_of(&[ts(0x100, 3), ts(0x100, 3), ts(0x100, 4)]).cc_gaps,
            0
        );
        // Two packets lost: one gap.
        assert_eq!(
            gaps_of(&[ts(0x100, 3), ts(0x100, 6), ts(0x100, 7)]).cc_gaps,
            1
        );
        // Null packets and payload-less packets do not count.
        let mut no_payload = ts(0x100, 9);
        no_payload[3] = 0x20 | 9; // adaptation field only
        no_payload[4] = 0;
        let s = gaps_of(&[ts(0x100, 1), ts(0x1fff, 7), no_payload, ts(0x100, 2)]);
        assert_eq!(s.cc_gaps, 0);
    }

    #[test]
    fn continuity_honours_discontinuity_and_damaged_packets() {
        // discontinuity_indicator set: a jump is announced, not a loss.
        let mut jump = ts(0x100, 9);
        jump[3] = 0x30 | 9; // adaptation field + payload
        jump[4] = 1;
        jump[5] = 0x80;
        assert_eq!(gaps_of(&[ts(0x100, 1), jump, ts(0x100, 10)]).cc_gaps, 0);
        // A damaged packet (transport_error_indicator) is counted, not checked.
        let damaged = |cc| {
            let mut p = ts(0x100, cc);
            p[1] |= 0x80;
            p
        };
        let s = gaps_of(&[ts(0x100, 1), damaged(5), ts(0x100, 2)]);
        assert_eq!((s.cc_gaps, s.error_packets), (0, 1));
        // So the packet after one that carried the next counter shows a gap: a weak signal,
        // which alone is no loss.
        let s = gaps_of(&[ts(0x100, 1), damaged(2), ts(0x100, 3)]);
        assert_eq!((s.cc_gaps, s.error_packets), (1, 1));
        assert!(!s.lossy());
        // More gaps than the damaged packets explain: lost.
        let s = gaps_of(&[ts(0x100, 1), damaged(2), ts(0x100, 3), ts(0x100, 6)]);
        assert_eq!((s.cc_gaps, s.error_packets), (2, 1));
        assert!(s.lossy());
    }

    #[test]
    fn raw_stall_restarts_only_after_the_window() {
        let secs = Duration::from_secs;
        let t0 = Instant::now();
        let mut state = None;
        // First observation arms the tracker; a fresh stream never counts as stalled.
        assert!(!raw_stall_due(&mut state, 0, t0));
        // Bytes flowing re-arm the window each tick.
        assert!(!raw_stall_due(&mut state, 1000, t0 + secs(5)));
        assert!(!raw_stall_due(&mut state, 2000, t0 + secs(20)));
        // Stalled since t=20s, but still inside RAW_STALL (8s): no restart.
        assert!(!raw_stall_due(&mut state, 2000, t0 + secs(25)));
        // Still stalled, now past the window: restart, and the tracker is cleared.
        assert!(raw_stall_due(&mut state, 2000, t0 + secs(28)));
        assert_eq!(state, None);
        // After a restart it re-arms and waits a full window again before firing.
        assert!(!raw_stall_due(&mut state, 2000, t0 + secs(29)));
        assert!(!raw_stall_due(&mut state, 2000, t0 + secs(36)));
        assert!(raw_stall_due(&mut state, 2000, t0 + secs(37)));
    }
}
