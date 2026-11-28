//! System1 (transcoder path, EP 0x81) TS streaming (the PSI keeper and the pure TS
//! helpers live in [`crate::ts`]).
//!
//! This is the path StationTV X uses for viewing, provided by a third party. It
//! replays the vendor bring-up and opens the firmware's B-CAS
//! card-session gate that otherwise makes cmd(5,0,2) fault to 0xFFFF:
//! 1. cmd(4,0,0); hot-load the trc firmware on EP 0x02; replay the captured
//!    decoder-window init; single-service (AUTO_PID, PROGRAM_NUMBER), full-mux, or all
//!    services (explicit route slots the worker keeps up with the PAT / PMTs).
//! 2. 0x082028 = 0xFFFF; cmd(4,0x20,0) -> state 1; cmd(5,0,2) -> state 2. (No CAS
//!    channel: the vendor runs one here, the stream does not need it.)
//! 3. Once per authentication, before that: B-CAS stage 2 (un-gates the main ES; needs
//!    a contract card) and the per-session link key (07/01/06, [`dtvdec`]).
//! 4. A worker thread reads EP 0x81, link-decrypts, feeds the sink, and every 12 s
//!    sends the SECURE_REALTIME keepalive (without it state 0x0002 drops to 0xFFFF).
//!
//! Streaming is single-owner: while a worker runs it does all device I/O; the host
//! only waits and calls the read-only accessors until [`S1::stream_stop`].

pub(crate) mod decoder_init;
mod worker;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use crate::device::Shared;
use crate::ts::link::{self, KeyVerdict};
use crate::ts::{packet, route};
use crate::{Error, Result, System, TsSink, cas, dtvdec};
use dt295_core::fe1::FrontEnd;
use dt295_core::fwdl::{bulk_download, write_idle_params};
use dt295_core::proto::{FW_STALL_LIMIT, Io, REG_FW_STATE, REG_IRQ, sleep_ms};

/// The TS endpoint.
pub(crate) use dt295_core::EP_TS;
/// Keepalive interval, well within the about 60 s the firmware gives a running session
/// after the last keepalive (hardware 2026-10-31, two units: 60-61 s, streaming or idle).
/// A session that only authenticated does not time out (not within 200 s).
pub(crate) const KA_INTERVAL: Duration = Duration::from_secs(12);
/// The first keepalive after authentication: early enough for a process that lives only
/// a few seconds, late enough not to draw the short challenge seen at ~3 s.
pub(crate) const KA_FIRST: Duration = Duration::from_secs(5);
/// After a failed keepalive the next one is tried this soon instead of a whole
/// [`KA_INTERVAL`] later -- once. A "short challenge" now and then (about 1 in 250 on
/// hardware, 2026-10-19) is transient and the session survives it, but the firmware
/// drops a session that goes too long without a keepalive; a second failure in a row
/// waits the normal interval (the session may really be gone: no hammering). Not
/// shorter: a keepalive ~3 s after authentication was seen to get a short challenge.
pub(crate) const KA_RETRY: Duration = Duration::from_secs(4);

