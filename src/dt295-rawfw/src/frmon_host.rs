//! Host side of our own FR monitor ([`dt295_frmon`]): install it over the stock
//! boot0 window, then drive its PING / READ / WRITE / CALL.
//!
//! The stock boot0's 0xBC vendor window already reads and writes any 24-bit chip
//! address with no patch, so this installs the monitor by writing its body to
//! boot0 RAM and arming the 8-byte hook, both through that window. Once the hook
//! is armed, boot0 enters our body on the staging debug dispatch and the monitor
//! answers. The monitor adds the one thing the window cannot do -- run code on the
//! chip (CALL) -- plus a liveness PING and convenience 32-bit READ/WRITE.
//!
//! It is RAM-only and gated: the device must be cold (boot state 0) and the hook
//! site must hold the expected stock bytes, so only stock firmware is ever
//! patched, and a re-plug cold-boots the chip and discards everything.

use dt295_frmon as fw;
use tracing::info;

use dt295_core::proto::Io;
use dt295_core::{Error, Result};

/// Vendor request byte for the staging-buffer control transfer (the stock 0xBC window).
const REQ_WINDOW: u8 = 0xBC;

/// The hardware transport for the monitor's 64-byte packets: hand the staging buffer the
/// request with a control OUT carrying the dispatch nibble, then read the 64-byte reply
/// back from the staging buffer. This is the chip's own staging-dispatch mechanism, the
/// one our boot0 hook answers on.
#[allow(missing_debug_implementations)] // holds a live USB/IO handle; not logged directly
struct Staging<'a, 'b> {
    io: &'b Io<'a>,
}

impl<'a, 'b> Staging<'a, 'b> {
    fn new(io: &'b Io<'a>) -> Self {
        Staging { io }
    }

    /// The control transfer's wValue / wIndex: the staging address split across the two,
    /// with the dispatch nibble OR'd into wValue's low byte.
    fn dispatch_wvalue_windex() -> (u16, u16) {
        let value = (((fw::STAGING >> 8) & 0xff00) as u16) | u16::from(fw::DISPATCH_MODE);
        let index = (fw::STAGING & 0xffff) as u16;
        (value, index)
    }

    /// Send a request without reading the reply back. The monitor runs synchronously in the
    /// control-OUT handler, so a CALL or WRITE has run when this returns; one transfer instead
    /// of the roundtrip's two.
    fn send(&self, request: &[u8; 64]) -> Result<()> {
        let (value, index) = Self::dispatch_wvalue_windex();
        self.io.control_out(REQ_WINDOW, value, index, request)?;
        Ok(())
    }

    /// Send a request and read the 64-byte reply back from the staging buffer.
    fn roundtrip(&mut self, request: &[u8; 64]) -> Result<[u8; 64]> {
        let (value, index) = Self::dispatch_wvalue_windex();
        self.io.control_out(REQ_WINDOW, value, index, request)?;
        let mut response = [0u8; 64];
        self.io.read_block(fw::STAGING, &mut response)?;
        Ok(response)
    }
}

/// Boot state register; a cold device reads 0.
const REG_BOOT_STATE: u32 = 0x0008_2008;

/// A monitor command session over the staging buffer, owning the sequence counter.
#[allow(missing_debug_implementations)] // holds a live USB/IO handle; not logged directly
pub struct Session<'a, 'b> {
    staging: Staging<'a, 'b>,
    seq: u32,
}

impl<'a, 'b> Session<'a, 'b> {
    pub fn new(io: &'b Io<'a>) -> Self {
        Session {
            staging: Staging::new(io),
            seq: 0,
        }
    }

