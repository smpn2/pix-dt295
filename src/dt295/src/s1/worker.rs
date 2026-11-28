//! The stream worker thread: queued EP 0x81 reads, resync, header statistics, the
//! link-key check and link decryption, PAT counting, the PSI keeper, the route slots of
//! an all-services stream, firmware-event polling, the keepalive, the 1 Hz demod sample,
//! the per-minute statistics line.
//!
//! The worker does all device I/O while it runs. It never holds the state lock across
//! a transfer: per buffer it locks [`State`](crate::device::State) for the pure
//! processing only, and for its I/O phase it works on a copy of the protocol state that
//! it stores back afterwards (nobody else touches it meanwhile: the host does no device
//! I/O while a worker runs).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dt295_usb::ReadQueue;
use tracing::{debug, error, info, warn};

use super::{
    CARD_FAILED, CARD_WORKS, EP_TS, KA_INTERVAL, bcas_state, is_pid0, ka_tick, link_decrypt,
    route_write, service_events,
};
use crate::device::{Shared, State};
use crate::ts::es::EsCheck;
use crate::ts::psi::StreamView;
use crate::ts::route::{Op, Router};
use crate::ts::stats::RawStats;
use crate::ts::{link, packet, pat};
use dt295_core::fe1::{self, FrontEnd};
use dt295_core::proto::{Io, sleep_ms};
use dt295_core::signal::{cn_s_mdb, cn_t_mdb};

use crate::{Result, System, TsSink};

/// EP 0x81 reads kept posted. Between two reads the worker services firmware events
/// (~10 control transfers) and, every 12 s, the keepalive RPC; with a single synchronous
/// read nothing was posted meanwhile, so the device had to buffer the TS itself, and a
/// heavy HD service (CS ショップチャンネル, ~10.5 Mbit/s) showed drops in TVTest.
/// 16 x 64 KiB keeps the host receiving across those gaps.
const RQ_N: usize = 16;
const RQ_SIZE: usize = 0x10000;
/// One read + the < 4-packet resync leftover.
const ACC_CAP: usize = RQ_SIZE + 1024;
const READ_WAIT: Duration = Duration::from_millis(200);
/// Firmware events while streaming: polled at most this often.
const EVENT_EVERY: Duration = Duration::from_millis(100);
/// How often the ES are judged descrambled or not ([`EsCheck`]).
const ES_EVERY: Duration = Duration::from_secs(2);
/// When the B-CAS state is logged once.
const BCAS_LOG_AFTER: Duration = Duration::from_secs(3);
/// Route slot writes per pass of the loop: each is a mailbox write and a doorbell, and
/// the reads must keep up with the TS meanwhile.
const ROUTE_BATCH: usize = 4;
/// Failed route writes after which the routes are left as they are.
const ROUTE_FAILS: u32 = 5;
/// ECM routes written at once; each later one waits [`ECM_GAP`] after the one before.
/// The more ECMs a stream takes up at once, the more often the firmware stopped acking
/// within seconds (CS110's ND06, 30 s and 10 min runs: 4 ECMs 0 of 34, 5: 1 of 20,
/// 7: 12 of 30, 8: about 40 of 90; 8 with 0.25-1 s between them: 2 of 34). A stream
/// past that streamed on (10 min runs; hardware 2026-10-30/31). 4 at once, then one
/// per 500 ms: 1 of 60 (ND06, 2026-10-31).
const ECMS_AT_ONCE: u32 = 4;
const ECM_GAP: Duration = Duration::from_millis(500);
const P: usize = packet::PACKET;

/// What a stream delivers.
#[derive(Debug)]
pub(crate) enum Output {
    /// Every service of the TS, on these routes (kept up with its PAT / PMTs).
    All(Box<Router>),
    /// One service; nothing is handed on before a PAT lists it.
    Single(u16),
    /// The PSI of a TS whose service delivers nothing (the fallback, full-mux).
    Psi(u16),
}

impl Output {
    /// The service the stream is tuned to.
    fn program(&self) -> Option<u16> {
        match self {
            Output::All(_) => None,
            Output::Single(p) | Output::Psi(p) => Some(*p),
        }
    }
}

/// What the worker needs from the host at start.
#[derive(Debug)]
pub(crate) struct Config {
    /// The tuned system and front end (the demod to sample).
    pub sys: Option<System>,
    pub fe: FrontEnd,
    pub output: Output,
}

/// A running worker. Its thread returns the sink when it ends.
#[derive(Debug)]
pub(crate) struct Worker {
    handle: JoinHandle<TsSink>,
}

/// How a stop went.
pub(crate) enum Joined {
    /// The worker exited; its sink is back.
    Done(TsSink),
    /// The worker panicked (a bug): it is gone, and so is the sink.
    Lost,
    /// The worker did not exit within 20 s; it was left running (detached).
    Stuck,
}

