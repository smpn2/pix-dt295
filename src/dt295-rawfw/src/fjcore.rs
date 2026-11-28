//! The Fujitsu I2C controller driven from the host through the 0xBC window, at its
//! USB-core-block instance 0x00FD8FE0. This is the SAME controller the vendor's aux core
//! drives at 0x01684FE0 (the 0x0168 demod I2C), but reached from its host face, which
//! answers the host directly -- unlike the aux face, which wedges the main core's shared
//! USB host interface. So the host can read (and tune) the demod blob-free, with no aux
//! core and no wedge. The register map, the setup values and the byte-level handshake
//! are from analysis provided by a third party, who drove this controller from the host
//! on a PIX-DT295.

use dt295_core::fe1::{self, FrontEnd, Port};
use dt295_core::proto::Io;
use dt295_core::{Error, Result, Status, System};
use dt295_frmon::{Pump, RING_SIZE};

use crate::IoAccess;

/// The USB-core-block instance base (the host face of the demod I2C controller).
const BASE: u32 = 0x00FD_8FE0;
const O_STATUS: u32 = 0x00; // bit7 busy, bit4 last ACK missing
const O_CONTROL: u32 = 0x04; // bit7 error, bit5 restart, bit4 start, bit3 ack, bit0 done
const O_DIVIDER: u32 = 0x08;
const O_DATA: u32 = 0x10;

const CT_INITIAL: u32 = 0x42;
const DIV_SETUP: u32 = 0x3c;
const DIV_MASK: u32 = 0x7f; // bit7 is a status bit, not what we wrote
const ST_BUSY: u32 = 0x80;
const ST_NACK: u32 = 0x10;
const CT_ERROR: u32 = 0x80;
const CT_RESTART: u32 = 0x20;
const CT_START: u32 = 0x10;
const CT_ACK: u32 = 0x08;
const CT_DONE: u32 = 0x01;
/// The in-chip path spins I2C_SPIN (0x20000); from the host each access is a ~60 us
/// control transfer and the bit has always settled, so a small cap is plenty.
const SPIN: u32 = 2000;