    fn exchange(
        &mut self,
        op: u32,
        address: u32,
        argument: u32,
        budget: u32,
        data: &[u8],
    ) -> Result<[u8; 64]> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let req = fw::pack(seq, op, address, argument, budget, data);
        let reply = self.staging.roundtrip(&req)?;
        let status = fw::check_reply(&reply, seq).map_err(|e| Error::Protocol(e.into()))?;
        if status != fw::STATUS_OK {
            return Err(Error::Protocol(format!(
                "monitor op {op} returned status {status}"
            )));
        }
        Ok(reply)
    }

    /// PING: confirm the body runs (its reply echoes 1 at the address field).
    pub fn ping(&mut self) -> Result<()> {
        let reply = self.exchange(fw::OP_PING, 0, 0, 0, &[])?;
        let echo = u32::from_be_bytes([
            reply[fw::OFF_ADDRESS],
            reply[fw::OFF_ADDRESS + 1],
            reply[fw::OFF_ADDRESS + 2],
            reply[fw::OFF_ADDRESS + 3],
        ]);
        if echo != 1 {
            return Err(Error::Protocol(format!(
                "monitor PING echo {echo}, expected 1"
            )));
        }
        Ok(())
    }

    /// READ up to 40 bytes (a multiple of 4) from `address` into `out`.
    pub fn read(&mut self, address: u32, out: &mut [u8]) -> Result<()> {
        check_span(address, out.len())?;
        let reply = self.exchange(fw::OP_READ, address, 0, out.len() as u32, &[])?;
        out.copy_from_slice(&reply[fw::OFF_DATA..fw::OFF_DATA + out.len()]);
        Ok(())
    }

    /// WRITE up to 40 bytes (a multiple of 4) from `data` to `address`.
    pub fn write(&mut self, address: u32, data: &[u8]) -> Result<()> {
        check_span(address, data.len())?;
        self.exchange(fw::OP_WRITE, address, 0, data.len() as u32, data)?;
        Ok(())
    }

    /// Fire-and-forget CALL: run the routine without reading its reply. The monitor runs
    /// synchronously in the control-OUT handler, so the routine has executed when this
    /// returns. The TS pump returns r4 = 0 (unused), so reading the reply is pure overhead;
    /// skipping it halves the pump's control transfers.
    pub fn call_fast(&mut self, address: u32) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let req = fw::pack(self.seq, fw::OP_CALL, address, 0, 0, &[]);
        self.staging.send(&req)
    }
}

/// The monitor's READ/WRITE are 32-bit-word loops, so the address and length must
/// be 4-aligned and the length at most the 40-byte payload.
fn check_span(address: u32, len: usize) -> Result<()> {
    if len > 40 || !len.is_multiple_of(4) || !address.is_multiple_of(4) {
        return Err(Error::Param(
            "monitor read/write needs a 4-aligned span of <= 40 bytes",
        ));
    }
    Ok(())
}

/// Write `data` at `address` through the window, then read it back and compare.
fn write_verify(io: &Io, address: u32, data: &[u8]) -> Result<()> {
    io.write_block(address, data)?;
    let mut back = vec![0u8; data.len()];
    io.read_block(address, &mut back)?;
    if back != data {
        return Err(Error::Firmware(format!(
            "frmon write-back mismatch at 0x{address:x}"
        )));
    }
    Ok(())
}

/// Is our FR monitor already loaded (a prior own-body session left it running), so a
/// blob-free datapath can be resumed without a re-plug? Checks the monitor BODY, not
/// the hook: the hook is a one-shot boot0 entry and is restored to the stock bytes once
/// the body is running, but the body itself persists at [`fw::BODY_ADDRESS`]. A pure
/// host-window read, so it never disturbs a cold device (which reads the stock boot0
/// body there); a re-plug clears RAM and this reads `false` again, forcing a fresh
/// bootstrap.
pub fn monitor_installed(io: &Io) -> Result<bool> {
    let body = fw::body();
    let mut cur = vec![0u8; body.len()];
    io.read_block(fw::BODY_ADDRESS, &mut cur)?;
    Ok(cur == body)
}

/// Install the monitor: the stock boot0 must be present at the hook site (the
/// authoritative "stock firmware, not yet booted" gate -- a downloaded firmware
/// would have different bytes there), then write the body, arm the hook (both
/// verified), and PING. Leaves the monitor live. The boot-state register is only
/// logged: its cold value is not relied upon here.
pub fn install(io: &Io) -> Result<()> {
    if install_check(io)? {
        let mut session = Session::new(io);
        session.ping()?;
        info!("frmon: monitor already installed (PING ok)");
        return Ok(());
    }
    install_write(io)
}

/// The install gate, read-only: inspect the boot0 hook site WITHOUT any write, so the board
/// is never touched on a chip we must not patch. Returns Ok(true) when our monitor is already
/// installed (the write can be skipped), Ok(false) when pristine stock boot0 is present (ready
/// to write), and Err when the site holds neither -- a booted firmware, or a half-written
/// monitor -- which needs a re-plug. Call this before the board bring-up.
pub fn install_check(io: &Io) -> Result<bool> {
    let state = io.read16(REG_BOOT_STATE).unwrap_or(0xffff);
    info!("frmon: boot state register 0x{state:04x}");
    let m = fw::monitor();
    let mut site = [0u8; 8];
    io.read_block(fw::HOOK_ADDRESS, &mut site)?;

    // If our own monitor is already armed and its body matches, it is installed (PING at the
    // call site confirms). Any other non-stock bytes (e.g. booted firmware) are refused below
    // -- the gate only ever lets us patch pristine stock boot0.
    if site == m.hook {
        let mut body = vec![0u8; m.body.len()];
        io.read_block(fw::BODY_ADDRESS, &mut body)?;
        if body == m.body {
            return Ok(true);
        }
    }
    if site != fw::STOCK_HOOK_SITE {
        return Err(Error::Firmware(format!(
            "boot0 hook site holds {site:02x?}, not the stock bytes {:02x?} \
             (boot state 0x{state:04x}); re-plug the device and retry",
            fw::STOCK_HOOK_SITE
        )));
    }
    Ok(false)
}

