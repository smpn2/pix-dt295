//! Our own replacement boot0 monitor for the MB86M01 (Fujitsu FR CPU), and a tiny
//! assembler for the handful of FR instructions it needs.
//!
//! Why this exists: to output the scrambled TS PT3-style (descrambled by host
//! software with a real card), the chip must run code that the stock PIXELA
//! firmware does not provide. Rather than ship a third party's firmware binary,
//! we write our own. This crate is the first piece: a minimal "monitor" that
//! boot0 enters through an 8-byte hook, letting the host upload a routine into
//! chip RAM and run it. Everything else (programming the TS-capture registers,
//! the TS-to-USB loop) is uploaded and driven from the host on top of this.
//!
//! The monitor is RAM-only: a re-plug cold-boots the chip and discards it, so
//! there is no brick risk. It never writes flash.
//!
//! FR facts used here (16-bit big-endian instructions, R15 = SP, R13 is the
//! implicit index base of the `@(R13, Rj)` loads). Every opcode below was checked
//! against bytes disassembled from a real MB86M01 image; the tests pin the ones
//! that matter. No third-party code is copied -- only the public instruction
//! encoding of the CPU and the chip's documented boot0 geometry are used.

#![forbid(unsafe_code)]

use std::collections::HashMap;

/// Boot0 body load address (`MB86H_DT295_BOOT0_BODY_ADDRESS`); our monitor lives
/// here and the hook jumps to it.
pub const BODY_ADDRESS: u32 = 0x000e_1000;
/// The body may be at most this many bytes (`..._BODY_BYTES`).
pub const BODY_CAPACITY: usize = 0x800;
/// Where the stock boot0 enters our hook (`..._HOOK_ADDRESS`), overwriting the 4
/// instructions that start at this address.
pub const HOOK_ADDRESS: u32 = 0x000e_a6fc;
/// Staging buffer the host exchanges 64-byte packets through (`..._STAGING_ADDRESS`).
pub const STAGING: u32 = 0x000c_4620;

/// Where the stock control-OUT handler continues after the hook site when the
/// request is not ours: `MODE` path (the 4 overwritten instructions dispatch on
/// the request mode byte at ctx+0x46; mode 1 falls through, else branches).
const STOCK_END: u32 = 0x000e_a8c8;
const STOCK_MODE1: u32 = 0x000e_a704;
const STOCK_MODE_OTHER: u32 = 0x000e_a75a;

/// Request-context field offsets the stock handler uses (relative to R11). The
/// window target is at offset 0 and read with a plain `LD @R11`.
const CTX_MODE: u8 = 0x46; // u8: wValue low byte (0x7d marks a debug dispatch)
const CTX_WLENGTH: u8 = 0x48; // u16: wLength

/// Our monitor's packet magic ("DTMN"), the first word of the 64-byte request.
/// Distinct from any third-party magic so the two never collide.
pub const MAGIC: u32 = 0x4454_4d4e;
/// The dispatch mode byte the staging control-OUT carries (`wValue` low = 0x7d).
pub const DISPATCH_MODE: u8 = 0x7d;

// 64-byte packet layout (all big-endian u32), shared by request and reply.
pub const OFF_MAGIC: usize = 0;
pub const OFF_SEQ: usize = 4;
pub const OFF_OP: usize = 8; // reply: status
pub const OFF_ADDRESS: usize = 12; // reply (PING): liveness echo = 1
pub const OFF_ARGUMENT: usize = 16;
pub const OFF_BUDGET: usize = 20; // READ/WRITE: byte count (multiple of 4)
pub const OFF_DATA: usize = 24; // 40 payload bytes

/// Monitor operations (our own numbering).
pub const OP_PING: u32 = 1;
/// READ: copy `budget` bytes (multiple of 4) from `address` into the reply data.
pub const OP_READ: u32 = 2;
/// WRITE: copy `budget` bytes (multiple of 4) from the data area to `address`.
pub const OP_WRITE: u32 = 3;
/// CALL: run the routine at `address`. R4 is the `argument` field when it is
/// non-zero, else the staging base. The explicit R4 lets a callee take its own
/// packet at a scratch address, which the resident entry needs: it reads its
/// command packet from R4 and replies in place, while the monitor's own request
/// occupies staging, so the resident packet must live elsewhere.
pub const OP_CALL: u32 = 4;

/// Reply status values.
pub const STATUS_OK: u32 = 0;
pub const STATUS_INVALID: u32 = 5;

/// The 8 stock boot0 bytes at [`HOOK_ADDRESS`] the hook overwrites
/// (`LDI:8 #0x46,R13; LDUB @(R13,R11),R0; CMP #1,R0; BNE 0xea75a`). The installer
/// verifies these before patching, so only the expected stock firmware is hooked
/// and the body's reproduction of that dispatch is known to be correct.
pub const STOCK_HOOK_SITE: [u8; 8] = [0xc4, 0x6d, 0x02, 0xb0, 0xa8, 0x10, 0xe3, 0x2b];

/// Pack one 64-byte monitor request (all fields big-endian). `data` is copied into
/// the 40-byte payload area, zero-padded; at most 40 bytes are taken.
pub fn pack(seq: u32, op: u32, address: u32, argument: u32, budget: u32, data: &[u8]) -> [u8; 64] {
    let mut p = [0u8; 64];
    p[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_be_bytes());
    p[OFF_SEQ..OFF_SEQ + 4].copy_from_slice(&seq.to_be_bytes());
    p[OFF_OP..OFF_OP + 4].copy_from_slice(&op.to_be_bytes());
    p[OFF_ADDRESS..OFF_ADDRESS + 4].copy_from_slice(&address.to_be_bytes());
    p[OFF_ARGUMENT..OFF_ARGUMENT + 4].copy_from_slice(&argument.to_be_bytes());
    p[OFF_BUDGET..OFF_BUDGET + 4].copy_from_slice(&budget.to_be_bytes());
    let n = data.len().min(40);
    p[OFF_DATA..OFF_DATA + n].copy_from_slice(&data[..n]);
    p
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The reply status word (`OFF_OP`).
pub fn reply_status(reply: &[u8; 64]) -> u32 {
    be32(reply, OFF_OP)
}

/// Check a reply echoes our magic and the request sequence. Returns the status
/// word on success, or a short reason on a framing mismatch.
pub fn check_reply(reply: &[u8; 64], seq: u32) -> Result<u32, &'static str> {
    if be32(reply, OFF_MAGIC) != MAGIC {
        return Err("monitor reply magic mismatch");
    }
    if be32(reply, OFF_SEQ) != seq {
        return Err("monitor reply sequence mismatch");
    }
    Ok(reply_status(reply))
}

// --- FR registers -----------------------------------------------------------
// Only general registers R0..R15 are named; R13 is reserved as the load index.
#[allow(dead_code)]
mod reg {
    pub const R0: u8 = 0;
    pub const R1: u8 = 1;
    pub const R2: u8 = 2;
    pub const R3: u8 = 3;
    pub const R4: u8 = 4;
    pub const R5: u8 = 5;
    pub const R6: u8 = 6;
    pub const R7: u8 = 7;
    pub const R8: u8 = 8;
    pub const R9: u8 = 9;
    pub const R10: u8 = 10;
    pub const R11: u8 = 11;
    pub const R12: u8 = 12;
    pub const R13: u8 = 13; // implicit index base for @(R13, Rj)
}

/// A pending branch whose 8-bit displacement is filled in once its label is known.
#[derive(Debug)]
struct Fixup {
    /// Byte offset of the displacement byte (the instruction's second byte).
    at: usize,
    target: String,
    kind: &'static str,
}

/// A tiny FR assembler: emits big-endian instructions and resolves local branch
/// labels. Panics on misuse (a bad register, an out-of-range branch) -- it builds
/// one fixed program, so any error is a bug caught by the tests, not input.
#[derive(Debug)]
pub struct Asm {
    base: u32,
    code: Vec<u8>,
    labels: HashMap<String, u32>,
    fixups: Vec<Fixup>,
}

impl Asm {
    pub fn new(base: u32) -> Self {
        Asm {
            base,
            code: Vec::new(),
            labels: HashMap::new(),
            fixups: Vec::new(),
        }
    }

    fn emit16(&mut self, w: u16) {
        self.code.push((w >> 8) as u8);
        self.code.push(w as u8);
    }

