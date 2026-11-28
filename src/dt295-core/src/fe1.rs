//! System1 front end -- demod + tuners: the exact captured vendor I2C sequences,
//! provided by a third party. It feeds the MB86M01 the TS
//! the System1 stream path expects (the MxL603-derived System2 front end locked but
//! configured the demod TS output differently, so cmd(5,0,2) underran to 0xFFFF).
//!
//! Demod addresses: ISDB-T 0x20, ISDB-S 0x22. The tuners are reached through the demod
//! I2C gateway: write `[FE C0 reg data...]`; read via `[FE C0 FB reg]` then `[FE C1]`.
//!
//! The two-tuner models (PIX-DT295W, PIX-DT195W: one board) have two front ends
//! ([`FrontEnd`]): the same chips twice, their demods a TC90522 with two pairs (0x20 /
//! 0x22 and 0x24 / 0x26) and two TS outputs ([`Port`]).
//! Which pair drives which output is set in the first pair's registers. Its sequences
//! are the PIX-DT295's, changed where the vendor driver's two-tuner board (PIX-DT295W)
//! differs from its one-tuner one.

use tracing::{debug, info, warn};

use crate::proto::{FW_STALL_LIMIT, Io, sleep_ms};
use crate::signal::{cn_s_mdb, cn_t_mdb};
use crate::{Error, Result, Status, System, i2c};

/// The first pair's demods, which also hold the chip's TS routing.
const T0: u8 = 0x20;
const S0: u8 = 0x22;

/// The I2C bus as the front end sees it (the device's type-9 I2C, or a test recorder).
pub trait Bus {
    fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()>;
    fn i2c_write_read(&mut self, slave: u8, wdata: &[u8], rbuf: &mut [u8]) -> Result<()>;
    fn sleep_ms(&mut self, ms: u64);
    /// The firmware stopped acking: every transfer fails from now on.
    fn wedged(&self) -> bool {
        false
    }
}

impl Bus for Io<'_> {
    fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
        i2c::write(self, slave, data)
    }
    fn i2c_write_read(&mut self, slave: u8, wdata: &[u8], rbuf: &mut [u8]) -> Result<()> {
        i2c::write_read(self, slave, wdata, rbuf)
    }
    fn sleep_ms(&mut self, ms: u64) {
        sleep_ms(ms);
    }
    fn wedged(&self) -> bool {
        self.p.fw_stall >= FW_STALL_LIMIT
    }
}

/// A TS output of the two-tuner board's demod chip, each into its own input of the bridge:
/// A into the System1 stream path (EP 0x81) and the raw-TS capture, B into System2
/// (EP 0x86). The PIX-DT295's demod puts its TS out of both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Port {
    #[default]
    A,
    B,
}

/// One front end (tuner), and the TS output its TS goes out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontEnd {
    pair: Pair,
    port: Port,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pair {
    /// The PIX-DT295's demods (a TC90532: one pair).
    Dt295,
    /// A pair of the two-tuner board's TC90522: 0 at 0x20 / 0x22, 1 at 0x24 / 0x26 (hardware
    /// 2026-11-20: all four answer).
    Dt195w(u8),
}

impl Default for FrontEnd {
    fn default() -> Self {
        Self::DT295
    }
}

impl FrontEnd {
    /// The PIX-DT295's front end.
    pub const DT295: Self = Self {
        pair: Pair::Dt295,
        port: Port::A,
    };

    /// The two-tuner board's tuner `n` (0 or 1), its TS out of `port`; `None` past the second.
    pub fn dt195w(n: usize, port: Port) -> Option<Self> {
        let pair = Pair::Dt195w(u8::try_from(n).ok().filter(|&n| n < 2)?);
        Some(Self { pair, port })
    }

    /// The 0-based tuner number.
    pub fn tuner(self) -> usize {
        match self.pair {
            Pair::Dt295 => 0,
            Pair::Dt195w(n) => usize::from(n),
        }
    }

    /// The TS output it uses.
    pub fn port(self) -> Port {
        self.port
    }

    /// The ISDB-T demod.
    fn t(self) -> u8 {
        match self.pair {
            Pair::Dt195w(1) => T0 + 4,
            _ => T0,
        }
    }

    /// The ISDB-S demod.
    fn s(self) -> u8 {
        self.t() + 2
    }

    /// The pair in the TS routing registers: 1 the first, 3 the second. Hardware
    /// 2026-11-20: T0 reg 0x0E = 0x33 put the second pair's ISDB-T TS out of A
    /// (0x31 and 0x30 did not), as 0x0F = 0x30 does for B in the vendor driver.
    fn route_src(self) -> u8 {
        match self.pair {
            Pair::Dt195w(1) => 3,
            _ => 1,
        }
    }

    /// The S0 register whose bits 5-4 are this front end's satellite GPIO (the vendor
    /// sets 0b10 before the ISDB-S demod wakes and 0b11 after it sleeps).
    fn s_gpio(self) -> u8 {
        match self.pair {
            Pair::Dt195w(1) => 0x1D,
            _ => 0x1E,
        }
    }

    /// MxL661 regs 0x01 (crystal load capacitance, bit 7 the clock output), 0x02 and
    /// 0x6D. The vendor's two-tuner board: tuner 1 load 9 pF (INF CapacitanceOfSystem1),
    /// tuner 2 25 pF (CapacitanceOfSystem2) and 0x02 = 0x40, 0x6D = 0x80 instead of
    /// 0x00 / 0x0A (MxLWare's crystal sharing: tuner 2 runs from tuner 1's clock output).
    fn mxl_xtal(self) -> [[u8; 2]; 3] {
        match self.pair {
            Pair::Dt195w(1) => [[0x01, 0x99], [0x02, 0x40], [0x6D, 0x80]],
            _ => [[0x01, 0x89], [0x02, 0x00], [0x6D, 0x0A]],
        }
    }
}

/// A tune whose firmware stopped acking did not happen: an error at once, not a lock
/// wait of seconds (a recorder gives its tuner command ~16 s, and the host recovers the
/// device on this error).
fn not_wedged(b: &impl Bus) -> Result<()> {
    (!b.wedged()).ok_or(Error::Timeout("firmware wedged (not acking commands)"))
}

// ---- captured init tables ----

#[rustfmt::skip]
const DEMOD_T_INIT: [[u8; 2]; 22] = [
    [0x01, 0x00], [0x03, 0x90], [0x06, 0x80], [0x1C, 0x20], [0x1D, 0x56], [0x1E, 0x6A],
    [0x1F, 0x9A], [0x23, 0x4D], [0x25, 0x00], [0x26, 0x00], [0x32, 0x21], [0x3B, 0x21],
    [0x3C, 0x3F], [0x70, 0x38], [0x73, 0x45], [0x74, 0x11], [0x75, 0x02], [0x76, 0x00],
    [0x77, 0x01], [0x7D, 0xD2], [0xE4, 0xF4], [0xF4, 0x80],
];
#[rustfmt::skip]
const DEMOD_S_INIT: [[u8; 2]; 19] = [
    [0x01, 0x90], [0x03, 0x00], [0x04, 0x02], [0x07, 0x33], [0x08, 0x11], [0x10, 0xB0],
    [0x11, 0x00], [0x13, 0x80], [0x15, 0x00], [0x17, 0x01], [0x1C, 0x59], [0x1D, 0xAA],
    [0x1E, 0x20], [0x1F, 0x19], [0x85, 0x7A], [0x87, 0x04], [0x8E, 0x26], [0xA3, 0x91],
    [0xA5, 0x40],
];
const TUNER_S_PARK: [[u8; 2]; 5] = [
    [0x2A, 0xB9],
    [0x29, 0x70],
    [0x28, 0x3E],
    [0x2A, 0x19],
    [0x1C, 0x00],
];

/// One tuner-table op.
#[derive(Clone, Copy)]
enum TOp {
    /// Raw `[FE C0 FF]`.
    Raw,
    /// Tuner write reg = value.
    W(u8, u8),
    /// Read the register (verify, the value is discarded; `old` is the expected one),
    /// then write `new`.
    M { reg: u8, _old: u8, new: u8 },
}

use TOp::{M, Raw, W};

#[rustfmt::skip]
const TUNER_T_INIT: &[TOp] = &[
    Raw,
    W(0x00, 0x01), W(0x96, 0x00), W(0x00, 0x00), W(0x14, 0x13), W(0x6D, 0x8A), W(0x6D, 0x0A),
    W(0xDF, 0x19), W(0x45, 0x1B), W(0xA9, 0x59), W(0xAA, 0x6A), W(0xBE, 0x4C), W(0xCF, 0x2A),
    W(0xD2, 0xF5), W(0xD0, 0x34), W(0x77, 0xE7), W(0x78, 0xE3), W(0x6F, 0x51), W(0x7B, 0x84),
    W(0x7C, 0x9F), W(0x56, 0x41), W(0xCD, 0x64), W(0xC3, 0x2C), W(0x9D, 0x61), W(0xF7, 0x52),
    W(0x58, 0x81), W(0xB1, 0x30), W(0x00, 0x01),
    M { reg: 0x31, _old: 0x41, new: 0xD1 },
    W(0x00, 0x00),
];
/// The rest of the tuner init, after the crystal writes ([`FrontEnd::mxl_xtal`]).
#[rustfmt::skip]
const TUNER_T_INIT_TAIL: &[TOp] = &[
    W(0x0B, 0x01),
    M { reg: 0x04, _old: 0x02, new: 0x01 },
    W(0x05, 0x1B),
    M { reg: 0x08, _old: 0x01, new: 0x05 },
    M { reg: 0x09, _old: 0x42, new: 0x42 },
    M { reg: 0x5E, _old: 0x00, new: 0x00 },
    W(0x0A, 0x00), W(0x0C, 0x00), W(0x13, 0x04), W(0x53, 0xFE), W(0x57, 0x91), W(0x5A, 0xF9),
    W(0x5B, 0x18), W(0x5C, 0xF1), W(0x62, 0xC2), W(0x6E, 0x01), W(0x6F, 0x51), W(0x87, 0x77),
    W(0x88, 0x55), W(0x93, 0x22), W(0x97, 0x02), W(0xBA, 0x30), W(0x98, 0xAF), W(0x9B, 0x20),
    W(0x9C, 0x1E), W(0xA0, 0x18), W(0xA5, 0x09), W(0xC2, 0xA9), W(0xC5, 0x7C), W(0xCD, 0xE8),
    W(0xCE, 0x7F), W(0xD5, 0x03), W(0xD9, 0x04), W(0xEA, 0x0D), W(0xDC, 0x1C), W(0x6D, 0x0A),
    W(0x03, 0x00), W(0x03, 0x01),
];

