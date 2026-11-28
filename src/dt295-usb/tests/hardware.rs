//! Hardware smoke test: needs a PIX-DT295 (bound to WinUSB on Windows) that no other
//! process is using. Run with `cargo test -p dt295-usb -- --ignored`.

use std::assert_matches;
use std::thread;
use std::time::{Duration, Instant};

use dt295_usb::{Device, Error, PIDS, VID};

const EP_EVENTS: u8 = 0x83;
const EP_TS: u8 = 0x81;

#[test]
#[ignore = "opens the real PIX-DT295"]
fn smoke() {
    let dev = Device::open(VID, &PIDS, 0).expect("open");

    // Interrupt IN (events): data or a timeout are both fine.
    let mut ev = [0u8; 64];
    match dev.read(EP_EVENTS, &mut ev, Duration::from_millis(100)) {
        Ok(_) | Err(Error::Timeout) => {}
        Err(e) => panic!("event read: {e}"),
    }

    // Queued TS reads: nothing streams before the firmware is up, so "still pending".
    {
        let mut q = dev.read_queue(EP_TS, 4, 0x10000).expect("read queue");
        eprintln!("RAW_IO: {}", q.raw_io());
        match q.next(Duration::from_millis(200)) {
            Ok(_) => {}
            Err(e) => panic!("queued read: {e}"),
        }
    }
    dev.abort_pipe(EP_TS).expect("abort with nothing pending");

    // abort_pipe unblocks a read waiting on another thread.
    thread::scope(|s| {
        let reader = s.spawn(|| {
            let mut buf = vec![0u8; 0x10000];
            let t0 = Instant::now();
            let r = dev.read(EP_TS, &mut buf, Duration::from_secs(5));
            (r, t0.elapsed())
        });
        thread::sleep(Duration::from_millis(300));
        dev.abort_pipe(EP_TS).expect("abort");
        let (r, took) = reader.join().expect("reader thread");
        assert_matches!(r, Err(Error::Cancelled));
        assert!(took < Duration::from_secs(2), "abort took {took:?}");
    });
}