/// How long a single-service bring-up may stay silent before it counts as delivering
/// nothing. On hardware a streaming service shows its first bytes 0.2-0.45 s after the
/// worker starts, so 1.2 s is ample.
const VERIFY_MS: u64 = 1200;
/// 0x090074 event: the firmware's B-CAS card state changed (a card pulled or inserted;
/// hardware 2026-10-28). The stream keeps running, but without a card session its ES
/// come out undescrambled.
pub(crate) const IRQ_BCAS: u16 = 0x0200;
/// The B-CAS message (274 B) the vendor driver reads on [`IRQ_BCAS`]; the top nibble of
/// its first word is the card state (0-5).
const REG_BCAS_MSG: u32 = 0x0822AC;
/// The route slots (decoder window): slots 0-3 at 0x101E, 5-63 at 0x1028 (slot 4, the
/// PAT, has none), each `0x8000 | PID`; the ECM slot each one is descrambled with, 7
/// bits per slot from 0x10A2 (an even slot in the high byte); the mailbox of a slot
/// write while streaming ([`route_write`]).
const REG_SLOT0: u32 = 0x101E;
const REG_SLOT5: u32 = 0x1028;
const REG_ECM_BIND: u32 = 0x10A2;
const REG_ROUTE: u32 = 0x81000;
/// The link-layer key state.
#[derive(Clone, Default)]
pub(crate) struct Link {
    /// Decrypt the stream (the key was obtained this session).
    pub on: bool,
    /// The key the stream is decrypted with.
    pub key: [u8; 16],
    /// The keys that may be the applied one, tried in order when `key` fails the PAT
    /// CRC: the one verified earlier in this process or by a previous one, then keys
    /// handed out to processes that ended before proving any
    /// ([`crate::Device::set_prior_link_keys`]). The device link-encrypts with the
    /// FIRST key it handed out after power-up; a later 07/01/06 (e.g. from a new
    /// process on a device that was not re-plugged) returns a fresh key it never applies.
    pub alt: Vec<[u8; 16]>,
    /// The key the last 07/01/06 handed out. Stays a candidate after another key was
    /// proved: TS left over from a stream of the previous secure session (encrypted with
    /// its key) can come first, the device's own TS then uses this one.
    pub fresh: Option<[u8; 16]>,
    /// `key` decrypted a CRC-valid PAT.
    pub verified: bool,
    /// Section-start PATs in a row no known key decrypted.
    pub fails: u32,
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("on", &self.on)
            .field("alt", &self.alt.len())
            .field("fresh", &self.fresh.is_some())
            .field("verified", &self.verified)
            .field("fails", &self.fails)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------- helpers

/// Wait for the firmware state `want`, write-1-clearing a pending state event (0x0004)
/// on the way; `Err(Firmware)` at 0xFFFF.
fn wait_state(io: &Io, want: u16, timeout_ms: u32) -> Result<()> {
    for _ in (0..=timeout_ms).step_by(20) {
        let s = io.read16(REG_FW_STATE).map_err(Error::into_usb)?;
        if matches!(io.read16(REG_IRQ), Ok(n74) if n74 & 0x0004 != 0) {
            let _ = io.write16(REG_IRQ, 0x0004);
        }
        if s == want {
            return Ok(());
        }
        if s == 0xFFFF {
            return Err(Error::Firmware("firmware state 0xFFFF".into()));
        }
        sleep_ms(20);
    }
    Err(Error::Timeout("firmware state"))
}

/// Sweep pending firmware events before a doorbell (as provided by a third party): read
/// the event register 0x90074, write-1-clear whatever is set, and read the EP 0x83
/// interrupt endpoint, until the register is empty. REQUIRED before each 0xB8 doorbell:
/// if unprocessed event bits accumulate, the next doorbell STALLs the firmware (the
/// remux then stops emitting TS although the state register still reads 2). Returns
/// the bits cleared.
fn drain_events(io: &Io, limit: usize) -> u16 {
    let mut cleared = 0;
    for _ in 0..limit {
        match io.read16(REG_IRQ) {
            Ok(f) if f != 0 => {
                cleared |= f;
                let _ = io.write16(REG_IRQ, f); // W1C the set bits
                let _ = io.drain_interrupt(5); // consume the matching EP 0x83 event
            }
            _ => break,
        }
    }
    cleared
}

/// Ring a ch0 doorbell and wait for the 0x90074 completion bit `mask`:
/// poll the event register, write-1-clear whatever is set, return once `mask` was seen.
/// Best effort -- the stream state is confirmed separately via [`wait_state`]. Unlike
/// [`Io::cmd_ack`] (which polls the ACK register) this matches the vendor's own
/// stream-control path.
fn ring(io: &Io, cmd: u8, sub: u8, par: u16, mask: u16, timeout_ms: u32) -> Result<()> {
    ring_events(io, cmd, sub, par, mask, timeout_ms).0
}

/// [`ring`], also returning every event bit it cleared (before and after the doorbell).
fn ring_events(
    io: &Io,
    cmd: u8,
    sub: u8,
    par: u16,
    mask: u16,
    timeout_ms: u32,
) -> (Result<()>, u16) {
    let mut cleared = drain_events(io, 8);
    let _ = io.doorbell(0, cmd, sub, par);
    let mut seen = 0u16;
    for _ in (0..timeout_ms).step_by(50) {
        let _ = io.drain_interrupt(50);
        if let Ok(f) = io.read16(REG_IRQ)
            && f != 0
        {
            seen |= f;
            let _ = io.write16(REG_IRQ, f); // W1C the set bits
            if seen & mask != 0 {
                return (Ok(()), cleared | seen);
            }
        }
    }
    cleared |= seen;
    (Err(Error::Timeout("stream command completion")), cleared)
}

/// Rewrite one route slot of the running stream (hardware 2026-10-29): the slot, its
/// ECM slot and the PID go to the mailbox at 0x81000, then cmd(5,1,0); the ACK
/// register's low byte is 0 on success. Also says whether a B-CAS card event was
/// cleared on the way.
pub(crate) fn route_write(io: &Io, op: route::Op) -> (Result<()>, bool) {
    let words = [
        u16::from(op.slot) << 8 | u16::from(op.ecm_slot),
        0x8000 | op.pid,
        op.pid,
    ];
    let mut b = [0u8; 6];
    for (d, w) in b.as_chunks_mut::<2>().0.iter_mut().zip(words) {
        *d = w.to_be_bytes();
    }
    if let Err(e) = io.write_block(REG_ROUTE, &b) {
        return (Err(e), false);
    }
    let (r, bits) = ring_events(io, 5, 1, 0, 0x0008, 1000);
    let r = r.and_then(|()| match io.read16(0x80014)? {
        ack if ack & 0xFF == 0 => Ok(()),
        ack => Err(Error::Protocol(format!(
            "route write rejected (0x{ack:04x})"
        ))),
    });
    (r, bits & IRQ_BCAS != 0)
}

/// The register of a route slot.
fn slot_reg(slot: u32) -> u32 {
    if slot < 4 {
        REG_SLOT0 + 2 * slot
    } else {
        REG_SLOT5 + 2 * (slot - 5)
    }
}

fn dump_fault(io: &Io, tag: &str) {
    let r = |a: u32| io.read16(a).unwrap_or(0);
    error!(
        "[{tag}] state(82008)={:04x} detail(8200A)={:04x} 82002={:04x} 82028={:04x} \
         ack(80014/16)={:04x}/{:04x} 90074={:04x}",
        r(0x82008),
        r(0x8200A),
        r(0x82002),
        r(0x82028),
        r(0x80014),
        r(0x80016),
        r(0x90074)
    );
    let mut ln = format!("[{tag}] nz 82000..82080:");
    for a in (0x82000..=0x82080u32).step_by(2) {
        if ln.len() >= 480 - 16 {
            break;
        }
        let v = r(a);
        if v != 0 {
            ln.push_str(&format!(" {a:05x}={v:04x}"));
        }
    }
    error!("{ln}");
}

/// Running-state housekeeping: consume pending event bits so the next doorbell does not
/// STALL. Returns the B-CAS state of a card event among them ([`IRQ_BCAS`]).
pub(crate) fn service_events(io: &Io) -> Option<u8> {
    let mut card = None;
    for reg in [0x90074u32, 0x901B0, 0x90198] {
        let Ok(st) = io.read16(reg) else { continue };
        let mk = io.read16(reg + 4).unwrap_or(0);
        let mut pend = st & mk;
        if reg == REG_IRQ && st & 4 != 0 {
            pend |= 4;
        }
        if reg == REG_IRQ && st & IRQ_BCAS != 0 {
            card = Some(bcas_state(io)); // the vendor reads the message before the clear
        }
        if pend != 0 {
            let _ = io.write16(reg, pend);
        }
    }
    let mut tmp = [0u8; 8];
    let _ = io.read_block(0x82002, &mut tmp);
    let _ = io.read_block(0x82028, &mut tmp);
    let _ = io.write16(0x901B0, 0x0004);
    card
}

/// How long [`reopen_card`] waits for the card to take stage 2 up.
const REOPEN_WAIT: Duration = Duration::from_secs(3);
/// B-CAS states: no card in (after a pull, and stage 2 without a card), stage 2 taken up
/// (the card comes to work once a stream runs), a working card, and a card in that did
/// not work (no video / audio; a bad contact, hardware 2026-10-29).
pub(crate) const NO_CARD: u8 = 0;
const CARD_TAKEN_UP: u8 = 2;
pub(crate) const CARD_WORKS: u8 = 3;
pub(crate) const CARD_FAILED: u8 = 5;

/// Open the B-CAS card session again in place, for a card put back in while the
/// secure session lives (the device idle at 0x11, no stream): stage 2 of the card
/// session alone, then up to [`REOPEN_WAIT`] for the card to take it up (state 2; it
/// works, state 3, once a stream runs: hardware 2026-10-29, state 2 for 4 s at idle).
/// The link key stays. `Ok(false)`: no card in, stage 2 refused, or the card did not
/// take it up; the host restarts the device instead.
pub(crate) fn reopen_card(io: &mut Io) -> Result<bool> {
    let st = io.read16(REG_FW_STATE).map_err(Error::into_usb)?;
    if st != 0x11 {
        info!("b-cas: firmware state 0x{st:04x}, not idle: no card session reopened in place");
        return Ok(false);
    }
    let card = bcas_state(io);
    info!("b-cas: card state {card}: reopening the card session in place");
    if card == NO_CARD {
        return Ok(false);
    }
    if card != CARD_WORKS && cas::bcas_stage2_only(io).is_err() {
        return Ok(false);
    }
    let t0 = Instant::now();
    loop {
        let card = bcas_state(io);
        if card == CARD_TAKEN_UP || card == CARD_WORKS {
            info!(
                "b-cas: card state {card} {} ms after stage 2: streaming again",
                t0.elapsed().as_millis()
            );
            return Ok(true);
        }
        if t0.elapsed() >= REOPEN_WAIT {
            info!(
                "b-cas: card state {card} {} ms after stage 2",
                t0.elapsed().as_millis()
            );
            return Ok(false);
        }
        sleep_ms(50);
    }
}

/// The firmware's B-CAS state, the top nibble of its B-CAS message at
/// [`REG_BCAS_MSG`] (as the vendor driver reads it); [`u8::MAX`] when unreadable.
pub(crate) fn bcas_state(io: &Io) -> u8 {
    io.read16(REG_BCAS_MSG).map_or(u8::MAX, |v| (v >> 12) as u8)
}

/// Send the SECURE_REALTIME keepalive if [`KA_INTERVAL`] elapsed. Called by whichever
/// thread owns the device I/O (the stream worker while one runs, else the host).
pub(crate) fn ka_tick(io: &mut Io) {
    let now = Instant::now();
    if io
        .p
        .ka_last
        .is_none_or(|t| now.duration_since(t) >= KA_INTERVAL)
    {
        let rc = cas::keepalive(io);
        (io.p.ka_last, io.p.ka_fails) = ka_schedule(now, rc.is_ok(), io.p.ka_fails);
        match rc {
            Ok(()) => info!("keepalive sent (secure session kept alive)"),
            Err(e) => {
                warn!(
                    "keepalive FAILED (rc={}) -- session may already be lost",
                    e.code()
                );
                if io.p.ka_fails == 1 {
                    info!("keepalive: retrying in {} s", KA_RETRY.as_secs());
                }
            }
        }
    }
}

/// The keepalive clock after an attempt at `now`: (`ka_last`, `ka_fails`). Due again
/// [`KA_INTERVAL`] after `ka_last`, so the first failure in a row backdates it to make
/// the retry due [`KA_RETRY`] from now (`None` this close to boot: due at once).
fn ka_schedule(now: Instant, ok: bool, fails: u32) -> (Option<Instant>, u32) {
    match (ok, fails) {
        (true, _) => (Some(now), 0),
        (false, 0) => (now.checked_sub(KA_INTERVAL - KA_RETRY), 1),
        (false, n) => (Some(now), n.saturating_add(1)),
    }
}

fn is_pid0(p: &[u8]) -> bool {
    p[0] == packet::SYNC && p[2] == 0x00 && (p[1] & 0x1F) == 0x00
}

/// Pick the link key from one raw (still link-encrypted) PID-0 packet: the device keeps
/// encrypting with the FIRST key it handed out after power-up, and a later 07/01/06 --
/// e.g. from a new process on a device that was not re-plugged -- returns a fresh key it
/// never applies (as provided by a third party). Hardware 2026-10-18: such a
/// warm re-open streamed 85 MB whose PATs all failed the CRC under the fresh key. If the
/// key fails and a known one decrypts the PAT, switch to it.
///
/// Every PAT is checked, not only the first: a stream can start with TS left over from a
/// stream of the previous secure session, encrypted with that session's key, and its own
/// TS follows with the fresh key (Linux VM 2026-10-24: recordings that kept the old key
/// had no PAT and SDT after the first ~0.2 s; `fw::boot` now discards such leftovers).
pub(crate) fn link_key_check(link: &mut Link, raw: &[u8]) {
    if !link.on {
        return;
    }
    match link::check_key(raw, &link.key, link.alt.iter().chain(&link.fresh)) {
        KeyVerdict::NotATest => {}
        KeyVerdict::Current => {
            if !link.verified {
                link.verified = true;
                link.alt = vec![link.key]; // the applied key, for a later re-derive
            }
            link.fails = 0;
        }
        KeyVerdict::Alternate(k) => {
            if link.verified {
                info!("dtvdec: the device switched link keys mid-stream; following it");
            } else {
                info!(
                    "dtvdec: the device still encrypts with the link key of an earlier session \
                     (re-open without a re-plug); using that key"
                );
            }
            link.key = k;
            link.verified = true;
            link.alt = vec![k];
            link.fails = 0;
        }
        KeyVerdict::Neither => {
            link.fails += 1;
            if link.fails == 20 {
                if link.verified {
                    warn!(
                        "dtvdec: the PAT stopped decrypting with the link key in use and with \
                         every other known one; the TS stays unreadable until the device is \
                         USB re-plugged"
                    );
                } else {
                    warn!(
                        "dtvdec: the PAT does not decrypt with the fresh link key{}; the TS \
                         stays unreadable until the device is USB re-plugged",
                        if !link.alt.is_empty() {
                            " nor with the saved ones"
                        } else {
                            " and no earlier key is known"
                        }
                    );
                }
            }
        }
    }
}

/// Link-decrypt whole packets in place, checking the key at each PID-0 packet first
/// ([`link_key_check`]). A switch applies from the PAT that showed it; the packets
/// before it keep the old key.
pub(crate) fn link_decrypt(link: &mut Link, buf: &mut [u8]) {
    let mut from = 0;
    for at in (0..buf.len() / packet::PACKET).map(|i| i * packet::PACKET) {
        if is_pid0(&buf[at..]) {
            let key = link.key;
            link_key_check(link, &buf[at..at + packet::PACKET]);
            if link.key != key {
                link::decrypt(&key, &mut buf[from..at]);
                from = at;
            }
        }
    }
    if link.on {
        link::decrypt(&link.key, &mut buf[from..]);
    }
}

/// What a bring-up streams.
#[derive(Debug, Clone, Copy)]
enum Sel<'a> {
    /// Single-service: AUTO_PID on, PROGRAM_NUMBER = the program; the remux finds that
    /// program's PIDs and (with the card) descrambles them.
    Single(u16),
    /// Full-mux: AUTO_PID off, PROGRAM 0, the route slots of the captured init (the SI
    /// and NULL; used to read the PAT and for the PSI fallback).
    FullMux,
    /// All services: AUTO_PID off, PROGRAM 0, every slot cleared and these routes set;
    /// the worker adds the rest as the PAT and PMTs come ([`route`]).
    Routes(&'a [route::Op]),
}

/// Bring the device (idle 0x11, tuned and locked) up to stream state 2. `trc` is the
/// transcoder image, hot-reloaded on each bring-up.
fn bringup(io: &mut Io, sel: Sel<'_>, trc: &[u8]) -> Result<()> {
    let t0 = Instant::now();
    let _ = io.write16(REG_IRQ, 0x0204);

    // event 6: cmd(4,0,0) (INIT prep), trc hot reload, decoder init
    let _ = ring(io, 4, 0, 0, 0x0008, 2000);
    let r = io.read16(0x80014).unwrap_or(0);
    if r != 0x0400 {
        error!("s1 INIT(4,0) rejected (resp={r:04x})");
        return Err(Error::Protocol(format!(
            "s1 INIT(4,0) rejected (resp={r:04x})"
        )));
    }
    if let Err((off, e)) = bulk_download(io, trc) {
        error!("s1 trc bulk failed at {off}");
        return Err(e.into_usb());
    }
    sleep_ms(50);
    let t_trc = Instant::now();
    let mut ver = [0u8; 8];
    if io.read_block(0x10, &mut ver).is_ok() {
        info!(
            "s1 trc loaded ({} B), ver={:02x}{:02x} {:02x}{:02x}",
            trc.len(),
            ver[0],
            ver[1],
            ver[2],
            ver[3]
        );
    }

    // decoder window init: the exact captured vendor sequence (reaches state 2)
    for w in decoder_init::WRITES {
        let (off, len) = (usize::from(w.off), usize::from(w.len));
        if let Some(data) = decoder_init::BLOB.get(off..off + len) {
            let _ = io.control_out(0xBC, w.w_value, w.w_index, data);
        }
    }
    let t_dec = Instant::now();

    match sel {
        Sel::Single(p) => {
            let _ = io.write_field(0x101A, 15, 1, 1); // AUTO_PID_MODE = 1
            let _ = io.write_field(0x1018, 0, 16, u32::from(p));
            info!("s1: single-service PROGRAM={p} (AUTO_PID on)");
        }
        Sel::FullMux => {
            let _ = io.write_field(0x101A, 15, 1, 0); // AUTO_PID_MODE = 0
            let _ = io.write_field(0x1018, 0, 16, 0); // PROGRAM_NUMBER = 0
        }
        Sel::Routes(ops) => {
            let _ = io.write_field(0x101A, 15, 1, 0); // AUTO_PID_MODE = 0
            let _ = io.write_field(0x1018, 0, 16, 0); // PROGRAM_NUMBER = 0
            let _ = io.write_field(0x101C, 0, 16, 0); // the stream types of the init's program
            // PAYLOAD_UNIT_START_DISABLE_MODE = 1: without it a general slot drops packets
            // without a payload, the PCR-only ones, and TVTest plays nothing (2026-10-29)
            let _ = io.write_field(0x101A, 12, 1, 1);
            // the init's slots (SI and NULL) and ECM bindings
            let slots = (0..4).chain(5..64).map(slot_reg);
            for a in slots.chain((0..32).map(|i| REG_ECM_BIND + 2 * i)) {
                let _ = io.write_field(a, 0, 16, 0);
            }
            // the first routes, the SI and NULL: no ECM yet
            for op in ops {
                let reg = slot_reg(u32::from(op.slot));
                let _ = io.write_field(reg, 0, 16, u32::from(0x8000 | op.pid));
            }
            info!("s1: all services ({} route(s) to start)", ops.len());
        }
    }

    let _ = io.write16(0x082028, 0xFFFF);
    let _ = ring(io, 4, 0x20, 0, 0x0008, 3000);
    let _ = ring(io, 5, 0, 2, 0x0008, 2000);
    if let Err(e) = wait_state(io, 0x0002, 3000) {
        dump_fault(io, "s1 stream start FAULT");
        return Err(e);
    }
    // SYSTEMINFOMASKRUN: start emitting TS now
    let _ = io.write16(0x082028, 0x0000);
    let end = Instant::now();
    info!(
        "s1: state 2 (streaming) [timing ms: init+trc={} decoder={} state2={} total={}]",
        t_trc.duration_since(t0).as_millis(),
        t_dec.duration_since(t_trc).as_millis(),
        end.duration_since(t_dec).as_millis(),
        end.duration_since(t0).as_millis()
    );
    // Keep the secure session alive on the scan's cadence: a scan brings up a new
    // channel every few seconds but each stream is too short for the worker's own 12 s
    // tick to fire. Without it the firmware faults to 0xffff mid-scan and every later
    // channel is lost.
    //
    // The session's first keepalive goes out now, not KA_FIRST after authentication: on a
    // fast host (an LXC on bare metal, 2026-10-25) a recorder's scan kills its tuner
    // command 3-5 s after the start, so none was ever sent, and about a minute later the
    // firmware faulted. Right after the stream starts it draws no short challenge
    // (25 of 25 accepted).
    if io.p.ka_seq == 0 {
        io.p.ka_last = None;
    }
    ka_tick(io);
    Ok(())
}

/// Return a running (state 2) stream to idle (0x11) without touching the worker or the
/// link key. Mirrors the vendor stop: 2 -> 4 (flush, reading EP 0x81 so the
/// stop completes) -> 1 -> idle.
pub(crate) fn to_idle(io: &mut Io) {
    let t0 = Instant::now();
    let tmo = io.p.cmd_timeout_ms;
    let mut st = io.read16(REG_FW_STATE).unwrap_or(0);
    if st == 0x0002 {
        service_events(io);
        let _ = io.doorbell(0, 5, 0, 4); // stop stage; flush while draining EP 0x81
        let mut flush = vec![0u8; 0x40000];
        let tf = Instant::now();
        while tf.elapsed() < Duration::from_millis(6000) {
            let _ = io.usb.read(EP_TS, &mut flush, Duration::from_millis(100));
            service_events(io);
            if io.read16_into(REG_FW_STATE, &mut st) && (st == 0x0004 || st == 0xFFFF) {
                break;
            }
        }
    }
    if st == 0x0004 || st == 0x0002 {
        let _ = ring(io, 5, 0, 1, 0x0008, 2000);
        let _ = wait_state(io, 0x0001, 3000);
    }
    io.read16_into(REG_FW_STATE, &mut st);
    if st == 0x0001 {
        write_idle_params(io);
        let _ = ring(io, 1, 0, 0, 0x0008, tmo);
        let _ = wait_state(io, 0x0011, tmo);
    }
    io.read16_into(REG_FW_STATE, &mut st);
    info!(
        "s1 to-idle: state={st:04x} [{} ms]",
        t0.elapsed().as_millis()
    );
}

/// The host side of System1: what only the thread calling the [`crate::Device`] API
/// touches (the stream worker gets its part when it starts).
#[derive(Debug)]
pub(crate) struct S1 {
    pub trc_path: Option<PathBuf>,
    /// The trc image, read once per path (it is re-sent on every bring-up, but the
    /// 1.5 MB file need not be re-read per tune).
    trc_img: Option<Arc<[u8]>>,
    /// Service to stream next (single-service); `None` = every service of the TS.
    pub service: Option<u16>,
    /// The B-CAS card session (stage 2) and the per-session link key were established.
    /// Done once per authentication and reused across tunes: re-firing stage 2 on every
    /// channel change errors (0x0a80) and the device does not re-key later streams.
    pub cas_ready: bool,
    worker: Option<worker::Worker>,
    /// A worker did not exit on stop: it was left running (detached); no new stream.
    pub worker_stuck: bool,
    pub cur_system: Option<System>,
    /// TSID of the current satellite tune (0 = none / terrestrial).
    pub cur_tsid: u16,
    /// The front end tuned (the worker samples its demod).
    pub fe: FrontEnd,
}

impl S1 {
    pub(crate) fn new() -> Self {
        S1 {
            trc_path: None,
            trc_img: None,
            service: None,
            cas_ready: false,
            worker: None,
            worker_stuck: false,
            cur_system: None,
            cur_tsid: 0,
            fe: FrontEnd::DT295,
        }
    }