// ---- I2C helpers (results are ignored where the captured sequence ignores them) ----

fn d_write(b: &mut impl Bus, demod: u8, reg: u8, val: u8) -> Result<()> {
    b.i2c_write(demod, &[reg, val])
}

fn d_read(b: &mut impl Bus, demod: u8, reg: u8) -> Result<u8> {
    let mut v = [0u8; 1];
    b.i2c_write_read(demod, &[reg], &mut v)?;
    Ok(v[0])
}

fn t_write(b: &mut impl Bus, demod: u8, regvals: &[u8]) -> Result<()> {
    let mut buf = [0u8; 8];
    buf[0] = 0xFE;
    buf[1] = 0xC0;
    buf[2..2 + regvals.len()].copy_from_slice(regvals);
    b.i2c_write(demod, &buf[..2 + regvals.len()])
}

fn t_read(b: &mut impl Bus, demod: u8, reg: u8) -> Result<u8> {
    let _ = b.i2c_write(demod, &[0xFE, 0xC0, 0xFB, reg]);
    let mut v = [0u8; 1];
    b.i2c_write_read(demod, &[0xFE, 0xC1], &mut v)?;
    Ok(v[0])
}

fn run_tuner_table(b: &mut impl Bus, demod: u8, tbl: &[TOp]) {
    for op in tbl {
        match *op {
            Raw => {
                let _ = b.i2c_write(demod, &[0xFE, 0xC0, 0xFF]);
            }
            W(reg, val) => {
                let _ = t_write(b, demod, &[reg, val]);
            }
            M { reg, new, .. } => {
                let _ = t_read(b, demod, reg);
                let _ = t_write(b, demod, &[reg, new]);
            }
        }
    }
}

fn park_tuner_s(b: &mut impl Bus, fe: FrontEnd) {
    for rv in TUNER_S_PARK {
        let _ = t_write(b, fe.s(), &rv);
    }
    let _ = t_write(b, fe.s(), &[0x11, 0xC0, 0xA7]);
    if fe.pair == Pair::Dt295 {
        // TS routing on the TC90522 (S reg 0x07 = the source of output A): the
        // two-tuner board sets it at power-on ([`route_init`]) and on a satellite tune only
        let _ = d_write(b, fe.s(), 0x07, 0x13);
    }
}

/// The S0 GPIO of `fe` (bits 5-4 of [`FrontEnd::s_gpio`]) to `bits`. The PIX-DT295
/// sequences write the value the register then holds.
fn s_gpio(b: &mut impl Bus, fe: FrontEnd, bits: u8, dt295: u8) {
    let reg = fe.s_gpio();
    let x = d_read(b, S0, reg);
    let v = match fe.pair {
        Pair::Dt295 => dt295,
        Pair::Dt195w(_) => (x.unwrap_or(0) & 0xCF) | bits,
    };
    let _ = d_write(b, S0, reg, v);
}

/// Tuner 0x10 / 0x11 for a terrestrial frequency: the frequency in 1/64 MHz, low byte
/// first (the vendor driver's MxL603 SetFreq; it accepts 90-770 MHz). A channel centre
/// is an odd number of MHz + 1/7, so the low byte is 0x49 or 0xC9.
fn terr_regs(f_hz: u32) -> (u8, u8) {
    let v = u64::from(f_hz) * 64 / 1_000_000;
    (v as u8, (v >> 8) as u8)
}

/// Tuner 0x31 / 0x7C: the vendor driver switches them at 700 MHz (UHF 51ch and up).
fn terr_band_regs(f_hz: u32) -> (u8, u8) {
    if f_hz >= 700_000_000 {
        (0x91, 0x9F)
    } else {
        (0x81, 0x1F)
    }
}

fn park_s(b: &mut impl Bus, fe: FrontEnd) {
    for [r, v] in [[0x1E, 0x28], [0x0A, 0x00], [0x03, 0x01], [0x17, 0x01]] {
        let _ = d_write(b, fe.s(), r, v);
    }
    s_gpio(b, fe, 0x30, 0x38);
    park_tuner_s(b, fe);
}

/// An ISDB-S demod init value of `fe`: the vendor's two-tuner board writes the first
/// pair's 0x07 / 0x1D / 0x1E as 0x11 / 0xBA / 0x30, and not the second pair's 0x07 /
/// 0x08 (on the TC90522 the first pair's 0x07 / 0x08 route the TS). `None`: not written.
fn s_init_value(fe: FrontEnd, reg: u8, val: u8) -> Option<u8> {
    match (fe.pair, reg) {
        (Pair::Dt195w(0), 0x07) => Some(0x11),
        (Pair::Dt195w(0), 0x1D) => Some(0xBA),
        (Pair::Dt195w(0), 0x1E) => Some(0x30),
        (Pair::Dt195w(1), 0x07 | 0x08) => None,
        _ => Some(val),
    }
}

/// The two-tuner board's TS routing at power-on, as the vendor's two-tuner board sets it:
/// bits 6-4 of T0 reg 0x0E to 7 and of S0 reg 0x07 to 1.
fn route_init(b: &mut impl Bus) {
    if let Ok(x) = d_read(b, T0, 0x0E) {
        let _ = d_write(b, T0, 0x0E, (x & 0x8F) | 0x70);
    }
    if let Ok(x) = d_read(b, S0, 0x07) {
        let _ = d_write(b, S0, 0x07, (x & 0x8F) | 0x10);
    }
}

/// Power-on of every front end of `fe`'s unit ([`power_on_init`]): the PIX-DT295's one,
/// or both of the two-tuner board's, the first first (the second runs from its clock).
pub fn power_on(b: &mut impl Bus, fe: FrontEnd) -> Result<()> {
    match fe.pair {
        Pair::Dt295 => power_on_init(b, fe),
        Pair::Dt195w(_) => {
            for n in 0..2 {
                if let Some(each) = FrontEnd::dt195w(n, fe.port) {
                    power_on_init(b, each)?;
                }
            }
            Ok(())
        }
    }
}

/// Front-end power-on of `fe`: the T demod, T tuner, S demod and S tuner init tables,
/// the S tuner parked. The two-tuner board's second front end runs from the first one's
/// clock: initialize the first one first ([`power_on`]).
pub fn power_on_init(b: &mut impl Bus, fe: FrontEnd) -> Result<()> {
    info!("front-end init (T/S sequence), tuner {}", fe.tuner() + 1);
    for [r, v] in DEMOD_T_INIT {
        let _ = d_write(b, fe.t(), r, v);
    }
    if fe.pair == Pair::Dt195w(0) {
        let _ = d_write(b, fe.t(), 0x04, 0x02); // the vendor's two-tuner board, first pair
    }
    run_tuner_table(b, fe.t(), TUNER_T_INIT);
    for rv in fe.mxl_xtal() {
        let _ = t_write(b, fe.t(), &rv);
    }
    run_tuner_table(b, fe.t(), TUNER_T_INIT_TAIL);
    for [r, v] in DEMOD_S_INIT {
        if let Some(v) = s_init_value(fe, r, v) {
            let _ = d_write(b, fe.s(), r, v);
        }
    }
    // TUNER_S_INIT: 2A=B9, wn 06=10,10,00, 0A=00, 0E=00
    let _ = t_write(b, fe.s(), &[0x2A, 0xB9]);
    let _ = t_write(b, fe.s(), &[0x06, 0x10, 0x10, 0x00]);
    let _ = t_write(b, fe.s(), &[0x0A, 0x00]);
    let _ = t_write(b, fe.s(), &[0x0E, 0x00]);
    park_tuner_s(b, fe);
    if let Pair::Dt195w(n) = fe.pair {
        route_init(b);
        if n == 1 {
            // The vendor's two-tuner board after the second front end's init: bits 5-4 of
            // its ISDB-T demod's reg 0x1C (0x20 from the init table) to 0b11. What it
            // switches is not known (an on/off it has for tuner 2's ISDB-T only).
            if let Ok(x) = d_read(b, fe.t(), 0x1C) {
                let _ = d_write(b, fe.t(), 0x1C, (x & 0xCF) | 0x30);
            }
        }
    }
    Ok(())
}

/// Lock polls in a row with bit 7 of demod reg 0x80 set after which an ISDB-T channel
/// counts as empty (~0.9 s at ~56 ms a poll). Hardware 2026-10-19, a UHF scan: on all
/// 40 empty channels 0x80 went 0x2f -> 0xaf within 0.2 s and stayed there; on the 10
/// present ones bit 7 never showed (0x2f -> 0x2e/0x0e -> 0x06 -> 0x02 -> 0x00 in
/// 0.7-1.0 s). Hardware 2026-11-17: bit 2 can also clear after bit 1 (0x06 -> 0x04 ->
/// 0x00), which [`t_locked`] takes as lock at 0x04.
const T_NO_SIGNAL_POLLS: u32 = 16;

/// The bit of demod reg 0x80 an ISDB-T lock ignores.
const T_LOCK_IGNORED: u8 = 0x04;

/// Whether demod reg 0x80 says ISDB-T lock: every bit clear but bit 2. Bits 2 and 1 clear
/// in either order on the way to 0x00 (0x06 -> 0x04 or 0x02 -> 0x00); 0x04 comes ~0.6 s
/// after the tune and 0x00 ~0.6 s later, but bit 2 can also stay set for hours: hardware
/// 2026-11-17, UHF 21ch on both units, C/N 33 dB, while the other channels reached 0x00.
/// The TS there was free of errors (no TEI, no continuity gap) once the post-lock writes
/// of [`wait_lock_t`] went out; what bit 2 means is not known. Bit 1 stays a no-lock:
/// no channel has been seen held at 0x02. Other drivers: m-tsudo's PT3 driver waits for
/// bit 3 alone (bit 7 gives up), px4_drv for bits 5 and 3 clear and reg 0xB0's low
/// nibble >= 8; Linux's tc90522 needs bits 7-1 clear (bit 2 included), but takes any of
/// bits 7-5 of reg 0x96 as lock before it reads 0x80. With 21ch held at 0x04, 0xB0 read
/// 0xA9 and 0x96 0xC4 / 0xC6, a lock for all three.
pub fn t_locked(reg80: u8) -> bool {
    reg80 & !T_LOCK_IGNORED == 0
}

/// How [`wait_lock_t_bare`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TWait {
    /// Reg 0x80 said lock ([`t_locked`]).
    Locked,
    /// Bit 7 of reg 0x80 stayed set: an empty channel.
    NoSignal,
    /// No lock within the wait (a signal that is slow or will not lock), or the firmware
    /// stopped acking.
    TimedOut,
}

