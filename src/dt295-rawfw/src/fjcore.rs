//! The Fujitsu I2C controller driven from the host through the 0xBC window, at its
//! USB-core-block instance 0x00FD8FE0. This is the SAME controller the vendor's aux core
//! drives at 0x01684FE0 (the 0x0168 demod I2C), but reached from its host face, which
//! answers the host directly -- unlike the aux face, which wedges the main core's shared
//! USB host interface. So the host can read (and tune) the demod blob-free, with no aux
//! core and no wedge. The register map, the setup values and the byte-level handshake
//! are from analysis provided by a third party, who drove this controller from the host
//! on a PIX-DT295.

use dt295_core::proto::Io;
use dt295_core::{Error, Result, Status, System, fe1};

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

// --- The capture engine (0x0190xxxx) ---
//
// What brings it up so the ring fills with the demod's TS, as a third party established on
// a PIX-DT295 by leaving out one write at a time, on the first session after a power-up and
// on a later one. What most of these registers do is not known; the comments say what
// leaving each out did. The vendor's sequence writes far more (a 0x3002FFFF reset pulse
// with 0x1900020, the mode word 0x1902000, most of the 0x1901xxx block, the input gate
// 0xF12788, the demod sub-block 0x16000xx, a reset of the TS endpoint); leaving any of those
// out changed nothing, so they are not written.

/// The unit gate (16-bit): at its cold value the capture block reads 0 and drops writes.
const UNIT_GATE: u32 = 0x00F1_0118;
/// The capture block's wake bit: bit 18 (bit 2 of the high half-word).
const CAP_WAKE: u32 = 0x0190_0404;
const CAP_WAKE_BIT: u32 = 0x0004_0000;
/// The input's run word: set by the arm, cleared by the stop.
const CAP_INPUT_RUN: u32 = 0x0190_1040;
/// The engine: [`CAP_RUN`] starts it, [`CAP_STOP`] stops it; it reads 0 once stopped.
const CAP_ENGINE: u32 = 0x0190_2010;
const CAP_RUN: u32 = 0x0200_0000;
const CAP_STOP: u32 = 0x0400_0000;
/// The ring's write offset; the hardware advances it.
const CAP_RING_WRITE: u32 = 0x0190_2028;

/// The capture arm (addr, value, is_half), in order; the engine start comes after it.
const CAPTURE_ARM: &[(u32, u32, bool)] = &[
    // The two words at the head of the block, first: written last, nothing reached the
    // ring on the first session after a power-up. Without either, nothing reaches it.
    (0x0190_0000, 0x0002_0000, false),
    (0x0190_0004, 0x0000_1100, false),
    // The ring. Without the base or the length nothing reaches it; without the two offsets
    // zeroed, a later session starts where the last one stopped and sends old records first.
    (0x0190_2020, dt295_frmon::RING_BASE, false),
    (0x0190_2024, dt295_frmon::RING_SIZE, false),
    (CAP_RING_WRITE, 0, false),
    (dt295_frmon::REG_RING_READ, 0, false), // the pump advances it
    // The input selection (without it nothing reaches the ring), and the word that goes with
    // it (without it, on the first session after a power-up, the ring fills but no record
    // starts with 0x47).
    (0x0190_1000, 0x0607, true),
    (0x0190_1004, 0x0400_0000, false),
    // 0xBC = 188, the TS packet length. Without it, or 0x1901080, nothing reaches the ring.
    (0x0190_1044, 0x00BC_0000, false),
    (0x0190_1080, 0x0100_0000, false),
    (CAP_INPUT_RUN, 0x0300_0000, false),
];