    /// Current address (base + offset).
    fn here(&self) -> u32 {
        self.base + self.code.len() as u32
    }

    pub fn label(&mut self, name: &str) {
        let at = self.here();
        if self.labels.insert(name.to_string(), at).is_some() {
            panic!("duplicate label {name}");
        }
    }

    fn rr(op: u16, a: u8, b: u8) -> u16 {
        assert!(a < 16 && b < 16, "register out of range");
        op | ((a as u16) << 4) | (b as u16)
    }

    // LDI:8 #i8, Rc
    pub fn ldi8(&mut self, i8: u8, rc: u8) {
        self.emit16(0xC000 | ((i8 as u16) << 4) | rc as u16);
    }
    // LDI:20 #i20, Rc  (opcode 0x9b, then low 16 bits)
    pub fn ldi20(&mut self, i20: u32, rc: u8) {
        assert!(i20 <= 0xf_ffff, "ldi20 immediate too large");
        self.emit16(0x9B00 | (((i20 >> 16) & 0xf) as u16) << 4 | rc as u16);
        self.emit16(i20 as u16);
    }
    // LDI:32 #i32, Rc  (opcode 0x9f 0x8r, then 4 bytes)
    pub fn ldi32(&mut self, i32: u32, rc: u8) {
        self.emit16(0x9F80 | rc as u16);
        self.code.extend_from_slice(&i32.to_be_bytes());
    }
    // LD @Ri, Rc
    pub fn ld(&mut self, ri: u8, rc: u8) {
        self.emit16(Self::rr(0x0400, ri, rc));
    }
    // ST Rc, @Ri
    pub fn st(&mut self, rc: u8, ri: u8) {
        self.emit16(Self::rr(0x1400, ri, rc));
    }
    // STB Rc, @Ri  (byte store)
    pub fn stb(&mut self, rc: u8, ri: u8) {
        self.emit16(Self::rr(0x1600, ri, rc));
    }
    // STH Rc, @Ri  (halfword store)
    pub fn sth(&mut self, rc: u8, ri: u8) {
        self.emit16(Self::rr(0x1500, ri, rc));
    }
    // LDUB @Ri, Rc  (zero-extended byte load)
    pub fn ldub(&mut self, ri: u8, rc: u8) {
        self.emit16(Self::rr(0x0600, ri, rc));
    }
    // LDUH @Ri, Rc  (zero-extended halfword load)
    pub fn lduh(&mut self, ri: u8, rc: u8) {
        self.emit16(Self::rr(0x0500, ri, rc));
    }
    // LDUB @(R13, Rj), Rc
    pub fn ldub_x(&mut self, rj: u8, rc: u8) {
        self.emit16(Self::rr(0x0200, rj, rc));
    }
    // LDUH @(R13, Rj), Rc
    pub fn lduh_x(&mut self, rj: u8, rc: u8) {
        self.emit16(Self::rr(0x0100, rj, rc));
    }
    // MOV Rj, Ri  (Ri = Rj)
    pub fn mov(&mut self, rj: u8, ri: u8) {
        self.emit16(Self::rr(0x8B00, rj, ri));
    }
    // AND Rj, Ri  (Ri &= Rj)
    pub fn and(&mut self, rj: u8, ri: u8) {
        self.emit16(Self::rr(0x8200, rj, ri));
    }
    // OR Rj, Ri  (Ri |= Rj)
    pub fn or(&mut self, rj: u8, ri: u8) {
        self.emit16(Self::rr(0x9200, rj, ri));
    }
    // LSR Rj, Ri  (Ri >>= Rj, logical)
    pub fn lsr(&mut self, rj: u8, ri: u8) {
        self.emit16(Self::rr(0xB200, rj, ri));
    }
    // LSL #u4, Ri  (shift left by 0..15)
    pub fn lsl_i(&mut self, imm: u8, ri: u8) {
        assert!(imm < 16);
        self.emit16(0xB400 | ((imm as u16) << 4) | ri as u16);
    }
    // LSL2 #u4, Ri  (shift left by 16..31; actual shift = imm + 16)
    pub fn lsl2_i(&mut self, imm: u8, ri: u8) {
        assert!(imm < 16);
        self.emit16(0xB500 | ((imm as u16) << 4) | ri as u16);
    }
    // EXTSB Ri  (sign-extend the low byte)
    pub fn extsb(&mut self, ri: u8) {
        assert!(ri < 16);
        self.emit16(0x9780 | ri as u16);
    }
    // ADDN #i4, Rc
    pub fn addn_i(&mut self, i4: u8, rc: u8) {
        assert!(i4 < 16);
        self.emit16(0xA000 | ((i4 as u16) << 4) | rc as u16);
    }
    // ADDN Rj, Ri  (Ri = Ri + Rj, no carry flag update)
    pub fn addn(&mut self, rj: u8, ri: u8) {
        self.emit16(Self::rr(0xA200, rj, ri));
    }
    // SUBN Rj, Ri  (Ri = Ri - Rj)
    pub fn subn(&mut self, rj: u8, ri: u8) {
        self.emit16(Self::rr(0xAE00, rj, ri));
    }
    // CMP Rj, Ri  (set flags from Ri - Rj)
    pub fn cmp(&mut self, rj: u8, ri: u8) {
        self.emit16(Self::rr(0xAA00, rj, ri));
    }
    // CMP #i4, Ri
    pub fn cmp_i(&mut self, i4: u8, ri: u8) {
        assert!(i4 < 16);
        self.emit16(0xA800 | ((i4 as u16) << 4) | ri as u16);
    }
    // JMP @Ri  (opcode 0x97 0x0r)
    pub fn jmp(&mut self, ri: u8) {
        assert!(ri < 16);
        self.emit16(0x9700 | ri as u16);
    }
    // CALL @Ri  (opcode 0x97 0x1r; JMP with bit 4 set, saves the return address in RP)
    pub fn call(&mut self, ri: u8) {
        assert!(ri < 16);
        self.emit16(0x9700 | 0x10 | ri as u16);
    }
    pub fn nop(&mut self) {
        self.emit16(0x9FA0);
    }
    // ANDCCR #u8 / ORCCR #u8  (andccr #0xEF masks interrupts, orccr #0x10 unmasks them)
    pub fn andccr(&mut self, u8: u8) {
        self.emit16(0x8300 | u8 as u16);
    }
    pub fn orccr(&mut self, u8: u8) {
        self.emit16(0x9300 | u8 as u16);
    }
    pub fn ret(&mut self) {
        self.emit16(0x9720);
    }

    /// Pad with NOPs until the next emitted instruction lands at absolute address `addr`, so a
    /// routine can be placed at a fixed on-chip address (to match a hardware PC-region gate, say).
    /// All instructions are 2- or 4-byte, so `here()` stays even; panics if already past `addr`.
    pub fn pad_to(&mut self, addr: u32) {
        assert!(self.here() <= addr, "pad_to: already at/past {addr:#x}");
        assert!(
            (addr - self.here()).is_multiple_of(2),
            "pad_to: odd gap to {addr:#x}"
        );
        while self.here() < addr {
            self.nop();
        }
    }

    fn branch(&mut self, op: u16, target: &str, kind: &'static str) {
        self.emit16(op); // placeholder low byte = 0
        self.fixups.push(Fixup {
            at: self.code.len() - 1,
            target: target.to_string(),
            kind,
        });
    }
    pub fn bne(&mut self, target: &str) {
        self.branch(0xE300, target, "bne");
    }
    pub fn beq(&mut self, target: &str) {
        self.branch(0xE200, target, "beq");
    }
    pub fn bra(&mut self, target: &str) {
        self.branch(0xE000, target, "bra");
    }
    // BHI (unsigned higher) / BLS (unsigned lower-or-same).
    pub fn bhi(&mut self, target: &str) {
        self.branch(0xEF00, target, "bhi");
    }
    pub fn bls(&mut self, target: &str) {
        self.branch(0xEE00, target, "bls");
    }
    // BC (unsigned lower / carry set).
    pub fn bc(&mut self, target: &str) {
        self.branch(0xE400, target, "bc");
    }

    /// Emit `ldi32 <abs(label)>, r0 ; jmp @r0` -- an unconditional long jump to a label, for loops
    /// longer than the +-256-byte conditional-branch range. Fixed up in finish() with the label's
    /// absolute address. Clobbers r0.
    pub fn jmp_label(&mut self, target: &str) {
        self.ldi32(0, 0); // placeholder; immediate is the last 4 bytes
        let at = self.code.len() - 4;
        self.fixups.push(Fixup {
            at,
            target: target.to_string(),
            kind: "ldi32abs",
        });
        self.jmp(0);
    }