    pub(crate) fn set_trc(&mut self, path: Option<PathBuf>) {
        self.trc_path = path;
        self.trc_img = None; // the cached image belongs to the old path
    }

    /// A stream worker is running (the device I/O belongs to it).
    pub(crate) fn running(&self) -> bool {
        self.worker.is_some()
    }

    /// Keep the secure session alive while idle (no stream / worker running). A channel
    /// scan's run of empty channels starts no worker, so nothing sent the keepalive; on
    /// hardware the firmware then stopped acking ~50 s into a 15-channel empty run and
    /// every later channel was lost. Only after a session exists, and not once the
    /// firmware has wedged.
    pub(crate) fn idle_keepalive(&self, sh: &Shared) {
        if self.running() || sh.live.streaming() {
            return;
        }
        let mut g = sh.lock();
        // no stream this session yet (either tuner's opens the card session): it does not
        // lapse
        if !g.card_open
            || g.proto.fw_stall >= FW_STALL_LIMIT
            || g.proto.ka_last.is_some_and(|t| t.elapsed() < KA_INTERVAL)
        {
            return; // not due: no device I/O
        }
        let mut io = Io::new(&sh.usb, &mut g.proto);
        // A faulted firmware needs re-init, not a keepalive (which would just time out).
        if matches!(io.read16(REG_FW_STATE), Ok(st) if st != 0xFFFF) {
            ka_tick(&mut io);
        }
    }