fn r(io: &Io, off: u32) -> Result<u32> {
    let mut b = [0u8; 4];
    io.read_block(BASE + off, &mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn w(io: &Io, off: u32, v: u32) -> Result<()> {
    io.write_block(BASE + off, &v.to_be_bytes())
}
fn abs_r(io: &Io, addr: u32) -> Result<u32> {
    let mut b = [0u8; 4];
    io.read_block(addr, &mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn abs_w(io: &Io, addr: u32, v: u32) -> Result<()> {
    io.write_block(addr, &v.to_be_bytes())
}

/// Clock the instance and release its reset, then write the divider and the control and
/// confirm they took. Idempotent. Each write is needed from a cold start (measured by a
/// third party on a PIX-DT295, leaving out one at a time): before the clock and the reset
/// release nothing written to the block lands, and 0xFD80C0 comes up as 3, where bit0 keeps
/// the divider and the control from taking and bit1 makes every register read busy.
/// 0xFD80B0 bit0 and the instance's +0x14 / +0x18 changed nothing there and are left alone.
fn setup(io: &Io) -> Result<()> {
    // Read-modify-write: these registers gate other blocks too (the USB core among them).
    abs_w(io, 0x00FD_8010, abs_r(io, 0x00FD_8010)? | 0x10)?; // block clock
    abs_w(io, 0x00FD_8014, abs_r(io, 0x00FD_8014)? & !0x10)?; // reset release
    abs_w(io, 0x00FD_80C0, abs_r(io, 0x00FD_80C0)? & !0x3)?; // the two hold bits
    w(io, O_DIVIDER, DIV_SETUP)?;
    w(io, O_CONTROL, CT_INITIAL)?;
    if r(io, O_CONTROL)? & !CT_DONE != CT_INITIAL {
        return Err(Error::State("fj I2C: control setup did not take".into()));
    }
    if r(io, O_DIVIDER)? & DIV_MASK != DIV_SETUP {
        return Err(Error::State("fj I2C: divider setup did not take".into()));
    }
    Ok(())
}

fn wait_idle(io: &Io) -> Result<()> {
    for _ in 0..SPIN {
        if r(io, O_STATUS)? & ST_BUSY == 0 {
            return Ok(());
        }
    }
    Err(Error::Timeout("fj I2C idle"))
}

/// Read one 8-bit register of the demod (or any slave) over the host face of the I2C,
/// using the corrected handshake (`xfer2`: CONTROL bit4 held through every byte, each
/// byte waiting on CONTROL DONE). `slave` is the 7-bit address.
pub fn demod_read(io: &Io, slave: u8, reg: u8) -> Result<u8> {
    setup(io)?;
    let mut rb = [0u8; 1];
    xfer2(io, slave, &[reg], &mut rb)?;
    Ok(rb[0])
}

// ---------------------------------------------------------------------------
// Blob-free COLD bring-up of the front end, from the host.
//
// A cold chip runs the stock boot0, which has already set the pin multiplexing for the
// board and brought the TS endpoint up (the pump's bursts go straight into it). What is
// left before the demods answer on this controller is what a third party established on a
// PIX-DT295 by leaving out one write at a time from a power-up: `setup` above, the I2C
// pins and the front-end reset. The vendor's own bring-up writes far more (the external
// memory 0xFD0090, 0xF10080..0xF10098, the pin multiplexing 0xF1011C, the host-interface
// word 0xF001F0, more GPIOs, 0xF1001C); none of it is needed here, so none of it is
// written. Never write 0xFD8010/0xFD8014 = 0x08080000 (kills all control transfers).

/// The I2C pin selection: bits 21:20 = 0b10 (bits 5:4 of the high half-word, 0x8000 cold).
/// Without it the setup passes but nothing ACKs.
const PIN_SELECT: u32 = 0x00F1_0114;
const PIN_MASK: u32 = 0x0030_0000;
const PIN_I2C: u32 = 0x0020_0000;
/// The GPIOs (16-bit registers): which pins are driven, and their levels. The System1 path
/// resets the front end through the same two (host 0x90014 / 0x90018).
const GPIO_DIR: u32 = 0x00F1_0014;
const GPIO_LEVEL: u32 = 0x00F1_0018;
/// GPIO1: the front-end (demod and tuner) reset, active low. Until it is released and
/// driven nothing ACKs.
const GPIO_FE_RESET: u16 = 0x0002;

/// A register touched during cold bring-up, with the original value to put back.
/// `half` = a 16-bit register (restored with `write16`), else a 32-bit word.
type Saved = Vec<(u32, bool, u32)>;

fn rd32(io: &Io, addr: u32) -> Result<u32> {
    let mut b = [0u8; 4];
    io.read_block(addr, &mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// Bring the front end up from cold: the controller ([`setup`]), the I2C pins, then the
/// front-end reset released, the level before the drive so the pin never drives the reset
/// level. Each write is a read-modify-write that leaves a chip already up as it is. Pushes
/// each modified register's original value into `saved` as it goes (so the caller can put
/// them back, even on a mid-sequence error) and a line to `log`.
fn board_up(io: &Io, saved: &mut Saved, log: &mut Vec<String>) -> Result<()> {
    setup(io)?;
    let pins = rd32(io, PIN_SELECT)?;
    saved.push((PIN_SELECT, false, pins));
    io.write_block(PIN_SELECT, &((pins & !PIN_MASK) | PIN_I2C).to_be_bytes())?;
    let level = io.read16(GPIO_LEVEL)?;
    saved.push((GPIO_LEVEL, true, u32::from(level)));
    io.write16(GPIO_LEVEL, level | GPIO_FE_RESET)?;
    let dir = io.read16(GPIO_DIR)?;
    saved.push((GPIO_DIR, true, u32::from(dir)));
    io.write16(GPIO_DIR, dir | GPIO_FE_RESET)?;
    // The System1 path waits 5 ms after the reset before the first transfer.
    std::thread::sleep(std::time::Duration::from_millis(5));
    log.push(format!(
        "board up: I2C pins 0x{pins:08x} -> 0x{:08x}, front-end reset released \
         (GPIO driven 0x{dir:04x}, levels 0x{level:04x} before)",
        rd32(io, PIN_SELECT)?
    ));
    Ok(())
}

// --- Host 0xBC access to 25-bit addresses via the stock boot0 bank window ---
// The demod 0x0160xxxx and capture 0x0190xxxx blocks lie past the 24-bit 0xBC window. The
// stock boot0's own bank window reaches them, as a third-party host driver does: write the
// target bank into the boot0 LDI:32 immediate (write side 0xEA3D8, read side 0xE9EBC) and
// access alias 0x80000 + (addr & 0x1FFFF). Every access stays inside the 24-bit window and
// writes the hardware register itself, so the host sets the capture engine up and the
// monitor only runs the pump.
const WRITE_WINDOW: u32 = 0x000E_A3D8;
const READ_WINDOW: u32 = 0x000E_9EBC;
const ALIAS_BASE: u32 = 0x0008_0000;
const BANK_MASK: u32 = 0x0001_FFFF;
const DIRECT_END: u32 = 0x0100_0000;

fn window_set(io: &Io, window: u32, bank: u32) -> Result<()> {
    io.write_block(window, &bank.to_be_bytes())?;
    if rd32(io, window)? != bank {
        return Err(Error::State(format!(
            "window 0x{window:x} would not hold 0x{bank:08x}"
        )));
    }
    Ok(())
}

fn win_write32(io: &Io, addr: u32, val: u32) -> Result<()> {
    if addr < DIRECT_END {
        return io.write_block(addr, &val.to_be_bytes());
    }
    window_set(io, WRITE_WINDOW, addr & !BANK_MASK)?;
    io.write_block(ALIAS_BASE + (addr & BANK_MASK), &val.to_be_bytes())
}

fn win_write16(io: &Io, addr: u32, val: u16) -> Result<()> {
    if addr < DIRECT_END {
        return io.write16(addr, val);
    }
    window_set(io, WRITE_WINDOW, addr & !BANK_MASK)?;
    io.write16(ALIAS_BASE + (addr & BANK_MASK), val)
}

fn win_read32(io: &Io, addr: u32) -> Result<u32> {
    if addr < DIRECT_END {
        return rd32(io, addr);
    }
    window_set(io, READ_WINDOW, addr & !BANK_MASK)?;
    rd32(io, ALIAS_BASE + (addr & BANK_MASK))
}

/// Restore both bank-window immediates to their default (0x00F00000), so later 0x80000..
/// 0x9FFFF accesses (e.g. the boot-state register 0x82008) map where they should.
fn restore_windows(io: &Io) {
    let _ = window_set(io, WRITE_WINDOW, 0x00F0_0000);
    let _ = window_set(io, READ_WINDOW, 0x00F0_0000);
}

// --- The two TS captures ---
//
// The chip has two TS captures of the same form, each an input (the demod's TS in) and a DMA
// engine that writes 192-byte records into a ring in SDRAM: the first at 0x0190xxxx, the
// second at 0x00D2xxxx. The input selection takes the TS pins; on a PIX-DT195W a pin setting
// (`TS_PINS_B`) puts the demod's output B on the second input, so the two tuners stream at
// once (the stock firmware's System2 path uses the second the same way).
//
// The first capture's bring-up is what a third party established on a PIX-DT295 by leaving
// out one write at a time, on the first session after a power-up and on a later one. What
// most of these registers do is not known; the comments say what leaving each out did. The
// vendor's sequence writes far more (a 0x3002FFFF reset pulse with 0x1900020, the mode word
// 0x1902000, most of the 0x1901xxx block, the input gate 0xF12788, the demod sub-block
// 0x16000xx, a reset of the TS endpoint); leaving any of those out changed nothing, so they
// are not written. The second capture takes the same input and engine writes.

/// Which TS capture a stream takes: the first (0x0190xxxx, the demod's output A) or the
/// second (0x00D2xxxx, output B: a two-tuner model's second tuner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    A,
    B,
}

/// The bulk IN endpoint of the second capture's TS (the first's is [`dt295_core::EP_TS`]).
pub const EP_TS_B: u8 = 0x86;

impl Capture {
    /// The capture that takes `fe`'s TS (by its demod output).
    pub fn of(fe: FrontEnd) -> Self {
        match fe.port() {
            Port::A => Capture::A,
            Port::B => Capture::B,
        }
    }

    /// The on-chip pump that drains this capture's ring into its endpoint.
    pub fn pump(self) -> &'static Pump {
        match self {
            Capture::A => &dt295_frmon::PUMP_A,
            Capture::B => &dt295_frmon::PUMP_B,
        }
    }

    /// The bulk IN endpoint the pump feeds.
    pub fn ep(self) -> u8 {
        match self {
            Capture::A => dt295_core::EP_TS,
            Capture::B => EP_TS_B,
        }
    }

    /// The input's registers (past the 24-bit window for the first: [`win_write32`]).
    fn input(self) -> u32 {
        match self {
            Capture::A => 0x0190_1000,
            Capture::B => 0x00D2_1000,
        }
    }

    /// Its block's control words (reset, clock, wake).
    fn block(self) -> u32 {
        match self {
            Capture::A => 0x0190_0000,
            Capture::B => 0x00D0_0000,
        }
    }

    /// The engine's registers.
    fn engine(self) -> u32 {
        match self {
            Capture::A => 0x0190_2000,
            Capture::B => 0x00D2_2000,
        }
    }

    /// The input selection. The stock firmware's System2 path writes 0x0606 for the second;
    /// 0x0607 takes the TS there too.
    fn select(self) -> u16 {
        match self {
            Capture::A => 0x0607,
            Capture::B => 0x0606,
        }
    }
}

/// The unit gate (16-bit): at its cold value (0x9000) both captures' blocks read 0 and drop
/// writes.
const UNIT_GATE: u32 = 0x00F1_0118;
/// The wake bit of a capture's block: bit 18 (bit 2 of the high half-word) of +0x404.
const CAP_WAKE: u32 = 0x404;
const CAP_WAKE_BIT: u32 = 0x0004_0000;
/// The first capture's two head words, written first: written last, nothing reached the ring
/// on the first session after a power-up. Without either, nothing reaches it.
const HEAD_A: [(u32, u32); 2] = [(0x0190_0000, 0x0002_0000), (0x0190_0004, 0x0000_1100)];

// The second capture shares its block's reset and clock words (0x00D00000 / 0x00D00004) with
// other units, so its bring-up changes only its own bits. Measured on a PIX-DT195W by putting
// each word back towards its cold value: only these bits stop the ring when left out.
/// Bit 7 holds the second capture in reset (cold 0xF0021F9F).
const B_RESET: u32 = 0x00D0_0000;
const B_RESET_BIT: u32 = 0x0000_0080;
/// Bits 10 and 13 clock its input and its engine (cold 0x01000000). The first capture's are
/// bits 8 and 12 of its own block's word.
const B_CLOCK: u32 = 0x00D0_0004;
const B_CLOCK_BITS: u32 = 0x0000_2400;
/// The TS pins (16-bit): bit 6 gives the second input the demod's output B. Without it the
/// second input takes output A, as the first does (cold 0x3830 on a PIX-DT195W).
const TS_PINS: u32 = 0x00F1_012C;
const TS_PINS_B: u16 = 0x0040;

/// Offsets in a capture's input.
const IN_SELECT: u32 = 0x000; // 16-bit
const IN_WORD: u32 = 0x004;
const IN_RUN: u32 = 0x040; // set by the arm, cleared by the stop
const IN_LENGTH: u32 = 0x044;
const IN_080: u32 = 0x080;
/// Offsets in a capture's engine.
const ENG_RUN: u32 = 0x010; // CAP_RUN starts it, CAP_STOP stops it; reads 0 once stopped
const ENG_BASE: u32 = 0x020;
const ENG_SIZE: u32 = 0x024;
const ENG_WRITE: u32 = 0x028; // the hardware advances it
const ENG_READ: u32 = 0x02C; // the pump advances it
const CAP_RUN: u32 = 0x0200_0000;
const CAP_STOP: u32 = 0x0400_0000;

/// Arm a capture, stopping its engine first if a stream left it running. Does not start it
/// ([`host_start_capture`]). The first capture's registers are past the 24-bit window and go
/// over the host bank window; the second's are inside it.
fn host_configure_capture(io: &Io, cap: Capture, log: &mut Vec<String>) -> Result<()> {
    let (input, engine) = (cap.input(), cap.engine());
    // The unit's feeder and the aux core's prefetch let go of the read offset before it is
    // zeroed below.
    crate::hwpath::stop(io, cap)?;
    crate::prefetch::release(io, cap.pump().index)?;
    if win_read32(io, engine + ENG_RUN)? != 0 {
        // A stream that ended without the stop: a worker that died, or an older build.
        log.push(format!("capture {cap:?} still running: stopping it first"));
        stop_engine(io, cap)?;
    }
    // Opening the gate again when it is open leaves a running capture as it is.
    io.write16(UNIT_GATE, 0xFFFF)?;
    let wake_reg = cap.block() + CAP_WAKE;
    let wake = win_read32(io, wake_reg)?;
    win_write32(io, wake_reg, wake | CAP_WAKE_BIT)?;
    if win_read32(io, wake_reg)? & CAP_WAKE_BIT == 0 {
        // The block is asleep, and nothing below would stick.
        return Err(Error::State(format!(
            "capture block asleep (0x{wake_reg:x} bit 18 did not take)"
        )));
    }
    match cap {
        Capture::A => {
            for (addr, val) in HEAD_A {
                win_write32(io, addr, val)?;
            }
        }
        Capture::B => {
            let pins = io.read16(TS_PINS)?;
            io.write16(TS_PINS, pins | TS_PINS_B)?;
            abs_w(io, B_CLOCK, abs_r(io, B_CLOCK)? | B_CLOCK_BITS)?;
            abs_w(io, B_RESET, abs_r(io, B_RESET)? & !B_RESET_BIT)?;
        }
    }
    // The ring. Without the base or the length nothing reaches it; without the two offsets
    // zeroed, a later session starts where the last one stopped and sends old records first.
    win_write32(io, engine + ENG_BASE, cap.pump().ring_base)?;
    win_write32(io, engine + ENG_SIZE, RING_SIZE)?;
    win_write32(io, engine + ENG_WRITE, 0)?;
    win_write32(io, engine + ENG_READ, 0)?;
    // The input selection (without it nothing reaches the ring), and the word that goes with
    // it (without it, on the first session after a power-up, the ring fills but no record
    // starts with 0x47).
    win_write16(io, input + IN_SELECT, cap.select())?;
    win_write32(io, input + IN_WORD, 0x0400_0000)?;
    // 0xBC = 188, the TS packet length. Without it, or +0x080, nothing reaches the ring.
    win_write32(io, input + IN_LENGTH, 0x00BC_0000)?;
    win_write32(io, input + IN_080, 0x0100_0000)?;
    win_write32(io, input + IN_RUN, 0x0300_0000)?;
    log.push(format!("capture {cap:?} configured"));
    Ok(())
}

/// Start a capture's engine.
fn host_start_capture(io: &Io, cap: Capture) -> Result<()> {
    win_write32(io, cap.engine() + ENG_RUN, CAP_RUN)
}

/// How many reads of the engine to wait for it to stop (each a control transfer).
const STOP_POLLS: u32 = 500;

/// Stop a capture's engine: its stop word, the input's run word cleared, then the engine read
/// back at 0. Measured by a third party on a PIX-DT295 (the first capture): left running,
/// the next session's TS has a continuity error; with the run word left set, often only about
/// 64 % of the next stream reaches the host.
fn stop_engine(io: &Io, cap: Capture) -> Result<()> {
    let engine = cap.engine() + ENG_RUN;
    win_write32(io, engine, CAP_STOP)?;
    win_write32(io, cap.input() + IN_RUN, 0)?;
    for _ in 0..STOP_POLLS {
        if win_read32(io, engine)? == 0 {
            return Ok(());
        }
    }
    Err(Error::Timeout("capture engine did not stop"))
}

/// Stop a capture's engine at the end of a stream, and put the bank window back.
pub fn stop_capture(io: &Io, cap: Capture) -> Result<()> {
    let path = crate::hwpath::stop(io, cap);
    let released = crate::prefetch::release(io, cap.pump().index);
    let stopped = stop_engine(io, cap).and(released).and(path);
    restore_windows(io);
    stopped
}

/// Put the saved registers back, in reverse order.
fn cold_restore(io: &Io, saved: &Saved) {
    for &(addr, half, val) in saved.iter().rev() {
        let _ = if half {
            io.write16(addr, val as u16)
        } else {
            io.write_block(addr, &val.to_be_bytes())
        };
    }
}

/// Wait for the current I2C byte to finish by polling CONTROL DONE (bit0), the way a
/// register-level driver does. Returns (done, last CONTROL value). The caller carries
/// the hardware CONTROL forward -- only toggling DONE / START / RESTART / ACK as the
/// protocol needs -- so per-byte state bits the controller set are not dropped (that
/// was why only the first, START-carrying byte clocked). Fails on a bus error.
fn wait_done(io: &Io) -> Result<(bool, u32)> {
    let mut last = r(io, O_CONTROL)?;
    for _ in 0..SPIN {
        let v = r(io, O_CONTROL)?;
        last = v;
        if v & CT_ERROR != 0 {
            return Err(Error::State(format!(
                "fj I2C bus error (CONTROL=0x{v:04x})"
            )));
        }
        if v & CT_DONE != 0 {
            return Ok((true, v));
        }
    }
    Ok((false, last))
}

/// A byte that never reached DONE within [`SPIN`] polls is a stuck or dead bus; fail rather
/// than read a stale DATA byte as if the transfer had completed.
fn require_done(done: bool) -> Result<()> {
    done.ok_or(Error::Timeout("fj I2C: byte did not complete"))
}

/// One cold register read, carrying the hardware CONTROL forward per byte (a third-party
/// host driver's model), with DONE diagnostics at every step.
fn cold_xfer_read(io: &Io, slave: u8, reg: u8, log: &mut Vec<String>) -> Result<u8> {
    let addr7 = u32::from(slave) & 0x7f;
    wait_idle(io)?;
    // address byte, write direction, with START.
    w(io, O_DATA, addr7 << 1)?;
    w(io, O_CONTROL, CT_INITIAL | CT_START)?;
    let (d, mut ctl) = wait_done(io)?;
    let st = r(io, O_STATUS)?;
    log.push(format!(
        "addr 0x{slave:02x} W: DONE={d} CONTROL=0x{ctl:04x} STATUS=0x{st:04x} ACK={}",
        if st & ST_NACK == 0 { "yes" } else { "NO" }
    ));
    if st & ST_NACK != 0 {
        let _ = w(
            io,
            O_CONTROL,
            ctl & !(CT_DONE | CT_ACK | CT_START | CT_RESTART),
        );
        return Err(Error::State(format!("slave 0x{slave:02x} did not ACK")));
    }
    // register byte: advance by clearing only DONE. CONTROL bit4 ("start") is the
    // bus-active bit here, held set through every byte and dropped only at STOP (it
    // read back 0x53 above), so clearing it mid-transfer stalled the controller.
    w(io, O_DATA, u32::from(reg))?;
    w(io, O_CONTROL, ctl & !CT_DONE)?;
    let (d, c) = wait_done(io)?;
    ctl = c;
    log.push(format!("reg 0x{reg:02x} W: DONE={d} CONTROL=0x{ctl:04x}"));
    // turn the bus around with a RESTART, re-send the address as a read.
    w(io, O_DATA, (addr7 << 1) | 1)?;
    w(io, O_CONTROL, (ctl & !CT_DONE) | CT_RESTART)?;
    let (d, c) = wait_done(io)?;
    ctl = c;
    let st = r(io, O_STATUS)?;
    log.push(format!(
        "restart R 0x{slave:02x}: DONE={d} CONTROL=0x{ctl:04x} ACK={}",
        if st & ST_NACK == 0 { "yes" } else { "NO" }
    ));
    // read one byte and NACK it (single-byte read): clear ACK, DONE, RESTART.
    w(io, O_CONTROL, ctl & !(CT_DONE | CT_ACK | CT_RESTART))?;
    let (d, c) = wait_done(io)?;
    ctl = c;
    let val = (r(io, O_DATA)? & 0xff) as u8;
    log.push(format!(
        "read byte: DONE={d} CONTROL=0x{ctl:04x} DATA=0x{val:02x}"
    ));
    // STOP: bits 0,3,4,5 down together.
    let _ = w(
        io,
        O_CONTROL,
        ctl & !(CT_DONE | CT_ACK | CT_START | CT_RESTART),
    );
    Ok(val)
}

/// Blob-free cold read: bring the front end up ([`board_up`]) and read one demod register
/// with full diagnostics. `restore` puts the board registers back afterwards, which holds
/// the front end in reset again. Returns the register value and the step-by-step report.
/// The demod is 7-bit slave 0x10.
pub fn demod_read_cold(io: &Io, slave: u8, reg: u8, restore: bool) -> (Result<u8>, Vec<String>) {
    let mut log = Vec::new();
    let mut saved: Saved = Vec::new();
    let result = (|| {
        board_up(io, &mut saved, &mut log)?;
        log.push(format!(
            "I2C setup: CONTROL=0x{:04x} DIVIDER=0x{:04x} STATUS=0x{:04x}",
            r(io, O_CONTROL)?,
            r(io, O_DIVIDER)?,
            r(io, O_STATUS)?
        ));
        cold_xfer_read(io, slave, reg, &mut log)
    })();
    if restore {
        cold_restore(io, &saved);
        log.push(format!("restored {} board registers", saved.len()));
    }
    (result, log)
}

/// Shared blob-free COLD arm, all host-side. With `full` (the first tune after a cold
/// re-plug / open) it does the whole bring-up: the front end ([`board_up`]), the demod's
/// one-time INITIALIZE, and the monitor (which runs our on-chip TS pump). On a later
/// channel change (`full` = false) all of that is already up on the chip, so it is skipped
/// and only the retune + capture rearm run (~1 s instead of ~2.5 s). Always: tune and lock
/// over the host-face I2C, arm the capture engine over the stock bank window, start it.
/// Returns whether the demod locked.
#[allow(clippy::too_many_arguments)]
fn cold_arm(
    io: &Io,
    fe: FrontEnd,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    full: bool,
    saved: &mut Saved,
    log: &mut Vec<String>,
) -> Result<bool> {
    cold_bring_up(io, fe, full, saved, log)?;
    let locked = cold_tune_status(&mut FjBus::new(io), fe, terrestrial, freq_hz, tsid, log)?.locked;
    // After the lock: arm the capture, then start it.
    let cap = Capture::of(fe);
    host_configure_capture(io, cap, log)?;
    host_start_capture(io, cap)?;
    log.push(format!("capture {cap:?} started"));
    Ok(locked)
}

/// The once-per-plug bring-up when `full` and the chip is still cold (the front end, the
/// demods' one-time init, the monitor), else only the host-face I2C setup of a retune.
fn cold_bring_up(
    io: &Io,
    fe: FrontEnd,
    full: bool,
    saved: &mut Saved,
    log: &mut Vec<String>,
) -> Result<()> {
    use crate::frmon_host;

    // Gate first (full path only), read-only, BEFORE any board write: a chip that is not
    // pristine stock boot0 (e.g. vendor firmware running) is refused here without the board
    // bring-up touching it. If our monitor is already installed -- a CLI cold-* command re-run
    // without a re-plug -- downgrade to a retune: the front end is up and initialized. The
    // monitor itself is written later (after power_on_init); it coexists with host-face 0xBC
    // access and the bank window (only the staging dispatch), so the capture engine is still
    // configured directly.
    let full = if full {
        !frmon_host::install_check(io)?
    } else {
        false
    };
    if full {
        board_up(io, saved, log)?;
        // Initialize the front end, then write the monitor: after the board is up but before
        // the tune and capture. Its BODY is the "brought up" marker (`monitor_installed`), so
        // a tune/capture failure below keeps it and the next tune only retunes, while a
        // failure in the bring-up above leaves it absent and the next tune redoes the bring-up.
        {
            let mut bus = FjBus::new(io);
            fe1::power_on(&mut bus, fe)?;
        }
        frmon_host::install_write(io)?;
        log.push("brought up from cold".into());
    } else {
        setup(io)?; // the host-face I2C controller (idempotent; cheap on a retune)
    }
    Ok(())
}

/// Tune `fe` over `bus` and read its status (the lock, the C/N).
fn cold_tune_status(
    bus: &mut impl fe1::Bus,
    fe: FrontEnd,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    log: &mut Vec<String>,
) -> Result<Status> {
    let status = {
        let sys = if terrestrial {
            // The same captured sequence as the System1 path. The T demod's 0x47 / 0x4F / 0x72
            // read 0x00 / 0x05 / 0x00 from a power-up on (and after a satellite tune), so there
            // is nothing to write there.
            fe1::tune_t(bus, fe, freq_hz)?;
            System::Terrestrial
        } else {
            let _ = fe1::tune_s(bus, fe, freq_hz, tsid.unwrap_or(dt295_core::chan::NO_TSID))?;
            System::Satellite
        };
        let st = fe1::read_status(bus, fe, sys)?;
        log.push(format!(
            "tune: tuner {} locked={} C/N={}.{:03} dB",
            fe.tuner() + 1,
            st.locked,
            st.cn_mdb / 1000,
            (st.cn_mdb % 1000).abs()
        ));
        st
    };
    Ok(status)
}

/// Best TS sync over the two record sizes we might see: 188 (bare TS) and 192 (our ring
/// records: 188 TS bytes + a 4-byte suffix). Returns (fraction of records whose first byte
/// is 0x47, record size, phase).
fn ts_sync(buf: &[u8]) -> (f32, usize, usize) {
    let mut best = (0.0f32, 188usize, 0usize);
    for &spacing in &[188usize, 192] {
        if buf.len() < spacing * 2 {
            continue;
        }
        for phase in 0..spacing {
            let (mut hit, mut total) = (0usize, 0usize);
            let mut i = phase;
            while i < buf.len() {
                if buf[i] == 0x47 {
                    hit += 1;
                }
                i += spacing;
                total += 1;
            }
            let frac = hit as f32 / total.max(1) as f32;
            if frac > best.0 {
                best = (frac, spacing, phase);
            }
        }
    }
    best
}

/// Reframe 192-byte ring records (188 TS + 4-byte suffix) at `phase` to bare 188-byte TS.
fn reframe_188(buf: &[u8], phase: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = phase;
    while i + 188 <= buf.len() {
        out.extend_from_slice(&buf[i..i + 188]);
        i += 192;
    }
    out
}

/// Blob-free COLD capture arm: `cold_arm`, then check the TS ring's write pointer advances
/// -- TS flowing from the demod into the capture ring with no resident, no aux core, no
/// vendor firmware. Leaves the engine running (the next arm stops it first).
pub fn cold_capture(
    io: &Io,
    fe: FrontEnd,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
) -> (Result<bool>, Vec<String>) {
    let mut log = Vec::new();
    let mut saved: Saved = Vec::new();
    let result = (|| -> Result<bool> {
        let locked = cold_arm(
            io,
            fe,
            terrestrial,
            freq_hz,
            tsid,
            true,
            &mut saved,
            &mut log,
        )?;
        let ring_write = Capture::of(fe).engine() + ENG_WRITE;
        let rp0 = win_read32(io, ring_write)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        let rp1 = win_read32(io, ring_write)?;
        let flowing = rp1 != rp0;
        log.push(format!(
            "ring write ptr 0x{ring_write:x}: 0x{rp0:08x} -> 0x{rp1:08x} => {}",
            if flowing {
                "ADVANCING (TS flowing into the ring)"
            } else {
                "static (no TS)"
            }
        ));
        Ok(locked && flowing)
    })();
    restore_windows(io);
    (result, log)
}

/// Blob-free COLD stream (end to end): `cold_arm`, upload our TS pump, then loop CALLing
/// the pump and reading EP 0x81 for `secs`, counting bytes and TS sync, and stop the
/// capture engine. The whole datapath -- demod power, tune, capture arm, ring->EP pump --
/// runs with no resident, no aux core and no vendor firmware. Returns (bytes, sync
/// fraction).
pub fn cold_stream(
    io: &Io,
    fe: FrontEnd,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    secs: u64,
    out_path: Option<&str>,
) -> (Result<(u64, f32)>, Vec<String>) {
    use std::time::{Duration, Instant};

    let cap = Capture::of(fe);
    let ep = cap.ep();
    let mut log = Vec::new();
    let mut saved: Saved = Vec::new();
    let result = (|| -> Result<(u64, f32)> {
        cold_arm(
            io,
            fe,
            terrestrial,
            freq_hz,
            tsid,
            true,
            &mut saved,
            &mut log,
        )?;
        crate::frmon_host::upload_pump(io, cap.pump(), false)?;
        log.push("TS pump uploaded".into());
        // Keep many reads posted on the EP the whole time: our pump only commits a FIFO
        // burst when the host has a read pending to drain it, so single reads after each
        // pump never find the FIFO ready. A read queue keeps ~48 reads outstanding.
        let _ = io.usb.abort_pipe(ep);
        let mut queue = io.usb.read_queue(ep, 48, 0x10000).ok();
        if queue.is_none() {
            log.push(format!(
                "WARN: EP 0x{ep:02x} read queue unavailable; using single reads"
            ));
        }
        let mut seq = 0;
        let mut fallback = vec![0u8; 0x10000];
        let mut saved_bytes: Vec<u8> = Vec::new();
        let mut total: u64 = 0;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            let _ = crate::frmon_host::call_pump(io, cap.pump(), &mut seq);
            match queue.as_mut() {
                Some(q) => {
                    if let Ok(Some(data)) = q.next(Duration::from_millis(1)) {
                        total += data.len() as u64;
                        if saved_bytes.len() < 4_000_000 {
                            saved_bytes.extend_from_slice(data);
                        }
                    }
                }
                None => {
                    if let Ok(n) = io.usb.read(ep, &mut fallback, Duration::from_millis(20))
                        && n > 0
                    {
                        total += n as u64;
                        if saved_bytes.len() < 4_000_000 {
                            saved_bytes.extend_from_slice(&fallback[..n]);
                        }
                    }
                }
            }
        }
        let secs_f = start.elapsed().as_secs_f32().max(0.001);
        let mbps = (total as f32 * 8.0) / secs_f / 1e6;
        let (sync, spacing, phase) = ts_sync(&saved_bytes);
        log.push(format!(
            "captured {total} bytes in {secs_f:.1}s = {mbps:.1} Mbps; TS sync {:.1}% at {spacing}-byte records (phase {phase})",
            sync * 100.0
        ));
        if let Some(path) = out_path {
            // Write bare 188-byte TS so the file plays / inspects as a transport stream.
            let data = if spacing == 192 && sync > 0.5 {
                reframe_188(&saved_bytes, phase)
            } else {
                saved_bytes.clone()
            };
            std::fs::write(path, &data).map_err(|e| Error::State(format!("write {path}: {e}")))?;
            log.push(format!("wrote {} bytes to {path}", data.len()));
        }
        Ok((total, sync))
    })();
    if let Err(e) = stop_capture(io, cap) {
        log.push(format!("capture stop: {e}"));
    }
    (result, log)
}

/// Blob-free COLD bring-up + tune + capture start, for the raw-TS stream worker. `full` does
/// the one-time bring-up (the first tune after a cold open); a channel change passes `full` =
/// false and only retunes + rearms. After this returns, `fe`'s capture ([`Capture::of`])
/// streams into its ring, and the ring goes to the capture's EP by the returned [`Feed`]: the
/// unit's output path ([`crate::hwpath`]), or the main core's pump (uploaded; the caller loops
/// [`crate::frmon_host::call_pump`]). The caller reads the EP either way. Returns the demod's
/// status after the tune (the lock, the C/N) too. No resident and no vendor firmware; the
/// unit cores and the aux core run only our code ([`crate::hwpath`], [`crate::prefetch`]).
///
/// The I/O is taken a step at a time, so a stream of the unit's other tuner keeps being pumped:
/// the bring-up in one go (the other worker must not run its pump halfway through it), the tune
/// one I2C transfer at a time ([`LockedBus`]; it sleeps between them), and the capture start
/// with the path's start (or the pump upload) in one go.
pub fn cold_prepare_stream<L: IoAccess>(
    l: &L,
    fe: FrontEnd,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    full: bool,
) -> Result<(Status, Feed)> {
    let cap = Capture::of(fe);
    let mut log = Vec::new();
    let flush = |log: &mut Vec<String>| {
        for line in log.drain(..) {
            tracing::info!("cold raw-TS: {line}");
        }
    };
    let up = l.with_io(|io| cold_bring_up(io, fe, full, &mut Vec::new(), &mut log));
    flush(&mut log);
    up?;
    let status = cold_tune_status(
        &mut LockedBus::new(l),
        fe,
        terrestrial,
        freq_hz,
        tsid,
        &mut log,
    );
    flush(&mut log);
    let status = status?;
    let started = l.with_io(|io| {
        let started =
            host_configure_capture(io, cap, &mut log).and_then(|()| host_start_capture(io, cap));
        // Always put the bank window back to its default, even if the configure failed after
        // moving it: the pump streams over the staging dispatch, not the window, and a stale
        // capture-bank immediate would mis-map a later direct 0x80000..0x9FFFF access (e.g. the
        // boot-state register 0x82008, the System1 start's reads). cold_capture / cold_stream
        // restore it the same way on every path.
        restore_windows(io);
        started?;
        // The unit's output path takes the ring to the endpoint if it can. Failing that, the
        // aux core's prefetch takes the ring and the pump drains what it stages; without it
        // the pump drains the ring itself (about 8.6 MB/s for both captures).
        match crate::hwpath::start(io, cap) {
            Ok(()) => return Ok(Feed::Unit),
            Err(Error::State(why)) => {
                tracing::warn!("cold raw-TS: {why}; the main core pumps the ring");
            }
            Err(e) => return Err(e),
        }
        let staged = match crate::prefetch::take(io, cap.pump().index) {
            Ok(()) => true,
            Err(Error::Usb(e)) => return Err(Error::Usb(e)),
            Err(e) => {
                tracing::warn!("cold raw-TS: {e}; the main core pumps the ring itself");
                false
            }
        };
        crate::frmon_host::upload_pump(io, cap.pump(), staged)?;
        Ok(Feed::Pump)
    });
    flush(&mut log);
    Ok((status, started?))
}

/// What moves a capture's TS from its ring to its endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// The unit's output path ([`crate::hwpath`]): nothing to call while it streams.
    Unit,
    /// The main core's pump ([`crate::frmon_host::call_pump`]), called every pass.
    Pump,
}

/// Read the demod's C/N (0.001 dB) over the host face, for a live level during cold
/// streaming. The C/N alone, one I2C transfer: the stream loop cannot pump the TS while it
/// reads (about 5-10 ms a transfer), and the lock is not needed there.
pub fn cold_cn(io: &Io, fe: FrontEnd, terrestrial: bool) -> Result<i32> {
    let mut bus = FjBus::new(io);
    let sys = if terrestrial {
        System::Terrestrial
    } else {
        System::Satellite
    };
    fe1::read_cn(&mut bus, fe, sys).map(|(_, cn_mdb)| cn_mdb)
}

/// A general host-face I2C transfer (write `wb`, then read `rb`) with the corrected
/// handshake: CONTROL bit4 is held set through every byte and only dropped at STOP, and
/// each byte waits on CONTROL DONE. `addr7` is the 7-bit slave address.
fn xfer2(io: &Io, addr7: u8, wb: &[u8], rb: &mut [u8]) -> Result<()> {
    let a = (u32::from(addr7) & 0x7f) << 1;
    let read_only = wb.is_empty();
    wait_idle(io)?;
    // address byte (+ read bit if there is nothing to write first), with START.
    w(io, O_DATA, a | u32::from(read_only))?;
    w(io, O_CONTROL, CT_INITIAL | CT_START)?;
    let (done, mut ctl) = wait_done(io)?;
    require_done(done)?;
    if r(io, O_STATUS)? & ST_NACK != 0 {
        let _ = w(
            io,
            O_CONTROL,
            ctl & !(CT_DONE | CT_ACK | CT_START | CT_RESTART),
        );
        return Err(Error::State(format!(
            "fj I2C: slave 0x{addr7:02x} did not ACK"
        )));
    }
    for &byte in wb {
        w(io, O_DATA, u32::from(byte))?;
        w(io, O_CONTROL, ctl & !CT_DONE)?;
        let (done, c) = wait_done(io)?;
        require_done(done)?;
        ctl = c;
    }
    if !rb.is_empty() {
        if !read_only {
            // turn the bus around: RESTART, re-send the address as a read.
            w(io, O_DATA, a | 1)?;
            w(io, O_CONTROL, (ctl & !CT_DONE) | CT_RESTART)?;
            let (done, c) = wait_done(io)?;
            require_done(done)?;
            ctl = c;
            if r(io, O_STATUS)? & ST_NACK != 0 {
                let _ = w(
                    io,
                    O_CONTROL,
                    ctl & !(CT_DONE | CT_ACK | CT_START | CT_RESTART),
                );
                return Err(Error::State(format!(
                    "fj I2C: slave 0x{addr7:02x} read NACK"
                )));
            }
        }
        let n = rb.len();
        for (i, slot) in rb.iter_mut().enumerate() {
            let base = ctl & !(CT_DONE | CT_RESTART);
            // ACK every byte but the last (NACK the last to end the read).
            w(
                io,
                O_CONTROL,
                if i + 1 < n {
                    base | CT_ACK
                } else {
                    base & !CT_ACK
                },
            )?;
            let (done, c) = wait_done(io)?;
            require_done(done)?;
            ctl = c;
            *slot = (r(io, O_DATA)? & 0xff) as u8;
        }
    }
    let _ = w(
        io,
        O_CONTROL,
        ctl & !(CT_DONE | CT_ACK | CT_START | CT_RESTART),
    );
    Ok(())
}

/// The demod front end driven over the host face of the I2C, so [`dt295_core::fe1`] can
/// run its captured tune sequences blob-free. fe1 addresses the demods by their 8-bit
/// address (ISDB-T 0x20, ISDB-S 0x22); the host face wants the 7-bit form, so we shift.
#[allow(missing_debug_implementations)]
pub struct FjBus<'a, 'b> {
    io: &'b Io<'a>,
    /// Set once a transfer failed: a dead host bus, so [`fe1`]'s per-step `not_wedged`
    /// checks bail out instead of spinning every remaining op to its USB timeout.
    wedged: bool,
}

impl<'a, 'b> FjBus<'a, 'b> {
    pub fn new(io: &'b Io<'a>) -> Self {
        Self { io, wedged: false }
    }
}

impl fe1::Bus for FjBus<'_, '_> {
    fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
        let mut rb: [u8; 0] = [];
        let r = xfer2(self.io, slave >> 1, data, &mut rb);
        if r.is_err() {
            self.wedged = true;
        }
        r
    }
    fn i2c_write_read(&mut self, slave: u8, wdata: &[u8], rbuf: &mut [u8]) -> Result<()> {
        let r = xfer2(self.io, slave >> 1, wdata, rbuf);
        if r.is_err() {
            self.wedged = true;
        }
        r
    }
    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    fn wedged(&self) -> bool {
        self.wedged
    }
}

/// [`FjBus`] that takes the I/O for each I2C transfer and gives it back, so a tune (1-2 s,
/// mostly sleeps) does not hold up the stream of the unit's other tuner. Each transfer is
/// whole under the lock (a tuner's write through its demod's gateway is one transfer).
#[allow(missing_debug_implementations)]
pub struct LockedBus<'l, L> {
    l: &'l L,
    wedged: bool,
}