/// Write the monitor (body then hook) and confirm it is live (PING). Call only after
/// [`install_check`] returned Ok(false) (pristine stock boot0). Idempotent.
pub fn install_write(io: &Io) -> Result<()> {
    let m = fw::monitor();
    // Body first: harmless until the hook is armed (nothing jumps to it yet).
    write_verify(io, fw::BODY_ADDRESS, &m.body)?;
    info!("frmon: body written and verified ({} bytes)", m.body.len());
    // Arm the hook and confirm the monitor is live. If either fails, the body just written
    // would read back as the "brought up" marker, so the next tune would skip the bring-up and
    // never arm the hook (the pump CALL then fails every time, until a re-plug). On failure,
    // undo the marker in the REVERSE of the install order -- hook first, then body: restore the
    // stock hook so boot0 no longer jumps to our body, THEN clear the body's first word (so
    // monitor_installed reads false). Clearing the body first, while a written hook still points
    // at it, could run a corrupted body. The next tune then re-does the bring-up.
    let armed = (|| -> Result<()> {
        write_verify(io, fw::HOOK_ADDRESS, &m.hook)?;
        info!("frmon: hook armed");
        let mut session = Session::new(io);
        session.ping()?;
        info!("frmon: monitor is live (PING ok)");
        Ok(())
    })();
    if armed.is_err() {
        let _ = io.write_block(fw::HOOK_ADDRESS, &fw::STOCK_HOOK_SITE);
        let _ = io.write_block(fw::BODY_ADDRESS, &[0u8; 4]);
    }
    armed
}

/// Run a TS pump once (the boot0-RAM routine uploaded by [`upload_pump`]): a fire-and-forget
/// monitor CALL (the pump returns nothing the host uses, so skipping the reply read halves its
/// control traffic). `seq` is the caller's monitor sequence number, advanced here. The CALL is
/// one control transfer, so two stream workers can each run their pump between the other's.
pub fn call_pump(io: &Io, pump: &fw::Pump, seq: &mut u32) -> Result<()> {
    *seq = seq.wrapping_add(1);
    let req = fw::pack(*seq, fw::OP_CALL, pump.address, 0, 0, &[]);
    Staging::new(io).send(&req)
}

/// Upload a routine's bytes to `addr` through the monitor, verifying each block by
/// reading it back (the routine is sizeable, so a bad placement must fail loudly).
pub(crate) fn upload_routine(io: &Io, addr: u32, code: &[u8]) -> Result<()> {
    let mut mon = Session::new(io);
    let mut off = 0usize;
    while off < code.len() {
        let n = (code.len() - off).min(40);
        let mut chunk = [0u8; 40];
        chunk[..n].copy_from_slice(&code[off..off + n]);
        let span = n.div_ceil(4) * 4;
        let at = addr + off as u32;
        mon.write(at, &chunk[..span])?;
        let mut back = [0u8; 40];
        mon.read(at, &mut back[..span])?;
        if back[..n] != chunk[..n] {
            return Err(Error::Firmware(format!(
                "frmon routine upload verify mismatch at 0x{at:x}"
            )));
        }
        off += n;
    }
    Ok(())
}

/// Upload a TS pump routine to boot0 RAM through the monitor, ready to be CALLed each
/// stream-loop pass ([`call_pump`]): the one that drains the aux core's staging when `staged`
/// (the prefetch has the capture's ring), else the one that drains the ring itself.
pub fn upload_pump(io: &Io, pump: &fw::Pump, staged: bool) -> Result<()> {
    let code = if staged {
        fw::ts_pump_staged(pump)
    } else {
        fw::ts_pump_for(pump)
    };
    upload_routine(io, pump.address, &code)?;
    info!(
        "frmon: TS pump uploaded at 0x{:x} ({} bytes, {})",
        pump.address,
        code.len(),
        if staged {
            "from the aux core's staging"
        } else {
            "from the ring"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_checks() {
        assert!(check_span(0x8038_0000, 16).is_ok());
        assert!(check_span(0x8038_0000, 40).is_ok());
        assert!(check_span(0x8038_0000, 44).is_err(), "too long");
        assert!(check_span(0x8038_0000, 6).is_err(), "not word-multiple");
        assert!(check_span(0x8038_0002, 4).is_err(), "unaligned address");
    }
}