    /// Resolve branches and return the assembled bytes.
    pub fn finish(mut self) -> Vec<u8> {
        for fx in &self.fixups {
            let target = *self
                .labels
                .get(&fx.target)
                .unwrap_or_else(|| panic!("{} to undefined label {}", fx.kind, fx.target));
            if fx.kind == "ldi32abs" {
                // Long jump: write the label's absolute address into the ldi32 immediate.
                self.code[fx.at..fx.at + 4].copy_from_slice(&target.to_be_bytes());
                continue;
            }
            let instr_addr = self.base + (fx.at - 1) as u32;
            let delta = target as i64 - (instr_addr as i64 + 2);
            assert!(delta % 2 == 0, "odd branch displacement for {}", fx.target);
            let disp = delta / 2;
            assert!(
                (-128..=127).contains(&disp),
                "{} to {} out of range ({disp})",
                fx.kind,
                fx.target
            );
            self.code[fx.at] = (disp as i8) as u8;
        }
        self.code
    }
}

/// The assembled monitor: the 8-byte hook for [`HOOK_ADDRESS`] and the body for
/// [`BODY_ADDRESS`].
#[derive(Debug, Clone)]
pub struct Monitor {
    pub hook: [u8; 8],
    pub body: Vec<u8>,
}

/// Build the 8-byte boot0 hook: `LDI:20 #BODY_ADDRESS, R12; JMP @R12; NOP`.
pub fn hook() -> [u8; 8] {
    let mut a = Asm::new(HOOK_ADDRESS);
    a.ldi20(BODY_ADDRESS, reg::R12);
    a.jmp(reg::R12);
    a.nop();
    let v = a.finish();
    let mut h = [0u8; 8];
    h.copy_from_slice(&v);
    h
}

/// Build the monitor body. Entered from the hook with R11 = the request context.
/// If the request is our debug dispatch (mode 0x7d, wLength 64, window target =
/// staging, first word = [`MAGIC`]) it is handled here; otherwise control returns
/// to the stock handler exactly as the overwritten instructions would have sent
/// it (dispatch on the mode byte: 1 continues at `STOCK_MODE1`, else
/// `STOCK_MODE_OTHER`).
pub fn body() -> Vec<u8> {
    use reg::*;
    let mut a = Asm::new(BODY_ADDRESS);

    // --- gate: is this our debug dispatch? ---
    a.ldi8(CTX_MODE, R13);
    a.ldub_x(R11_INDEX, R0); // R0 = ctx[0x46] (mode)
    a.ldi8(DISPATCH_MODE, R1);
    a.cmp(R1, R0);
    a.bne("stock");
    a.ldi8(CTX_WLENGTH, R13);
    a.lduh_x(R11_INDEX, R0); // R0 = ctx[0x48] (wLength)
    a.ldi8(0x40, R1);
    a.cmp(R1, R0);
    a.bne("stock");
    a.ld(R11_INDEX, R0); // R0 = ctx[0x00] (window target)
    a.ldi20(STAGING, R1);
    a.cmp(R1, R0);
    a.bne("stock");
    a.ld(R1, R0); // R0 = *staging (first word)
    a.ldi32(MAGIC, R13);
    a.cmp(R13, R0);
    a.bne("stock");

    // --- ours: R4 = staging base; R1 = &status (staging+8); R2 = op ---
    a.mov(R1, R4);
    a.ldi8(OFF_OP as u8, R1);
    a.addn(R4, R1);
    a.ld(R1, R2);

    // PING
    a.cmp_i(OP_PING as u8, R2);
    a.bne("not_ping");
    a.ldi8(STATUS_OK as u8, R3);
    a.st(R3, R1); // status = 0
    a.ldi8(OFF_ADDRESS as u8, R5);
    a.addn(R4, R5);
    a.ldi8(1, R3);
    a.st(R3, R5); // liveness echo at +12 = 1
    a.bra("done");

    // READ: src = address(+12), n = budget(+20), dst = +24
    a.label("not_ping");
    a.cmp_i(OP_READ as u8, R2);
    a.bne("not_read");
    a.ldi8(OFF_ADDRESS as u8, R5);
    a.addn(R4, R5);
    a.ld(R5, R6); // R6 = src
    a.ldi8(OFF_BUDGET as u8, R5);
    a.addn(R4, R5);
    a.ld(R5, R7); // R7 = n bytes
    a.ldi8(OFF_DATA as u8, R5);
    a.addn(R4, R5); // R5 = dst
    a.ldi8(4, R9);
    a.label("read_loop");
    a.cmp_i(0, R7);
    a.beq("read_done");
    a.ld(R6, R8);
    a.st(R8, R5);
    a.addn_i(4, R6);
    a.addn_i(4, R5);
    a.subn(R9, R7);
    a.bra("read_loop");
    a.label("read_done");
    a.ldi8(STATUS_OK as u8, R3);
    a.st(R3, R1);
    a.bra("done");

    // WRITE: dst = address(+12), n = budget(+20), src = +24
    a.label("not_read");
    a.cmp_i(OP_WRITE as u8, R2);
    a.bne("not_write");
    a.ldi8(OFF_ADDRESS as u8, R5);
    a.addn(R4, R5);
    a.ld(R5, R6); // R6 = dst
    a.ldi8(OFF_BUDGET as u8, R5);
    a.addn(R4, R5);
    a.ld(R5, R7); // R7 = n
    a.ldi8(OFF_DATA as u8, R5);
    a.addn(R4, R5); // R5 = src
    a.ldi8(4, R9);
    a.label("write_loop");
    a.cmp_i(0, R7);
    a.beq("write_done");
    a.ld(R5, R8);
    a.st(R8, R6);
    a.addn_i(4, R5);
    a.addn_i(4, R6);
    a.subn(R9, R7);
    a.bra("write_loop");
    a.label("write_done");
    a.ldi8(STATUS_OK as u8, R3);
    a.st(R3, R1);
    a.bra("done");

    // CALL: run routine at address(+12). R4 = argument(+16) if non-zero, else the
    // staging base. Read the routine and the R4 override from the monitor's own
    // packet (R4 = staging base here) before R4 is overwritten.
    a.label("not_write");
    a.cmp_i(OP_CALL as u8, R2);
    a.bne("bad");
    a.ldi8(OFF_ADDRESS as u8, R5);
    a.addn(R4, R5);
    a.ld(R5, R12); // R12 = routine
    a.ldi8(OFF_ARGUMENT as u8, R5);
    a.addn(R4, R5);
    a.ld(R5, R6); // R6 = R4 override (0 = keep staging base)
    a.ldi8(STATUS_OK as u8, R3);
    a.st(R3, R1); // default status OK; the routine may overwrite it
    a.cmp_i(0, R6);
    a.beq("call_go");
    a.mov(R6, R4); // R4 = explicit packet pointer for the callee
    a.label("call_go");
    a.call(R12);
    a.bra("done");

    // Unknown op.
    a.label("bad");
    a.ldi8(STATUS_INVALID as u8, R3);
    a.st(R3, R1);

    // Return into the stock handler's end-of-request epilogue.
    a.label("done");
    a.ldi20(STOCK_END, R12);
    a.jmp(R12);

    // --- not ours: reproduce the overwritten instructions' dispatch ---
    a.label("stock");
    a.ldi8(CTX_MODE, R13);
    a.ldub_x(R11_INDEX, R0); // R0 = mode
    a.cmp_i(1, R0);
    a.bne("stock_other");
    a.ldi20(STOCK_MODE1, R12);
    a.jmp(R12);
    a.label("stock_other");
    a.ldi20(STOCK_MODE_OTHER, R12);
    a.jmp(R12);

    let v = a.finish();
    assert!(
        v.len() <= BODY_CAPACITY,
        "monitor body exceeds boot0 capacity"
    );
    v
}

/// R11 holds the request context; the `@(R13, Rj)` loads use it as the index.
const R11_INDEX: u8 = 11;

pub fn monitor() -> Monitor {
    Monitor {
        hook: hook(),
        body: body(),
    }
}