/// Poll demod reg 0x80 until lock ([`t_locked`]) or no-signal; return how it ended. No
/// side-effect writes -- the caller decides what (if anything) to write after it.
pub fn wait_lock_t_bare(b: &mut impl Bus, fe: FrontEnd) -> TWait {
    let t0 = std::time::Instant::now();
    let mut v = 0u8;
    let mut no_signal = 0;
    // Up to ~3.4 s for a signal that is there but slow to lock. An empty channel gives
    // up after T_NO_SIGNAL_POLLS: a channel scan meets 40 of them on UHF alone.
    for _ in 0..60 {
        if b.wedged() {
            return TWait::TimedOut;
        }
        if let Ok(x) = d_read(b, fe.t(), 0x80) {
            v = x;
            no_signal = if x & 0x80 != 0 { no_signal + 1 } else { 0 };
            if no_signal >= T_NO_SIGNAL_POLLS {
                info!(
                    "fe1: T no signal (reg0x80=0x{x:02x}) [{} ms]",
                    t0.elapsed().as_millis()
                );
                return TWait::NoSignal;
            }
            if t_locked(x) {
                info!(
                    "fe1: T locked (reg0x80=0x{x:02x}) [{} ms]",
                    t0.elapsed().as_millis()
                );
                return TWait::Locked;
            }
        }
        b.sleep_ms(50);
    }
    warn!(
        "fe1: T did not lock (reg0x80=0x{v:02x}) [{} ms]",
        t0.elapsed().as_millis()
    );
    TWait::TimedOut
}

/// Lock polls after the post-lock writes for a lock that went away to come back (~1 s).
const T_RELOCK_POLLS: u32 = 20;

/// Wait for ISDB-T lock, then write the stock post-lock registers, which the demod needs
/// before it puts any TS out.
///
/// They are written after a wait that timed out on a signal too: a caller that reads
/// the status on (the System1 start of `dt295 serve`, the CLI's lock wait) can see a
/// lock that came late, and its TS then flows. Written before lock they do not change
/// the lock: hardware 2026-11-17, written ~0.17 s into the wait (reg 0x80 still 0x2F),
/// four channels locked at the usual 0.6-1.1 s with their whole TS. An empty channel is
/// left as it is (a scan meets ~40 of them).
///
/// After the writes a lock must still hold, or come back within ~1 s: the BonDriver and
/// the raw-TS path read the status once, right after the tune, and a weak signal taken
/// at 0x04 could fall back to 0x06 for a moment. On the channels seen so far (C/N
/// 32-34 dB) reg 0x80 never went back.
pub fn wait_lock_t(b: &mut impl Bus, fe: FrontEnd) {
    let end = wait_lock_t_bare(b, fe);
    if end == TWait::NoSignal || b.wedged() {
        return;
    }
    // Stock post-lock writes (the vendor's settle after an ISDB-T lock). The demod puts
    // no TS out before them: hardware 2026-11-17, a channel held at 0x80 = 0x04 gave none
    // until they were written, then its whole TS.
    b.sleep_ms(120);
    let _ = d_write(b, fe.t(), 0x1D, 0x02);
    let _ = d_write(b, fe.t(), 0x1E, 0x2A);
    let _ = d_write(b, fe.t(), 0x1F, 0x8A);
    for r in [0x77, 0x76, 0x5C] {
        if let Ok(rr) = d_read(b, fe.t(), r) {
            let _ = d_write(b, fe.t(), r, rr);
        }
    }
    if end == TWait::Locked {
        hold_lock_t(b, fe);
    }
}

/// After the post-lock writes: return once reg 0x80 says lock, polling up to
/// [`T_RELOCK_POLLS`] times if it does not (logged then).
fn hold_lock_t(b: &mut impl Bus, fe: FrontEnd) {
    let t0 = std::time::Instant::now();
    let mut v = 0u8;
    for i in 0..T_RELOCK_POLLS {
        if b.wedged() {
            return;
        }
        if let Ok(x) = d_read(b, fe.t(), 0x80) {
            if t_locked(x) {
                if i > 0 {
                    info!(
                        "fe1: T lock back after the post-lock writes (reg0x80=0x{x:02x}) [{} ms]",
                        t0.elapsed().as_millis()
                    );
                }
                return;
            }
            v = x;
        }
        b.sleep_ms(50);
    }
    warn!(
        "fe1: T lock lost after the post-lock writes (reg0x80=0x{v:02x}) [{} ms]",
        t0.elapsed().as_millis()
    );
}

/// The forward (pre-lock) part of an ISDB-T tune: park S, wake T, write the PRE block,
/// the tuner frequency/band, the twiddles, and the TS-output registers 0x0E/0x0F/0x01/
/// 0x23. Used by [`tune_t`]; it leaves 0x1D/0x1E/0x1F at the PRE value 0xAA, so the
/// caller sets their final 02/2A/8A values.
fn tune_t_forward(b: &mut impl Bus, fe: FrontEnd, freq_hz: u32) {
    const PRE: [[u8; 2]; 6] = [
        [0x03, 0x00],
        [0x1D, 0xAA],
        [0x1E, 0xAA],
        [0x1F, 0xAA],
        [0x25, 0x00],
        [0x23, 0x4D],
    ];
    let (r10, r11) = terr_regs(freq_hz);
    let (r31, r7c) = terr_band_regs(freq_hz);
    let post = [
        [0x00, 0x01],
        [0x31, r31],
        [0x00, 0x00],
        [0x7C, r7c],
        [0x12, 0x00],
    ];
    info!(
        "fe1: tune T {:.3} MHz (reg10={r10:02x} reg11={r11:02x})",
        f64::from(freq_hz) / 1e6
    );
    park_s(b, fe);
    // Wake the T side the way the vendor does before every terrestrial tune
    // (frontend_all.txt): an ISDB-S tune puts it to sleep (tuner 0x12 = 0, 0x0B = 0,
    // demod 0x03 = 0x90) and without tuner 0x0B = 1 every later ISDB-T tune of the
    // session saw reg 0x80 = 0xaf, no lock (hardware 2026-10-19: a UHF scan after BS/CS).
    let _ = t_write(b, fe.t(), &[0x0B, 0x01]);
    let _ = t_write(b, fe.t(), &[0x12, 0x01]);
    let _ = d_write(b, fe.t(), 0x03, 0x00);
    for [r, v] in PRE {
        let _ = d_write(b, fe.t(), r, v);
    }
    let _ = t_read(b, fe.t(), 0x0D);
    for rv in [[0x0D, 0x00], [0x0F, 0x20], [0x10, r10], [0x11, r11]] {
        let _ = t_write(b, fe.t(), &rv);
    }
    for rv in post {
        let _ = t_write(b, fe.t(), &rv);
    }
    let _ = t_read(b, fe.t(), 0x6E);
    let _ = t_write(b, fe.t(), &[0x6E, 0x81]);
    let _ = t_read(b, fe.t(), 0x14);
    let _ = t_write(b, fe.t(), &[0x14, 0x13]);
    let _ = t_write(b, fe.t(), &[0x12, 0x01]);
    let _ = t_read(b, fe.t(), 0x6E);
    let _ = t_write(b, fe.t(), &[0x6E, 0x01]);
    route_t(b, fe);
    let _ = d_write(b, fe.t(), 0x01, 0x40);
    let _ = d_write(b, fe.t(), 0x23, 0x4C);
}

/// Put the ISDB-T demod's TS out of `fe`'s output: T0 reg 0x0E for A (bits 2-0 the
/// source pair, bits 6-4 kept), 0x0F for B (bits 6-4). The PIX-DT295's demod puts it
/// out of both. Only `fe`'s output is touched, so the other front end's stream goes on.
fn route_t(b: &mut impl Bus, fe: FrontEnd) {
    let src = fe.route_src();
    match (fe.pair, fe.port) {
        (Pair::Dt295, _) => {
            let _ = d_read(b, T0, 0x0E);
            let _ = d_write(b, T0, 0x0E, 0x71);
            let _ = d_write(b, T0, 0x0F, 0x10);
        }
        (_, Port::A) => {
            let x = d_read(b, T0, 0x0E).unwrap_or(0x70);
            let _ = d_write(b, T0, 0x0E, (x & 0x70) | src);
        }
        (_, Port::B) => {
            let _ = d_write(b, T0, 0x0F, src << 4);
        }
    }
}

/// Put the ISDB-S demod's TS out of `fe`'s output instead of the ISDB-T one's: the
/// output released on T0 (0x77), then S0 reg 0x07 for A (bits 2-0, bits 6-4 kept), 0x08
/// for B (bits 6-4). The PIX-DT295 writes both outputs. Each write is followed by the
/// tune sequence's 3 ms.
fn route_s(b: &mut impl Bus, fe: FrontEnd) {
    let src = fe.route_src();
    match (fe.pair, fe.port) {
        (Pair::Dt295, _) => {
            let _ = d_write(b, T0, 0x0E, 0x77);
            b.sleep_ms(3);
            let _ = d_write(b, T0, 0x0F, 0x77);
            b.sleep_ms(3);
            let _ = d_read(b, S0, 0x07);
            b.sleep_ms(3);
            let _ = d_write(b, S0, 0x07, 0x11);
            b.sleep_ms(3);
            let _ = d_write(b, S0, 0x08, 0x10);
            b.sleep_ms(3);
        }
        (_, Port::A) => {
            let _ = d_write(b, T0, 0x0E, 0x77);
            b.sleep_ms(3);
            let x = d_read(b, S0, 0x07).unwrap_or(0x10);
            b.sleep_ms(3);
            let _ = d_write(b, S0, 0x07, (x & 0x70) | src);
            b.sleep_ms(3);
        }
        (_, Port::B) => {
            let _ = d_write(b, T0, 0x0F, 0x77);
            b.sleep_ms(3);
            let _ = d_write(b, S0, 0x08, src << 4);
            b.sleep_ms(3);
        }
    }
}

/// Tune ISDB-T (`freq_hz` = the channel centre) and wait for lock (a missing lock is
/// logged, not an error: the caller reads the status). The stock path: 0x1D/0x1E/0x1F
/// = 02/2A/8A are written AFTER lock, or at the end of a wait that timed out (in
/// [`wait_lock_t`]).
pub fn tune_t(b: &mut impl Bus, fe: FrontEnd, freq_hz: u32) -> Result<()> {
    tune_t_forward(b, fe, freq_hz);
    not_wedged(b)?;
    wait_lock_t(b, fe);
    not_wedged(b)
}