/// Start the worker on `sh` (the stream must be running in the device).
pub(crate) fn spawn(sh: Arc<Shared>, cfg: Config, sink: TsSink) -> Result<Worker> {
    sh.live.set_streaming(true);
    let live = Arc::clone(&sh.live);
    let spawned = thread::Builder::new()
        .name("dt295-s1-worker".into())
        .spawn(move || {
            let mut sink = sink;
            body(&sh, cfg, &mut sink);
            sink
        });
    match spawned {
        Ok(handle) => Ok(Worker { handle }),
        Err(e) => {
            live.set_streaming(false);
            Err(e.into())
        }
    }
}

/// Stop the worker. Unstick it if it is blocked inside an EP 0x81 read on a wedged
/// device (without this the read can outlast its timeout and the handle closes with a
/// transfer pending -- the next open then fails "access denied"). Re-abort each second:
/// the worker may start a new read before it sees the stop. A healthy worker exits
/// within one 200 ms read; a keepalive on a wedged firmware can take ~10 s (two type-8
/// ack timeouts), hence 20 s. A worker that still runs then is left alone (never freed
/// under it; it keeps its own reference to the device).
pub(crate) fn join(sh: &Shared, w: Worker) -> Joined {
    sh.live.set_streaming(false);
    for _ in 0..20 {
        let _ = sh.usb.abort_pipe(EP_TS);
        let t0 = Instant::now();
        while !w.handle.is_finished() && t0.elapsed() < Duration::from_secs(1) {
            sleep_ms(10);
        }
        if w.handle.is_finished() {
            // The worker has exited: no more C/N samples, so clear the live value now (a
            // stopped stream falls back to the tune-time level, not its last reading).
            sh.live.reset_cn();
            return match w.handle.join() {
                Ok(sink) => Joined::Done(sink),
                Err(_) => {
                    error!("s1 worker panicked");
                    Joined::Lost
                }
            };
        }
    }
    sh.live.abandon();
    error!(
        "s1 worker did not exit within 20 s; leaving it allocated (no use-after-free) -- re-plug \
         the device"
    );
    Joined::Stuck
}

/// How many bytes of `buf` (synced: a packet starts at 0) to deliver, and whether the
/// packet grid broke. A packet start without the sync byte means bytes were lost or
/// added, e.g. where TS left over from an earlier stream ends mid-packet and the new
/// stream begins (Linux VM 2026-10-24: 24 bytes off, and the rest of that 64 KiB read
/// went out misaligned, its link-encrypted packets undecrypted). Then only the packets
/// before the one cut short there are delivered.
fn deliverable(buf: &[u8]) -> (usize, bool) {
    let whole = buf.len() - buf.len() % P;
    match (0..whole).step_by(P).find(|&i| buf[i] != packet::SYNC) {
        Some(bad) => (bad.saturating_sub(P), true),
        None => (whole, false),
    }
}

/// An ECM's route: it names its own slot.
fn is_ecm_route(op: &Op) -> bool {
    op.ecm_slot == op.slot
}

/// The first queued route write that may go now. While an ECM waits its gap, the
/// ECMs, and the writes to their slots or bound to them, wait with it (in order); the
/// rest goes ahead.
fn next_route(q: &VecDeque<Op>, ecm_waits: bool) -> Option<usize> {
    if !ecm_waits {
        return (!q.is_empty()).then_some(0);
    }
    let mut held: Vec<u8> = Vec::new();
    for (i, op) in q.iter().enumerate() {
        if is_ecm_route(op) || held.contains(&op.slot) || held.contains(&op.ecm_slot) {
            held.push(op.slot);
        } else {
            return Some(i);
        }
    }
    None
}

/// What the PSI keeper needs to know about the stream.
fn view(st: &State, program: Option<u16>, delivered_any: bool) -> StreamView {
    StreamView {
        program,
        delivered_any,
        // an undecryptable stream (wrong link key) is not starved of PSI
        undecryptable: st.link.on && !st.link.verified && st.link.fails > 0,
    }
}

/// The per-minute statistics: continuity errors in what the DEVICE delivered (before
/// anything is added), TEI, gaps between reads, bytes per second, and the demod
/// samples. Logged only for a minute worth a look, so a long run shows when (and on
/// which PIDs) the TS was damaged.
struct Minute {
    no: u32,
    t0: Instant,
    err: u64,
    bytes: u64,
    gap_ms: u64,
    tei: u64,
    near_ka: u64,
    /// Bit s: a continuity error in second s of the minute.
    secs: u64,
    pids: Vec<u16>,
    /// Bytes delivered per second of the minute: does the output sit at a ceiling while
    /// the TS is damaged (a rate limit inside the device), or not?
    sec_bytes: [u64; 64],
    eb: u64,
    f0: u64,
    eb_max: u32,
    f0_max: u32,
    /// C/N range in 0.1 dB.
    cn: Option<(i32, i32)>,
    unlock: u64,
    /// Bit s: a demod error counter non-zero in second s.
    dm_secs: u64,
}

impl Minute {
    fn new(no: u32, t0: Instant) -> Self {
        Minute {
            no,
            t0,
            err: 0,
            bytes: 0,
            gap_ms: 0,
            tei: 0,
            near_ka: 0,
            secs: 0,
            pids: Vec::new(),
            sec_bytes: [0; 64],
            eb: 0,
            f0: 0,
            eb_max: 0,
            f0_max: 0,
            cn: None,
            unlock: 0,
            dm_secs: 0,
        }
    }