/// Capture ring and EP registers the TS pump drives (DT295). The ring read/write pointers
/// are byte offsets into the ring.
///
/// The ring: 4 MiB of SDRAM at 0x02000000, which holds about 0.85 s of a full CS
/// transponder (~4.9 MB/s), so the host may call the pump that late before TS is lost. The
/// vendor's 192 KiB ring held about 40 ms, and it overflows even on a terrestrial channel
/// when the host stalls (measured by a third party on a PIX-DT295, which also ran its ring
/// at 0x02000000). The length is a multiple of the 192-byte record, so the hardware wraps
/// on a record boundary, and of the 512-byte burst, so no burst is ever cut short at the
/// ring end; the length register keeps only multiples of 64.
pub const RING_BASE: u32 = 0x0200_0000; // physical, what the capture DMA writes
pub const RING_SIZE: u32 = 0x003f_fc00; // 2730 x 1536 (lcm of 192 and 512)
pub const REG_RING_WRITE: u32 = 0x0190_2028; // hardware write offset
pub const REG_RING_READ: u32 = 0x0190_202c; // consumer offset (we write it back)
pub const EP_FIFO: u32 = 0x00fd_bd84; // 32-bit FIFO data port
pub const EP_FIFO_CTRL: u32 = 0x00fd_bd0c; // bit2 = ready; write 4 = commit
/// The byte lanes a 32-bit store writes, bit0 = the most significant byte (0xF, all four, is
/// the default). The commit writes 8 first, so its store of 4 to [`EP_FIFO_CTRL`] lands in
/// the low byte only, then puts 0xF back.
pub const EP_BYTE_ENABLE: u32 = 0x00fd_814c;
/// One TS pump: which capture ring it drains and which bulk IN endpoint it feeds.
///
/// The chip has a second TS capture (input 0x00D21000, engine 0x00D22000) besides the one at
/// 0x0190xxxx; on a PIX-DT195W it takes the demod's output B. [`PUMP_A`] drains the first into
/// EP 0x81, [`PUMP_B`] the second into EP 0x86. An endpoint's FIFO status is at
/// 0x00FDBD0C + 8 x (n - 1) and its data port at 0x00FDBD80 + 4 x n.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pump {
    /// Where the routine is uploaded (boot0 RAM, inside the 0x800 bytes at [`BODY_ADDRESS`]).
    pub address: u32,
    /// The ring's physical base (what the capture DMA writes; the CPU reads it | 0x80000000).
    pub ring_base: u32,
    /// The hardware's write offset register.
    pub ring_write: u32,
    /// The consumer offset register (the pump writes it back).
    pub ring_read: u32,
    /// The endpoint's 32-bit FIFO data port.
    pub fifo: u32,
    /// The endpoint's FIFO status (bit2 = ready; writing 4 commits).
    pub fifo_ctrl: u32,
    /// Which capture this is (0 the first, 1 the second): its slot in the aux core's
    /// prefetch ([`AUX_STAGE`] and the mailbox words).
    pub index: usize,
}

/// The first capture (0x0190xxxx) into EP 0x81.
pub const PUMP_A: Pump = Pump {
    address: PUMP_ADDRESS,
    ring_base: RING_BASE,
    ring_write: REG_RING_WRITE,
    ring_read: REG_RING_READ,
    fifo: EP_FIFO,
    fifo_ctrl: EP_FIFO_CTRL,
    index: 0,
};

/// The second capture (0x00D2xxxx) into EP 0x86. Its ring is the 4 MiB after the first one's
/// (below 0x02800000, which the chip's ROM uses).
pub const PUMP_B: Pump = Pump {
    address: PUMP_B_ADDRESS,
    ring_base: RING_B_BASE,
    ring_write: 0x00d2_2028,
    ring_read: 0x00d2_202c,
    fifo: 0x00fd_bd98,
    fifo_ctrl: 0x00fd_bd34,
    index: 1,
};

/// The second capture's ring (physical). Same [`RING_SIZE`] as the first.
pub const RING_B_BASE: u32 = 0x0240_0000;

/// TS record geometry: 192-byte ring records = 188 TS bytes + a 4-byte suffix.
pub const RECORD_BYTES: u32 = 192;
pub const TS_BYTES: u32 = 188;
/// Records forwarded per pump call, bounding on-chip time so the host reads EP
/// between calls (the resident uses the same ~64/SERVICE batch).
pub const PUMP_BATCH: u8 = 64;

/// The largest EP FIFO burst: 512 bytes (a full USB bulk packet, as the resident
/// uses), far fewer commits than one per TS packet. The tail burst at the ring end
/// is shorter.
pub const FIFO_BLOCK_BYTES: u32 = 512;

// The ring wraps on a record and after a whole burst, the length register holds it (multiples
// of 64), and each ring stays within its 4 MiB (the second ends below 0x02800000).
const _: () = assert!(
    RING_SIZE.is_multiple_of(RECORD_BYTES)
        && RING_SIZE.is_multiple_of(FIFO_BLOCK_BYTES)
        && RING_SIZE.is_multiple_of(64)
        && RING_SIZE <= 0x0040_0000
        && RING_B_BASE >= RING_BASE + 0x0040_0000
        && RING_B_BASE + 0x0040_0000 <= 0x0280_0000
);

/// How long the pump spins for the EP FIFO to become ready before giving up for this
/// call. The FIFO drains to the host's queued (async) EP reads at USB speed, so it is
/// normally ready again within microseconds; this only bounds the case where the host has
/// stopped reading (stream ending), after which the pump returns and the stream loop
/// stops it.
pub const FIFO_POLL: u32 = 0x0001_0000;

/// Our own TS pump for [`PUMP_A`] (see [`ts_pump_for`]).
pub fn ts_pump() -> Vec<u8> {
    ts_pump_for(&PUMP_A)
}

/// Our own TS pump: forward up to [`PUMP_BATCH`] FIFO bursts of a capture ring to its
/// endpoint, replacing the resident's SERVICE-driven ring->EP copy. It streams the
/// ring contiguously -- 192-byte records with their 4-byte suffix -- in bursts of up
/// to 512 bytes (the host strips the suffix with 192-byte framing), guard-committing
/// each. A burst is clamped so it never crosses the ring-end: the tail burst is
/// short and leaves the consumer offset at 0 (a record boundary), which keeps the
/// delivered byte stream clean across the wrap. It stops when less than one burst is
/// available or the FIFO is not ready. Entered by a monitor CALL; uses only
/// caller-saved r0..r7 and makes no nested calls, so no stack frame. Returns r4 = 0.
pub fn ts_pump_for(p: &Pump) -> Vec<u8> {
    use reg::*;
    let mut a = Asm::new(p.address);

    a.ldi8(PUMP_BATCH, R7); // r7 = burst budget
    a.ldi32(p.ring_base | 0x8000_0000, R6); // r6 = ring base, CPU view (constant across the call)

    a.label("loop");
    a.cmp_i(0, R7);
    a.beq("done");

    // r2 = read offset, r3 = write offset.
    a.ldi32(p.ring_read, R1);
    a.ld(R1, R2);
    a.ldi32(p.ring_write, R1);
    a.ld(R1, R3);

    // available = (W - R) mod ring size, into r0. The ring size is past ldi20's 20 bits.
    a.mov(R3, R0);
    a.subn(R2, R0); // r0 = W - R (two's complement)
    a.ldi32(RING_SIZE - 1, R1);
    a.cmp(R1, R0);
    a.bls("have_avail"); // r0 <= ring_size-1 => W >= R, value is the avail
    a.ldi32(RING_SIZE, R1);
    a.addn(R1, R0); // wrapped: avail = W - R + ring_size
    a.label("have_avail");

    // burst = min(512, ring_size - R), in r5. Clamping to the ring end means a burst
    // never straddles the wrap, and the tail burst lands R back on 0.
    a.ldi32(RING_SIZE, R5);
    a.subn(R2, R5); // r5 = ring_size - R (bytes to the ring end)
    a.ldi20(FIFO_BLOCK_BYTES, R1);
    a.cmp(R1, R5);
    a.bls("have_burst"); // r5 <= 512 => burst = r5 (the short tail)
    a.mov(R1, R5); // else burst = 512
    a.label("have_burst");

    // Need the whole burst present: avail (r0) >= burst (r5), i.e. stop if r0 < r5.
    a.cmp(R5, R0);
    a.bc("done");

    // FIFO ready? Wait (bounded) for it instead of stopping at the first not-ready. The
    // resident's SERVICE drains a full batch by letting the FIFO catch up; stopping early
    // delivered only a fraction of the rate and overflowed the ring (dropping the sparse
    // SI / PCR). The host's async EP reads drain the FIFO at USB speed while this polls; if
    // the host has stopped reading, the budget runs out and the pump returns.
    a.ldi32(p.fifo_ctrl, R1); // r1 = ctrl addr (kept through the poll)
    a.ldi20(FIFO_POLL, R3); // r3 = poll budget (free until the copy reuses r3)
    a.label("fifo_wait");
    a.ld(R1, R0);
    a.ldi8(4, R4); // r4 = ready mask (free until EP_FIFO is loaded into it)
    a.and(R4, R0);
    a.cmp_i(0, R0);
    a.bne("fifo_ok"); // ready -> drain this burst
    a.ldi8(1, R4);
    a.subn(R4, R3); // budget--
    a.cmp_i(0, R3);
    a.beq("done"); // host not reading -> stop this call
    a.bra("fifo_wait");
    a.label("fifo_ok");

    // Copy burst/4 words from ring[R] to the FIFO (contiguous: burst <= ring end - R).
    // r3 = end address = base + R + burst; r1 = running source pointer.
    a.mov(R6, R1);
    a.addn(R2, R1); // src = base + R
    a.mov(R1, R3);
    a.addn(R5, R3); // end = src + burst
    a.ldi32(p.fifo, R4);
    a.label("copyw");
    a.ld(R1, R0);
    a.st(R0, R4); // FIFO <- word
    a.addn_i(4, R1);
    a.cmp(R1, R3);
    a.bne("copyw");

    // Commit, the low byte only: 0xFD814C=8, 0xFDBD0C=4, 0xFD814C=0xF.
    a.ldi32(EP_BYTE_ENABLE, R1);
    a.ldi8(8, R0);
    a.st(R0, R1);
    a.ldi32(p.fifo_ctrl, R3);
    a.ldi8(4, R0);
    a.st(R0, R3);
    a.ldi8(0xf, R0);
    a.st(R0, R1); // r1 still EP_BYTE_ENABLE

    // Advance the read offset by the burst, wrapping at the ring end.
    a.addn(R5, R2); // R += burst
    a.ldi32(RING_SIZE, R1);
    a.cmp(R1, R2);
    a.bne("nowrap");
    a.ldi8(0, R2);
    a.label("nowrap");
    a.ldi32(p.ring_read, R1);
    a.st(R2, R1);

    a.ldi8(1, R1);
    a.subn(R1, R7);
    a.bra("loop");

    a.label("done");
    a.ldi8(0, R4);
    a.ret();

    a.finish()
}

