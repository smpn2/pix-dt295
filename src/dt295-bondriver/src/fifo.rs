//! The TS FIFO between the stream from `dt295 serve` (producer, via the `TsSink`) and
//! the host's `GetTsStream` calls: a fixed ring buffer that drops the oldest bytes when
//! the host does not read in time, counting what it dropped.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Bytes handed out per `GetTsStream`.
pub const TS_CHUNK: usize = 188 * 256;
/// FIFO size: ~6 MB, ~1.6 s at 3.7 MB/s.
pub const TS_FIFO_SIZE: usize = 188 * 512 * 64;

/// A byte ring buffer with a "data available" condition (like a
/// manual-reset event: set exactly while the FIFO is non-empty).
#[derive(Debug)]
pub struct TsFifo {
    ring: Mutex<Ring>,
    ready: Condvar,
}

#[derive(Debug)]
struct Ring {
    buf: Box<[u8]>,
    head: usize,
    len: usize,
    /// Bytes lost to overflow since the last [`TsFifo::take_dropped`].
    dropped: u64,
}

impl TsFifo {
    /// An empty FIFO of `capacity` bytes (> 0).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "TS FIFO capacity must be non-zero");
        Self {
            ring: Mutex::new(Ring {
                buf: vec![0; capacity].into_boxed_slice(),
                head: 0,
                len: 0,
                dropped: 0,
            }),
            ready: Condvar::new(),
        }
    }

    // A panic elsewhere while holding the lock cannot leave the ring inconsistent
    // (every update is a few index assignments), so a poisoned lock is still usable.
    fn lock(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Append `data`; on overflow drop the oldest bytes (a push larger than the
    /// whole FIFO keeps only its newest bytes).
    pub fn push(&self, mut data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut r = self.lock();
        let cap = r.buf.len();
        if data.len() > cap {
            r.dropped += (data.len() - cap) as u64;
            data = &data[data.len() - cap..];
        }
        let n = data.len();
        if r.len + n > cap {
            let drop = r.len + n - cap;
            r.head = (r.head + drop) % cap;
            r.len -= drop;
            r.dropped += drop as u64;
        }
        let tail = (r.head + r.len) % cap;
        let first = n.min(cap - tail);
        r.buf[tail..tail + first].copy_from_slice(&data[..first]);
        r.buf[..n - first].copy_from_slice(&data[first..]);
        r.len += n;
        drop(r);
        self.ready.notify_all();
    }

    /// Take up to `max` of the oldest bytes, handing them to `copy` as the two
    /// contiguous parts of the ring (the second may be empty). Returns the count.
    pub fn pop_with(&self, max: usize, copy: impl FnOnce(&[u8], &[u8])) -> usize {
        let mut r = self.lock();
        let cap = r.buf.len();
        let n = r.len.min(max);
        let first = n.min(cap - r.head);
        copy(&r.buf[r.head..r.head + first], &r.buf[..n - first]);
        r.head = (r.head + n) % cap;
        r.len -= n;
        n
    }

    /// Take up to `dst.len()` of the oldest bytes into `dst`. Returns the count.
    pub fn pop(&self, dst: &mut [u8]) -> usize {
        self.pop_with(dst.len(), |a, b| {
            dst[..a.len()].copy_from_slice(a);
            dst[a.len()..a.len() + b.len()].copy_from_slice(b);
        })
    }

    /// Bytes buffered.
    pub fn len(&self) -> usize {
        self.lock().len
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `GetReadyCount`: buffered [`TS_CHUNK`]s, rounded up.
    pub fn ready_count(&self) -> u32 {
        u32::try_from(self.len().div_ceil(TS_CHUNK)).unwrap_or(u32::MAX)
    }

    /// Wait until data is available, at most `timeout` (`None` = forever).
    /// True if data is available.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        let r = self.lock();
        let r = match timeout {
            None => self
                .ready
                .wait_while(r, |r| r.len == 0)
                .unwrap_or_else(PoisonError::into_inner),
            Some(t) => {
                self.ready
                    .wait_timeout_while(r, t, |r| r.len == 0)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
        r.len > 0
    }

    /// Discard everything buffered (the dropped-bytes counter is kept).
    pub fn purge(&self) {
        let mut r = self.lock();
        r.head = 0;
        r.len = 0;
    }

    /// Bytes dropped by overflow since the last call; resets the counter.
    pub fn take_dropped(&self) -> u64 {
        std::mem::take(&mut self.lock().dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    fn pop_all(f: &TsFifo) -> Vec<u8> {
        let mut out = vec![0; f.len()];
        let n = f.pop(&mut out);
        out.truncate(n);
        out
    }

    #[test]
    fn push_pop_in_order() {
        let f = TsFifo::new(16);
        assert!(f.is_empty());
        f.push(&[1, 2, 3]);
        f.push(&[]);
        f.push(&[4, 5]);
        assert_eq!(f.len(), 5);
        let mut buf = [0u8; 2];
        assert_eq!(f.pop(&mut buf), 2);
        assert_eq!(buf, [1, 2]);
        assert_eq!(pop_all(&f), [3, 4, 5]);
        assert_eq!(f.pop(&mut buf), 0);
        assert_eq!(f.take_dropped(), 0);
    }

    #[test]
    fn wraparound() {
        let f = TsFifo::new(10);
        f.push(&[0, 1, 2, 3, 4, 5, 6]);
        let mut buf = [0u8; 5];
        assert_eq!(f.pop(&mut buf), 5);
        // head = 5, len = 2: these 6 bytes wrap past the end of the ring.
        f.push(&[7, 8, 9, 10, 11, 12]);
        let mut parts = (0, 0);
        let n = f.pop_with(usize::MAX, |a, b| parts = (a.len(), b.len()));
        assert_eq!(n, 8);
        assert_eq!(parts, (5, 3), "the read is split at the ring's end");
        f.push(&[13, 14, 15]);
        assert_eq!(pop_all(&f), [13, 14, 15]);

        let f = TsFifo::new(10);
        f.push(&[0; 8]);
        let mut buf = [0u8; 8];
        f.pop(&mut buf);
        f.push(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(pop_all(&f), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn overflow_drops_oldest() {
        let f = TsFifo::new(10);
        f.push(&[0, 1, 2, 3, 4, 5, 6, 7]);
        f.push(&[8, 9, 10, 11, 12]);
        assert_eq!(f.len(), 10);
        assert_eq!(f.take_dropped(), 3);
        assert_eq!(f.take_dropped(), 0, "the counter resets");
        assert_eq!(pop_all(&f), [3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        // Overflow while the ring has wrapped.
        let f = TsFifo::new(4);
        f.push(&[1, 2, 3]);
        let mut b = [0u8; 2];
        f.pop(&mut b);
        f.push(&[4, 5, 6, 7]);
        assert_eq!(f.take_dropped(), 1);
        assert_eq!(pop_all(&f), [4, 5, 6, 7]);
    }

    #[test]
    fn oversized_push_keeps_newest() {
        let f = TsFifo::new(4);
        f.push(&[9]);
        f.push(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(f.take_dropped(), 7);
        assert_eq!(pop_all(&f), [6, 7, 8, 9]);
    }

    #[test]
    fn purge_empties_but_keeps_drop_count() {
        let f = TsFifo::new(4);
        f.push(&[1, 2, 3, 4, 5]);
        f.purge();
        assert!(f.is_empty());
        assert!(!f.wait(Some(Duration::ZERO)));
        assert_eq!(f.take_dropped(), 1);
        f.push(&[6]);
        assert_eq!(pop_all(&f), [6]);
    }

    #[test]
    fn ready_count_rounds_up() {
        let f = TsFifo::new(TS_CHUNK * 3);
        assert_eq!(f.ready_count(), 0);
        f.push(&[0x47]);
        assert_eq!(f.ready_count(), 1);
        f.push(&vec![0; TS_CHUNK - 1]);
        assert_eq!(f.ready_count(), 1);
        f.push(&[0]);
        assert_eq!(f.ready_count(), 2);
        let mut chunk = vec![0; TS_CHUNK];
        assert_eq!(f.pop(&mut chunk), TS_CHUNK);
        assert_eq!(f.ready_count(), 1);
    }

    #[test]
    fn wait_times_out_when_empty() {
        let f = TsFifo::new(8);
        let t = Instant::now();
        assert!(!f.wait(Some(Duration::from_millis(30))));
        assert!(t.elapsed() >= Duration::from_millis(25));
        f.push(&[1]);
        assert!(f.wait(Some(Duration::ZERO)));
        assert!(f.wait(None), "data available: returns at once");
    }

    #[test]
    fn wait_wakes_on_push() {
        let f = Arc::new(TsFifo::new(8));
        for timeout in [Some(Duration::from_secs(10)), None] {
            let producer = {
                let f = Arc::clone(&f);
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(20));
                    f.push(&[1, 2]);
                })
            };
            assert!(f.wait(timeout));
            producer.join().unwrap();
            assert_eq!(pop_all(&f), [1, 2]);
        }
    }
}
