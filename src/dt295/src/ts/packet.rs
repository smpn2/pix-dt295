//! TS packet helpers.
//!
//! The header accessors take one packet (normally exactly [`PACKET`] bytes). A slice
//! too short for the field asked for reads as a NULL packet without payload (PID
//! [`NULL_PID`], flags clear), which every caller ignores, so a truncated packet can
//! never cause a panic.

/// TS packet size, and the NULL packet PID (one definition, in `dt295-core`).
pub use dt295_core::ts::{NULL_PID, PACKET};
/// Sync byte.
pub const SYNC: u8 = 0x47;

/// Packets in a row that must start with the sync byte for [`find_sync`] to lock.
const SYNC_RUN: usize = 4;

/// PID of a packet (the 13 bits after the sync byte).
pub fn pid(p: &[u8]) -> u16 {
    match p {
        [_, b1, b2, ..] => (u16::from(b1 & 0x1F) << 8) | u16::from(*b2),
        _ => NULL_PID,
    }
}

/// transport_error_indicator: the demodulator could not correct the packet.
pub fn tei(p: &[u8]) -> bool {
    p.get(1).is_some_and(|b| b & 0x80 != 0)
}

/// payload_unit_start_indicator.
pub fn pusi(p: &[u8]) -> bool {
    p.get(1).is_some_and(|b| b & 0x40 != 0)
}

/// transport_scrambling_control (0..=3). The device marks its link-encrypted packets
/// with 01, a value ARIB leaves unused.
pub fn tsc(p: &[u8]) -> u8 {
    p.get(3).map_or_default(|b| b >> 6)
}

/// adaptation_field_control (0..=3).
pub fn afc(p: &[u8]) -> u8 {
    p.get(3).map_or_default(|b| (b >> 4) & 3)
}

/// continuity_counter (0..=15).
pub fn cc(p: &[u8]) -> u8 {
    p.get(3).map_or_default(|b| b & 0x0F)
}

/// Offset of the payload, or `None` without payload (afc 0/2) or a bad adaptation
/// field length.
pub fn payload_offset(p: &[u8]) -> Option<usize> {
    let s = match afc(p) {
        1 => 4,
        3 => 5 + usize::from(*p.get(4)?),
        _ => return None,
    };
    (s < p.len().min(PACKET)).then_some(s)
}

/// Offset of the first position where four packets in a row start with the sync
/// byte, or `None` (fewer than four packets' worth found).
pub fn find_sync(buf: &[u8]) -> Option<usize> {
    let span = SYNC_RUN * PACKET;
    let last = buf.len().checked_sub(span)?;
    (0..=last).find(|&p| (0..SYNC_RUN).all(|i| buf[p + i * PACKET] == SYNC))
}

/// Overwrite a packet with a NULL packet (PID 0x1FFF, payload 0xFF).
pub fn make_null(p: &mut [u8]) {
    const HEADER: [u8; 4] = [SYNC, 0x1F, 0xFF, 0x10];
    let n = p.len().min(PACKET);
    for (i, b) in p[..n].iter_mut().enumerate() {
        *b = HEADER.get(i).copied().unwrap_or(0xFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(b1: u8, b2: u8, b3: u8) -> [u8; PACKET] {
        let mut p = [0xFF; PACKET];
        p[..4].copy_from_slice(&[SYNC, b1, b2, b3]);
        p
    }

    #[test]
    fn header_fields() {
        let p = packet(0xC1, 0x23, 0x7A);
        assert_eq!(pid(&p), 0x0123);
        assert!(tei(&p));
        assert!(pusi(&p));
        assert_eq!(tsc(&p), 1);
        assert_eq!(afc(&p), 3);
        assert_eq!(cc(&p), 0x0A);
        let q = packet(0x1F, 0xFF, 0x10);
        assert_eq!(pid(&q), NULL_PID);
        assert!(!tei(&q) && !pusi(&q));
        assert_eq!((tsc(&q), afc(&q), cc(&q)), (0, 1, 0));
    }

    #[test]
    fn short_slices_read_as_null_without_payload() {
        assert_eq!(pid(&[SYNC, 0x00]), NULL_PID);
        assert!(!pusi(&[]));
        assert_eq!(afc(&[SYNC, 0, 0]), 0);
        assert_eq!(payload_offset(&[SYNC, 0, 0, 0x30]), None);
        assert_eq!(payload_offset(&[SYNC, 0, 0, 0x10]), None);
    }

    #[test]
    fn payload_offsets() {
        let mut p = packet(0x40, 0x00, 0x10);
        assert_eq!(payload_offset(&p), Some(4));
        p[3] = 0x20; // adaptation field only
        assert_eq!(payload_offset(&p), None);
        p[3] = 0x00; // reserved
        assert_eq!(payload_offset(&p), None);
        p[3] = 0x30;
        p[4] = 7;
        assert_eq!(payload_offset(&p), Some(12));
        p[4] = 182; // one payload byte left
        assert_eq!(payload_offset(&p), Some(187));
        p[4] = 183; // adaptation field fills the packet
        assert_eq!(payload_offset(&p), None);
        p[4] = 0xFF; // bad length
        assert_eq!(payload_offset(&p), None);
    }

    #[test]
    fn sync_needs_four_packets_in_a_row() {
        let mut buf = vec![0u8; 3 + 5 * PACKET];
        for k in 0..5 {
            buf[3 + k * PACKET] = SYNC;
        }
        buf[1] = SYNC; // a stray sync byte before the grid
        assert_eq!(find_sync(&buf), Some(3));
        assert_eq!(find_sync(&buf[3..3 + 4 * PACKET]), Some(0));
        assert_eq!(find_sync(&buf[3..3 + 4 * PACKET - 1]), None);
        buf[3 + 2 * PACKET] = 0;
        assert_eq!(find_sync(&buf), None);
        assert_eq!(find_sync(&[]), None);
    }

    #[test]
    fn null_packet() {
        let mut p = packet(0x40, 0x11, 0x5C);
        p[50] = 0;
        make_null(&mut p);
        assert_eq!(&p[..4], &[SYNC, 0x1F, 0xFF, 0x10]);
        assert!(p[4..].iter().all(|&b| b == 0xFF));
        let mut short = [0u8; 2];
        make_null(&mut short);
        assert_eq!(short, [SYNC, 0x1F]);
    }
}