// ---- The aux core's prefetch ----
//
// The pump's time goes about half to its SDRAM reads (~175 ns a word; the FIFO stores take
// ~230 ns) and the chip's CPU runs both captures' pumps, so the two carry about 8.6 MB/s of
// TS in all. The chip has a second FR core (unit 0x017's; the stock firmware runs its USB
// stack there) that this stub leaves idle. It cannot feed the FIFOs itself: it reaches the
// USB block only while 0x00F1010C bit 3 hands the block over, and then the main core loses it
// (its EP0 fails). It can read SDRAM, though, so it copies each ring into its own RAM, where
// the main core reads ~4x faster, and the main core's pump only moves that to the FIFO.

/// The aux core's unit registers (main view): +0 resets (bit 17 the core), +4 clocks.
pub const AUX_UNIT: u32 = 0x0170_0000;
/// Its reset vector (main view; write-only).
pub const AUX_VECTOR: u32 = 0x017f_fffc;
/// The main core's view of an address in the aux core's 16 KiB RAM (aux view 0x000A0000).
pub const fn aux_main_view(aux: u32) -> u32 {
    AUX_UNIT + aux
}
/// Where the prefetch is loaded and entered (aux view).
pub const AUX_ENTRY: u32 = 0x000a_2d00;
/// Each capture's staging: [`AUX_SLOTS`] bursts of [`FIFO_BLOCK_BYTES`] (aux view).
pub const AUX_STAGE: [u32; 2] = [0x000a_0000, 0x000a_1000];
pub const AUX_SLOTS: u32 = 8;
// The mailbox (aux view). A main-core store into this RAM rewrites the words around it with
// what it read a moment before, so words two writers share would lose updates: each word sits
// in its own 64 bytes.
/// The prefetch's signature (it writes it on entry): which image runs.
pub const AUX_SIG: u32 = 0x000a_3c00;
/// A counter the prefetch bumps every pass: it runs.
pub const AUX_ALIVE: u32 = 0x000a_3c40;
/// Per capture, the host's request: 0 = leave the ring alone, else a generation to take it.
pub const AUX_RUN: [u32; 2] = [0x000a_3c80, 0x000a_3cc0];
/// Per capture, the request the prefetch has taken (it copies only while this is not 0).
pub const AUX_ACK: [u32; 2] = [0x000a_3d00, 0x000a_3d40];
/// Per capture, bursts staged (the prefetch's) and bursts sent (the main pump's).
pub const AUX_HEAD: [u32; 2] = [0x000a_3d80, 0x000a_3dc0];
pub const AUX_TAIL: [u32; 2] = [0x000a_3e00, 0x000a_3e40];
/// How long the prefetch pauses (register-only loop passes) when it has nothing to copy: a
/// tight loop on the shared bus starves the main core's reads of this RAM.
const AUX_IDLE_PAUSE: u32 = 0x200;