    fn sec(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.t0).as_secs()
    }

    fn cc_error(&mut self, pid: u16, near_ka: bool) {
        self.err += 1;
        let sec = self.sec(Instant::now());
        if sec < 64 {
            self.secs |= 1 << sec;
        }
        if near_ka {
            self.near_ka += 1;
        }
        if !self.pids.contains(&pid) && self.pids.len() < 4 {
            self.pids.push(pid);
        }
    }

    fn demod(&mut self, now: Instant, locked: bool, cnx: i32, eb: u32, f0: u32) {
        if !locked {
            self.unlock += 1;
        }
        self.cn = Some(match self.cn {
            Some((lo, hi)) => (lo.min(cnx), hi.max(cnx)),
            None => (cnx, cnx),
        });
        self.eb += u64::from(eb);
        self.f0 += u64::from(f0);
        self.eb_max = self.eb_max.max(eb);
        self.f0_max = self.f0_max.max(f0);
        let sec = self.sec(now);
        if (eb != 0 || f0 != 0) && sec < 64 {
            self.dm_secs |= 1 << sec;
        }
    }

    /// The minute's log line, if it is worth one: TS damage, TEI, or reads pausing
    /// (> 100 ms; the 12 s keepalive alone accounts for ~80 ms). A minute with only C/N
    /// samples is not logged (C/N is normal data).
    fn line(&self, dm_sat: bool) -> Option<String> {
        if self.err == 0
            && self.tei == 0
            && self.gap_ms <= 100
            && self.eb == 0
            && self.f0 == 0
            && self.unlock == 0
        {
            return None;
        }
        let mut pids = String::new();
        for p in &self.pids {
            let _ = write!(pids, " {p:04x}");
        }
        let mut secs = String::new();
        for w in 0..64 {
            if secs.len() >= 900 - 12 {
                break;
            }
            if self.secs & (1 << w) != 0 {
                let _ = write!(secs, " {w:02}({}K)", self.sec_bytes[w] / 1024);
            }
        }
        // the minute's per-second output: max and median, for comparison
        let mut v: Vec<u64> = self.sec_bytes[..60].to_vec();
        v.sort_unstable();
        let _ = write!(
            secs,
            " [per-second max {}K, median {}K]",
            v[59] / 1024,
            v[30] / 1024
        );
        let mut dsecs = String::new();
        for w in 0..64 {
            if dsecs.len() >= 200 - 4 {
                break;
            }
            if self.dm_secs & (1 << w) != 0 {
                let _ = write!(dsecs, " {w:02}");
            }
        }
        let demod = match self.cn {
            Some((lo, hi)) if dm_sat => format!(
                "; demod C/N {}.{}-{}.{} dB, unlocked {} s, err 0xEB sum {} max {}, 0xF0 sum {} \
                 max {}, at seconds:{}",
                lo / 10,
                lo % 10,
                hi / 10,
                hi % 10,
                self.unlock,
                self.eb,
                self.eb_max,
                self.f0,
                self.f0_max,
                if self.dm_secs != 0 { &dsecs } else { " -" }
            ),
            Some((lo, hi)) => {
                format!(
                    "; demod C/N {}.{}-{}.{} dB, unlocked {} s",
                    lo / 10,
                    lo % 10,
                    hi / 10,
                    hi % 10,
                    self.unlock
                )
            }
            None => String::new(),
        };
        Some(format!(
            "s1 stats min {}: {} continuity error(s){}{} ({} within 1 s of a keepalive), TEI {}, \
             error seconds:{}, longest gap {} ms, {:.1} MB{}",
            self.no,
            self.err,
            if self.pids.is_empty() { "" } else { " on PID" },
            pids,
            self.near_ka,
            self.tei,
            if self.secs != 0 { &secs } else { " -" },
            self.gap_ms,
            self.bytes as f64 / 1_048_576.0,
            demod
        ))
    }
}

/// What a read attempt gave.
enum Read {
    Data(usize),
    Timeout,
    Failed(dt295_usb::Error),
}

/// The keep-posted EP 0x81 reads; `None` if they cannot be set up (no TS, but the
/// worker still services the firmware and the keepalive).
fn open_queue<'d>(sh: &'d Shared) -> Option<ReadQueue<'d>> {
    match sh.usb.read_queue(EP_TS, RQ_N, RQ_SIZE) {
        Ok(q) => {
            if !q.raw_io() {
                warn!(
                    "s1 worker: RAW_IO unavailable; queued reads reach the host controller one at \
                     a time (a system latency spike can drop TS)"
                );
            }
            Some(q)
        }
        Err(e) => {
            error!("s1 worker: cannot post the EP 0x81 reads ({e}); no TS");
            None
        }
    }
}

fn body(sh: &Shared, cfg: Config, sink: &mut TsSink) {
    let mut run = Run::new(sh, cfg);
    {
        let mut rq = open_queue(sh);
        while sh.live.streaming() {
            run.read(rq.as_mut());
            run.deliver(sink);
            run.service();
            run.psi_tick(sink);
            run.es_tick();
            run.minute();
        }
        // leaving this scope drops the queue: it cancels and reaps every posted read
    }
    run.finish();
}

