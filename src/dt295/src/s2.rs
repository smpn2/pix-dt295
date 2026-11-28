//! System2 (EP 0x86): the stream of a two-tuner model's second tuner.
//!
//! The demod chip puts the second front end's TS out of its output B
//! ([`Port::B`]), which feeds the bridge's System2 input. The firmware descrambles it
//! with the card, like System1, and puts the whole TS out on EP 0x86, link-encrypted like
//! EP 0x81 but with a key of its own: the one of secure endpoint 0x1B. No transcoder
//! firmware is involved, so System1 (the first tuner) streams, starts and stops beside
//! it. The sequence is the vendor's Mac driver's (StationTV for Mac, `DT195Lib`):
//!
//! * start: the INIT_S2 fields (0x1B02-0x1B18), ch1 cmd(4,0x10,0), the endpoint's link
//!   key (07/01/06), the EP 0x86 reads posted, 0x081100 = 0xFF00, ch1 cmd(5,0,2),
//!   0x081100 = 0
//! * stop: ch1 cmd(5,0,1), then the endpoint released (07/03/04)
//!
//! EP 0x86 carries 192-byte units: 4 bytes, then the 188-byte packet.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use dt295_core::fe1::{self, Bus, FrontEnd, Port};
use dt295_core::proto::{FW_STALL_LIMIT, Io, sleep_ms};
use dt295_core::signal::{cn_s_mdb, cn_t_mdb};

use crate::device::{LiveInner, Shared};
use crate::raw::{RawTune, RawWorker};
use crate::ts::link;
use crate::ts::packet::{self, PACKET, SYNC};
use crate::ts::stats::RawStats;
use crate::{Error, Live, Result, Status, System, TsSink, cas, dtvdec, s1};

/// The bulk IN endpoint of System2.
const EP_TS2: u8 = 0x86;
/// One unit on EP 0x86: 4 bytes, then the packet.
const UNIT: usize = 192;
const UNIT_HEAD: usize = UNIT - PACKET;
/// SYSTEMINFOS2MASK: 0xFF00 across the start, 0 after it.
const REG_INFO_MASK: u32 = 0x08_1100;
/// The command ACK wait (the Mac driver's).
const CMD_TIMEOUT_MS: u32 = 3000;
/// Reads kept posted on EP 0x86 (as System1 keeps them on EP 0x81: about 0.3 s of a
/// terrestrial TS, so a host thread held up for a moment loses nothing).
const RQ_N: usize = 16;
const RQ_SIZE: usize = 0x10000;
const READ_WAIT: Duration = Duration::from_millis(200);
/// The demod sample (C/N) and the keepalive check, at most this often.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// How long the worker may take to post its reads before the start goes on without.
const READY_WAIT: Duration = Duration::from_secs(1);

/// A two-tuner model's second tuner ([`crate::Device::tuner2`]): tune it and stream its
/// TS on System2, beside the first tuner (the [`crate::Device`]) and its stream. The
/// `Device` it came from keeps the unit: it boots the firmware, and while neither
/// tuner streams its [`crate::Device::idle_tick`] keeps the secure session alive.
pub struct Tuner2 {
    sh: Arc<Shared>,
    live: Arc<LiveInner>,
    fe: FrontEnd,
    /// The system of the last tune.
    sys: Option<System>,
    /// PROGRAM_NUMBER_S2 (0: none).
    service: u16,
    /// The boot ([`crate::device::State::boot_gen`]) the last tune was on.
    tuned_on: Option<u64>,
    worker: Option<Worker>,
    /// The raw-TS stream worker, while one runs ([`Tuner2::stream_start_cold`]).
    raw: Option<RawWorker>,
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<TsSink>,
}

impl fmt::Debug for Tuner2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tuner2")
            .field("sys", &self.sys)
            .field("service", &self.service)
            .field("streaming", &self.worker.is_some())
            .field("raw", &self.raw)
            .finish_non_exhaustive()
    }
}

impl Tuner2 {
    pub(crate) fn new(sh: Arc<Shared>) -> Self {
        Tuner2 {
            sh,
            live: Arc::new(LiveInner::new()),
            fe: FrontEnd::dt195w(1, Port::B).expect("the second front end"),
            sys: None,
            service: 0,
            tuned_on: None,
            worker: None,
            raw: None,
        }
    }

    /// The live values of this tuner's stream (C/N, bytes delivered, unplug).
    pub fn live(&self) -> Live {
        Live::new(Arc::clone(&self.live))
    }