/// Arm the capture engine over the host bank window, stopping it first if a stream left it
/// running. Does not start it ([`host_start_capture`]).
fn host_configure_capture(io: &Io, log: &mut Vec<String>) -> Result<()> {
    if win_read32(io, CAP_ENGINE)? != 0 {
        // A stream that ended without the stop: a worker that died, or an older build.
        log.push("capture engine still running: stopping it first".into());
        stop_engine(io)?;
    }
    io.write16(UNIT_GATE, 0xFFFF)?;
    let wake = win_read32(io, CAP_WAKE)?;
    win_write32(io, CAP_WAKE, wake | CAP_WAKE_BIT)?;
    if win_read32(io, CAP_WAKE)? & CAP_WAKE_BIT == 0 {
        // The block is asleep, and nothing below would stick.
        return Err(Error::State(
            "capture block asleep (0x1900404 bit 18 did not take)".into(),
        ));
    }
    for &(addr, val, half) in CAPTURE_ARM {
        if half {
            win_write16(io, addr, val as u16)?;
        } else {
            win_write32(io, addr, val)?;
        }
    }
    log.push("capture configured over the host bank window".into());
    Ok(())
}

/// Start the capture engine, over the host bank window.
fn host_start_capture(io: &Io) -> Result<()> {
    win_write32(io, CAP_ENGINE, CAP_RUN)
}

/// How many reads of the engine to wait for it to stop (each a control transfer).
const STOP_POLLS: u32 = 500;

/// Stop the capture engine: its stop word, the input's run word cleared, then the engine read
/// back at 0. Measured by a third party on a PIX-DT295: left running, the next session's TS
/// has a continuity error; with the run word left set, often only about 64 % of the next
/// stream reaches the host.
fn stop_engine(io: &Io) -> Result<()> {
    win_write32(io, CAP_ENGINE, CAP_STOP)?;
    win_write32(io, CAP_INPUT_RUN, 0)?;
    for _ in 0..STOP_POLLS {
        if win_read32(io, CAP_ENGINE)? == 0 {
            return Ok(());
        }
    }
    Err(Error::Timeout("capture engine did not stop"))
}