/// The state of one worker run.
struct Run<'a> {
    sh: &'a Shared,
    t_start: Instant,
    /// The receive buffer: one read + the resync leftover.
    acc: Vec<u8>,
    acclen: usize,
    synced: bool,
    fails: u32,
    lost_logged: bool,
    /// Header statistics of the device's TS, before anything is added.
    stats: RawStats,
    /// Bytes handed to the sink.
    total: u64,
    /// Null packets of the device's TS left out of the sink.
    nulls: u64,
    reads: u64,
    full_reads: u64,
    last_data: Option<Instant>,
    /// The longest wait between two completed reads (ms).
    max_gap: u64,
    /// The first continuity errors: (ms since start, PID).
    cc_first: Vec<(u64, u16)>,
    /// Packet starts without the sync byte, and the first one (ms since start).
    grid_breaks: u32,
    grid_first: Option<u64>,
    /// A PAT listed the single-service program: the output has started.
    open: bool,
    /// Valid PATs before that which did not list it (TS left over from an earlier
    /// stream; for the log).
    pat_lacks: u32,
    /// Bytes of a single-service stream not handed on before that.
    held: u64,
    /// CRC-valid PATs (single-packet sections) the device delivered.
    pats: u64,
    win: Minute,
    /// Firmware-event polling: at most every [`EVENT_EVERY`] (10 Hz, as provided by a
    /// third party) instead of after every read (~20/s, ~240 control transfers/s at
    /// 10 Mbit/s). An A/B on hardware (67 min, every read vs every 250 ms) showed drops
    /// in both, so this only trims needless control traffic.
    ev_last: Option<Instant>,
    /// Demod sample once a second (what the vendor driver polls while streaming), to
    /// tell RF errors from losses inside the device: the remux may drop an uncorrectable
    /// packet instead of passing it with TEI set, so TEI 0 alone does not clear the RF
    /// side. Stops for the stream at the first failed read.
    dm_sat: bool,
    dm_fe: FrontEnd,
    dm_on: bool,
    dm_last: Option<Instant>,
    /// PSI keeper output, handed to the sink before the buffer.
    out: Vec<u8>,
    /// Whether the delivered video / audio are descrambled, judged every [`ES_EVERY`].
    es: EsCheck,
    es_last: Instant,
    /// Windows in a row without video / audio (one is not enough: a stream's first).
    es_missing: u32,
    /// What the host was last told (logged when it changes).
    es_clear: Option<bool>,
    /// The B-CAS state a few seconds into the stream was logged.
    bcas_logged: bool,
    output: Output,
    /// All services: the slot writes not done yet, and how many failed.
    route_q: VecDeque<Op>,
    route_fails: u32,
    /// ECM routes written in this stream, the last one when.
    ecm_routes: u32,
    ecm_last: Option<Instant>,
}

impl<'a> Run<'a> {
    fn new(sh: &'a Shared, cfg: Config) -> Self {
        let t_start = Instant::now();
        Run {
            sh,
            t_start,
            acc: vec![0u8; ACC_CAP],
            acclen: 0,
            synced: false,
            fails: 0,
            lost_logged: false,
            stats: RawStats::new(),
            total: 0,
            nulls: 0,
            reads: 0,
            full_reads: 0,
            last_data: None,
            max_gap: 0,
            cc_first: Vec::new(),
            grid_breaks: 0,
            grid_first: None,
            open: false,
            pat_lacks: 0,
            held: 0,
            pats: 0,
            win: Minute::new(0, t_start),
            ev_last: None,
            dm_sat: cfg.sys == Some(System::Satellite),
            dm_fe: cfg.fe,
            dm_on: cfg.sys.is_some(),
            dm_last: None,
            out: Vec::new(),
            es: EsCheck::new(),
            es_last: t_start,
            es_missing: 0,
            es_clear: None,
            bcas_logged: false,
            output: cfg.output,
            route_q: VecDeque::new(),
            route_fails: 0,
            ecm_routes: 0,
            ecm_last: None,
        }
    }

    /// One EP 0x81 read into the receive buffer (without the queue only its wait).
    fn read(&mut self, rq: Option<&mut ReadQueue<'_>>) {
        let Some(q) = rq else {
            thread::sleep(READ_WAIT);
            return;
        };
        let r = match q.next(READ_WAIT) {
            Ok(Some(data)) => {
                let got = data.len().min(RQ_SIZE);
                if self.acclen + got > ACC_CAP {
                    // never: the leftover is < 4 packets
                    self.acclen = 0;
                    self.synced = false;
                }
                self.acc[self.acclen..self.acclen + got].copy_from_slice(&data[..got]);
                Read::Data(got)
            }
            Ok(None) => Read::Timeout,
            Err(e) => Read::Failed(e),
        };
        match r {
            Read::Data(got) => {
                self.fails = 0;
                if got > 0 {
                    self.received(got);
                }
            }
            Read::Timeout | Read::Failed(dt295_usb::Error::Cancelled) => self.fails = 0,
            Read::Failed(e) => {
                self.fails += 1;
                if self.fails >= 3 {
                    // Errors that return immediately (device unplugged, pipe broken) made
                    // this loop spin a core at 100%. A healthy read either delivers or
                    // waits its timeout, so only repeated instant failures pause.
                    if matches!(e, dt295_usb::Error::NoDevice) && !self.lost_logged {
                        error!("s1 worker: device lost (unplugged?)");
                        self.lost_logged = true;
                        self.sh.live.set_lost(); // the host opens it again
                    }
                    sleep_ms(50);
                }
            }
        }
    }