/// Read a register of `fe`'s ISDB-T demod; `0xee` if the read fails. Reg 0x80
/// bit 7 set = no signal / no lock.
pub fn read_t(b: &mut impl Bus, fe: FrontEnd, reg: u8) -> u8 {
    d_read(b, fe.t(), reg).unwrap_or(0xee)
}

/// Read a register of `fe`'s ISDB-S demod; `0xff` if the read fails. Reg 0xC3: lock
/// as [`s_locked`] reads it.
pub fn read_s(b: &mut impl Bus, fe: FrontEnd, reg: u8) -> u8 {
    d_read(b, fe.s(), reg).unwrap_or(0xff)
}

// ---- ISDB-S tune (TUNE_S_SEQ) ----
//
// The captured vendor satellite I2C sequence, replayed with these substitutions:
//   freq (FE C0 00 ..): reg 0x00-0x04 from freq_hz (sat_freq_regs)
//   band (FE C0 09 ..): by frequency (sat_band_reg)
//   reg 0x8F / 0x90:    the broadcast TSID, high / low byte
//   reg 0x50:           by the tuner's status (TUNER_50)
// freq_hz is the L-band IF (as the ini supplies it); 0x8F / 0x90 are the TC90522
// TS-select registers (Linux tc90522s_set_tsid writes them the same way). The capture
// was for BS-15 / TS1 = TSID 0x40F1. A BS TSID packs 0x4 | renumber count (bits 11-9) |
// transponder (8-4) | relative TS (3-0); CS110 TSIDs start with 0x6 or 0x7. The
// PLL-related tuner writes need an ~80 ms settle or reg 0xC3 never clears (as provided
// by a third party). An op with rd > 0 writes its bytes then reads rd bytes. Slave
// 0x20 / 0x22 is the front end's ISDB-T / ISDB-S demod; three ops stand for steps that
// differ by front end or by what is read: GPIO_WAKE (the S GPIO before the S side wakes),
// ROUTE (the TS routing, `route_s`) and TUNER_50 (the tuner's reg 0x50).

/// [`TUNE_S_SEQ`]: the S GPIO to 0b10 (S0 reg 0x1E read, then 0x28 on the PIX-DT295).
const GPIO_WAKE: u8 = 0xF0;
/// [`TUNE_S_SEQ`]: the TS routing ([`route_s`]).
const ROUTE: u8 = 0xF1;
/// [`TUNE_S_SEQ`]: the tuner's two status bytes read (no subaddress), then reg 0x50
/// ([`sat_reg50`]) and a 40 ms wait, as the vendor driver does after a good read. A failed
/// read still writes 0xF0 and waits (the vendor driver then writes nothing).
const TUNER_50: u8 = 0xF2;

struct SOp {
    slave: u8,
    rd: usize,
    data: &'static [u8],
}

const fn s(slave: u8, rd: usize, data: &'static [u8]) -> SOp {
    SOp { slave, rd, data }
}

#[rustfmt::skip]
const TUNE_S_SEQ: &[SOp] = &[
    s(0x20, 0, &[0x25, 0x00]), s(0x20, 0, &[0x23, 0x4d]), s(0x20, 0, &[0x03, 0x90]),
    s(0x20, 0, &[0xfe, 0xc0, 0x12, 0x00]), s(0x20, 0, &[0xfe, 0xc0, 0x0b, 0x00]),
    s(GPIO_WAKE, 0, &[]), s(0x22, 0, &[0xfe, 0xc0, 0x11, 0x00, 0xa7]),
    s(0x22, 0, &[0xfe, 0xc0, 0x2a, 0xb9]), s(0x22, 0, &[0xfe, 0xc0, 0x1c, 0xc0]),
    s(0x22, 0, &[0xfe, 0xc0, 0x29, 0x71]),
    s(0x22, 0, &[0x17, 0x00]), s(0x22, 0, &[0x1e, 0x20]), s(0x22, 0, &[0x0a, 0xff]),
    s(0x22, 0, &[0x03, 0x01]),
    s(0x22, 0, &[0x1c, 0xaa]), s(0x22, 0, &[0x1f, 0x2a]),
    s(0x22, 0, &[0x8f, 0x40]), s(0x22, 0, &[0x90, 0xf1]),
    s(0x22, 0, &[0x1e, 0x28]), s(0x22, 0, &[0x0a, 0x00]), s(0x22, 0, &[0x03, 0x01]),
    s(0x22, 0, &[0xfe, 0xc0, 0x11, 0x00, 0xa7]),
    s(0x22, 0, &[0xfe, 0xc0, 0x2a, 0xb9]), s(0x22, 0, &[0xfe, 0xc0, 0x1c, 0xc0]),
    s(0x22, 0, &[0xfe, 0xc0, 0x29, 0x71]),
    s(0x22, 0, &[0xfe, 0xc0, 0x00, 0x0a, 0x4c, 0x00, 0x00, 0x00]),
    s(0x22, 0, &[0xfe, 0xc0, 0x09, 0x8c]),
    s(0x22, 0, &[0xfe, 0xc0, 0x37, 0xac]), s(0x22, 0, &[0xfe, 0xc0, 0x05, 0x80]),
    s(0x22, 0, &[0xfe, 0xc0, 0x2a, 0xbb]),
    s(ROUTE, 0, &[]),
    s(TUNER_50, 0, &[]),
    s(0x22, 0, &[0x1e, 0x20]), s(0x22, 0, &[0x0a, 0xff]), s(0x22, 0, &[0x03, 0x01]),
    s(0x22, 0, &[0x03, 0x01]),
    s(0x22, 0, &[0x1c, 0x08]), s(0x22, 0, &[0x1f, 0x08]),
    s(0x22, 1, &[0xa5]), s(0x22, 0, &[0xa5, 0x40]),
    s(0x22, 1, &[0xa3]), s(0x22, 0, &[0xa3, 0x80]),
];

/// Whether demod reg 0xC3 says ISDB-S lock: bits 7-4 clear. Linux's tc90522 reads bit 7
/// as the input level, bits 6-5 as the carrier and bit 4 as the lock, and wants reg 0xC5
/// bits 1-0 too (not read here); px4_drv looks at bit 4 alone. Bits 3-0 are no lock bits:
/// PT3's driver reads bit 2 as the TMCC's emergency-alarm flag and bit 1 as its extension
/// flag, so a test for 0xC3 == 0x00 took a TS with either flag set for one without lock.
pub fn s_locked(c3: u8) -> bool {
    c3 & 0xF0 == 0
}

fn wait_lock_s(b: &mut impl Bus, fe: FrontEnd) -> bool {
    let mut v = 0xFFu8;
    for _ in 0..20 {
        // ~8 s at 0.4 s intervals
        if b.wedged() {
            return false;
        }
        if let Ok(x) = d_read(b, fe.s(), 0xC3) {
            v = x;
            if s_locked(x) {
                info!("fe1: S locked (0xC3={x:02x})");
                return true;
            }
        }
        b.sleep_ms(400);
    }
    warn!("fe1: S no lock (0xC3={v:02x})");
    false
}

/// The TSIDs the transponder carries, from its TMCC (demod S reg 0xCE: eight
/// big-endian slots; 0 and 0xFFFF are empty), in slot order. (hardware 2026-10-29)
pub fn ts_ids_s(b: &mut impl Bus, fe: FrontEnd) -> Result<Vec<u16>> {
    let mut x = [0u8; 16];
    b.i2c_write_read(fe.s(), &[0xCE], &mut x)?;
    let (slots, _) = x.as_chunks::<2>();
    Ok(slots
        .iter()
        .map(|&w| u16::from_be_bytes(w))
        .filter(|&t| t != 0 && t != 0xFFFF)
        .collect())
}

/// The TSID the demod streams (demod S reg 0xE6, big-endian; a TSID the transponder
/// does not carry reads as its first TS, the one streamed then; without a TSID,
/// 0xFFFF). (hardware 2026-10-29)
pub fn ts_id_s(b: &mut impl Bus, fe: FrontEnd) -> Result<u16> {
    let mut x = [0u8; 2];
    b.i2c_write_read(fe.s(), &[0xE6], &mut x)?;
    Ok(u16::from_be_bytes(x))
}

/// After a satellite lock: the TS the demod streams, which should be `tsid`. The
/// selection can follow the lock by a moment, so it is read again for up to 1 s.
/// Without a TSID ([`NO_TSID`](crate::chan::NO_TSID)) the demod streams the
/// transponder's first TS. Logs the transponder's TSIDs.
fn check_ts_s(b: &mut impl Bus, fe: FrontEnd, tsid: u16) -> Option<u16> {
    let none = crate::chan::NO_TSID;
    let mut got = None;
    for _ in 0..10 {
        got = ts_id_s(b, fe).ok();
        if tsid == none || got == Some(tsid) {
            break;
        }
        b.sleep_ms(100);
    }
    let ids = ts_ids_s(b, fe).unwrap_or_default();
    if got == Some(none) {
        got = ids.first().copied();
    }
    let all: Vec<String> = ids.iter().map(|t| format!("{t:04x}")).collect();
    match got {
        Some(t) if t == tsid || tsid == none => {
            info!("fe1: S TS {t:04x} (transponder: {})", all.join(" "));
        }
        _ => warn!(
            "fe1: S streams TS {} instead of {tsid:04x} (transponder: {}): check the TSID",
            got.map_or("-".into(), |t| format!("{t:04x}")),
            all.join(" ")
        ),
    }
    got
}

/// Tuner reg 0x09 for the IF `f_hz`: the HORUS3A's G_CTL (bits 7-5) and F_CTL (bits 4-0)
/// by frequency, the table of Linux's horus3a.c, which rounds the frequency to MHz first.
/// The BS / CS values used before, 0x8C and 0x44, are this table's at BS-15's 1318 MHz
/// and below 1800 MHz: the CS transponders from ND12 (1813 MHz) and most BS ones got the
/// value of another band.
fn sat_band_reg(f_hz: u32) -> u8 {
    // (below this many MHz, F_CTL, G_CTL)
    const BANDS: [(u32, u8, u8); 9] = [
        (975, 0x1C, 1),
        (1050, 0x18, 2),
        (1150, 0x14, 2),
        (1250, 0x10, 3),
        (1350, 0x0C, 4),
        (1450, 0x0A, 4),
        (1600, 0x07, 5),
        (1800, 0x04, 2),
        (2000, 0x02, 1),
    ];
    let mhz = (f_hz + 500_000) / 1_000_000;
    let (f_ctl, g_ctl) = BANDS
        .iter()
        .find(|&&(below, ..)| mhz < below)
        .map_or((0x00, 0), |&(_, f, g)| (f, g));
    (g_ctl << 5) | f_ctl
}

