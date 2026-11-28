//! The units' output path: a capture's TS goes from its ring to its endpoint without a CPU
//! copying it.
//!
//! Units 0x016 and 0x018 (the stock firmware runs its TS multiplexer on their cores) each
//! read a ring into their buffer, and their engine and output block hand the records to a
//! USB stream channel, which writes them into an endpoint's FIFO (up to about 40 MB/s).
//! Capture A goes through unit 0x016 and channel 0 to EP 0x81, capture B through unit 0x018
//! and channel 2 to EP 0x86. Our feeder ([`dt295_frmon::unit_feeder`]) runs on the unit's own
//! core and sends each record as the capture writes it; the main core's pump is not called.
//!
//! The path needs three system bits the stock firmware sets: 0x00F1010C bit 15 (without it
//! the output block never drains into the channel), 0x00F10130 bit 0 (without it the
//! channel takes one byte of every eight) and 0x00F10128 bit 11 (without it the flow stops
//! after about 64 KiB). All three are 16-bit registers.
//!
//! The endpoint is in DMA mode while the path runs; stopping the path puts it back in CPU
//! mode, so the main core's pump can still feed it if a later stream's path does not start.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use dt295_frmon as fw;
use tracing::{info, warn};

use dt295_core::proto::Io;
use dt295_core::{Error, Result};

use crate::fjcore::Capture;
use crate::frmon_host::Session;

/// How many reads (control transfers) to wait for the feeder to start or stop.
const POLLS: u32 = 200;
/// How long a read of the endpoint waits for what an earlier stream left in it.
const DRAIN_WAIT: Duration = Duration::from_millis(20);
/// Most bytes discarded from the endpoint before a stream (far more than its FIFO holds).
const DRAIN_MAX: usize = 1 << 20;

/// The USB block's write byte enables (bit 0 = the most significant byte; 0xF = all).
const BYTE_ENABLE: u32 = fw::EP_BYTE_ENABLE;
/// The endpoints' DMA-mode word: bit 16 + n puts EP n in DMA mode, bit n is its CPU-mode
/// interrupt enable.
const EP_DMA: u32 = 0x00fd_bc14;
/// The stream channels' clock enables and resets (bit 16 + k for channel k < 3).
const CHAN_CLOCK: u32 = 0x00fd_8010;
const CHAN_RESET: u32 = 0x00fd_8014;
/// A channel's configuration: bits 12-15 the endpoint FIFO, the rest as the stock firmware.
const CHAN_CONF: u32 = 0xb09f_0000;

/// The system bits of the path (16-bit registers: address, bit).
const SYSTEM_BITS: [(u32, u16); 3] = [
    (0x00f1_010c, 0x8000),
    (0x00f1_0130, 0x0001),
    (0x00f1_0128, 0x0800),
];

/// Which unit, channel and endpoint a capture's path uses.
struct Path {
    /// The unit's registers (main view): +0 resets (bit 17 its core), +4 clocks; its RAM at
    /// + 0x000A0000.
    unit: u32,
    /// The stream channel: +0 control, +4 configuration, +8 status.
    channel: u32,
    /// The endpoint's number (its control word at 0x00FDBD08 + 8 x (n - 1)).
    ep: u32,
    /// The endpoint's FIFO.
    fifo: u32,
}

fn path(cap: Capture) -> Path {
    match cap {
        Capture::A => Path {
            unit: 0x0160_0000,
            channel: 0x00fd_8200,
            ep: 1,
            fifo: 0,
        },
        Capture::B => Path {
            unit: 0x0180_0000,
            channel: 0x00fd_8240,
            ep: 6,
            fifo: 4,
        },
    }
}

/// The feeder images (assembled once).
fn image(cap: Capture) -> &'static [u8] {
    static IMAGES: OnceLock<[Vec<u8>; 2]> = OnceLock::new();
    let images =
        IMAGES.get_or_init(|| [fw::unit_feeder(&fw::PUMP_A), fw::unit_feeder(&fw::PUMP_B)]);
    &images[cap.pump().index]
}