    fn received(&mut self, got: usize) {
        let now = Instant::now();
        self.acclen += got;
        self.reads += 1;
        self.win.bytes += got as u64;
        let s = self.win.sec(now) as usize;
        if s < 64 {
            self.win.sec_bytes[s] += got as u64;
        }
        if got == RQ_SIZE {
            self.full_reads += 1;
        }
        if let Some(t) = self.last_data {
            let gap = now.duration_since(t).as_millis() as u64;
            self.max_gap = self.max_gap.max(gap);
            self.win.gap_ms = self.win.gap_ms.max(gap);
        }
        self.last_data = Some(now);
    }

    /// Resync to the 188-byte packet grid across read boundaries.
    fn resync(&mut self) {
        if self.synced {
            return;
        }
        match packet::find_sync(&self.acc[..self.acclen]) {
            Some(p) => {
                self.acc.copy_within(p..self.acclen, 0);
                self.acclen -= p;
                self.synced = true;
            }
            None if self.acclen > 4 * P => {
                let keep = 4 * P - 1;
                self.acc.copy_within(self.acclen - keep..self.acclen, 0);
                self.acclen = keep;
            }
            None => {}
        }
    }

    /// Process the whole packets received so far and hand them to the sink, resyncing
    /// where the packet grid breaks (so less than 4 packets are left over).
    fn deliver(&mut self, sink: &mut TsSink) {
        loop {
            self.resync();
            if !self.deliver_run(sink) {
                break;
            }
        }
    }

    /// Deliver the packets up to a lost sync; `true` = sync was lost (resync and go on).
    fn deliver_run(&mut self, sink: &mut TsSink) -> bool {
        if !self.synced || self.acclen < P {
            return false;
        }
        let (n, lost) = deliverable(&self.acc[..self.acclen]);
        if lost {
            self.synced = false;
            self.grid_breaks += 1;
            self.grid_first
                .get_or_insert(self.t_start.elapsed().as_millis() as u64);
        }
        if n > 0 {
            self.process(n, sink);
            self.acc.copy_within(n..self.acclen, 0);
            self.acclen -= n;
        }
        lost
    }

    /// Statistics, link decryption, PSI and the sink for the first `n` bytes (whole
    /// packets) of the buffer.
    fn process(&mut self, n: usize, sink: &mut TsSink) {
        self.out.clear();
        // where the output starts in this buffer (`None`: none of it goes out)
        let from;
        {
            let mut g = self.sh.lock();
            let st = &mut *g;
            let buf = &mut self.acc[..n];
            // header statistics, before decryption (the header is never encrypted)
            let near_ka = st.proto.ka_last.is_some_and(|t| t.elapsed().as_secs() <= 1);
            let (cc_first, win, t_start) = (&mut self.cc_first, &mut self.win, self.t_start);
            let tei = self.stats.observe(buf, |pid| {
                if cc_first.len() < 4 {
                    cc_first.push((t_start.elapsed().as_millis() as u64, pid));
                }
                win.cc_error(pid, near_ka);
            });
            self.win.tei += tei;
            link_decrypt(&mut st.link, buf);
            if let Output::All(r) = &mut self.output {
                r.observe(buf);
                let ops = r.plan();
                if !ops.is_empty() {
                    info!("s1 routes: {} slot write(s); {}", ops.len(), r.summary());
                    if r.dropped() > 0 {
                        warn!(
                            "s1 routes: {} PID(s) got no slot (data broadcasting first)",
                            r.dropped()
                        );
                    }
                    self.route_q.extend(ops);
                }
            }
            // Single-service output starts at the first PAT that lists the program: after
            // the secure session lapsed, the stream can begin with TS left over from an
            // earlier stream even after `fw::boot` drained the endpoint (Linux VM
            // 2026-10-24), and a recorder's service filter that sees another channel's
            // PAT first ends with "service not found".
            let mut start = match self.output {
                Output::Single(_) if !self.open => None,
                _ => Some(0),
            };
            // the device's own PAT, before any rewrite
            let (packets, _) = buf.as_chunks::<P>();
            let pid0 = packets.iter().filter(|p| is_pid0(*p));
            self.pats += pid0.filter(|p| pat::pat_section(*p).is_some()).count() as u64;
            if let (None, Output::Single(prog)) = (start, &self.output) {
                let prog = *prog;
                for (i, p) in packets.iter().enumerate().filter(|(_, p)| is_pid0(*p)) {
                    match pat::pat_lists(p, prog) {
                        Some(true) => {
                            start = Some(i * P);
                            self.open = true;
                            self.held += (i * P) as u64;
                            if self.pat_lacks > 0 {
                                info!(
                                    "s1: dropped {} bytes before the first PAT of service {prog}, \
                                     with {} PAT(s) of other services (left over from an earlier \
                                     stream)",
                                    self.held, self.pat_lacks
                                );
                            }
                            break;
                        }
                        Some(false) => self.pat_lacks += 1,
                        None => {}
                    }
                }
            }
            from = start;
            if from.is_none() {
                self.held += n as u64;
            }
            // the A/V check needs the device's own PAT, before the keeper blanks it (BS/CS)
            if let Some(s) = from {
                match &self.output {
                    Output::All(r) => self.es.observe_av(&buf[s..], &r.scrambled_av()),
                    Output::Single(p) => self.es.observe(&buf[s..], Some(*p)),
                    Output::Psi(_) => {}
                }
            }
            // may blank + re-send PAT/SDT (CS)
            let v = view(st, self.output.program(), self.total > 0);
            st.psi.observe(buf, &v, Instant::now(), &mut self.out);
        }
        if let Some(s) = from {
            if !self.out.is_empty() {
                sink(&self.out[..]);
            }
            // The null packets carry nothing (the routes take them so that the first TS
            // comes at once); they go after the statistics and the PSI saw them.
            let kept = dt295_core::ts::drop_null(&mut self.acc[s..n]);
            self.nulls += ((n - s - kept) / P) as u64;
            if kept > 0 {
                sink(&self.acc[s..s + kept]);
            }
        }
        self.total += n as u64;
        self.sh.live.set_delivered(self.total); // for the PAT probe's present / absent test
    }