/// The aux core's prefetch, and its signature (a hash of the image, which it writes to
/// [`AUX_SIG`] on entry). For each capture whose request ([`AUX_RUN`]) it has taken: while
/// a staging slot is free and the ring holds a burst, it copies the burst into the slot and
/// advances the capture's read offset. A new request empties the staging; 0 stops the
/// copying. It never touches USB.
pub fn aux_prefetch() -> (Vec<u8>, u32) {
    let sig = {
        let image = aux_prefetch_with(0);
        image.iter().fold(0x811c_9dc5u32, |h, &b| {
            (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
        }) | 1
    };
    (aux_prefetch_with(sig), sig)
}

fn aux_prefetch_with(sig: u32) -> Vec<u8> {
    use reg::*;
    let mut a = Asm::new(AUX_ENTRY);
    a.ldi32(sig, R0);
    a.ldi32(AUX_SIG, R1);
    a.st(R0, R1);
    a.ldi32(AUX_ALIVE, R8);
    a.label("pass");
    a.ld(R8, R0);
    a.addn_i(1, R0);
    a.st(R0, R8);
    a.ldi8(0, R12); // r12 = did anything this pass
    for (c, p) in [PUMP_A, PUMP_B].iter().enumerate() {
        let l = |n: &str| format!("{n}{c}");
        // A new request: take it (emptying the staging first when it is one to run).
        a.ldi32(AUX_RUN[c], R1);
        a.ld(R1, R2); // r2 = run
        a.ldi32(AUX_ACK[c], R1);
        a.ld(R1, R3); // r3 = ack
        a.cmp(R3, R2);
        a.beq(&l("taken"));
        a.cmp_i(0, R2);
        a.beq(&l("answer"));
        a.ldi8(0, R0);
        a.ldi32(AUX_HEAD[c], R4);
        a.st(R0, R4);
        a.label(&l("answer"));
        a.st(R2, R1); // ack = run
        a.ldi8(1, R12);
        a.bra(&l("next"));
        a.label(&l("taken"));
        a.cmp_i(0, R2);
        a.beq(&l("next"));
        // A free slot? head - tail < AUX_SLOTS.
        a.ldi32(AUX_HEAD[c], R1);
        a.ld(R1, R2); // r2 = head
        a.ldi32(AUX_TAIL[c], R1);
        a.ld(R1, R3); // r3 = tail
        a.mov(R2, R0);
        a.subn(R3, R0);
        a.ldi8(AUX_SLOTS as u8, R1);
        a.cmp(R1, R0);
        a.bc(&l("room"));
        a.bra(&l("next"));
        a.label(&l("room"));
        // A whole burst in the ring? (W - R) mod size >= 512.
        a.ldi32(p.ring_read, R1);
        a.ld(R1, R4); // r4 = R
        a.ldi32(p.ring_write, R1);
        a.ld(R1, R5); // r5 = W
        a.mov(R5, R0);
        a.subn(R4, R0);
        a.ldi32(RING_SIZE - 1, R1);
        a.cmp(R1, R0);
        a.bls(&l("avail"));
        a.ldi32(RING_SIZE, R1);
        a.addn(R1, R0);
        a.label(&l("avail"));
        a.ldi20(FIFO_BLOCK_BYTES, R1);
        a.cmp(R1, R0);
        a.bc(&l("next"));
        // Copy ring[R .. R + 512] into slot head % AUX_SLOTS. The ring size is a multiple of
        // 512, so a burst never straddles its end.
        a.ldi32(p.ring_base | 0x8000_0000, R1);
        a.addn(R4, R1); // r1 = source
        a.mov(R2, R5);
        a.ldi8((AUX_SLOTS - 1) as u8, R0);
        a.and(R0, R5);
        a.lsl_i(9, R5);
        a.ldi32(AUX_STAGE[c], R0);
        a.addn(R0, R5); // r5 = slot
        a.mov(R1, R6);
        a.ldi20(FIFO_BLOCK_BYTES, R0);
        a.addn(R0, R6); // r6 = source end
        a.label(&l("copy"));
        a.ld(R1, R0);
        a.st(R0, R5);
        a.addn_i(4, R1);
        a.addn_i(4, R5);
        a.cmp(R6, R1);
        a.bne(&l("copy"));
        // R += 512 (wrapping), back to the capture; then the slot is the main pump's.
        a.ldi20(FIFO_BLOCK_BYTES, R0);
        a.addn(R0, R4);
        a.ldi32(RING_SIZE, R0);
        a.cmp(R0, R4);
        a.bne(&l("nowrap"));
        a.ldi8(0, R4);
        a.label(&l("nowrap"));
        a.ldi32(p.ring_read, R1);
        a.st(R4, R1);
        a.addn_i(1, R2);
        a.ldi32(AUX_HEAD[c], R1);
        a.st(R2, R1);
        a.ldi8(1, R12);
        a.label(&l("next"));
    }
    a.cmp_i(0, R12);
    a.bne("again");
    a.ldi20(AUX_IDLE_PAUSE, R0);
    a.ldi8(1, R4);
    a.label("pause");
    a.subn(R4, R0);
    a.cmp_i(0, R0);
    a.bne("pause");
    a.label("again");
    a.jmp_label("pass");
    a.finish()
}

/// The TS pump when the aux core prefetches ([`aux_prefetch`]): up to [`PUMP_BATCH`] staged
/// bursts of the capture into its endpoint, each guard-committed, the sent count back to
/// [`AUX_TAIL`]. Stops when the staging is empty or the FIFO stays not ready. Same calling
/// convention as [`ts_pump_for`].
pub fn ts_pump_staged(p: &Pump) -> Vec<u8> {
    use reg::*;
    let c = p.index;
    let mut a = Asm::new(p.address);
    a.ldi8(PUMP_BATCH, R7); // r7 = burst budget
    a.bra("loop");
    a.label("done");
    a.ldi8(0, R4);
    a.ret();
    a.label("loop");
    a.cmp_i(0, R7);
    a.beq("done");
    a.ldi32(aux_main_view(AUX_HEAD[c]), R1);
    a.ld(R1, R2); // r2 = head
    a.ldi32(aux_main_view(AUX_TAIL[c]), R1);
    a.ld(R1, R6); // r6 = tail
    a.cmp(R6, R2);
    a.beq("done");
    a.ldi32(p.fifo_ctrl, R1);
    a.ldi20(FIFO_POLL, R3);
    a.label("fifo_wait");
    a.ld(R1, R0);
    a.ldi8(4, R4);
    a.and(R4, R0);
    a.cmp_i(0, R0);
    a.bne("fifo_ok");
    a.ldi8(1, R4);
    a.subn(R4, R3);
    a.cmp_i(0, R3);
    a.beq("done");
    a.bra("fifo_wait");
    a.label("fifo_ok");
    // r1 = slot tail % AUX_SLOTS, r5 = its end.
    a.mov(R6, R1);
    a.ldi8((AUX_SLOTS - 1) as u8, R0);
    a.and(R0, R1);
    a.lsl_i(9, R1);
    a.ldi32(aux_main_view(AUX_STAGE[c]), R0);
    a.addn(R0, R1);
    a.mov(R1, R5);
    a.ldi20(FIFO_BLOCK_BYTES, R0);
    a.addn(R0, R5);
    a.ldi32(p.fifo, R4);
    a.label("copyw");
    a.ld(R1, R0);
    a.st(R0, R4);
    a.addn_i(4, R1);
    a.cmp(R5, R1);
    a.bne("copyw");
    // Commit, the low byte only (as ts_pump_for).
    a.ldi32(EP_BYTE_ENABLE, R1);
    a.ldi8(8, R0);
    a.st(R0, R1);
    a.ldi32(p.fifo_ctrl, R3);
    a.ldi8(4, R0);
    a.st(R0, R3);
    a.ldi8(0xf, R0);
    a.st(R0, R1);
    a.addn_i(1, R6);
    a.ldi32(aux_main_view(AUX_TAIL[c]), R1);
    a.st(R6, R1);
    a.ldi8(1, R1);
    a.subn(R1, R7);
    a.jmp_label("loop");
    a.finish()
}

/// Where the TS pump is uploaded (boot0 RAM, past the monitor body at 0xe1000).
pub const PUMP_ADDRESS: u32 = 0x000e_1200;
/// Where the second TS pump ([`PUMP_B`]) is uploaded, after the first, still inside the
/// 0x800 bytes at [`BODY_ADDRESS`].
pub const PUMP_B_ADDRESS: u32 = 0x000e_1400;

// ---- The units' output path ----
//
// Units 0x016 and 0x018 (the stock firmware runs its TS multiplexer on their cores) each
// have ring readers, a buffer, an engine and an output block, and a USB stream channel takes
// the output block's packets straight into an endpoint's FIFO, at up to about 40 MB/s. The
// feeder below runs on the unit's own FR core: it reads each whole record the capture has
// written into the unit's buffer with reader 2 and has the engine send it to the output
// block. No CPU copies the TS. The host sets the unit, the channel and the endpoint up
// (`dt295-rawfw`'s `hwpath`).

/// Where a unit's feeder is loaded and entered (the unit core's view; the main core sees the
/// unit's RAM at the unit's base + this).
pub const FEEDER_ENTRY: u32 = 0x000a_2d00;
/// The unit core's reset vector (unit view; write-only).
pub const FEEDER_VECTOR: u32 = 0x000f_fffc;
// The feeder's mailbox (unit view), each word in its own 64 bytes (a main-core store rewrites
// the words around it with what it read a moment before).
/// The host's request: a generation to feed (a new one starts at the capture's read offset),
/// 0 to stop.
pub const FEEDER_GO: u32 = 0x000a_3f00;
/// Records sent since the last generation started (the feeder's).
pub const FEEDER_SENT: u32 = 0x000a_3f40;
/// 1 once the feeder has seen [`FEEDER_GO`] at 0 (the host clears it before a start).
pub const FEEDER_STOPPED: u32 = 0x000a_3f80;
/// A counter the feeder bumps every pass: it runs.
pub const FEEDER_ALIVE: u32 = 0x000a_3fc0;
/// How long the feeder pauses (register-only loop passes) when it has nothing to send, so
/// that its polling leaves the bus to the main core.
const FEEDER_PAUSE: u32 = 0x100;
// The unit's registers the feeder drives (unit view; the buffer manager's are 16-bit, in the
// upper half of the word).
/// Reader 2: start offset, end offset, length, go (0x01000000; bits 24-25 busy).
const RD2_START: u32 = 0x1048;
const RD2_END: u32 = 0x1038;
const RD2_LEN: u32 = 0x1078;
const RD2_GO: u32 = 0x1068;
/// Buffer region 2: the write position (reader 2's) and the read position (the engine's).
const BUF2_WRITE: u32 = 0x0001_0058;
const BUF2_READ: u32 = 0x0001_0068;
/// The engine: length - 1, and the command (bits 0-2 busy).
const ENG_LEN: u32 = 0x2004;
const ENG_CMD: u32 = 0x2000;
/// One record from buffer region 2 to the output block, as it is.
pub const ENG_SEND: u32 = 0x0d00_0041;

/// The feeder for capture `p` (any unit core: it uses the unit's registers at its local 0).
/// While [`FEEDER_GO`] holds a generation it sends every whole record of the ring the capture
/// has written, one at a time (reader 2 into the buffer, then the engine), and hands the read
/// offset back; a new generation starts again at the capture's read offset. With
/// [`FEEDER_GO`] at 0 it sends nothing and writes 1 to [`FEEDER_STOPPED`]. It never touches
/// USB.
pub fn unit_feeder(p: &Pump) -> Vec<u8> {
    use reg::*;
    let mut a = Asm::new(FEEDER_ENTRY);
    a.ldi32(FEEDER_GO, R8);
    a.ldi32(FEEDER_SENT, R10);
    a.ldi32(FEEDER_ALIVE, R12);
    a.ldi8(1, R7);
    a.ldi8(0, R6); // r6 = the generation being fed
    a.ldi32(RING_SIZE, R9);
    a.ldi32(RECORD_BYTES, R11);
    a.label("pass");
    a.ld(R12, R1);
    a.addn_i(1, R1);
    a.st(R1, R12);
    a.ld(R8, R0);
    a.cmp_i(0, R0);
    a.bne("go");
    a.ldi32(FEEDER_STOPPED, R4);
    a.st(R7, R4);
    a.ldi8(0, R6);
    a.bra("pause");
    a.label("go");
    a.cmp(R6, R0);
    a.beq("feed");
    // a new generation: from the capture's read offset (the host's, 0 after a set-up)
    a.mov(R0, R6);
    a.ldi32(p.ring_read, R4);
    a.ld(R4, R2); // r2 = read offset
    a.ldi8(0, R3); // r3 = records sent
    a.st(R3, R10);
    a.label("feed");
    // r1 = what the ring holds: (write - read) mod size
    a.ldi32(p.ring_write, R4);
    a.ld(R4, R1);
    a.cmp(R2, R1);
    a.bc("wrapped");
    a.subn(R2, R1);
    a.bra("held");
    a.label("wrapped");
    a.subn(R2, R1);
    a.addn(R9, R1);
    a.label("held");
    a.cmp(R11, R1);
    a.bc("pause");
    // r13 = where reader 2 puts the record in the buffer
    a.ldi32(BUF2_WRITE, R4);
    a.ld(R4, R13);
    a.ldi20(RD2_START, R4);
    a.st(R2, R4);
    a.mov(R2, R5);
    a.addn(R11, R5);
    a.cmp(R9, R5);
    a.bc("nowrap");
    a.subn(R9, R5);
    a.label("nowrap"); // r5 = the record's end, the next read offset
    a.ldi20(RD2_END, R4);
    a.st(R5, R4);
    a.ldi20(RD2_LEN, R4);
    a.st(R11, R4);
    a.ldi20(RD2_GO, R4);
    a.ldi32(0x0100_0000, R0);
    a.st(R0, R4);
    a.label("reading");
    a.ld(R4, R0);
    a.ldi32(0x0300_0000, R1);
    a.and(R1, R0);
    a.bne("reading");
    // the engine reads from the record's start, whatever it consumed before
    a.ldi32(BUF2_READ, R4);
    a.st(R13, R4);
    a.ldi20(ENG_LEN, R4);
    a.mov(R11, R0);
    a.subn(R7, R0);
    a.st(R0, R4);
    a.ldi20(ENG_CMD, R4);
    a.ldi32(ENG_SEND, R0);
    a.st(R0, R4);
    a.label("sending");
    a.ld(R4, R0);
    a.ldi8(7, R1);
    a.and(R1, R0);
    a.bne("sending");
    a.mov(R5, R2);
    a.ldi32(p.ring_read, R4);
    a.st(R2, R4);
    a.addn(R7, R3);
    a.st(R3, R10);
    a.bra("pass");
    a.label("pause");
    a.ldi32(FEEDER_PAUSE, R1);
    a.label("wait");
    a.subn(R7, R1);
    a.cmp_i(0, R1);
    a.bne("wait");
    a.bra("pass");
    a.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_match_real_bytes() {
        // Each pair below was read from a real MB86M01 image and disassembled.
        let mut a = Asm::new(0);
        a.ldi20(0x000e_14f4, 12);
        assert_eq!(a.finish(), vec![0x9b, 0xec, 0x14, 0xf4]);

        let mut a = Asm::new(0);
        a.ldi32(0x585a_4431, 13);
        assert_eq!(a.finish(), vec![0x9f, 0x8d, 0x58, 0x5a, 0x44, 0x31]);

        let mut a = Asm::new(0);
        a.ldi8(0x46, 13); // c4 6d
        a.ldub_x(11, 0); // 02 b0  (@(R13,R11),R0)
        a.lduh_x(11, 0); // 01 b0
        a.ldi8(0x7d, 1); // c7 d1
        a.cmp(1, 0); // aa 10
        a.cmp_i(1, 0); // a8 10
        a.ld(11, 0); // 04 b0
        a.ld(1, 0); // 04 10
        a.st(2, 1); // 14 12
        a.mov(1, 4); // 8b 14
        a.jmp(12); // 97 0c
        a.call(12); // 97 1c
        a.addn_i(8, 1); // a0 81
        a.addn(8, 1); // a2 81
        a.subn(4, 1); // ae 41
        a.nop(); // 9f a0
        assert_eq!(
            a.finish(),
            vec![
                0xc4, 0x6d, 0x02, 0xb0, 0x01, 0xb0, 0xc7, 0xd1, 0xaa, 0x10, 0xa8, 0x10, 0x04, 0xb0,
                0x04, 0x10, 0x14, 0x12, 0x8b, 0x14, 0x97, 0x0c, 0x97, 0x1c, 0xa0, 0x81, 0xa2, 0x81,
                0xae, 0x41, 0x9f, 0xa0,
            ]
        );

        // The stock main FR masks and unmasks interrupts with these two bytes.
        let mut a = Asm::new(0);
        a.andccr(0xef); // 83 ef
        a.orccr(0x10); // 93 10
        assert_eq!(a.finish(), vec![0x83, 0xef, 0x93, 0x10]);
    }

    #[test]
    fn data_plane_opcodes_match_real_bytes() {
        // Each pinned against bytes disassembled from the resident's EP forwarder.
        let mut a = Asm::new(0);
        a.ldub(1, 1); // 06 11  LDUB @r1,r1
        a.lsr(5, 1); // b2 51  LSR r5,r1
        a.and(2, 1); // 82 21  AND r2,r1
        a.or(2, 1); // 92 21  OR r2,r1
        a.lsl2_i(8, 1); // b5 81  LSL2 #8,r1
        a.lsl2_i(0, 2); // b5 02  LSL2 #0,r2
        a.lsl_i(8, 2); // b4 82  LSL #8,r2
        a.extsb(14); // 97 8e  EXTSB r14
        assert_eq!(
            a.finish(),
            vec![
                0x06, 0x11, 0xb2, 0x51, 0x82, 0x21, 0x92, 0x21, 0xb5, 0x81, 0xb5, 0x02, 0xb4, 0x82,
                0x97, 0x8e,
            ]
        );
        // BHI (0xEF) / BLS (0xEE) opcode bytes, forward to the next instruction.
        let mut a = Asm::new(0);
        a.bhi("h");
        a.label("h");
        a.bls("l");
        a.label("l");
        a.ret();
        let v = a.finish();
        assert_eq!(v[0], 0xef);
        assert_eq!(v[2], 0xee);
    }

    #[test]
    fn branch_displacement_resolves() {
        // bne forward to a label 4 bytes ahead: addr0 bne(+? ), then two 2-byte
        // instrs, then label. disp = (6 - (0 + 2)) / 2 = 2.
        let mut a = Asm::new(0x1000);
        a.bne("t"); // at 0x1000
        a.nop(); // 0x1002
        a.nop(); // 0x1004
        a.label("t"); // 0x1006
        a.ret();
        let v = a.finish();
        assert_eq!(&v[0..2], &[0xe3, 0x02]);
    }

    #[test]
    fn hook_is_the_expected_eight_bytes() {
        // LDI:20 #0xe1000,R12 ; JMP @R12 ; NOP
        assert_eq!(hook(), [0x9b, 0xec, 0x10, 0x00, 0x97, 0x0c, 0x9f, 0xa0]);
    }

    #[test]
    fn body_fits_and_reproduces_the_overwritten_dispatch() {
        let b = body();
        assert!(b.len() <= BODY_CAPACITY);
        // The stock path must start by reading the mode byte and comparing to 1,
        // exactly as the 4 instructions the hook overwrote did:
        //   LDI:8 #0x46,R13 ; LDUB @(R13,R11),R0 ; CMP #1,R0 ; BNE ...
        let stock = find_subslice(&b, &[0xc4, 0x6d, 0x02, 0xb0, 0xa8, 0x10, 0xe3])
            .expect("stock dispatch prologue present");
        assert!(stock > 0);
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn call_path_takes_an_explicit_r4() {
        // The CALL path must load the argument field and conditionally move it into
        // R4 (the callee's packet pointer), so the resident entry can be handed a
        // packet at a scratch address while staging holds the monitor's own request.
        let b = body();
        assert!(b.len() <= BODY_CAPACITY);
        // `ldi8 #0x10,R13`? no -- the argument is loaded as a word offset. Look for
        // the CMP #0 / BEQ / MOV R6,R4 shape: cmp #0,R6 = 0xa806, beq = 0xe2??,
        // mov R6,R4 = 0x8b64.
        let needle = [0xa8, 0x06];
        let at = find_subslice(&b, &needle).expect("cmp #0,R6 present in CALL path");
        // The MOV R6,R4 (0x8b64) must appear shortly after the compare+branch.
        assert!(
            find_subslice(&b[at..], &[0x8b, 0x64]).is_some(),
            "mov R6,R4 present after the argument check"
        );
    }

    #[test]
    fn ts_pump_assembles_and_is_well_formed() {
        let p = ts_pump();
        // Fits in boot0 RAM below the second pump (0xe1400) with room to spare.
        assert!(p.len() < 0x200, "pump is {} bytes", p.len());
        assert!(p.len().is_multiple_of(2), "16-bit instruction stream");
        // Ends with RET (0x97 0x20).
        assert_eq!(&p[p.len() - 2..], &[0x97, 0x20]);
        // The copy loop stores to the FIFO: a `st Rc,@r4` (0x14 0x4x) is present.
        assert!(
            p.array_windows()
                .any(|&[a, b]| a == 0x14 && (b & 0xf0) == 0x40),
            "a store to @r4 (the FIFO) is present"
        );
        // The pump sits after the body (0xe1000, 216 B), and the second pump after it, both
        // inside the 0x800 bytes boot0 reserves for the body.
        assert!(PUMP_ADDRESS >= BODY_ADDRESS + body().len() as u32);
        assert!(PUMP_ADDRESS + p.len() as u32 <= PUMP_B_ADDRESS);
        let b = ts_pump_for(&PUMP_B);
        assert_eq!(b.len(), p.len(), "the same routine, other constants");
        assert!(PUMP_B_ADDRESS + b.len() as u32 <= BODY_ADDRESS + BODY_CAPACITY as u32);
    }

    #[test]
    fn the_second_pump_drains_its_own_ring_into_ep_0x86() {
        let b = ts_pump_for(&PUMP_B);
        let has = |v: u32| b.windows(4).any(|w| w == v.to_be_bytes());
        for v in [
            0x8240_0000,
            0x00d2_2028,
            0x00d2_202c,
            0x00fd_bd98,
            0x00fd_bd34,
        ] {
            assert!(has(v), "0x{v:08x} in the second pump");
        }
        for v in [
            0x8200_0000,
            0x0190_2028,
            0x0190_202c,
            0x00fd_bd84,
            0x00fd_bd0c,
        ] {
            assert!(!has(v), "0x{v:08x} not in the second pump");
        }
        assert_eq!(ts_pump(), ts_pump_for(&PUMP_A));
    }

    #[test]
    fn the_aux_prefetch_fits_below_its_mailbox() {
        let (image, sig) = aux_prefetch();
        assert!(
            AUX_ENTRY + image.len() as u32 <= AUX_SIG,
            "prefetch is {} bytes",
            image.len()
        );
        assert!(AUX_STAGE[1] + AUX_SLOTS * FIFO_BLOCK_BYTES <= AUX_ENTRY);
        assert_eq!(aux_prefetch(), (image.clone(), sig), "deterministic");
        assert!(
            image.windows(4).any(|w| w == sig.to_be_bytes()),
            "writes its signature"
        );
        // Both rings, by their registers.
        for p in [PUMP_A, PUMP_B] {
            for v in [p.ring_read, p.ring_write, p.ring_base | 0x8000_0000] {
                assert!(image.windows(4).any(|w| w == v.to_be_bytes()), "0x{v:08x}");
            }
        }
        // The mailbox words keep 64 bytes apart.
        let mut words: Vec<u32> = [AUX_SIG, AUX_ALIVE]
            .into_iter()
            .chain(AUX_RUN)
            .chain(AUX_ACK)
            .chain(AUX_HEAD)
            .chain(AUX_TAIL)
            .collect();
        words.sort();
        assert!(words.windows(2).all(|w| w[1] - w[0] >= 64));
        assert!(*words.last().unwrap() < 0x000a_4000);
    }

    #[test]
    fn the_staged_pumps_fit_and_drain_their_own_staging() {
        for p in [PUMP_A, PUMP_B] {
            let code = ts_pump_staged(&p);
            assert!(
                p.address + code.len() as u32 <= p.address + 0x200,
                "{} bytes",
                code.len()
            );
            let has = |v: u32| code.windows(4).any(|w| w == v.to_be_bytes());
            assert!(has(aux_main_view(AUX_STAGE[p.index])));
            assert!(has(aux_main_view(AUX_TAIL[p.index])));
            assert!(has(p.fifo) && has(p.fifo_ctrl));
            assert!(!has(aux_main_view(AUX_STAGE[1 - p.index])));
            assert!(!has(p.ring_read), "the prefetch owns the read offset");
        }
    }

    #[test]
    fn the_unit_feeders_follow_their_own_capture() {
        for p in [PUMP_A, PUMP_B] {
            let code = unit_feeder(&p);
            assert!(code.len() < 0x1000, "{} bytes", code.len());
            assert!(FEEDER_ENTRY + (code.len() as u32) <= FEEDER_GO);
            let has = |v: u32| code.windows(4).any(|w| w == v.to_be_bytes());
            assert!(has(p.ring_write) && has(p.ring_read));
            assert!(has(FEEDER_GO) && has(FEEDER_SENT) && has(FEEDER_STOPPED));
            assert!(has(ENG_SEND) && has(BUF2_WRITE) && has(BUF2_READ));
            let other = if p.index == 0 { PUMP_B } else { PUMP_A };
            assert!(!has(other.ring_write));
            assert!(!has(p.fifo), "the feeder never touches USB");
        }
    }

    #[test]
    fn packet_pack_and_check() {
        let data: Vec<u8> = (0..40).collect();
        let p = pack(0x1234, OP_WRITE, 0x8038_0000, 0, 16, &data);
        assert_eq!(&p[OFF_MAGIC..OFF_MAGIC + 4], &MAGIC.to_be_bytes());
        assert_eq!(&p[OFF_OP..OFF_OP + 4], &OP_WRITE.to_be_bytes());
        assert_eq!(
            &p[OFF_ADDRESS..OFF_ADDRESS + 4],
            &0x8038_0000u32.to_be_bytes()
        );
        assert_eq!(&p[OFF_BUDGET..OFF_BUDGET + 4], &16u32.to_be_bytes());
        assert_eq!(&p[OFF_DATA..OFF_DATA + 4], &[0, 1, 2, 3]);

        // A reply echoes magic + seq and carries the status word.
        let mut reply = p;
        reply[OFF_OP..OFF_OP + 4].copy_from_slice(&STATUS_OK.to_be_bytes());
        assert_eq!(check_reply(&reply, 0x1234), Ok(STATUS_OK));
        assert!(check_reply(&reply, 0x9999).is_err(), "sequence mismatch");
        reply[OFF_MAGIC] ^= 0xff;
        assert!(check_reply(&reply, 0x1234).is_err(), "magic mismatch");
    }

    #[test]
    fn stock_hook_site_matches_the_body_reproduction() {
        // The first three instructions of the stock dispatch the hook overwrites
        // are exactly what the body's "stock" path re-emits.
        assert_eq!(
            &STOCK_HOOK_SITE[0..6],
            &[0xc4, 0x6d, 0x02, 0xb0, 0xa8, 0x10]
        );
    }
}