fn read(io: &Io, addr: u32) -> Result<u32> {
    let mut b = [0u8; 4];
    Session::new(io).read(addr, &mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn write(io: &Io, addr: u32, v: u32) -> Result<()> {
    Session::new(io).write(addr, &v.to_be_bytes())
}

/// Set `bits` in the 16-bit register at `addr` (a read-modify-write of its word).
fn set16(io: &Io, addr: u32, bits: u16) -> Result<()> {
    let word = addr & !3;
    let shift = if addr & 2 == 0 { 16 } else { 0 };
    let v = read(io, word)?;
    write(io, word, v | (u32::from(bits) << shift))
}

/// Is the feeder running: its pass counter moving.
fn alive(io: &Io, unit: u32) -> Result<bool> {
    let first = read(io, unit + fw::FEEDER_ALIVE)?;
    for _ in 0..8 {
        if read(io, unit + fw::FEEDER_ALIVE)? != first {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Is our feeder loaded on the unit: the first words at its entry are its image's. A
/// unit's RAM holds what it powered on with until [`start_feeder`] loads it (unit 0x018's
/// go word read non-zero on a freshly plugged PIX-DT195W).
fn loaded(io: &Io, cap: Capture, unit: u32) -> Result<bool> {
    let img = image(cap);
    for (i, w) in img.as_chunks::<4>().0.iter().take(2).enumerate() {
        if read(io, unit + fw::FEEDER_ENTRY + 4 * i as u32)? != u32::from_be_bytes(*w) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Hold the unit's core and its blocks in reset.
fn hold(io: &Io, unit: u32) -> Result<()> {
    write(io, unit, 0xffff_ffff)
}

/// Load the feeder and start the unit's core (it waits for its go word).
fn start_feeder(io: &Io, cap: Capture, p: &Path) -> Result<()> {
    hold(io, p.unit)?;
    crate::frmon_host::upload_routine(io, p.unit + fw::FEEDER_ENTRY, image(cap))?;
    for w in [
        fw::FEEDER_GO,
        fw::FEEDER_SENT,
        fw::FEEDER_STOPPED,
        fw::FEEDER_ALIVE,
    ] {
        write(io, p.unit + w, 0)?;
    }
    write(io, p.unit + fw::FEEDER_VECTOR, fw::FEEDER_ENTRY)?;
    // Resets and clocks all on, the clocks back off, then everything but bit 16 and the core
    // (bit 17) out of reset; then the rest of the resets pulsed and released.
    for (a, v) in [
        (p.unit + 4, 0xffff_ffff),
        (p.unit, 0xffff_ffff),
        (p.unit + 4, 0x0080_0000),
        (p.unit + 4, 0),
        (p.unit, 0xfffc_ffff),
        (p.unit, 0x0000_00f7),
        (p.unit, 0),
    ] {
        write(io, a, v)?;
    }
    if !alive(io, p.unit)? {
        hold(io, p.unit)?;
        return Err(Error::State(format!(
            "the feeder on unit 0x{:03x} did not start",
            p.unit >> 20
        )));
    }
    Ok(())
}

/// The unit's blocks: interrupts off, clocks on, reader 2 on the capture's ring into buffer
/// region 2, the engine, and the output block running (188-byte TS in 192-byte records).
fn set_up_unit(io: &Io, cap: Capture, p: &Path) -> Result<()> {
    let u = p.unit;
    let v = read(io, u + 0x404)?;
    write(io, u + 0x404, v | 0x0004_0000)?;
    let ring = cap.pump();
    for (a, v) in [
        (0x10u32, 0xffff_ffffu32),
        (0x20, 0xffff_ffff),
        (0xb8, 0xffff_ffff),
        (0xc8, 0xffff_ffff),
        (0x90, 0xffff_ffff),
        (0x50, 0x0341_7673),
        (0x60, 0),
        (0x30, 0x0341_7673),
        (0x40, 0xfc00_0004),
        (0xb4, 0),
        (0xa0, 0x0001_0000),
        (0x4, 0xffff_ffff),
        (0x1000, 0xfff0_0000),
        (0x1018, ring.ring_base & 0x07ff_ffe0),
        (0x1028, fw::RING_SIZE & 0x00ff_ffe0),
        // the buffer manager (16-bit registers): region bounds and sizes as the stock mux
        (0x1_0010, 0),
        (0x1_0030, 0),
        (0x1_0034, 0x0800 << 16),
        (0x1_0038, 0x1000 << 16),
        (0x1_003c, 0x1840 << 16),
        (0x1_0040, 0x1840 << 16),
        (0x1_0080, 0x00b8 << 16),
        (0x1_0084, 0x00b8 << 16),
        (0x1_0088, 0x00c0 << 16),
        (0x1_008c, 0x00b8 << 16),
        (0x1_00b0, 0x1880 << 16),
        (0x1_00b4, 0x1920 << 16),
        (0x2020, 0),
        (0x2024, 0),
        (0x202c, 0x0300_0400),
        (0x2030, 0x0501_0101),
        (0x3008, 0),
        (0x5000, 0x0110_80c0),
        (0x5008, 0x1100_0000),
        (0x200c, 0x0100_0000),
        (0x5004, 0x0100_0000),
    ] {
        write(io, u + a, v)?;
    }
    Ok(())
}

/// Reset the endpoint (its control word: 1, then wait for bit 0 to clear) and write its control
/// back (0x00FF6000, the value it has from the bring-up), as the stock firmware does.
fn reset_ep(io: &Io, p: &Path) -> Result<()> {
    let ctrl = 0x00fd_bd08 + 8 * (p.ep - 1);
    write(io, BYTE_ENABLE, 8)?;
    write(io, ctrl, 1)?;
    write(io, BYTE_ENABLE, 0xf)?;
    for _ in 0..POLLS {
        if read(io, ctrl)? & 1 == 0 {
            break;
        }
    }
    write(io, BYTE_ENABLE, 6)?;
    write(io, ctrl, 0x00ff_6000)?;
    write(io, BYTE_ENABLE, 0xf)?;
    let v = read(io, ctrl)?;
    write(io, ctrl, v)
}

/// Put the endpoint in DMA mode (`dma`: bit 16 + n set, bit n clear) or back in CPU mode, one
/// byte of [`EP_DMA`] at a time.
fn ep_mode(io: &Io, p: &Path, dma: bool) -> Result<()> {
    let (on, off) = (1u32 << (16 + p.ep), 1u32 << p.ep);
    write(io, BYTE_ENABLE, 2)?;
    let v = read(io, EP_DMA)?;
    write(io, EP_DMA, if dma { v | on } else { v & !on })?;
    write(io, BYTE_ENABLE, 8)?;
    let v = read(io, EP_DMA)?;
    write(io, EP_DMA, if dma { v & !off } else { v | off })?;
    write(io, BYTE_ENABLE, 0xf)
}

/// The endpoint in DMA mode (the stock order) and the stream channel woken and pointed at it.
fn set_up_channel(io: &Io, p: &Path) -> Result<()> {
    reset_ep(io, p)?;
    ep_mode(io, p, true)?;
    // The channel's clock on and a reset pulse (without them it ignores writes), then its
    // configuration and the run state.
    let k = (p.channel - 0x00fd_8200) / 0x20;
    let bit = 1u32 << (16 + k);
    let v = read(io, CHAN_CLOCK)?;
    write(io, CHAN_CLOCK, v | bit)?;
    let v = read(io, CHAN_RESET)?;
    write(io, CHAN_RESET, v | bit)?;
    write(io, p.channel, 0)?;
    write(io, p.channel, 1)?;
    let v = read(io, CHAN_RESET)?;
    write(io, CHAN_RESET, v & !bit)?;
    write(io, p.channel, 0)?;
    write(io, p.channel + 4, CHAN_CONF | (p.fifo << 12))?;
    write(io, p.channel, 2)?;
    Ok(())
}

/// Read and discard what capture `cap`'s endpoint still holds from an earlier stream: the
/// records it sent after the host had stopped reading (the feeder runs until the capture is
/// stopped), and the packet it was still filling, which the endpoint sends only when full
/// unless committed (as the pump commits a burst; an empty one goes as a zero-length packet).
fn drain(io: &Io, cap: Capture, p: &Path) -> Result<usize> {
    write(io, BYTE_ENABLE, 8)?;
    write(io, 0x00fd_bd0c + 8 * (p.ep - 1), 4)?;
    write(io, BYTE_ENABLE, 0xf)?;
    let mut buf = vec![0u8; 0x10000];
    let mut total = 0;
    while total < DRAIN_MAX {
        match io.usb.read(cap.ep(), &mut buf, DRAIN_WAIT) {
            Ok(n) if n > 0 => total += n,
            _ => break,
        }
    }
    Ok(total)
}

/// Start capture `cap`'s path: the capture is set up and its read offset where the stream
/// starts. On an error before the endpoint is touched the caller can still pump the ring.
pub fn start(io: &Io, cap: Capture) -> Result<()> {
    static GENERATION: AtomicU32 = AtomicU32::new(1);
    let p = path(cap);
    start_feeder(io, cap, &p)?;
    set_up_unit(io, cap, &p)?;
    set_up_channel(io, &p)?;
    for (addr, bit) in SYSTEM_BITS {
        set16(io, addr, bit)?;
    }
    let left = drain(io, cap, &p)?;
    if left > 0 {
        info!(
            "unit path: discarded {left} byte(s) an earlier stream left in EP 0x{:02x}",
            cap.ep()
        );
    }
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed).max(1);
    write(io, p.unit + fw::FEEDER_GO, generation)?;
    info!(
        "unit path: capture {cap:?} through unit 0x{:03x} and channel {} to EP 0x{:02x}",
        p.unit >> 20,
        (p.channel - 0x00fd_8200) / 0x20,
        cap.ep()
    );
    Ok(())
}

/// Make capture `cap`'s feeder stop sending, if it runs, and wait for it to answer; the unit
/// and the channel keep running, so what the feeder sent still reaches the endpoint.
pub fn pause(io: &Io, cap: Capture) -> Result<()> {
    let p = path(cap);
    if read(io, p.unit + fw::FEEDER_GO)? == 0 || !alive(io, p.unit)? {
        return Ok(());
    }
    // The feeder wrote 1 here while it waited for its go word (during the path's set-up);
    // it writes nothing here while the go word holds a generation, so clear it first.
    write(io, p.unit + fw::FEEDER_STOPPED, 0)?;
    write(io, p.unit + fw::FEEDER_GO, 0)?;
    for _ in 0..POLLS {
        if read(io, p.unit + fw::FEEDER_STOPPED)? == 1 {
            return Ok(());
        }
    }
    warn!("unit path: the feeder of capture {cap:?} did not stop");
    Ok(())
}

/// Stop capture `cap`'s path if it runs: the feeder stops sending (it is waited for), the
/// unit's core and blocks are held, the channel stopped and the endpoint put back in CPU mode.
/// After this the unit does not touch the capture.
///
/// A path whose feeder no longer runs is held too, when its go word (with the feeder
/// loaded) or the endpoint's DMA mode shows it was started: a host that died mid-stream
/// leaves the feeder stuck on a full endpoint, its pass counter still.
pub fn stop(io: &Io, cap: Capture) -> Result<()> {
    let p = path(cap);
    let running = alive(io, p.unit)?;
    let started = read(io, p.unit + fw::FEEDER_GO)? != 0 && loaded(io, cap, p.unit)?;
    let dma = read(io, EP_DMA)? & (1 << (16 + p.ep)) != 0;
    if running {
        pause(io, cap)?;
    } else if started || dma {
        info!("unit path: capture {cap:?}'s feeder was left stuck (its host stopped mid-stream)");
    }
    if running || started || dma {
        hold(io, p.unit)?;
        write(io, p.unit + fw::FEEDER_GO, 0)?;
        write(io, p.channel, 0)?;
    }
    // The endpoint back in CPU mode (also after a host that died with the path running), so
    // the main core's pump can feed it if the next stream's path does not start.
    if dma {
        ep_mode(io, &p, false)?;
        reset_ep(io, &p)?;
    }
    Ok(())
}