    /// Firmware events, the keepalive and the demod sample, when due. Works on a copy
    /// of the protocol state (no lock held across the transfers) and stores it back.
    fn service(&mut self) {
        if !self.sh.live.streaming() {
            return;
        }
        let now = Instant::now();
        let ka_last = self.sh.lock().proto.ka_last;
        let since = |t: Instant| now.duration_since(t);
        let ev_due = self.ev_last.is_none_or(|t| since(t) >= EVENT_EVERY);
        let ka_due = ka_last.is_none_or(|t| since(t) >= KA_INTERVAL);
        let route_due = self.route_fails < ROUTE_FAILS
            && next_route(&self.route_q, self.ecm_waits(now)).is_some();
        let dm_due = self.dm_on
            && self
                .dm_last
                .is_none_or(|t| since(t) >= Duration::from_secs(1));
        if !(ev_due || ka_due || dm_due || route_due) {
            return;
        }
        let mut p = self.sh.lock().proto.clone();
        {
            let mut io = Io::new(&self.sh.usb, &mut p);
            if ev_due {
                if let Some(state) = service_events(&io) {
                    self.card_event(state);
                }
                self.ev_last = Some(now);
            }
            ka_tick(&mut io);
            if route_due {
                self.write_routes(&io);
            }
            if !self.bcas_logged && self.t_start.elapsed() >= BCAS_LOG_AFTER {
                // 3 with a working card, 0 with none, 5 with one that did not work
                // (a bad contact; hardware 2026-10-29)
                self.bcas_logged = true;
                let card = bcas_state(&io);
                self.sh.live.set_card_state(card);
                if card == CARD_FAILED {
                    warn!(
                        "b-cas: card state {card} into the stream: the card does not work; check \
                         that it sits right"
                    );
                } else {
                    info!("b-cas: card state {card} into the stream");
                }
            }
            if dm_due {
                self.dm_last = Some(now);
                self.demod_sample(&mut io, now);
            }
        }
        let mut st = self.sh.lock();
        if !self.sh.live.abandoned() {
            st.proto = p;
        }
    }

    /// A card pulled or inserted: the host starts the stream again (CardWatch); until
    /// then its ES stay scrambled.
    fn card_event(&self, state: u8) {
        self.sh.live.note_card_event(state);
        warn!("b-cas: the card was pulled or inserted (card state {state})");
    }

    /// The next ECM route waits its gap.
    fn ecm_waits(&self, now: Instant) -> bool {
        self.ecm_routes >= ECMS_AT_ONCE
            && self
                .ecm_last
                .is_some_and(|t| now.duration_since(t) < ECM_GAP)
    }

    /// Write the next few route slots of an all-services stream (an ECM before the PIDs
    /// bound to it: the queue keeps the router's order). A failed write ends the pass: on
    /// a firmware that stopped acking each takes a full timeout, with the reads waiting.
    fn write_routes(&mut self, io: &Io) {
        let t0 = Instant::now();
        let mut written = 0;
        while written < ROUTE_BATCH {
            let now = Instant::now();
            let Some(op) =
                next_route(&self.route_q, self.ecm_waits(now)).and_then(|i| self.route_q.remove(i))
            else {
                break; // what is left waits for an ECM
            };
            if is_ecm_route(&op) {
                self.ecm_routes += 1;
                self.ecm_last = Some(now);
            }
            written += 1;
            let (r, card) = route_write(io, op);
            debug!(
                "s1 route: slot {} <- PID 0x{:04x} (ECM slot {}): {r:?}",
                op.slot, op.pid, op.ecm_slot
            );
            if card {
                self.card_event(bcas_state(io));
            }
            if let Err(e) = r {
                self.route_fails += 1;
                let give_up = self.route_fails >= ROUTE_FAILS;
                warn!(
                    "s1 routes: slot {} <- PID 0x{:04x} failed ({e}){}",
                    op.slot,
                    op.pid,
                    if give_up {
                        "; the routes stay as they are"
                    } else {
                        ""
                    }
                );
                if give_up {
                    self.route_q.clear();
                }
                break;
            }
        }
        debug!(
            "s1 routes: {written} slot write(s) in {} ms",
            t0.elapsed().as_millis()
        );
    }