    /// The boot the unit is on; an error when its firmware is not up.
    fn boot(&self) -> Result<u64> {
        match self.sh.lock().boot_gen {
            0 => Err(Error::State("the firmware is not booted".into())),
            g => Ok(g),
        }
    }

    fn no_stream(&self) -> Result<()> {
        match (&self.worker, &self.raw) {
            (None, None) => Ok(()),
            _ => Err(Error::State("a stream is running".into())),
        }
    }

    /// Start a blob-free raw-TS stream of this tuner (see [`crate::Device::stream_start_cold`]):
    /// its TS goes out of the demod's output B into the chip's second capture and EP 0x86, so it
    /// streams beside the first tuner's raw-TS stream. No firmware is booted. `full` brings a
    /// unit fresh from a plug up ([`Tuner2::cold_monitor_installed`]); whichever tuner starts
    /// first does it, and a bring-up already done is not done again. A channel change is a
    /// stop + a fresh start.
    pub fn stream_start_cold(
        &mut self,
        terrestrial: bool,
        freq_hz: u32,
        tsid: Option<u16>,
        full: bool,
        duration: Duration,
        sink: TsSink,
    ) -> Result<()> {
        self.no_stream()?;
        let tune = RawTune {
            terrestrial,
            freq_hz,
            tsid,
            full,
            duration,
        };
        self.raw = Some(RawWorker::spawn(
            Arc::clone(&self.sh),
            Arc::clone(&self.live),
            self.fe,
            tune,
            sink,
        )?);
        Ok(())
    }

    /// Whether the unit has been brought up for raw-TS (see
    /// [`crate::Device::cold_monitor_installed`]). It may be called while the first tuner
    /// streams.
    pub fn cold_monitor_installed(&self) -> bool {
        crate::raw::monitor_installed(&self.sh)
    }

    /// Whether the raw-TS worker is running (see [`crate::Device::raw_running`]).
    pub fn raw_running(&self) -> bool {
        self.raw.as_ref().is_some_and(RawWorker::running)
    }

    /// Whether a raw-TS worker slot is occupied (running, or ended but not yet reaped).
    pub fn raw_present(&self) -> bool {
        self.raw.is_some()
    }

    /// Stop the raw-TS worker and return what it captured (or its fault); see
    /// [`crate::Device::stream_stop_raw`]. No-op if none is running.
    pub fn stream_stop_raw(&mut self) -> Result<crate::RawCaptureStats> {
        crate::raw::stop(&mut self.raw, &self.sh, &self.live)
    }

    /// Tune ISDB-T (`freq_hz` = the channel centre) and wait for lock (a missing lock is
    /// logged, not an error: read the status).
    pub fn tune_t(&mut self, freq_hz: u32) -> Result<()> {
        self.no_stream()?;
        let boot = self.boot()?;
        self.sys = Some(System::Terrestrial);
        self.tuned_on = Some(boot);
        fe1::tune_t(&mut LockBus::new(&self.sh), self.fe, freq_hz)
    }

    /// Tune ISDB-S (`freq_hz` = IF, `tsid` = the broadcast TSID) and wait for lock.
    pub fn tune_s(&mut self, freq_hz: u32, tsid: u16) -> Result<()> {
        self.no_stream()?;
        let boot = self.boot()?;
        self.sys = Some(System::Satellite);
        self.tuned_on = Some(boot);
        fe1::tune_s(&mut LockBus::new(&self.sh), self.fe, freq_hz, tsid).map(|_| ())
    }

    /// Lock and C/N right after a tune (not while streaming).
    pub fn read_status(&mut self, sys: System) -> Result<Status> {
        self.no_stream()?;
        fe1::read_status(&mut LockBus::new(&self.sh), self.fe, sys)
    }

    /// The service whose ECM the stream is descrambled with (PROGRAM_NUMBER_S2); `None`:
    /// none named. The stream is the whole TS either way. Terrestrial TSs came out
    /// descrambled with none (hardware 2026-11-20).
    pub fn set_service_id(&mut self, sid: Option<u16>) {
        self.service = sid.unwrap_or(0);
    }