    /// The PSI keeper's TS for the next stream (`None` = terrestrial: keep nothing visible).
    fn psi_tsid(&self) -> Option<u16> {
        (self.cur_system == Some(System::Satellite) && self.cur_tsid != 0).then_some(self.cur_tsid)
    }

    /// Start a worker on the running bring-up (`sink` moves into it).
    fn worker_start(
        &mut self,
        sh: &Arc<Shared>,
        output: worker::Output,
        sink: &mut Option<TsSink>,
    ) -> Result<()> {
        let Some(s) = sink.take() else {
            return Err(Error::State(
                "no TS sink (an earlier stream worker failed)".into(),
            ));
        };
        let now = Instant::now();
        {
            let mut st = sh.lock();
            let tsid = self.psi_tsid();
            // PSI keeper: the TS of the fallback is known silent
            st.psi
                .begin(tsid, matches!(output, worker::Output::Psi(_)), now);
            // Do NOT reset the keepalive clock here. During a channel scan each worker
            // only lives ~2 s, so resetting it per worker meant the keepalive never
            // fired and the firmware faulted to 0xffff in the middle of the scan. The clock
            // persists across channels; a one-time init is enough.
            st.proto.ka_last.get_or_insert(now);
        }
        sh.live.reset_stream();
        let cfg = worker::Config {
            sys: self.cur_system,
            fe: self.fe,
            output,
        };
        self.worker = Some(worker::spawn(Arc::clone(sh), cfg, s)?);
        Ok(())
    }