/// Stop the capture engine at the end of a stream, and put the bank window back.
pub fn stop_capture(io: &Io) -> Result<()> {
    let stopped = stop_engine(io);
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
fn cold_arm(
    io: &Io,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    full: bool,
    saved: &mut Saved,
    log: &mut Vec<String>,
) -> Result<Status> {
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
            fe1::power_on_init(&mut bus)?;
        }
        frmon_host::install_write(io)?;
    } else {
        setup(io)?; // the host-face I2C controller (idempotent; cheap on a retune)
    }
    let status = {
        let mut bus = FjBus::new(io);
        let sys = if terrestrial {
            // The same captured sequence as the System1 path. The T demod's 0x47 / 0x4F / 0x72
            // read 0x00 / 0x05 / 0x00 from a power-up on (and after a satellite tune), so there
            // is nothing to write there.
            fe1::tune_t(&mut bus, freq_hz)?;
            System::Terrestrial
        } else {
            let _ = fe1::tune_s(&mut bus, freq_hz, tsid.unwrap_or(dt295_core::chan::NO_TSID))?;
            System::Satellite
        };
        let st = fe1::read_status(&mut bus, sys)?;
        log.push(format!(
            "tune: locked={} C/N={}.{:03} dB",
            st.locked,
            st.cn_mdb / 1000,
            (st.cn_mdb % 1000).abs()
        ));
        st
    };
    // After the lock: arm the capture engine, then start it.
    host_configure_capture(io, log)?;
    host_start_capture(io)?;
    log.push("capture started (0x1902010 = 0x02000000, host window)".into());
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
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
) -> (Result<bool>, Vec<String>) {
    let mut log = Vec::new();
    let mut saved: Saved = Vec::new();
    let result = (|| -> Result<bool> {
        let locked = cold_arm(io, terrestrial, freq_hz, tsid, true, &mut saved, &mut log)?.locked;
        let rp0 = win_read32(io, CAP_RING_WRITE)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        let rp1 = win_read32(io, CAP_RING_WRITE)?;
        let flowing = rp1 != rp0;
        log.push(format!(
            "ring write ptr 0x1902028: 0x{rp0:08x} -> 0x{rp1:08x} => {}",
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
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    secs: u64,
    out_path: Option<&str>,
) -> (Result<(u64, f32)>, Vec<String>) {
    use crate::frmon_host::MonitorResident;
    use dt295_core::EP_TS;
    use std::time::{Duration, Instant};

    let mut log = Vec::new();
    let mut saved: Saved = Vec::new();
    let result = (|| -> Result<(u64, f32)> {
        cold_arm(io, terrestrial, freq_hz, tsid, true, &mut saved, &mut log)?;
        crate::frmon_host::upload_ts_pump(io)?;
        log.push("TS pump uploaded".into());
        // Keep many reads posted on EP 0x81 the whole time: our pump only commits a FIFO
        // burst when the host has a read pending to drain it, so single reads after each
        // pump never find the FIFO ready. A read queue keeps ~48 reads outstanding.
        let _ = io.usb.abort_pipe(EP_TS);
        let mut queue = io.usb.read_queue(EP_TS, 48, 0x10000).ok();
        if queue.is_none() {
            log.push("WARN: EP 0x81 read queue unavailable; using single reads".into());
        }
        let mut rt = MonitorResident::new(io);
        let mut fallback = vec![0u8; 0x10000];
        let mut saved_bytes: Vec<u8> = Vec::new();
        let mut total: u64 = 0;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            let _ = rt.pump();
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
                    if let Ok(n) = io.usb.read(EP_TS, &mut fallback, Duration::from_millis(20))
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
    if let Err(e) = stop_capture(io) {
        log.push(format!("capture stop: {e}"));
    }
    (result, log)
}

/// Blob-free COLD bring-up + arm + TS-pump upload, for the BonDriver cold raw-TS worker.
/// `full` does the one-time bring-up (the first tune after a cold open); a channel change
/// passes `full` = false and only retunes + rearms. After this returns, the capture engine
/// streams into the ring and the monitor's TS pump is uploaded; the caller loops
/// [`crate::frmon_host::MonitorResident::pump`] + EP 0x81 reads. Returns the demod's status
/// after the tune (the lock, the C/N). No resident, no aux core, no vendor firmware.
pub fn cold_prepare_stream(
    io: &Io,
    terrestrial: bool,
    freq_hz: u32,
    tsid: Option<u16>,
    full: bool,
) -> Result<Status> {
    let mut log = Vec::new();
    let mut saved: Saved = Vec::new();
    let armed = cold_arm(io, terrestrial, freq_hz, tsid, full, &mut saved, &mut log);
    for line in &log {
        tracing::info!("cold raw-TS: {line}");
    }
    // Always put the bank window back to its default, even if cold_arm failed after moving it
    // (in the capture configure): the worker's pump streams over the staging dispatch, not the
    // window, and a stale capture-bank immediate would mis-map a later direct 0x80000..0x9FFFF
    // access (e.g. the boot-state register 0x82008, the System1 start's reads). cold_capture /
    // cold_stream restore it the same way on every path.
    restore_windows(io);
    let status = armed?;
    crate::frmon_host::upload_ts_pump(io)?;
    Ok(status)
}

/// Read the demod's C/N (0.001 dB) over the host face, for a live level during cold
/// streaming. The C/N alone, one I2C transfer: the stream loop cannot pump the TS while it
/// reads (about 5-10 ms a transfer), and the lock is not needed there.
pub fn cold_cn(io: &Io, terrestrial: bool) -> Result<i32> {
    let mut bus = FjBus::new(io);
    let sys = if terrestrial {
        System::Terrestrial
    } else {
        System::Satellite
    };
    fe1::read_cn(&mut bus, sys).map(|(_, cn_mdb)| cn_mdb)
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

/// Blob-free COLD tune: bring the front end up ([`board_up`]), then run fe1's captured
/// front-end init + tune over the host face and read the lock / C/N. No resident, no aux
/// core, no vendor firmware. `restore` puts the board registers back afterwards, which holds
/// the front end in reset again.
pub fn cold_tune(
    io: &Io,
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
        fe1::power_on_init(&mut bus)?;
        log.push("front-end init done".into());
        let sys = if terrestrial {
            fe1::tune_t(&mut bus, freq_hz)?;
            System::Terrestrial
        } else {
            let got = fe1::tune_s(&mut bus, freq_hz, tsid.unwrap_or(dt295_core::chan::NO_TSID))?;
            log.push(format!("tune_s selected TSID {got:?}"));
            System::Satellite
        };
        let st: Status = fe1::read_status(&mut bus, sys)?;
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