    /// Start streaming the tuned TS into `sink` (the whole TS, descrambled and
    /// link-decrypted). The packets still scrambled are left out: those of the first
    /// ~0.3 s (before the card has the first ECM: hardware 2026-11-20), and a service
    /// the card has no contract for.
    pub fn stream_start(&mut self, sink: TsSink) -> Result<()> {
        self.no_stream()?;
        let (Some(tuned_on), Some(sys)) = (self.tuned_on, self.sys) else {
            return Err(Error::State("tune first".into()));
        };
        if self.boot()? != tuned_on {
            return Err(Error::State(
                "the unit started again since the tune: tune again".into(),
            ));
        }
        let key = {
            let mut g = self.sh.lock();
            let st = &mut *g;
            let mut io = Io::new(&self.sh.usb, &mut st.proto);
            if !st.card_open {
                // the card session, once per boot (System1 may have opened it)
                let _ = cas::bcas_stage2(&mut io);
                let card = s1::bcas_state(&io);
                self.live.set_card_state(card);
                info!("b-cas: card state {card}");
                st.card_open = true;
            }
            init_s2(&io, self.service)?;
            io.cmd_ack(1, 4, 0x10, 0, CMD_TIMEOUT_MS)?;
            dtvdec::link_key(&mut io, dtvdec::ENDPOINT_SYSTEM2)?
        };
        self.live.reset_stream();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let run = Run {
            sh: Arc::clone(&self.sh),
            live: Arc::clone(&self.live),
            fe: self.fe,
            sys,
            key,
            stop: Arc::clone(&stop),
        };
        let span = tracing::Span::current();
        let handle = thread::Builder::new()
            .name("dt295-s2-worker".into())
            .spawn(move || {
                let _span = span.enter();
                run.run(sink, ready_tx)
            })?;
        // the reads are posted before the start, as the vendor does
        let _ = ready_rx.recv_timeout(READY_WAIT);
        let started = {
            let mut g = self.sh.lock();
            let mut io = Io::new(&self.sh.usb, &mut g.proto);
            let _ = io.write16(REG_INFO_MASK, 0xFF00);
            let r = io.cmd_ack(1, 5, 0, 2, CMD_TIMEOUT_MS);
            let _ = io.write16(REG_INFO_MASK, 0);
            r
        };
        self.worker = Some(Worker { stop, handle });
        self.sh.s2_streaming.store(true, Ordering::Release);
        if let Err(e) = started {
            warn!("s2: the stream did not start ({e})");
            let _ = self.stream_stop();
            return Err(e);
        }
        info!("TS stream running on EP 0x86 (System2, tuner 2)");
        Ok(())
    }

    /// Stop the stream (its worker, then the firmware's System2) and release its link
    /// key.
    pub fn stream_stop(&mut self) -> Result<()> {
        let Some(w) = self.worker.take() else {
            return Ok(());
        };
        w.stop.store(true, Ordering::Release);
        let _ = self.sh.usb.abort_pipe(EP_TS2);
        if w.handle.join().is_err() {
            warn!("s2: the stream worker panicked");
        }
        self.sh.s2_streaming.store(false, Ordering::Release);
        let mut g = self.sh.lock();
        let mut io = Io::new(&self.sh.usb, &mut g.proto);
        let stopped = io.cmd_ack(1, 5, 0, 1, CMD_TIMEOUT_MS);
        if let Err(e) = dtvdec::release_endpoint(&mut io, dtvdec::ENDPOINT_SYSTEM2) {
            warn!("s2: releasing the link key failed ({e})");
        }
        stopped
    }
}

impl Drop for Tuner2 {
    /// Stops its stream, a raw-TS one too (as [`crate::Device`] does): a worker left
    /// running would stream until [`crate::RAW_STREAM_CAP`] and keep the unit open.
    fn drop(&mut self) {
        if self.raw.is_some() {
            let _ = self.stream_stop_raw();
        }
        let _ = self.stream_stop();
        self.sh.tuner2.store(false, Ordering::Release);
    }
}

/// The INIT_S2 fields, as the vendor's Mac driver writes them: input packet size 188 and
/// input clock edge 0, ACCUMULATION_MODE_OUT off, output packet size 1 (the 192-byte
/// units) on DMA output 1, SCRAMBLE_MODE_S2 1 (System1's value too), CA_TYPE 0, and the
/// program.
fn init_s2(io: &Io, service: u16) -> Result<()> {
    const FIELDS: [(u32, u32, u32, u32); 7] = [
        (0x1B02, 11, 2, 0), // TSIN_PACKET_SIZE_S2
        (0x1B02, 10, 1, 0), // STINCLK_HL_S2
        (0x1B02, 2, 1, 0),  // ACCUMULATION_MODE_OUT_S2
        (0x1B04, 11, 2, 1), // TSOUT_PACKET_SIZE_S2
        (0x1B04, 8, 2, 1),  // DMA_SEL_OUT_S2
        (0x1B0E, 14, 2, 1), // SCRAMBLE_MODE_S2
        (0x1B0E, 4, 2, 0),  // CA_TYPE_S2
    ];
    for (addr, pos, width, val) in FIELDS {
        io.write_field(addr, pos, width, val)
            .map_err(Error::into_usb)?;
    }
    io.write_field(0x1B18, 0, 16, u32::from(service)) // PROGRAM_NUMBER_S2
        .map_err(Error::into_usb)
}