    /// Stop the running worker; its sink comes back into `sink`. `Err(State)` if it did
    /// not exit (it is left running, detached, and no new stream starts on this device).
    fn join_worker(&mut self, sh: &Shared, sink: &mut Option<TsSink>) -> Result<()> {
        let Some(w) = self.worker.take() else {
            return Ok(());
        };
        match worker::join(sh, w) {
            worker::Joined::Done(s) => {
                *sink = Some(s);
                Ok(())
            }
            worker::Joined::Lost => Ok(()),
            worker::Joined::Stuck => {
                self.worker_stuck = true;
                Err(Error::State("the stream worker did not exit".into()))
            }
        }
    }

    /// Wait (bounded) until the running worker has delivered any TS at all. A service
    /// that can be output streams within a fraction of a second; one that cannot (a pay
    /// service without a contract, or one not on this TS) delivers nothing.
    fn first_bytes(&self, sh: &Shared, ms: u64) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(ms) {
            if sh.live.delivered() > 0 {
                return true;
            }
            sleep_ms(20);
        }
        sh.live.delivered() > 0
    }

    /// Stream this TS in plain full-mux mode (no PID registration) for its PSI -- PAT,
    /// PMTs, SDT/NIT/EIT as far as the device passes them. Used when the tuned service
    /// delivers no TS at all: single-service outputs only descrambled ES, so a pay
    /// service without a contract (or one not on this TS) yields nothing, not even a PAT,
    /// and the host could not see the TS's services. With the PSI flowing they are
    /// visible and get registered; their picture stays dark until the service can be
    /// output. In full-mux the device does pass the PAT of such TSes (BS pay TSes).
    fn psi_fallback(
        &mut self,
        sh: &Arc<Shared>,
        trc: &[u8],
        program: u16,
        sink: &mut Option<TsSink>,
    ) -> Result<()> {
        // Plain full-mux, NO PID registration: registering the PSI whitelist here
        // silenced the stream entirely on BS pay TSes -- 0 bytes for 12 s -- whereas
        // plain full-mux on the same TSes delivered their PAT within 1.2-1.8 s.
        sh.with_io(|io| bringup(io, Sel::FullMux, trc))?;
        info!(
            "s1: service {program} delivers no TS (a pay service without a contract, or not on \
             this TS); streaming this TS in full-mux for its PSI so its services stay visible"
        );
        self.worker_start(sh, worker::Output::Psi(program), sink)
    }