impl<'l, L: IoAccess> LockedBus<'l, L> {
    pub fn new(l: &'l L) -> Self {
        Self { l, wedged: false }
    }
}

impl<L: IoAccess> fe1::Bus for LockedBus<'_, L> {
    fn i2c_write(&mut self, slave: u8, data: &[u8]) -> Result<()> {
        let r = self.l.with_io(|io| xfer2(io, slave >> 1, data, &mut []));
        if r.is_err() {
            self.wedged = true;
        }
        r
    }
    fn i2c_write_read(&mut self, slave: u8, wdata: &[u8], rbuf: &mut [u8]) -> Result<()> {
        let r = self.l.with_io(|io| xfer2(io, slave >> 1, wdata, rbuf));
        if r.is_err() {
            self.wedged = true;
        }
        r
    }
    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    fn wedged(&self) -> bool {
        self.wedged
    }
}

/// Blob-free COLD tune: bring the front end up ([`board_up`]), then run fe1's captured
/// front-end init + tune over the host face and read the lock / C/N. No resident, no aux
/// core, no vendor firmware. `restore` puts the board registers back afterwards, which holds
/// the front end in reset again.
pub fn cold_tune(
    io: &Io,
    fe: FrontEnd,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    restore: bool,
) -> (Result<bool>, Vec<String>) {
    let mut log = Vec::new();
    let mut saved: Saved = Vec::new();
    let result = (|| {
        board_up(io, &mut saved, &mut log)?;
        log.push(format!(
            "I2C setup: CONTROL=0x{:04x} DIVIDER=0x{:04x}",
            r(io, O_CONTROL)?,
            r(io, O_DIVIDER)?
        ));
        let mut bus = FjBus::new(io);
        fe1::power_on(&mut bus, fe)?;
        log.push("front-end init done".into());
        let sys = if terrestrial {
            fe1::tune_t(&mut bus, fe, freq_hz)?;
            System::Terrestrial
        } else {
            let got = fe1::tune_s(
                &mut bus,
                fe,
                freq_hz,
                tsid.unwrap_or(dt295_core::chan::NO_TSID),
            )?;
            log.push(format!("tune_s selected TSID {got:?}"));
            System::Satellite
        };
        let st: Status = fe1::read_status(&mut bus, fe, sys)?;
        log.push(format!(
            "status: locked={} C/N={}.{:03} dB raw=0x{:06x}",
            st.locked,
            st.cn_mdb / 1000,
            (st.cn_mdb % 1000).abs(),
            st.raw
        ));
        Ok(st.locked)
    })();
    if restore {
        cold_restore(io, &saved);
        log.push(format!("restored {} board registers", saved.len()));
    }
    (result, log)
}