/// The I2C bus of the second front end's tunes: the device lock taken for each transfer
/// only, so the first tuner's stream worker gets the device between them (a tune takes
/// 1-2 s, longer than its posted reads last).
pub(crate) struct LockBus<'a> {
    sh: &'a Shared,
}

impl<'a> LockBus<'a> {
    pub(crate) fn new(sh: &'a Shared) -> Self {
        LockBus { sh }
    }
}

impl Bus for LockBus<'_> {
    fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
        let mut g = self.sh.lock();
        Io::new(&self.sh.usb, &mut g.proto).i2c_write(slave, data)
    }
    fn i2c_write_read(&mut self, slave: u8, wdata: &[u8], rbuf: &mut [u8]) -> Result<()> {
        let mut g = self.sh.lock();
        Io::new(&self.sh.usb, &mut g.proto).i2c_write_read(slave, wdata, rbuf)
    }
    fn sleep_ms(&mut self, ms: u64) {
        sleep_ms(ms);
    }
    fn wedged(&self) -> bool {
        self.sh.lock().proto.fw_stall >= FW_STALL_LIMIT
    }
}

/// What the stream worker needs.
struct Run {
    sh: Arc<Shared>,
    live: Arc<LiveInner>,
    fe: FrontEnd,
    sys: System,
    key: [u8; 16],
    stop: Arc<AtomicBool>,
}