/// Tuner reg 0x00-0x04 for the IF `f_hz`, as the vendor driver writes them: the IF
/// rounded to MHz; at 1100 MHz and below the divider 4 (reg 0x04 = 0x80, twice the MHz),
/// above it 2 (0x00); reg 0x00-0x01 = that value << 1, big-endian, reg 0x02 / 0x03 = 0.
/// The formula of Linux's horus3a.c, which takes divider 4 up to 1155 MHz.
fn sat_freq_regs(f_hz: u32) -> [u8; 5] {
    let mhz = (f_hz + 500_000) / 1_000_000;
    let (ms, r04) = if mhz <= 1100 {
        (2 * mhz, 0x80)
    } else {
        (mhz, 0x00)
    };
    let [hi, lo] = ((ms << 1) as u16).to_be_bytes();
    [hi, lo, 0x00, 0x00, r04]
}

/// Tuner reg 0x50 from the second of the tuner's two status bytes, as the vendor driver
/// picks it: 0xF1 when bits 6-5 are both set, else 0xF0. A failed read (`None`) keeps
/// 0xF0, the value written before (the vendor driver then writes nothing).
fn sat_reg50(status: Option<u8>) -> u8 {
    if status.is_some_and(|x| x & 0x60 == 0x60) {
        0xF1
    } else {
        0xF0
    }
}

/// Tune ISDB-S (`freq_hz` = IF, `tsid` = the wanted TS) and wait for lock. Returns the
/// TSID the demod streams, when it could be read. A `tsid` up to
/// [`MAX_TS_NUMBER`](crate::chan::MAX_TS_NUMBER) is a TS number: the tune selects no TS,
/// then the transponder's TS whose TSID has those low 3 bits ([`select_ts_number`]); an
/// error when it has none.
pub fn tune_s(b: &mut impl Bus, fe: FrontEnd, freq_hz: u32, tsid: u16) -> Result<Option<u16>> {
    let number = (tsid <= crate::chan::MAX_TS_NUMBER).then_some(tsid);
    let tsid = if number.is_some() {
        crate::chan::NO_TSID
    } else {
        tsid
    };
    let freq = sat_freq_regs(freq_hz);
    let [r8f, r90] = tsid.to_be_bytes();
    let band = sat_band_reg(freq_hz);
    info!(
        "fe1: tune S {:.3} MHz tsid={tsid:04x} (freq={:02x}{:02x} 04={:02x} 09={band:02x} 8f={r8f:02x} 90={r90:02x})",
        f64::from(freq_hz) / 1e6,
        freq[0],
        freq[1],
        freq[4]
    );
    for op in TUNE_S_SEQ {
        let slave = match op.slave {
            GPIO_WAKE => {
                let reg = fe.s_gpio();
                let x = d_read(b, S0, reg);
                b.sleep_ms(3);
                let v = match fe.pair {
                    Pair::Dt295 => 0x28,
                    Pair::Dt195w(_) => (x.unwrap_or(0) & 0xCF) | 0x20,
                };
                let _ = d_write(b, S0, reg, v);
                b.sleep_ms(3);
                continue;
            }
            ROUTE => {
                route_s(b, fe);
                continue;
            }
            TUNER_50 => {
                let mut st = [0u8; 2];
                let ok = b.i2c_write_read(fe.s(), &[0xFE, 0xC1], &mut st).is_ok();
                b.sleep_ms(3);
                let v = sat_reg50(ok.then_some(st[1]));
                debug!(
                    "fe1: S tuner status {:02x}{:02x} (read: {ok}): reg 0x50 = {v:02x}",
                    st[0], st[1]
                );
                let _ = b.i2c_write(fe.s(), &[0xFE, 0xC0, 0x50, v]);
                b.sleep_ms(40);
                continue;
            }
            T0 => fe.t(),
            _ => fe.s(),
        };
        let mut d = [0u8; 8];
        let len = op.data.len();
        d[..len].copy_from_slice(op.data);
        let mut settle = 3;
        if op.slave == S0 && op.rd == 0 {
            match (len, d[0], d[1], d[2]) {
                (8, 0xFE, 0xC0, 0x00) => d[3..].copy_from_slice(&freq),
                (4, 0xFE, 0xC0, 0x09) => d[3] = band,
                (2, 0x8F, ..) => d[1] = r8f,
                (2, 0x90, ..) => d[1] = r90,
                _ => {}
            }
        }
        if op.rd == 0 {
            let _ = b.i2c_write(slave, &d[..len]);
            if op.slave == S0
                && len >= 3
                && d[0] == 0xFE
                && d[1] == 0xC0
                && matches!(d[2], 0x00 | 0x1C | 0x29 | 0x2A)
            {
                settle = 80; // PLL settle
            }
        } else {
            let mut rb = [0u8; 4];
            let _ = b.i2c_write_read(slave, &d[..len], &mut rb[..op.rd]);
        }
        b.sleep_ms(settle);
    }
    not_wedged(b)?;
    let locked = wait_lock_s(b, fe);
    not_wedged(b)?;
    if !locked {
        return Ok(None);
    }
    match number {
        Some(n) => select_ts_number(b, fe, n),
        None => Ok(check_ts_s(b, fe, tsid)),
    }
}

/// After a lock without a TSID: select the transponder's TS whose TSID has the low 3 bits
/// `n` (the relative TS number, as BonDriver_PTx-ST_mod reads a TSID of 7 or less; Linux's
/// tc90522 writes the TSID it picks to reg 0x8F / 0x90 the same way). The TMCC can follow
/// the lock by a moment, so it is read again for up to 1 s. An error when the transponder
/// has no such TS, so the channel fails instead of streaming another TS.
fn select_ts_number(b: &mut impl Bus, fe: FrontEnd, n: u16) -> Result<Option<u16>> {
    let mut ids = Vec::new();
    for _ in 0..10 {
        ids = ts_ids_s(b, fe).unwrap_or_default();
        if !ids.is_empty() {
            break;
        }
        b.sleep_ms(100);
    }
    let Some(&t) = ids.iter().find(|&&t| t & crate::chan::MAX_TS_NUMBER == n) else {
        let all: Vec<String> = ids.iter().map(|t| format!("{t:04x}")).collect();
        warn!("fe1: S no TS {n} (transponder: {})", all.join(" "));
        return Err(Error::Param("the transponder has no TS of that number"));
    };
    info!("fe1: S TS {n} is TSID {t:04x}");
    let [hi, lo] = t.to_be_bytes();
    b.i2c_write(fe.s(), &[0x8F, hi])?;
    b.i2c_write(fe.s(), &[0x90, lo])?;
    Ok(check_ts_s(b, fe, t))
}

/// ISDB-S demod sample, the registers the vendor driver polls while streaming (as
/// provided by a third party): lock 0xC3 ([`s_locked`]), C/N 0xBC (2 B) and the two 3-byte error
/// counters 0xEB and 0xF0 ("mostly 0, now and then non-zero"). (lock, cn_raw, eb, f0).
pub fn sample_s(b: &mut impl Bus, fe: FrontEnd) -> Result<(u8, u32, u32, u32)> {
    let lock = d_read(b, fe.s(), 0xC3)?;
    let mut x = [0u8; 3];
    b.i2c_write_read(fe.s(), &[0xBC], &mut x[..2])?;
    let cn = u32::from(u16::from_be_bytes([x[0], x[1]]));
    b.i2c_write_read(fe.s(), &[0xEB], &mut x)?;
    let eb = be24(x);
    b.i2c_write_read(fe.s(), &[0xF0], &mut x)?;
    let f0 = be24(x);
    Ok((lock, cn, eb, f0))
}

/// ISDB-T demod sample for the live signal level: lock 0x80 ([`t_locked`]) and the 3-byte
/// C/N at 0x8B. (lock, cn_raw).
pub fn sample_t(b: &mut impl Bus, fe: FrontEnd) -> Result<(u8, u32)> {
    let lock = d_read(b, fe.t(), 0x80)?;
    let mut x = [0u8; 3];
    b.i2c_write_read(fe.t(), &[0x8B], &mut x)?;
    Ok((lock, be24(x)))
}

fn be24(x: [u8; 3]) -> u32 {
    (u32::from(x[0]) << 16) | (u32::from(x[1]) << 8) | u32::from(x[2])
}

/// Lock and C/N right after a tune. The real C/N: the TC90522 CNR is the 3-byte value
/// at demod reg 0x8B (ISDB-T) / the 2-byte value at 0xBC (ISDB-S), run through the
/// polynomial fits (an older reading of the single byte 0x82 as dB showed ~108).
pub fn read_status(b: &mut impl Bus, fe: FrontEnd, sys: System) -> Result<Status> {
    let mut st = Status {
        locked: match sys {
            System::Terrestrial => t_locked(d_read(b, fe.t(), 0x80).unwrap_or(0xFF)),
            System::Satellite => s_locked(d_read(b, fe.s(), 0xC3).unwrap_or(0xFF)),
        },
        ..Status::default()
    };
    if let Ok((raw, cn_mdb)) = read_cn(b, fe, sys) {
        st.raw = raw;
        st.cn_mdb = cn_mdb;
    }
    Ok(st)
}

