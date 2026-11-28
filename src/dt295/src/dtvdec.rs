//! The per-session link keys of the link-layer encryption of EP 0x81 and EP 0x86 (the
//! key derivation; the packet decryption is [`crate::ts::link::decrypt`]).
//!
//! The firmware hands a key out in reply to a `07/01/06` type-8 RPC naming a secure
//! endpoint (3: System1, 0x1B: System2), wrapped with the boot-auth session key (SS): as
//! provided by a third party, cross-checked against our own reverse engineering (and
//! the vendor's Mac driver for System2).
//! * request: `[0..3]` = 07 01 06, `[10..12]` = 0x0003 (BE), `[12..28]` =
//!   AES-128-CBC(SS, IV = 0) of `[4 B nonce][endpoint (BE)][0][0]`;
//! * reply: AES-128-CBC(SS, IV = 0) decrypt of `reply[12..12+n]` gives `[0..4]` counter,
//!   `[4..20]` link key, `[20..32]` zero.

use tracing::{info, warn};

use crate::auth::{Block, cbc0_decrypt, nonce, t8_raw};
use crate::cas::rpc_block;
use crate::{Error, Result};
use dt295_core::proto::Io;

/// The secure endpoint of System1's TS (EP 0x81).
pub(crate) const ENDPOINT_SYSTEM1: u32 = 0x03;
/// The secure endpoint of System2's TS (EP 0x86), as the vendor's Mac driver names it.
pub(crate) const ENDPOINT_SYSTEM2: u32 = 0x1B;

/// The 07/01/06 request carrying `nonce4`, for endpoint `ep`.
fn key_request(ss: &[u8; 16], nonce4: [u8; 4], ep: u32) -> Result<Block> {
    let mut pt = [0u8; 16];
    pt[..4].copy_from_slice(&nonce4);
    pt[4..8].copy_from_slice(&ep.to_be_bytes());
    rpc_block(ss, 0x07, 0x01, 0x06, 3, &pt)
}

/// (link key, counter) of a 07/01/06 reply: its body from offset 12, trailing zeros
/// trimmed to a 16-byte multiple (at least two blocks), CBC(SS)-decrypted.
fn unwrap_key(ss: &[u8; 16], reply: &Block) -> ([u8; 16], [u8; 4]) {
    let mut body = [0u8; 496];
    body.copy_from_slice(&reply[12..508]);
    let mut n = body.len();
    while n > 16 && body[n - 1] == 0 {
        n -= 1;
    }
    n = (n / 16 * 16).max(32);
    cbc0_decrypt(ss, &mut body[..n]);
    let mut key = [0u8; 16];
    key.copy_from_slice(&body[4..20]);
    let mut counter = [0u8; 4];
    counter.copy_from_slice(&body[..4]);
    (key, counter)
}

/// Derive the link key of endpoint `ep` via the firmware's 07/01/06 RPC (needs the
/// boot-auth session key). The nonce MUST be fresh each session: the device anti-replays
/// it, and a repeated nonce yields a stale / wrong link key that leaves the TS
/// undecryptable.
pub(crate) fn link_key(io: &mut Io, ep: u32) -> Result<[u8; 16]> {
    let Some(ss) = io.p.sesskey else {
        warn!("dtvdec: no boot-auth session key (authenticate first)");
        return Err(Error::State("no boot-auth session key".into()));
    };
    let mut n = [0u8; 4];
    nonce(&mut n);
    let (resp, _) = t8_raw(io, &key_request(&ss, n, ep)?).map_err(|e| {
        warn!("dtvdec: 07/01/06 transport error");
        e.into_usb()
    })?;
    if resp[..3] != [0x07, 0x01, 0x06] {
        warn!(
            "dtvdec: 07/01/06 unexpected reply {:02x}{:02x}{:02x}",
            resp[0], resp[1], resp[2]
        );
        return Err(Error::Protocol("07/01/06: unexpected reply".into()));
    }
    let (key, c) = unwrap_key(&ss, &resp);
    // sanity: a valid key is not all-equal bytes
    if key.iter().all(|&b| b == key[0]) {
        warn!(
            "dtvdec: link key looks invalid (counter {:02x}{:02x}{:02x}{:02x})",
            c[0], c[1], c[2], c[3]
        );
        return Err(Error::Protocol("07/01/06: invalid link key".into()));
    }
    info!(
        "dtvdec: link key of endpoint 0x{ep:02x} derived via 07/01/06 (counter \
         {:02x}{:02x}{:02x}{:02x})",
        c[0], c[1], c[2], c[3]
    );
    Ok(key)
}

/// Release endpoint `ep`'s link key (07/03/04 `{ep, 1}`), as the vendor's Mac driver does
/// after a System2 stream.
pub(crate) fn release_endpoint(io: &mut Io, ep: u32) -> Result<()> {
    let ss =
        io.p.sesskey
            .ok_or_else(|| Error::State("no boot-auth session key".into()))?;
    let mut pt = [0u8; 12];
    nonce(&mut pt[..4]);
    pt[4..8].copy_from_slice(&ep.to_be_bytes());
    pt[8..12].copy_from_slice(&1u32.to_be_bytes());
    let (resp, _) =
        t8_raw(io, &rpc_block(&ss, 0x07, 0x03, 0x04, 2, &pt)?).map_err(Error::into_usb)?;
    if resp[..3] != [0x07, 0x03, 0x04] {
        return Err(Error::Protocol("07/03/04: unexpected reply".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::{SESSKEY, unhex};

    fn ss() -> [u8; 16] {
        unhex(SESSKEY).try_into().unwrap()
    }

    /// The key request block (with a fixed nonce) and the key unwrapped from a model
    /// reply: known answers.
    #[test]
    fn key_request_and_unwrap() {
        let req = key_request(&ss(), [0x7e, 0x31, 0x25, 0x9a], ENDPOINT_SYSTEM1).unwrap();
        assert_eq!(
            req[..32].to_vec(),
            unhex("070106000000000000000003330a780c991448fdef4adf46936f5af000000000")
        );
        assert!(req[32..].iter().all(|&b| b == 0));

        let mut reply = [0u8; 512];
        let r = unhex(
            "07010600000000000000000082b1a23521a666e10e8851ca0b79ddd29f730966e590c6d38391005f\
             b65fb9ee00000000",
        );
        reply[..r.len()].copy_from_slice(&r);
        let (key, counter) = unwrap_key(&ss(), &reply);
        assert_eq!(key.to_vec(), unhex("2031425364758697a8b9cadbecfd0e1f"));
        assert_eq!(counter, [0, 0, 0, 5]);
    }
}