    /// Start streaming the tuned service into `sink` ([`crate::Device::stream_start`]).
    pub(crate) fn stream_start(&mut self, sh: &Arc<Shared>, sink: TsSink) -> Result<()> {
        if self.running() {
            return Err(Error::State("a stream is already running".into()));
        }
        if self.worker_stuck {
            // a previous worker never exited: never start another
            error!("s1 stream: a previous stream worker is stuck; re-plug the device");
            return Err(Error::State("a previous stream worker is stuck".into()));
        }
        let Some(path) = self.trc_path.clone() else {
            return Err(Error::Param("the trc firmware is not set"));
        };
        let st = sh
            .with_io(|io| io.read16(REG_FW_STATE))
            .map_err(Error::into_usb)?;
        if st != 0x11 {
            error!("s1 stream: firmware not idle/ready (state 0x{st:04x})");
            return Err(Error::State(format!(
                "firmware not idle/ready (state 0x{st:04x})"
            )));
        }
        let trc = match &self.trc_img {
            Some(img) => Arc::clone(img),
            None => {
                let img: Arc<[u8]> = match std::fs::read(&path) {
                    Ok(v) if !v.is_empty() => v.into(),
                    other => {
                        error!("s1: cannot read trc '{}'", path.display());
                        return Err(match other {
                            Err(e) => e.into(),
                            Ok(_) => Error::Firmware(format!("'{}' is empty", path.display())),
                        });
                    }
                };
                self.trc_img = Some(Arc::clone(&img));
                img
            }
        };
        self.start_inner(sh, &trc, sink)
    }

