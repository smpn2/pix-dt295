//! TS packets as every path delivers them.

/// A TS packet.
pub const PACKET: usize = 188;
/// The null packet's PID.
pub const NULL_PID: u16 = 0x1FFF;

/// Drop the null packets (PID 0x1FFF) from `ts`, whole 188-byte packets, moving the others
/// to its front in order. Returns the bytes kept (`ts[..kept]`). The demodulator fills a
/// terrestrial TS with them, nearly half of its packets; the stock firmware's System1 and
/// System2 pass them on too (System1 routes them, so the first TS arrives at once).
pub fn drop_null(ts: &mut [u8]) -> usize {
    let mut kept = 0;
    for i in (0..ts.len() / PACKET).map(|i| i * PACKET) {
        if (u16::from(ts[i + 1] & 0x1f) << 8) | u16::from(ts[i + 2]) == NULL_PID {
            continue;
        }
        if kept != i {
            ts.copy_within(i..i + PACKET, kept);
        }
        kept += PACKET;
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(pid: u16, n: u8) -> Vec<u8> {
        let mut p = vec![n; PACKET];
        p[0] = 0x47;
        p[1] = (pid >> 8) as u8 & 0x1f;
        p[2] = pid as u8;
        p
    }

    #[test]
    fn null_packets_are_dropped_in_order() {
        let mut stream = [
            ts(0x1fff, 0),
            ts(0x100, 1),
            ts(0x1fff, 2),
            ts(0x1fff, 3),
            ts(0x200, 4),
        ]
        .concat();
        let kept = drop_null(&mut stream);
        assert_eq!(&stream[..kept], &[ts(0x100, 1), ts(0x200, 4)].concat()[..]);
        let mut only_null = [ts(0x1fff, 0), ts(0x1fff, 1)].concat();
        assert_eq!(drop_null(&mut only_null), 0);
        let mut none = [ts(0x100, 1), ts(0x101, 2)].concat();
        assert_eq!(drop_null(&mut none), 2 * PACKET);
        assert_eq!(none, [ts(0x100, 1), ts(0x101, 2)].concat());
        // the transport_error_indicator and the other flags of byte 1 are not the PID
        let mut flagged = ts(0x1fff, 0);
        flagged[1] |= 0xe0;
        assert_eq!(drop_null(&mut flagged), 0);
    }
}