    fn demod_sample(&mut self, io: &mut Io, now: Instant) {
        let sample = if self.dm_sat {
            fe1::sample_s(io, self.dm_fe)
        } else {
            fe1::sample_t(io, self.dm_fe).map(|(lock, cn)| (lock, cn, 0, 0))
        };
        match sample {
            Err(_) => {
                self.dm_on = false;
                warn!("s1: demod sample failed; not sampling for this stream");
            }
            Ok((lock, cn, eb, f0)) => {
                let (locked, mdb) = if self.dm_sat {
                    (fe1::s_locked(lock), cn_s_mdb(cn))
                } else {
                    (fe1::t_locked(lock), cn_t_mdb(cn))
                };
                self.sh.live.publish_cn(mdb); // the host's live signal level
                self.win.demod(now, locked, mdb / 100, eb, f0);
            }
        }
    }

    /// PSI keeper re-sends (~5x/s even without data).
    fn psi_tick(&mut self, sink: &mut TsSink) {
        self.out.clear();
        {
            let mut g = self.sh.lock();
            let st = &mut *g;
            let v = view(st, self.output.program(), self.total > 0);
            st.psi.tick(&v, Instant::now(), &mut self.out);
        }
        if !self.out.is_empty() {
            sink(&self.out[..]);
        }
    }

    /// Every [`ES_EVERY`]: judge whether the video / audio come out descrambled, for the
    /// host ([`crate::Live::es_clear`]); log when that changes.
    fn es_tick(&mut self) {
        if self.es_last.elapsed() < ES_EVERY {
            return;
        }
        self.es_last = Instant::now();
        let v = self.es.verdict();
        self.es_missing = if v == Some(false) {
            self.es_missing + 1
        } else {
            0
        };
        let clear = match v {
            Some(false) if self.es_missing < 2 => return, // wait for a second window
            v => v,
        };
        self.sh.live.set_es_clear(clear);
        let Some(clear) = clear else { return };
        if self.es_clear != Some(clear) {
            let card = self.sh.live.card();
            if !clear && card == CARD_WORKS {
                // the card session is up: this card cannot descramble the service
                warn!(
                    "s1: no video / audio in the stream although the card works (card state \
                     {card}): no contract for it? ({})",
                    self.es.last_window()
                );
            } else if !clear {
                warn!(
                    "s1: no video / audio in the stream: no B-CAS card session ({})",
                    self.es.last_window()
                );
            } else if self.es_clear.is_some() {
                info!("s1: the video / audio are back ({})", self.es.last_window());
            }
            self.es_clear = Some(clear);
        }
    }

    /// The per-minute statistics line.
    fn minute(&mut self) {
        if self.win.t0.elapsed() >= Duration::from_mins(1) {
            if let Some(line) = self.win.line(self.dm_sat) {
                info!("{line}");
            }
            self.win = Minute::new(self.win.no + 1, Instant::now());
        }
    }