    fn start_inner(&mut self, sh: &Arc<Shared>, trc: &[u8], sink: TsSink) -> Result<()> {
        let mut sink = Some(sink);
        let mut derived = None;
        {
            let mut g = sh.lock();
            let st = &mut *g;
            let mut io = Io::new(&sh.usb, &mut st.proto);
            let _ = io.write16(REG_IRQ, 0x0204);
            // Establish the B-CAS card session and the link key while still at idle
            // (0x11), BEFORE the stream sequence: the firmware's stream gate checks the
            // card session, so cmd(5,0,2) faults to 0xFFFF unless stage 2 was accepted
            // (0x0a00) first.
            if !self.cas_ready {
                if !st.card_open {
                    // the card session, once per boot (the second tuner may have opened it)
                    let _ = cas::bcas_stage2(&mut io);
                    st.card_open = true;
                }
                // stage 2 answers 0x0a00 also with no card in; this says more
                let card = bcas_state(&io);
                sh.live.set_card_state(card);
                info!("b-cas: card state {card}");
                st.link.verified = false; // a fresh key: the PAT check re-proves it
                st.link.fails = 0;
                match dtvdec::link_key(&mut io, dtvdec::ENDPOINT_SYSTEM1) {
                    Ok(k) => {
                        st.link.key = k;
                        st.link.fresh = Some(k);
                        self.cas_ready = true;
                        derived = Some(k);
                    }
                    Err(_) => warn!("s1: link key not obtained; TS stays link-encrypted"),
                }
            }
            st.link.on = self.cas_ready; // reuse the cached key (stream_stop cleared the flag)
        }
        // To the host's journal at once: if the device's secure session had lapsed,
        // this is the key it applies, and a recorder may SIGKILL the process before
        // the stream proves it.
        if let Some(k) = derived {
            sh.note_derived_key(k);
        }

        let Some(program) = self.service else {
            // Every service of the TS: start with the SI routes; the worker adds the
            // PMTs, ECMs and ES as they come. Nothing to verify: a TS the card has no
            // contract for still delivers its PSI.
            let mut router = route::Router::new();
            let ops = router.plan();
            sh.with_io(|io| bringup(io, Sel::Routes(&ops), trc))?;
            self.worker_start(sh, worker::Output::All(Box::new(router)), &mut sink)
                .inspect_err(|_| error!("s1: cannot start worker"))?;
            info!("TS stream running on EP 0x81 (System1, all services)");
            return Ok(());
        };

        // One service, tuned directly in one bring-up.
        info!("s1: service {program}");
        sh.with_io(|io| bringup(io, Sel::Single(program), trc))?;
        self.worker_start(sh, worker::Output::Single(program), &mut sink)
            .inspect_err(|_| error!("s1: cannot start worker"))?;
        // A single-service bring-up that delivers nothing at all: a pay service the card
        // has no contract for (the transcoder outputs only descrambled ES), or a service
        // not on this TS (a wrong ServiceID). Stream the TS's PSI instead, so the host
        // still sees (and a scan registers) its services.
        let psi = !self.first_bytes(sh, VERIFY_MS);
        if psi {
            info!("s1: service {program} delivered no TS within {VERIFY_MS} ms");
            self.join_worker(sh, &mut sink)?;
            sh.with_io(to_idle);
            self.psi_fallback(sh, trc, program, &mut sink)?;
        }
        info!(
            "TS stream running on EP 0x81 (System1, {})",
            if psi { "PSI only" } else { "single-service" }
        );
        Ok(())
    }