/// Read the C/N alone, in one I2C transfer: its raw register value and the C/N in 0.001 dB.
/// Half of [`read_status`], for a caller that must not hold the bus long (the raw-TS stream
/// loop, which cannot pump the TS while it reads).
pub fn read_cn(b: &mut impl Bus, fe: FrontEnd, sys: System) -> Result<(u32, i32)> {
    match sys {
        System::Terrestrial => {
            let mut cn = [0u8; 3];
            b.i2c_write_read(fe.t(), &[0x8B], &mut cn)?;
            let raw = be24(cn);
            Ok((raw, cn_t_mdb(raw)))
        }
        System::Satellite => {
            let mut cn = [0u8; 2];
            b.i2c_write_read(fe.s(), &[0xBC], &mut cn)?;
            let raw = u32::from(u16::from_be_bytes(cn));
            Ok((raw, cn_s_mdb(raw)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    /// Records the I2C traffic: lock registers read 0x00, every other read 0x5A.
    #[derive(Default)]
    struct Rec(String);

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    impl Bus for Rec {
        fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
            write!(self.0, " W{slave:02x}={}", hex(data)).unwrap();
            Ok(())
        }
        fn i2c_write_read(&mut self, slave: u8, w: &[u8], r: &mut [u8]) -> Result<()> {
            write!(self.0, " R{slave:02x}={}/{}", hex(w), r.len()).unwrap();
            r.fill(0x5A);
            if w.len() == 1 && ((slave == 0x20 && w[0] == 0x80) || (slave == 0x22 && w[0] == 0xC3))
            {
                r[0] = 0;
            }
            Ok(())
        }
        fn sleep_ms(&mut self, ms: u64) {
            write!(self.0, " S{ms}").unwrap();
        }
    }

    fn rec(f: impl FnOnce(&mut Rec)) -> String {
        let mut r = Rec::default();
        f(&mut r);
        r.0
    }

    // The expected I2C sequences, byte for byte.
    const POWER_ON: &str = " W20=0100 W20=0390 W20=0680 W20=1c20 W20=1d56 W20=1e6a W20=1f9a \
        W20=234d W20=2500 W20=2600 W20=3221 W20=3b21 W20=3c3f W20=7038 W20=7345 W20=7411 W20=7502 \
        W20=7600 W20=7701 W20=7dd2 W20=e4f4 W20=f480 W20=fec0ff W20=fec00001 W20=fec09600 \
        W20=fec00000 W20=fec01413 W20=fec06d8a W20=fec06d0a W20=fec0df19 W20=fec0451b W20=fec0a959 \
        W20=fec0aa6a W20=fec0be4c W20=fec0cf2a W20=fec0d2f5 W20=fec0d034 W20=fec077e7 W20=fec078e3 \
        W20=fec06f51 W20=fec07b84 W20=fec07c9f W20=fec05641 W20=fec0cd64 W20=fec0c32c W20=fec09d61 \
        W20=fec0f752 W20=fec05881 W20=fec0b130 W20=fec00001 W20=fec0fb31 R20=fec1/1 W20=fec031d1 \
        W20=fec00000 W20=fec00189 W20=fec00200 W20=fec06d0a W20=fec00b01 W20=fec0fb04 R20=fec1/1 \
        W20=fec00401 W20=fec0051b W20=fec0fb08 R20=fec1/1 W20=fec00805 W20=fec0fb09 R20=fec1/1 \
        W20=fec00942 W20=fec0fb5e R20=fec1/1 W20=fec05e00 W20=fec00a00 W20=fec00c00 W20=fec01304 \
        W20=fec053fe W20=fec05791 W20=fec05af9 W20=fec05b18 W20=fec05cf1 W20=fec062c2 W20=fec06e01 \
        W20=fec06f51 W20=fec08777 W20=fec08855 W20=fec09322 W20=fec09702 W20=fec0ba30 W20=fec098af \
        W20=fec09b20 W20=fec09c1e W20=fec0a018 W20=fec0a509 W20=fec0c2a9 W20=fec0c57c W20=fec0cde8 \
        W20=fec0ce7f W20=fec0d503 W20=fec0d904 W20=fec0ea0d W20=fec0dc1c W20=fec06d0a W20=fec00300 \
        W20=fec00301 W22=0190 W22=0300 W22=0402 W22=0733 W22=0811 W22=10b0 W22=1100 W22=1380 \
        W22=1500 W22=1701 W22=1c59 W22=1daa W22=1e20 W22=1f19 W22=857a W22=8704 W22=8e26 W22=a391 \
        W22=a540 W22=fec02ab9 W22=fec006101000 W22=fec00a00 W22=fec00e00 W22=fec02ab9 W22=fec02970 \
        W22=fec0283e W22=fec02a19 W22=fec01c00 W22=fec011c0a7 W22=0713";
    // tune_t: the vendor's T wake (fec00b01 fec01201 0300) follows the S park.
    const TUNE_T_HEAD: &str = " W22=1e28 W22=0a00 W22=0301 W22=1701 R22=1e/1 W22=1e38 \
        W22=fec02ab9 W22=fec02970 W22=fec0283e W22=fec02a19 W22=fec01c00 W22=fec011c0a7 W22=0713 \
        W20=fec00b01 W20=fec01201 W20=0300 \
        W20=0300 W20=1daa W20=1eaa W20=1faa W20=2500 W20=234d W20=fec0fb0d R20=fec1/1 W20=fec00d00 \
        W20=fec00f20 W20=fec01049";
    const TUNE_T_TAIL: &str = " W20=fec00001 W20=fec03181 W20=fec00000 W20=fec07c1f W20=fec01200 \
        W20=fec0fb6e R20=fec1/1 W20=fec06e81 W20=fec0fb14 R20=fec1/1 W20=fec01413 W20=fec01201 \
        W20=fec0fb6e R20=fec1/1 W20=fec06e01 R20=0e/1 W20=0e71 W20=0f10 W20=0140 W20=234c R20=80/1 \
        S120 W20=1d02 W20=1e2a W20=1f8a R20=77/1 W20=775a R20=76/1 W20=765a R20=5c/1 W20=5c5a \
        R20=80/1";
    const TUNE_S_1318_40F1: &str = " W20=2500 S3 W20=234d S3 W20=0390 S3 W20=fec01200 S3 \
        W20=fec00b00 S3 R22=1e/1 S3 W22=1e28 S3 W22=fec01100a7 S3 W22=fec02ab9 S80 W22=fec01cc0 \
        S80 W22=fec02971 S80 W22=1700 S3 W22=1e20 S3 W22=0aff S3 W22=0301 S3 W22=1caa S3 W22=1f2a \
        S3 W22=8f40 S3 W22=90f1 S3 W22=1e28 S3 W22=0a00 S3 W22=0301 S3 W22=fec01100a7 S3 \
        W22=fec02ab9 S80 W22=fec01cc0 S80 W22=fec02971 S80 W22=fec0000a4c000000 S80 W22=fec0098c \
        S3 W22=fec037ac S3 W22=fec00580 S3 W22=fec02abb S80 W20=0e77 S3 W20=0f77 S3 R22=07/1 S3 \
        W22=0711 S3 W22=0810 S3 R22=fec1/2 S3 W22=fec050f0 S40 W22=1e20 S3 W22=0aff S3 W22=0301 S3 \
        W22=0301 S3 W22=1c08 S3 W22=1f08 S3 R22=a5/1 S3 W22=a540 S3 R22=a3/1 S3 W22=a380 S3 \
        R22=c3/1";
    const TUNE_S_1613_7040: &str = " W20=2500 S3 W20=234d S3 W20=0390 S3 W20=fec01200 S3 \
        W20=fec00b00 S3 R22=1e/1 S3 W22=1e28 S3 W22=fec01100a7 S3 W22=fec02ab9 S80 W22=fec01cc0 \
        S80 W22=fec02971 S80 W22=1700 S3 W22=1e20 S3 W22=0aff S3 W22=0301 S3 W22=1caa S3 W22=1f2a \
        S3 W22=8f70 S3 W22=9040 S3 W22=1e28 S3 W22=0a00 S3 W22=0301 S3 W22=fec01100a7 S3 \
        W22=fec02ab9 S80 W22=fec01cc0 S80 W22=fec02971 S80 W22=fec0000c9a000000 S80 W22=fec00944 \
        S3 W22=fec037ac S3 W22=fec00580 S3 W22=fec02abb S80 W20=0e77 S3 W20=0f77 S3 R22=07/1 S3 \
        W22=0711 S3 W22=0810 S3 R22=fec1/2 S3 W22=fec050f0 S40 W22=1e20 S3 W22=0aff S3 W22=0301 S3 \
        W22=0301 S3 W22=1c08 S3 W22=1f08 S3 R22=a5/1 S3 W22=a540 S3 R22=a3/1 S3 W22=a380 S3 \
        R22=c3/1";

    #[test]
    fn power_on_sequence() {
        assert_eq!(
            rec(|b| power_on_init(b, FrontEnd::DT295).unwrap()),
            POWER_ON
        );
    }

    fn dt195w(n: usize, port: Port) -> FrontEnd {
        FrontEnd::dt195w(n, port).unwrap()
    }

    /// The writes (" Wss=...") of a recorded sequence to slave `slave`.
    fn writes_to(rec: &str, slave: &str) -> Vec<String> {
        let tag = format!("W{slave}=");
        rec.split(' ')
            .filter_map(|op| op.strip_prefix(&tag).map(str::to_owned))
            .collect()
    }

    #[test]
    fn dt195w_power_on() {
        // the first pair: the PIX-DT295's sequence, plus T 04=02, S 07/1D/1E as the
        // vendor's two-tuner board has them and its routing, and no S 07=13 at the park
        let first = rec(|b| power_on_init(b, dt195w(0, Port::A)).unwrap());
        assert!(first.contains(" W20=f480 W20=0402 W20=fec0ff "), "{first}");
        assert!(first.contains(" W20=fec00189 W20=fec00200 W20=fec06d0a "));
        assert!(first.contains(" W22=0711 W22=0811 "));
        assert!(first.contains(" W22=1dba W22=1e30 "));
        assert!(first.ends_with(" W22=fec011c0a7 R20=0e/1 W20=0e7a R22=07/1 W22=071a"));
        // the second pair: 0x24 / 0x26, crystal sharing, no S 07/08, the T1 0x1C bits
        let second = rec(|b| power_on_init(b, dt195w(1, Port::B)).unwrap());
        assert!(second.contains(" W24=fec00199 W24=fec00240 W24=fec06d80 W24=fec00b01 "));
        let s1 = writes_to(&second, "26");
        assert!(
            !s1.iter()
                .any(|w| w.starts_with("07") || w.starts_with("08")),
            "{s1:?}"
        );
        assert!(s1.contains(&"1daa".to_owned()) && s1.contains(&"1e20".to_owned()));
        assert!(second.ends_with(" R20=0e/1 W20=0e7a R22=07/1 W22=071a R24=1c/1 W24=1c7a"));
        // the second pair's demods only, but for the routing on the first pair's
        assert_eq!(writes_to(&second, "20"), ["0e7a"]);
        assert_eq!(writes_to(&second, "22"), ["071a"]);
    }

    #[test]
    fn dt195w_routing_touches_only_its_own_output() {
        // ISDB-T: output A takes the source in T0 0x0E bits 2-0, B in 0x0F bits 6-4
        let a = rec(|b| tune_t(b, dt195w(1, Port::A), 557_142_857).unwrap());
        assert_eq!(writes_to(&a, "20"), ["0e53"]);
        let b2 = rec(|b| tune_t(b, dt195w(1, Port::B), 557_142_857).unwrap());
        assert_eq!(writes_to(&b2, "20"), ["0f30"]);
        let a0 = rec(|b| tune_t(b, dt195w(0, Port::A), 557_142_857).unwrap());
        assert!(a0.contains(" R20=0e/1 W20=0e51 W20=0140 "), "{a0}");
        // ISDB-S: the output released on T0, the source on S0 0x07 (A) / 0x08 (B), and the
        // second front end's GPIO in S0 0x1D
        let s = rec(|b| {
            tune_s(b, dt195w(1, Port::B), 1_318_000_000, 0x40F1).unwrap();
        });
        assert_eq!(writes_to(&s, "20"), ["0f77"]);
        assert_eq!(writes_to(&s, "22"), ["1d6a", "0830"]);
        let s0 = rec(|b| {
            tune_s(b, dt195w(0, Port::A), 1_318_000_000, 0x40F1).unwrap();
        });
        assert!(s0.contains(" W20=0e77 S3 R22=07/1 S3 W22=0751 S3 "), "{s0}");
        assert!(!s0.contains("W20=0f") && !s0.contains("W22=08"));
    }

    #[test]
    fn read_s_and_t_surface_the_lock_registers() {
        let mut b = Rec::default();
        // The lock registers read 0 in the mock (= locked); the wrappers pass that through.
        assert_eq!(
            read_t(&mut b, FrontEnd::DT295, 0x80),
            0x00,
            "T0 0x80 bit7 clear = terr lock"
        );
        assert_eq!(
            read_s(&mut b, FrontEnd::DT295, 0xC3),
            0x00,
            "S0 0xC3 == 0 = sat lock"
        );
        // Any other register reads the mock's 0x5A -- i.e. not locked.
        assert_eq!(read_t(&mut b, FrontEnd::DT295, 0x10), 0x5A);
        assert_eq!(read_s(&mut b, FrontEnd::DT295, 0x10), 0x5A);
        // The reads go to the two demod slaves (0x20 terr, 0x22 sat), one byte each.
        assert_eq!(b.0, " R20=80/1 R22=c3/1 R20=10/1 R22=10/1");
    }

    #[test]
    fn tune_t_sequence() {
        let want = |r11: &str| format!("{TUNE_T_HEAD} W20=fec011{r11}{TUNE_T_TAIL}");
        assert_eq!(
            rec(|b| tune_t(b, FrontEnd::DT295, 557_142_857).unwrap()),
            want("8b")
        );
        assert_eq!(
            rec(|b| tune_t(b, FrontEnd::DT295, 473_142_857).unwrap()),
            want("76")
        );
        assert_eq!(terr_regs(557_142_857), (0x49, 0x8b));
        // odd channel number -> bit 7 of reg 0x10
        assert_eq!(terr_regs(479_142_857), (0xC9, 0x77));
        // CATV and the VHF range the same way (C13, C22, C63)
        assert_eq!(terr_regs(111_142_857), (0xC9, 0x1B));
        assert_eq!(terr_regs(167_142_857), (0xC9, 0x29));
        assert_eq!(terr_regs(465_142_857), (0x49, 0x74));
        // 700 MHz and up (UHF 51ch, 52ch): two tuner settings change
        let tail = rec(|b| tune_t(b, FrontEnd::DT295, 701_142_857).unwrap());
        assert!(tail.contains("W20=fec01049 W20=fec011af"), "{tail}");
        assert!(
            tail.contains("W20=fec03191 W20=fec00000 W20=fec07c9f"),
            "{tail}"
        );
        assert_eq!(terr_band_regs(699_999_999), (0x81, 0x1F));
    }

    /// Demod reg 0x80 (T) and 0xC3 (S) reads follow `script` (its last value repeats); the
    /// I2C log keeps the writes.
    struct Lock {
        script: Vec<u8>,
        polls: usize,
        writes: String,
    }

    impl Bus for Lock {
        fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
            write!(self.writes, " W{slave:02x}={}", hex(data)).unwrap();
            Ok(())
        }
        fn i2c_write_read(&mut self, slave: u8, w: &[u8], r: &mut [u8]) -> Result<()> {
            r.fill(0x5A);
            if (slave == T0 && w == [0x80]) || (slave == S0 && w == [0xC3]) {
                r[0] = self.script[self.polls.min(self.script.len() - 1)];
                self.polls += 1;
            }
            Ok(())
        }
        fn sleep_ms(&mut self, _ms: u64) {}
    }

    /// (lock polls, whether the post-lock writes went out) of `wait_lock_t` on the reg 0x80
    /// values `script`.
    fn wait_t(script: &[u8]) -> (usize, bool) {
        let mut b = Lock {
            script: script.to_vec(),
            polls: 0,
            writes: String::new(),
        };
        wait_lock_t(&mut b, FrontEnd::DT295);
        (b.polls, b.writes.contains("W20=1d02"))
    }

    #[test]
    fn t_lock_wait_gives_up_on_an_empty_channel() {
        // the hardware's empty channel: 0x2f, then 0xaf from ~0.15 s on
        assert_eq!(wait_t(&[0x2f, 0x2f, 0xaf]), (2 + 16, false));
        // bit 7 must stay set: one clear poll starts the count again
        let mut s = vec![0xaf; 15];
        s.extend([0x2f, 0xaf]);
        assert_eq!(wait_t(&s), (15 + 1 + 16, false));
    }

    #[test]
    fn t_lock_wait_keeps_a_present_signal() {
        // a present channel (2026-10-19): no bit 7, bit 2 clears first, locks at 0x00
        let s = [
            0x2f, 0x2f, 0x2f, 0x2f, 0x2f, 0x2f, 0x2e, 0x0e, 0x06, 0x06, 0x02, 0x00,
        ];
        // (+1: the read after the post-lock writes)
        assert_eq!(wait_t(&s), (12 + 1, true));
        // bit 1 clears first (2026-11-17): lock at 0x04, also for a channel held there
        assert_eq!(wait_t(&[0x2f, 0x2e, 0x0e, 0x06, 0x04, 0x00]), (5 + 1, true));
        assert_eq!(wait_t(&[0x2f, 0x2e, 0x0e, 0x04]), (4 + 1, true));
        // a signal that does not lock waits the whole ~3.4 s, then gets the writes for a
        // lock that comes later
        assert_eq!(wait_t(&[0x2f, 0x06]), (60, true));
    }

    #[test]
    fn t_lock_must_hold_after_the_post_lock_writes() {
        // back from 0x06 on the third poll after the writes
        assert_eq!(wait_t(&[0x2f, 0x04, 0x06, 0x06, 0x04]), (2 + 3, true));
        // gone for good: ~1 s more, then the caller's status read says no lock
        assert_eq!(
            wait_t(&[0x2f, 0x04, 0x06]),
            (2 + T_RELOCK_POLLS as usize, true)
        );
    }

    #[test]
    fn t_lock_ignores_bit_2_only() {
        for x in [0x00, 0x04] {
            assert!(t_locked(x), "0x{x:02x}");
        }
        for x in [0x01, 0x02, 0x06, 0x08, 0x0e, 0x2e, 0x2f, 0x84, 0xaf] {
            assert!(!t_locked(x), "0x{x:02x}");
        }
        let mut b = Lock {
            script: vec![0x04],
            polls: 0,
            writes: String::new(),
        };
        assert!(
            read_status(&mut b, FrontEnd::DT295, System::Terrestrial)
                .unwrap()
                .locked
        );
        b.script = vec![0x06];
        assert!(
            !read_status(&mut b, FrontEnd::DT295, System::Terrestrial)
                .unwrap()
                .locked
        );
    }

    #[test]
    fn s_lock_ignores_bits_3_0() {
        for x in [0x00, 0x01, 0x02, 0x04, 0x0f] {
            assert!(s_locked(x), "0x{x:02x}");
        }
        for x in [0x10, 0x14, 0x20, 0x40, 0x80, 0xff] {
            assert!(!s_locked(x), "0x{x:02x}");
        }
        let mut b = Lock {
            script: vec![0x10, 0x04],
            polls: 0,
            writes: String::new(),
        };
        assert!(wait_lock_s(&mut b, FrontEnd::DT295));
        assert_eq!(b.polls, 2);
        assert!(
            read_status(&mut b, FrontEnd::DT295, System::Satellite)
                .unwrap()
                .locked
        );
        b.script = vec![0x14];
        assert!(
            !read_status(&mut b, FrontEnd::DT295, System::Satellite)
                .unwrap()
                .locked
        );
    }

    #[test]
    fn sat_band_follows_the_frequency() {
        // the IFs of BS-1 .. BS-23 and some CS110 NDs, against Linux's horus3a.c table
        for (khz, want) in [
            (1_049_480, 0x58),
            (1_087_840, 0x54),
            (1_126_200, 0x54),
            (1_164_560, 0x70),
            (1_241_280, 0x70),
            (1_279_640, 0x8C),
            (1_318_000, 0x8C),
            (1_356_360, 0x8A),
            (1_433_080, 0x8A),
            (1_471_440, 0xA7),
            (1_613_000, 0x44),
            (1_773_000, 0x44),
            (1_813_000, 0x22),
            (1_973_000, 0x22),
            (2_013_000, 0x00),
            (2_053_000, 0x00),
        ] {
            assert_eq!(sat_band_reg(khz * 1000), want, "{khz} kHz");
        }
        // rounded to MHz first, as Linux does
        assert_eq!(sat_band_reg(1_799_500_000), 0x22);
        assert_eq!(sat_band_reg(1_799_499_999), 0x44);
        // the TSID plays no part: a BS tune without one gets the BS value
        let got = rec(|b| {
            tune_s(b, FrontEnd::DT295, 1_318_000_000, crate::chan::NO_TSID).unwrap();
        });
        assert!(got.contains(" W22=fec0098c "), "{got}");
        let got = rec(|b| {
            tune_s(b, FrontEnd::DT295, 1_853_000_000, 0x70E0).unwrap();
        });
        assert!(got.contains(" W22=fec00922 "), "{got}");
    }

    #[test]
    fn sat_freq_as_the_vendor_writes_it() {
        // the IFs of BS-1 .. BS-23 and two CS110 NDs: the vendor driver's reg 0x00-0x04
        for (khz, want) in [
            (1_049_480, [0x10, 0x64, 0, 0, 0x80]),
            (1_087_840, [0x11, 0x00, 0, 0, 0x80]),
            (1_126_200, [0x08, 0xCC, 0, 0, 0]),
            (1_164_560, [0x09, 0x1A, 0, 0, 0]),
            (1_202_920, [0x09, 0x66, 0, 0, 0]),
            (1_241_280, [0x09, 0xB2, 0, 0, 0]),
            (1_279_640, [0x0A, 0x00, 0, 0, 0]),
            (1_318_000, [0x0A, 0x4C, 0, 0, 0]),
            (1_356_360, [0x0A, 0x98, 0, 0, 0]),
            (1_394_720, [0x0A, 0xE6, 0, 0, 0]),
            (1_433_080, [0x0B, 0x32, 0, 0, 0]),
            (1_471_440, [0x0B, 0x7E, 0, 0, 0]),
            (1_613_000, [0x0C, 0x9A, 0, 0, 0]),
            (2_053_000, [0x10, 0x0A, 0, 0, 0]),
        ] {
            assert_eq!(sat_freq_regs(khz * 1000), want, "{khz} kHz");
        }
        // divider 4 up to 1100 MHz after the rounding to MHz
        assert_eq!(sat_freq_regs(1_100_499_999)[4], 0x80);
        assert_eq!(sat_freq_regs(1_100_500_000), [0x08, 0x9A, 0, 0, 0]);
        // in the sequence: BS-1
        let got = rec(|b| {
            tune_s(b, FrontEnd::DT295, 1_049_480_000, 0x4010).unwrap();
        });
        assert!(got.contains(" W22=fec0001064000080 S80 "), "{got}");
    }

    /// [`Rec`] with the tuner's status bytes (`[FE C1]`, two bytes) reading `status`, or
    /// failing for `None`.
    struct Status50(Rec, Option<[u8; 2]>);

    impl Bus for Status50 {
        fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
            self.0.i2c_write(slave, data)
        }
        fn i2c_write_read(&mut self, slave: u8, w: &[u8], r: &mut [u8]) -> Result<()> {
            self.0.i2c_write_read(slave, w, r)?;
            if w == [0xFE, 0xC1] {
                r.copy_from_slice(&self.1.ok_or(Error::Timeout("no ack"))?);
            }
            Ok(())
        }
        fn sleep_ms(&mut self, ms: u64) {
            self.0.sleep_ms(ms);
        }
    }

    #[test]
    fn sat_reg50_follows_the_tuner_status() {
        for (status, want) in [
            ([0x00, 0x60], "f1"),
            ([0x00, 0xFF], "f1"),
            ([0xFF, 0x40], "f0"),
            ([0xFF, 0x20], "f0"),
            ([0x60, 0x00], "f0"),
        ] {
            let mut b = Status50(Rec::default(), Some(status));
            tune_s(&mut b, FrontEnd::DT295, 1_318_000_000, 0x40F1).unwrap();
            let want = format!(" R22=fec1/2 S3 W22=fec050{want} S40 ");
            assert!(b.0.0.contains(&want), "{status:02x?}: {}", b.0.0);
        }
        // a failed read keeps 0xF0 and the wait, and the tune goes on
        assert_eq!(sat_reg50(None), 0xF0);
        let mut b = Status50(Rec::default(), None);
        tune_s(&mut b, FrontEnd::DT295, 1_318_000_000, 0x40F1).unwrap();
        assert!(
            b.0.0.contains(" R22=fec1/2 S3 W22=fec050f0 S40 W22=1e20 "),
            "{}",
            b.0.0
        );
        assert!(b.0.0.ends_with(" R22=ce/16"), "{}", b.0.0);
        // the second front end of the PIX-DT195W reads and writes its own tuner
        let mut b = Status50(Rec::default(), Some([0x00, 0x60]));
        tune_s(&mut b, dt195w(1, Port::B), 1_318_000_000, 0x40F1).unwrap();
        assert!(
            b.0.0.contains(" R26=fec1/2 S3 W26=fec050f1 S40 "),
            "{}",
            b.0.0
        );
    }

    #[test]
    fn tune_s_sequence() {
        for (hz, tsid, want) in [
            (1_318_000_000, 0x40F1, TUNE_S_1318_40F1),
            (1_613_000_000, 0x7040, TUNE_S_1613_7040),
        ] {
            let got = rec(|b| {
                tune_s(b, FrontEnd::DT295, hz, tsid).unwrap();
            });
            assert_eq!(&got[..want.len()], want);
            // locked: the TSID streamed (read again while it is not the one asked for;
            // the recorder answers 0), then the transponder's TSIDs
            let tail = &got[want.len()..];
            assert_eq!(tail.matches(" R22=e6/2").count(), 10);
            assert!(tail.ends_with(" R22=ce/16"), "{tail}");
        }
    }

    /// A locked ISDB-S demod with these TMCC TSIDs: streams the one selected at reg 0x8F /
    /// 0x90 (none selected: reads 0xFFFF), or the first for one it does not carry.
    struct Tmcc {
        ids: Vec<u16>,
        select: [u8; 2],
        writes: String,
    }

    impl Tmcc {
        fn new(ids: &[u16]) -> Self {
            Self {
                ids: ids.to_vec(),
                select: [0; 2],
                writes: String::new(),
            }
        }
    }

    impl Bus for Tmcc {
        fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
            if slave == S0 && data.len() == 2 {
                match data[0] {
                    0x8F => self.select[0] = data[1],
                    0x90 => self.select[1] = data[1],
                    _ => return Ok(()),
                }
                write!(self.writes, " {}", hex(data)).unwrap();
            }
            Ok(())
        }
        fn i2c_write_read(&mut self, slave: u8, w: &[u8], r: &mut [u8]) -> Result<()> {
            r.fill(0);
            let selected = u16::from_be_bytes(self.select);
            match (slave, w) {
                (S0, [0xCE]) => {
                    for (slot, id) in r.chunks_mut(2).zip(&self.ids) {
                        slot.copy_from_slice(&id.to_be_bytes());
                    }
                }
                (S0, [0xE6]) => {
                    let streams = match selected {
                        0xFFFF => 0xFFFF,
                        t if self.ids.contains(&t) => t,
                        _ => self.ids[0],
                    };
                    r.copy_from_slice(&streams.to_be_bytes());
                }
                _ => {}
            }
            Ok(())
        }
        fn sleep_ms(&mut self, _: u64) {}
    }

    #[test]
    fn ts_numbers() {
        // TS 1 of BS-1: tuned without a TSID, then 0x4011 selected and streamed
        let mut b = Tmcc::new(&[0x4010, 0x4011, 0x4012]);
        assert_eq!(
            tune_s(&mut b, FrontEnd::DT295, 1_049_480_000, 1).unwrap(),
            Some(0x4011)
        );
        assert_eq!(b.writes, " 8fff 90ff 8f40 9011");
        // by the low 3 bits, not the slot: BS-3's renumbered TS1 and TS2
        let mut b = Tmcc::new(&[0x4030, 0x4631, 0x4632]);
        assert_eq!(
            tune_s(&mut b, FrontEnd::DT295, 1_087_840_000, 2).unwrap(),
            Some(0x4632)
        );
        // CS110's one TS for the 0x0 of BonDriver_BDA's sample
        let mut b = Tmcc::new(&[0x6020]);
        assert_eq!(
            tune_s(&mut b, FrontEnd::DT295, 1_613_000_000, 0).unwrap(),
            Some(0x6020)
        );
        // a TS the transponder does not carry: an error, not another TS
        let mut b = Tmcc::new(&[0x4010, 0x4011, 0x4012]);
        assert!(tune_s(&mut b, FrontEnd::DT295, 1_049_480_000, 3).is_err());
        // a TSID is as before
        let mut b = Tmcc::new(&[0x4010, 0x4011]);
        assert_eq!(
            tune_s(&mut b, FrontEnd::DT295, 1_049_480_000, 0x4011).unwrap(),
            Some(0x4011)
        );
        assert_eq!(b.writes, " 8f40 9011");
    }

    /// A firmware that stopped acking: every transfer fails; counts the time slept.
    #[derive(Default)]
    struct Wedged(u64);

    impl Bus for Wedged {
        fn i2c_write(&mut self, _: u8, _: &[u8]) -> Result<()> {
            Err(Error::Timeout("wedged"))
        }
        fn i2c_write_read(&mut self, _: u8, _: &[u8], _: &mut [u8]) -> Result<()> {
            Err(Error::Timeout("wedged"))
        }
        fn sleep_ms(&mut self, ms: u64) {
            self.0 += ms;
        }
        fn wedged(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_wedged_firmware_fails_the_tune_without_a_lock_wait() {
        let mut b = Wedged::default();
        assert!(tune_s(&mut b, FrontEnd::DT295, 1_318_000_000, 0x40F1).is_err());
        assert!(b.0 < 1000, "slept {} ms", b.0); // not the 8 s lock wait
        let mut b = Wedged::default();
        assert!(tune_t(&mut b, FrontEnd::DT295, 557_142_857).is_err());
        assert!(b.0 < 1000, "slept {} ms", b.0);
    }

    #[test]
    fn sample_reads() {
        let mut r = Rec::default();
        assert_eq!(
            sample_s(&mut r, FrontEnd::DT295).unwrap(),
            (0, 23130, 5_921_370, 5_921_370)
        );
        assert_eq!(r.0, " R22=c3/1 R22=bc/2 R22=eb/3 R22=f0/3");
        let mut r = Rec::default();
        assert_eq!(sample_t(&mut r, FrontEnd::DT295).unwrap(), (0, 5_921_370));
        assert_eq!(r.0, " R20=80/1 R20=8b/3");
    }

    #[test]
    fn status_reads_real_cn() {
        let mut r = Rec::default();
        let st = read_status(&mut r, FrontEnd::DT295, System::Satellite).unwrap();
        assert!(st.locked);
        assert_eq!((st.raw, st.cn_mdb), (0x5A5A, cn_s_mdb(0x5A5A)));
        let st = read_status(&mut r, FrontEnd::DT295, System::Terrestrial).unwrap();
        assert_eq!((st.raw, st.cn_mdb), (0x5A5A5A, cn_t_mdb(0x5A5A5A)));
    }

    #[test]
    fn cn_alone_is_one_transfer() {
        let mut r = Rec::default();
        assert_eq!(
            read_cn(&mut r, FrontEnd::DT295, System::Satellite).unwrap(),
            (0x5A5A, cn_s_mdb(0x5A5A))
        );
        assert_eq!(r.0, " R22=bc/2");
        let mut r = Rec::default();
        assert_eq!(
            read_cn(&mut r, FrontEnd::DT295, System::Terrestrial).unwrap(),
            (0x5A5A5A, cn_t_mdb(0x5A5A5A))
        );
        assert_eq!(r.0, " R20=8b/3");
    }
}