impl Run {
    /// Read EP 0x86 until stopped: reframe, decrypt, drop what is still scrambled, hand
    /// the rest to the sink; sample the demod and keep the session alive about once a
    /// second, without waiting for the device lock (the first tuner's stream may hold
    /// it). Returns the sink.
    fn run(self, mut sink: TsSink, ready: mpsc::Sender<()>) -> TsSink {
        let mut q = match self.sh.usb.read_queue(EP_TS2, RQ_N, RQ_SIZE) {
            Ok(q) => q,
            Err(e) => {
                warn!("s2: cannot post reads on EP 0x86 ({e})");
                return sink;
            }
        };
        let _ = ready.send(());
        self.live.set_streaming(true);
        let t0 = Instant::now();
        let mut carry: Vec<u8> = Vec::with_capacity(RQ_SIZE + UNIT);
        let mut out = Vec::with_capacity(RQ_SIZE);
        let mut stats = RawStats::new();
        let mut scrambled = 0u64;
        let mut delivered = 0u64;
        // what the device sent, null packets included: the liveness count, as System1's
        let mut received = 0u64;
        let mut nulls = 0u64;
        let mut sampled = t0;
        let mut errors = 0u32;
        while !self.stop.load(Ordering::Acquire) {
            match q.next(READ_WAIT) {
                Ok(Some(d)) => {
                    carry.extend_from_slice(d);
                    reframe(&mut carry, &mut out);
                    stats.observe(&out, |_| {});
                    link::decrypt(&self.key, &mut out);
                    scrambled += keep_clear(&mut out);
                    received += out.len() as u64;
                    let kept = dt295_core::ts::drop_null(&mut out);
                    nulls += ((out.len() - kept) / PACKET) as u64;
                    out.truncate(kept);
                    if !out.is_empty() {
                        sink(&out);
                        delivered += out.len() as u64;
                    }
                    self.live.set_delivered(received);
                }
                Ok(None) => {}
                Err(dt295_usb::Error::NoDevice) => {
                    warn!("s2: the unit is gone");
                    self.live.set_lost();
                    break;
                }
                Err(_) if self.stop.load(Ordering::Acquire) => break,
                Err(e) => {
                    errors += 1;
                    if errors <= 3 {
                        warn!("s2: EP 0x86 read failed ({e})");
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
            if sampled.elapsed() >= SAMPLE_EVERY {
                sampled = Instant::now();
                self.sample();
            }
        }
        self.live.set_streaming(false);
        info!(
            "s2: streamed {} packets in {:.1} s ({} null packets left out), {} continuity \
             error(s), {} TEI, {} still scrambled (left out)",
            delivered / PACKET as u64,
            t0.elapsed().as_secs_f64(),
            nulls,
            stats.cc_errors,
            stats.tei_packets,
            scrambled
        );
        sink
    }

    /// The demod's lock and C/N for the live level, and the keepalive when due, if the
    /// device lock is free now.
    fn sample(&self) {
        let Some(mut g) = self.sh.try_lock() else {
            return;
        };
        let mut io = Io::new(&self.sh.usb, &mut g.proto);
        let sample = match self.sys {
            System::Terrestrial => fe1::sample_t(&mut io, self.fe).map(|(_, cn)| cn_t_mdb(cn)),
            System::Satellite => fe1::sample_s(&mut io, self.fe).map(|(_, cn, ..)| cn_s_mdb(cn)),
        };
        if let Ok(mdb) = sample {
            self.live.publish_cn(mdb);
        }
        s1::ka_tick(&mut io);
    }
}

/// Move the whole 188-byte packets out of the 192-byte units at the head of `carry`
/// into `out`, keeping a partial unit (and resyncing on a unit without a sync byte).
fn reframe(carry: &mut Vec<u8>, out: &mut Vec<u8>) {
    out.clear();
    let mut i = 0;
    while i + UNIT <= carry.len() {
        let next_ok = i + UNIT + UNIT_HEAD >= carry.len() || carry[i + UNIT + UNIT_HEAD] == SYNC;
        if carry[i + UNIT_HEAD] == SYNC && next_ok {
            out.extend_from_slice(&carry[i + UNIT_HEAD..i + UNIT]);
            i += UNIT;
        } else {
            i += 1;
        }
    }
    carry.drain(..i);
}

/// Drop the packets still scrambled (transport_scrambling_control set after the link
/// decryption) from `buf`; returns how many.
fn keep_clear(buf: &mut Vec<u8>) -> u64 {
    let mut kept = 0;
    let mut dropped = 0;
    for i in (0..buf.len() / PACKET).map(|n| n * PACKET) {
        if packet::tsc(&buf[i..i + PACKET]) != 0 {
            dropped += 1;
            continue;
        }
        if kept != i {
            buf.copy_within(i..i + PACKET, kept);
        }
        kept += PACKET;
    }
    buf.truncate(kept);
    dropped
}

/// Lock-poison tolerant `try_lock` of the device state, for [`Shared::try_lock`].
pub(crate) fn try_lock_state<T>(m: &std::sync::Mutex<T>) -> Option<std::sync::MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(PoisonError::into_inner(p)),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u8, tsc: u8) -> [u8; PACKET] {
        let mut p = [0xAA; PACKET];
        p[0] = SYNC;
        p[1] = 0;
        p[2] = pid;
        p[3] = (tsc << 6) | 0x10;
        p
    }

    fn unit(pid: u8) -> Vec<u8> {
        let mut u = vec![0x12, 0x34, 0x56, 0x78];
        u.extend_from_slice(&packet(pid, 0));
        u
    }

    #[test]
    fn units_become_packets_across_reads() {
        let mut all = Vec::new();
        for pid in 1..=4 {
            all.extend(unit(pid));
        }
        let mut carry = Vec::new();
        let mut got = Vec::new();
        let mut out = Vec::new();
        // in pieces that split units, after 5 bytes of garbage
        let mut stream = vec![0x47, 0, 0, 0, 0];
        stream.extend(&all);
        for piece in stream.chunks(100) {
            carry.extend_from_slice(piece);
            reframe(&mut carry, &mut out);
            got.extend_from_slice(&out);
        }
        assert_eq!(got.len(), 4 * PACKET);
        for (n, p) in got.chunks(PACKET).enumerate() {
            assert_eq!(p, packet(n as u8 + 1, 0));
        }
        assert!(carry.len() < UNIT);
    }

    #[test]
    fn scrambled_packets_are_left_out() {
        let mut buf = Vec::new();
        for (pid, tsc) in [(1, 0), (2, 3), (3, 0), (4, 2), (5, 0)] {
            buf.extend_from_slice(&packet(pid, tsc));
        }
        assert_eq!(keep_clear(&mut buf), 2);
        let pids: Vec<u8> = buf.chunks(PACKET).map(|p| p[2]).collect();
        assert_eq!(pids, [1, 3, 5]);
    }
}