    /// Stop the stream and return the firmware to idle.
    pub(crate) fn stream_stop(&mut self, sh: &Shared) -> Result<()> {
        if !self.running() {
            return Ok(());
        }
        let mut sink = None;
        let joined = self.join_worker(sh, &mut sink);
        let mut g = sh.lock();
        let st = &mut *g;
        if joined.is_ok() {
            to_idle(&mut Io::new(&sh.usb, &mut st.proto)); // back to idle/ready (0x11)
        }
        // (a stuck worker: do not drive the device under it)
        st.link.on = false;
        joined?;
        info!("TS stream stopped (System1)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_retries_once_early() {
        let t0 = Instant::now() + Duration::from_hours(1); // well clear of boot
        let due = |last: Option<Instant>| last.map(|l| l + KA_INTERVAL - t0);
        // success: due a whole interval later, failures cleared
        assert_eq!(ka_schedule(t0, true, 0), (Some(t0), 0));
        assert_eq!(ka_schedule(t0, true, 5).1, 0);
        // the first failure: due KA_RETRY from now
        let (last, fails) = ka_schedule(t0, false, 0);
        assert_eq!((due(last), fails), (Some(KA_RETRY), 1));
        // a second one in a row: the normal interval again
        let (last, fails) = ka_schedule(t0, false, 1);
        assert_eq!((due(last), fails), (Some(KA_INTERVAL), 2));
        assert_eq!(ka_schedule(t0, false, u32::MAX).1, u32::MAX);
    }

    #[test]
    fn link_key_follows_a_switch_mid_stream() {
        use crate::ts::testutil::{link_encrypt, make_pat_packet};
        let (old, new) = ([1u8; 16], [2u8; 16]);
        let pat = make_pat_packet(&[(0, 0x0010), (101, 0x01f0)], 0, false);
        let mut data = [0x5Au8; packet::PACKET];
        data[..4].copy_from_slice(&[packet::SYNC, 0x01, 0x00, 0x10]);
        // after a session lapse: the old session's key first, then the fresh one
        let plain = [pat, data, pat, data].concat();
        let mut buf = plain.clone();
        let (packets, _) = buf.as_chunks_mut::<{ packet::PACKET }>();
        for (i, p) in packets.iter_mut().enumerate() {
            link_encrypt(if i < 2 { &old } else { &new }, p);
        }
        let mut link = Link {
            on: true,
            key: new,
            alt: vec![old],
            fresh: Some(new),
            ..Link::default()
        };
        link_decrypt(&mut link, &mut buf);
        assert_eq!(buf, plain);
        assert!(link.verified);
        assert_eq!(link.key, new);

        // a damaged PAT changes nothing
        let mut bad = make_pat_packet(&[(0, 0x0010)], 0, true);
        link_encrypt(&new, &mut bad);
        link_key_check(&mut link, &bad);
        assert!(link.verified);
        assert_eq!((link.key, link.fails), (new, 1));
    }

    #[test]
    fn psi_tsid_only_for_satellite() {
        let mut s = S1::new();
        s.cur_system = Some(System::Terrestrial);
        assert_eq!(s.psi_tsid(), None);
        s.cur_system = Some(System::Satellite);
        assert_eq!(s.psi_tsid(), None);
        s.cur_tsid = 0x40F1;
        assert_eq!(s.psi_tsid(), Some(0x40F1));
    }
}