    /// The end-of-stream log lines.
    fn finish(self) {
        let st = self.sh.lock();
        if let Some(s) = st.psi.end_summary() {
            info!("{s}");
        }
        let (stats, total, nulls) = (&self.stats, self.total, self.nulls);
        let pats = self.pats;
        info!(
            "s1 worker: received {total} bytes ({nulls} null packets left out), {pats} PAT(s), \
             {} SDT packet(s), {} distinct PID(s)",
            stats.sdt_packets, stats.distinct_pids
        );
        if self.reads > 0 {
            let mut first = String::new();
            for (ms, pid) in &self.cc_first {
                let _ = write!(first, " {ms}ms/pid{pid:04x}");
            }
            let grid = match self.grid_first {
                Some(ms) => format!(
                    "; {} break(s) in the packet grid, first at {ms}ms",
                    self.grid_breaks
                ),
                None => String::new(),
            };
            info!(
                "s1 worker: {} read(s) ({} full 64K), longest gap {} ms; {} continuity error(s) in \
                 the device's TS{}{first}; {} TEI packet(s){grid}",
                self.reads,
                self.full_reads,
                self.max_gap,
                stats.cc_errors,
                if stats.cc_errors > 0 {
                    ", first at"
                } else {
                    ""
                },
                stats.tei_packets
            );
        }
        // A healthy stream carries ~10 PATs/s. Megabytes with none
        // decodable: after a re-open without a power cycle the device keeps the link key
        // of the session that first streamed, so without that key a host can neither
        // detect (scan) nor decode the TS.
        if total >= 4 * 1024 * 1024 && pats == 0 {
            warn!(
                "s1: this stream carried TS but no decodable PAT -- the link key does not match \
                 (re-open without a re-plug and the earlier session's key unknown); USB re-plug \
                 the device."
            );
        }
        if pats == 0 && stats.packets > 0 {
            // Was PSI emitted at all, and if so, does our link key decrypt it?
            let (mut plain_ok, mut key_ok, mut hex) = (-1, -1, String::from("-"));
            if let Some(raw) = &stats.raw_pat {
                plain_ok = i32::from(pat::pat_section(raw).is_some());
                if st.link.on {
                    let mut t = *raw;
                    link::decrypt(&st.link.key, &mut t);
                    key_ok = i32::from(pat::pat_section(&t).is_some());
                }
                hex = raw[..24].iter().map(|b| format!("{b:02x} ")).collect();
            }
            info!(
                "s1 diag: {} pkt(s), {} link-encrypted (TSC=01), {} PID-0 pkt(s); sample PAT CRC \
                 ok: as-sent={plain_ok} with-link-key={key_ok} (-1 = n/a); raw: {hex}",
                stats.packets, stats.link_encrypted, stats.pid0_packets
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_stops_before_a_broken_packet_grid() {
        let mut pkt = [0xFFu8; P];
        pkt[0] = packet::SYNC;
        // whole packets; a partial one waits for the next read
        let mut buf = pkt.repeat(3);
        buf.extend_from_slice(&pkt[..10]);
        assert_eq!(deliverable(&buf), (3 * P, false));
        // the third packet lost its last 24 bytes: the two before it go out
        let mut buf = pkt.repeat(2);
        buf.extend_from_slice(&pkt[..P - 24]);
        buf.extend_from_slice(&pkt.repeat(4));
        assert_eq!(deliverable(&buf), (2 * P, true));
        // no sync byte at the start, or right after it: nothing
        assert_eq!(deliverable(&[0u8; 2 * P]), (0, true));
        let mut buf = pkt.repeat(2);
        buf[P] = 0;
        assert_eq!(deliverable(&buf), (0, true));
    }

    #[test]
    fn a_waiting_ecm_holds_back_only_what_is_bound_to_it() {
        let op = |slot, pid, ecm_slot| Op {
            slot,
            pid,
            ecm_slot,
        };
        // the 5th ECM (slot 59) waits; the 6th, its video, and a later write to its
        // slot wait with it; the 4th ECM's video and the SI go ahead
        let q: VecDeque<Op> = [
            op(59, 0x605, 59),
            op(58, 0x606, 58),
            op(20, 0x1004, 60),
            op(21, 0x1005, 59),
            op(59, 0x1009, 0),
            op(22, 0x12, 0),
        ]
        .into();
        assert_eq!(next_route(&q, false), Some(0));
        assert_eq!(next_route(&q, true), Some(2));
        let mut q = q;
        q.remove(2);
        assert_eq!(next_route(&q, true), Some(4));
        q.remove(4);
        assert_eq!(next_route(&q, true), None);
        assert_eq!(next_route(&VecDeque::new(), false), None);
    }

    #[test]
    fn quiet_minute_is_not_logged() {
        let mut m = Minute::new(3, Instant::now());
        m.bytes = 50 << 20;
        m.gap_ms = 100;
        m.demod(m.t0, true, 245, 0, 0);
        assert!(m.line(true).is_none());
    }

    #[test]
    fn minute_line_format() {
        let t0 = Instant::now();
        let mut m = Minute::new(2, t0);
        m.err = 3;
        m.pids = vec![0x0111, 0x0112];
        m.near_ka = 1;
        m.tei = 4;
        m.secs = (1 << 5) | (1 << 17);
        m.sec_bytes[5] = 300 * 1024;
        m.sec_bytes[17] = 2048;
        for (i, b) in m.sec_bytes.iter_mut().enumerate().take(60) {
            if *b == 0 {
                *b = (1000 + i as u64) * 1024;
            }
        }
        m.gap_ms = 140;
        m.bytes = 3 * 1_048_576 + 524_288;
        assert_eq!(
            m.line(false).unwrap(),
            "s1 stats min 2: 3 continuity error(s) on PID 0111 0112 (1 within 1 s of a keepalive), \
             TEI 4, error seconds: 05(300K) 17(2K) [per-second max 1059K, median 1030K], \
             longest gap 140 ms, 3.5 MB"
        );
        m.demod(t0, false, 123, 5, 0);
        m.demod(t0 + Duration::from_secs(7), true, 98, 0, 0);
        assert!(
            m.line(false)
                .unwrap()
                .ends_with("3.5 MB; demod C/N 9.8-12.3 dB, unlocked 1 s")
        );
        assert!(m.line(true).unwrap().ends_with(
            "3.5 MB; demod C/N 9.8-12.3 dB, unlocked 1 s, err 0xEB sum 5 max 5, 0xF0 sum 0 max 0, \
             at seconds: 00"
        ));
        m.secs = 0;
        assert!(
            m.line(false)
                .unwrap()
                .contains("error seconds: -, longest gap")
        );
    }
}
