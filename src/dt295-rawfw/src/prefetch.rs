//! The aux core's prefetch ([`dt295_frmon::aux_prefetch`]): start it, hand it a capture's ring
//! for a stream, take the ring back before the capture is set up or stopped.
//!
//! The prefetch copies a ring into the aux core's RAM, where the main core's pump
//! ([`dt295_frmon::ts_pump_staged`]) reads it about four times faster than SDRAM, so the two
//! captures together carry more TS than the main core alone can move. It owns the capture's
//! read offset while it has the ring: the host takes the ring back (and waits for the aux core
//! to let go) before it writes that offset itself.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use dt295_frmon as fw;
use tracing::{info, warn};

use dt295_core::proto::Io;
use dt295_core::{Error, Result};

use crate::frmon_host::Session;

/// How many reads (control transfers) to wait for the prefetch to take a request.
const ACK_POLLS: u32 = 200;

/// The image and its signature (assembled once).
fn image() -> &'static (Vec<u8>, u32) {
    static IMAGE: OnceLock<(Vec<u8>, u32)> = OnceLock::new();
    IMAGE.get_or_init(fw::aux_prefetch)
}

/// A main-core word (through the monitor).
fn read(io: &Io, addr: u32) -> Result<u32> {
    let mut b = [0u8; 4];
    Session::new(io).read(addr, &mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn write(io: &Io, addr: u32, v: u32) -> Result<()> {
    Session::new(io).write(addr, &v.to_be_bytes())
}

/// A mailbox word, by its aux-core address.
fn peek(io: &Io, aux: u32) -> Result<u32> {
    read(io, fw::aux_main_view(aux))
}

fn poke(io: &Io, aux: u32, v: u32) -> Result<()> {
    write(io, fw::aux_main_view(aux), v)
}

/// Is our prefetch running: its signature in place and its pass counter moving.
fn running(io: &Io) -> Result<bool> {
    if peek(io, fw::AUX_SIG)? != image().1 {
        return Ok(false);
    }
    let alive = peek(io, fw::AUX_ALIVE)?;
    for _ in 0..4 {
        if peek(io, fw::AUX_ALIVE)? != alive {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Hold the aux core in reset (it stops wherever it is; it never touches USB, so any moment
/// will do).
fn hold(io: &Io) -> Result<()> {
    write(io, fw::AUX_UNIT, 0xffff_ffff)
}

/// Start the prefetch unless it runs: hold the aux core, load the image, clear the mailbox,
/// point the reset vector at the image and let the core go.
fn ensure(io: &Io) -> Result<()> {
    if running(io)? {
        return Ok(());
    }
    hold(io)?;
    let image = &image().0;
    crate::frmon_host::upload_routine(io, fw::aux_main_view(fw::AUX_ENTRY), image)?;
    let words = [fw::AUX_SIG, fw::AUX_ALIVE]
        .into_iter()
        .chain(fw::AUX_RUN)
        .chain(fw::AUX_ACK)
        .chain(fw::AUX_HEAD)
        .chain(fw::AUX_TAIL);
    for w in words {
        poke(io, w, 0)?;
    }
    write(io, fw::AUX_VECTOR, fw::AUX_ENTRY)?;
    // Resets and clocks all on, the clocks back off, then the core (bit 17) and bit 16 out
    // of reset.
    for (addr, v) in [
        (fw::AUX_UNIT + 4, 0xffff_ffff),
        (fw::AUX_UNIT, 0xffff_ffff),
        (fw::AUX_UNIT + 4, 0x0080_0000),
        (fw::AUX_UNIT + 4, 0),
        (fw::AUX_UNIT, 0xfffc_ffff),
    ] {
        write(io, addr, v)?;
    }
    if !running(io)? {
        hold(io)?;
        return Err(Error::State("the aux core's prefetch did not start".into()));
    }
    info!("aux prefetch: started ({} bytes)", image.len());
    Ok(())
}

/// Wait for the prefetch to have taken request `want` for capture `index`.
fn wait_ack(io: &Io, index: usize, want: u32) -> Result<bool> {
    for _ in 0..ACK_POLLS {
        if peek(io, fw::AUX_ACK[index])? == want {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Hand capture `index`'s ring to the prefetch (starting it if need be), its staging empty.
/// The capture must be set up and its read offset where the stream starts. On an error the
/// caller pumps the ring itself ([`dt295_frmon::ts_pump_for`]).
pub fn take(io: &Io, index: usize) -> Result<()> {
    static GENERATION: AtomicU32 = AtomicU32::new(1);
    ensure(io)?;
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed).max(1);
    poke(io, fw::AUX_TAIL[index], 0)?;
    poke(io, fw::AUX_RUN[index], generation)?;
    if !wait_ack(io, index, generation)? {
        // It may still take the request late and move the read offset once more, beside
        // the caller's own pump: take the ring back as a stream's end does (wait for the
        // aux core to let go, or hold it).
        release(io, index)?;
        return Err(Error::State(
            "the aux core's prefetch did not take the ring".into(),
        ));
    }
    Ok(())
}

/// Take capture `index`'s ring back from the prefetch, if it runs: after this returns, the aux
/// core does not touch the capture. One that does not answer is held in reset (the other
/// capture's stream then stalls, and its restart starts the prefetch again).
pub fn release(io: &Io, index: usize) -> Result<()> {
    if !running(io)? {
        return Ok(());
    }
    poke(io, fw::AUX_RUN[index], 0)?;
    if !wait_ack(io, index, 0)? {
        warn!("aux prefetch: it did not let go of capture {index}; stopping it");
        hold(io)?;
    }
    Ok(())
}
