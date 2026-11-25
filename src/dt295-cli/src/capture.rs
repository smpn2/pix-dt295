//! `dt295 capture`: open, tune and stream every service of the TS (or the one of `--sid`)
//! into a file or stdout, for a while or until stopped. On the System1 path it boots the
//! vendor firmware, waits for lock and the device's B-CAS reader descrambles; on the
//! raw-TS path (`--raw-ts`, or a raw-TS ini / unit) it boots nothing and streams the
//! scrambled TS for the host to descramble ([`rawts`]). With `--bondriver` and
//! `--channel` it takes its unit, firmware and channel from a BonDriver_DT295 ini, and
//! with `-t 0` it runs until a signal or until the reader closes the output: what a
//! recorder (Mirakurun / mirakc) expects of a tuner command.

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use dt295::chan::NO_TSID;
use dt295::{RAW_STALL, StallWatch, System, TsSink, raw_stall_due};

use crate::CaptureArgs;
use crate::bonini::{self, BonDriver};
use crate::rawts;
use crate::serve::conf::Conf;
use crate::serve::proto::Tuning;
use crate::tuner::{self, DeviceArgs, Unit, stopping};

/// TS buffered for a slow reader or disk, in the worker's reads (up to 64 KiB each, as
/// `dt295 serve`): seconds of TS. Beyond it the TS is dropped (and counted), so the stream
/// worker never waits for the output: on the raw-TS path the chip's ring holds under a
/// second of TS, on the System1 path the worker keeps the secure session alive.
const QUEUE_READS: usize = 256;

/// The stream worker's end of the output: it queues the TS for the writer thread
/// ([`Sink`]) and never blocks on it.
struct Feed {
    tx: Mutex<Option<SyncSender<Vec<u8>>>>,
    /// Bytes the device delivered (queued or dropped).
    bytes: AtomicU64,
    /// Bytes dropped because the output did not keep up.
    dropped: AtomicU64,
}

impl Feed {
    fn new() -> (Arc<Self>, Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::sync_channel(QUEUE_READS);
        let feed = Self {
            tx: Mutex::new(Some(tx)),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        };
        (Arc::new(feed), rx)
    }

    fn push(&self, ts: &[u8]) {
        self.bytes.fetch_add(ts.len() as u64, Ordering::Relaxed);
        let tx = self.tx.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = tx.as_ref()
            && let Err(TrySendError::Full(_)) = tx.try_send(ts.to_vec())
        {
            self.dropped.fetch_add(ts.len() as u64, Ordering::Relaxed);
        }
    }

    /// No more TS (also from a worker that would not stop): the writer drains the queue
    /// and ends.
    fn finish(&self) {
        self.tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// The stream's destination, written on the writer thread from the [`Feed`]'s queue.
struct Sink {
    out: Box<dyn Write + Send>,
    bytes: u64,
    /// Whether the first window of >= 2 packets was 0x47-aligned.
    sync: Option<bool>,
    /// The first write error (later data is dropped).
    error: Option<io::Error>,
}

impl Sink {
    fn new(out: Box<dyn Write + Send>) -> Self {
        Self {
            out,
            bytes: 0,
            sync: None,
            error: None,
        }
    }

    fn write(&mut self, data: &[u8]) {
        // Quick 0x47 sync check on the first packet-sized window we see.
        if self.sync.is_none() && data.len() >= 188 * 2 {
            self.sync = Some(data[0] == 0x47 && data[188] == 0x47);
        }
        self.bytes += data.len() as u64;
        if self.error.is_none()
            && let Err(e) = self.out.write_all(data)
        {
            self.error = Some(e);
        }
    }

    fn sync_text(&self) -> &'static str {
        match self.sync {
            Some(true) => "OK (0x47 aligned)",
            Some(false) => "NOT aligned (check scramble/packet mode)",
            None => "unknown",
        }
    }
}

fn lock(sink: &Mutex<Sink>) -> MutexGuard<'_, Sink> {
    sink.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The stream worker's end of the output.
fn ts_sink(feed: &Arc<Feed>) -> TsSink {
    let feed = Arc::clone(feed);
    Box::new(move |ts: &[u8]| feed.push(ts))
}

/// The writer thread: what the worker queued into `sink`, until [`Feed::finish`].
fn write_ts(sink: Arc<Mutex<Sink>>, queue: Receiver<Vec<u8>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for chunk in queue {
            lock(&sink).write(&chunk);
        }
    })
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// The tune and service of the options: `--channel` (looked up in the ini), else
/// `--ter` / `--sat` / `--tsid`; `--sid` wins over a line's service.
fn target(a: &CaptureArgs, ini: Option<&BonDriver>) -> Result<(Tuning, Option<u16>)> {
    if let Some(name) = &a.channel {
        let spaces = ini.map_or(&[][..], |b| &b.spaces[..]);
        let Some(ch) = bonini::tune(name, spaces) else {
            let Some(b) = ini else {
                bail!("no channel {name:?}: not a built-in 13ch..62ch / C13ch..C63ch");
            };
            if let Some(why) = bonini::untunable(name, spaces) {
                bail!(
                    "channel {name:?} of {} does not tune: {why}",
                    b.ini.display()
                );
            }
            let lines = match &b.tuner.channel_file {
                Some(f) => format!("a channel of {}", f.display()),
                None => format!("a [BS] / [CS110] line of {}", b.ini.display()),
            };
            bail!("no channel {name:?}: neither {lines} nor a built-in 13ch..62ch / C13ch..C63ch");
        };
        let tuning = match ch.sys {
            System::Terrestrial => Tuning::Terrestrial { hz: ch.freq_hz },
            System::Satellite => Tuning::Satellite {
                hz: ch.freq_hz,
                tsid: ch.tsid,
            },
        };
        return Ok((tuning, a.sid.or(ch.sid)));
    }
    match (a.ter, a.sat) {
        (Some(hz), _) => Ok((Tuning::Terrestrial { hz }, a.sid)),
        (None, Some(hz)) => Ok((
            Tuning::Satellite {
                hz,
                tsid: a.tsid.unwrap_or(NO_TSID),
            },
            a.sid,
        )),
        (None, None) => bail!("--channel, --ter or --sat is required"),
    }
}

/// The unit of `dt295 serve`'s configuration (`--serial`, or its only unit) with the
/// firmware and the state file the daemon uses (options given win), and whether it is
/// a raw-TS unit (`raw-ts = true`).
fn conf_device_args(conf: &Conf, given: &DeviceArgs) -> Result<(DeviceArgs, bool)> {
    let serial = match (&given.serial, conf.units.as_slice()) {
        (Some(s), _) => s.clone(),
        (None, [only]) => only.serial.clone(),
        (None, units) => bail!(
            "which unit? the configuration has {}: give --serial",
            units.len()
        ),
    };
    let raw = conf
        .units
        .iter()
        .any(|u| u.raw_ts && u.serial.eq_ignore_ascii_case(&serial));
    let dev = DeviceArgs {
        fw: given.fw.clone().or_else(|| Some(conf.idle_firmware())),
        trc: given.trc.clone().or_else(|| Some(conf.trc_firmware())),
        index: 0,
        state: given
            .state
            .clone()
            .or_else(|| Some(conf.state_file(&serial))),
        state_dir: None,
        serial: Some(serial),
    };
    Ok((dev, raw))
}

/// The unit and its files: from the ini (`--bondriver`), else from the daemon's
/// configuration (`-c`), else from the options alone; and whether to stream on the
/// raw-TS path: `--raw-ts`, a `RawTS=1` ini or a `raw-ts = true` unit.
fn device_args(a: &CaptureArgs, ini: Option<&BonDriver>) -> Result<(DeviceArgs, bool)> {
    let (dev, raw) = match (ini, &a.config) {
        (Some(b), _) => (b.device_args(&a.dev), b.tuner.raw_ts),
        (None, Some(c)) => conf_device_args(&Conf::load(c)?, &a.dev)?,
        (None, None) => (a.dev.clone(), false),
    };
    let raw = raw || a.raw_ts;
    // with --raw-ts itself clap refuses them already
    if raw && (a.dev.trc.is_some() || a.dev.state.is_some() || a.sid.is_some()) {
        bail!(
            "--trc, --state and --sid are for the System1 path: on the raw-TS path nothing is \
             booted, no state is kept and every service of the TS is streamed"
        );
    }
    Ok((dev, raw))
}

/// Tune, wait for lock and start streaming `sid` into the output.
fn start_stream(unit: &mut Unit, tuning: Tuning, sid: Option<u16>, feed: &Arc<Feed>) -> Result<()> {
    let sys = match tuning {
        Tuning::Terrestrial { hz } => {
            unit.dev.tune_t(hz).context("tune (ISDB-T)")?;
            System::Terrestrial
        }
        Tuning::Satellite { hz, tsid } => {
            unit.dev.tune_s(hz, tsid).context("tune (ISDB-S)")?;
            System::Satellite
        }
    };
    unit.wait_lock(sys)?;

    unit.dev.set_service_id(sid);
    unit.dev.stream_start(ts_sink(feed)).context("stream start")
}

/// One pass of a streaming loop: wait a little, show the progress on a terminal. False
/// once the output failed.
fn tick(sink: &Mutex<Sink>, tty: bool) -> bool {
    thread::sleep(Duration::from_millis(200));
    let s = lock(sink);
    if s.error.is_some() {
        return false;
    }
    if tty {
        eprint!("captured {:.2} MiB\r", mib(s.bytes));
    }
    true
}

/// Whether the streaming loop goes on: no stop signal, time left (`limit` none = until
/// stopped, `-t 0`) and the output still taking the TS.
fn going_on(t0: Instant, limit: Option<Duration>, sink: &Mutex<Sink>, tty: bool) -> bool {
    !stopping() && limit.is_none_or(|l| t0.elapsed() < l) && tick(sink, tty)
}

/// The System1 capture: boot the vendor firmware, tune, wait for lock and stream the
/// descrambled TS while [`going_on`]; a firmware that stopped acking starts the stream
/// over, an unplug ends the capture. Err: it never streamed (or a restart failed); Ok:
/// how the stream ended.
fn capture_system1(
    dev: &DeviceArgs,
    tuning: Tuning,
    sid: Option<u16>,
    (feed, sink): (&Arc<Feed>, &Mutex<Sink>),
    limit: Option<Duration>,
    tty: bool,
) -> Result<Result<()>> {
    let mut unit = Unit::open(dev)?;
    start_recovering(&mut unit, tuning, sid, feed)?;

    let t0 = Instant::now();
    let mut stall = StallWatch::new();
    let mut lost = false;
    while going_on(t0, limit, sink, tty) {
        // the stream's worker notices an unplug and ends the stream
        if unit.dev.live().device_lost() {
            lost = true;
            break;
        }
        // a firmware that stopped acking leaves the stream silent
        if unit.dev.needs_recovery() && stall.start_over(Instant::now()) {
            tracing::warn!("the firmware stopped acking: starting the stream over");
            if let Err(e) = unit.dev.stream_stop() {
                tracing::warn!("stream stop failed: {e}");
            }
            unit.restart()?;
            start_recovering(&mut unit, tuning, sid, feed)?;
        }
        // as soon as the stream proves its link key: a recorder ends this process with
        // SIGKILL, so the end of the run may never come
        unit.save_link_key();
    }
    let stopped = unit.dev.stream_stop(); // the worker no longer writes after this
    unit.save_link_key();
    if lost {
        return Ok(Err(anyhow!("the unit was unplugged")));
    }
    Ok(stopped.context("stream stop"))
}

/// [`start_stream`], and once more on a device started again if the firmware faulted:
/// Linux VM 2026-10-24, a stream start 46 s after a recording ended faulted to 0xFFFF
/// (that process's secure session timing out under this one). For the first start and
/// for a start over alike.
fn start_recovering(
    unit: &mut Unit,
    tuning: Tuning,
    sid: Option<u16>,
    feed: &Arc<Feed>,
) -> Result<()> {
    let Err(e) = start_stream(unit, tuning, sid, feed) else {
        return Ok(());
    };
    if stopping() || !unit.dev.needs_recovery() {
        return Err(e);
    }
    tracing::warn!("{e:#}; the firmware faulted, starting the device once more");
    unit.restart()?;
    start_stream(unit, tuning, sid, feed)
}

/// The raw-TS capture ([`rawts`]): open the unit without booting anything, bring the
/// chip up (once per plug) or only retune, and stream the scrambled TS while
/// [`going_on`]. A worker that ends on its own (no lock, a fault) or a stalled stream
/// restarts the channel, as `dt295 serve` does; an unplug ends the capture. Err: it
/// never streamed; Ok: how the stream ended (also a stop before the first TS).
fn capture_raw(
    dev: &DeviceArgs,
    tuning: Tuning,
    (feed, sink): (&Arc<Feed>, &Mutex<Sink>),
    limit: Option<Duration>,
    tty: bool,
) -> Result<Result<()>> {
    let mut dev = tuner::open_device(dev)?;
    let serial = dev.serial().map(str::to_owned);
    let present = || {
        serial.as_deref().is_none_or(|s| {
            dt295::present_serials().map_or(true, |p| p.iter().any(|x| x.eq_ignore_ascii_case(s)))
        })
    };
    // what the worker delivered, whether or not the output kept up
    let bytes = || feed.bytes();

    let t_start = Instant::now();
    let full =
        rawts::start(&mut dev, tuning, ts_sink(feed)).map_err(|why| anyhow!("raw-TS: {why}"))?;
    // the cold bring-up and the lock wait take seconds
    if !rawts::wait_first_ts(&dev, bytes) {
        if stopping() {
            return Ok(dev.stream_stop_raw().map(drop).context("raw stream stop"));
        }
        bail!("raw-TS: {}", rawts::why_no_ts(&mut dev, full));
    }
    tracing::info!("raw-TS: streaming ({} ms)", t_start.elapsed().as_millis());

    let t0 = Instant::now();
    let mut stall: Option<(u64, Instant)> = None;
    // Consecutive restarts that delivered no TS (off air / no lock): back off a little each
    // time, up to a few seconds, so a dead channel does not spin re-tuning every ~2 s.
    let mut idle_restarts = 0u32;
    let end = loop {
        if !going_on(t0, limit, sink, tty) {
            break Ok(());
        }
        // the worker owns the device while streaming, so never touch it here. It ends on
        // its own on a capture fault or an unplug.
        if !dev.raw_running() {
            let ended = dev.stream_stop_raw();
            if !present() {
                break Err(anyhow!("the unit was unplugged"));
            }
            let got = match ended {
                Ok(s) => s.bytes,
                Err(e) => {
                    tracing::warn!("raw-TS: the stream worker failed: {e}");
                    0
                }
            };
            if got == 0 {
                idle_restarts = (idle_restarts + 1).min(5);
                thread::sleep(Duration::from_secs(u64::from(idle_restarts)));
            } else {
                idle_restarts = 0;
            }
            if stopping() {
                break Ok(());
            }
            tracing::warn!("raw-TS: the stream worker ended ({got} bytes); restarting the channel");
            if let Err(why) = rawts::start(&mut dev, tuning, ts_sink(feed)) {
                break Err(anyhow!("raw-TS: the restart failed: {why}"));
            }
            stall = None;
            continue;
        }
        // a halt never faults the worker: the only stall signal is the byte count not
        // advancing. Restart the channel (stop + retune); a transient stall recovers.
        if raw_stall_due(&mut stall, bytes(), Instant::now()) {
            tracing::warn!(
                "raw-TS: no TS for {RAW_STALL:?} while streaming; restarting the channel"
            );
            if let Err(e) = dev.stream_stop_raw() {
                // it still holds the device I/O: a re-plug is the reset
                break Err(anyhow!("raw-TS: the stalled worker would not stop ({e})"));
            }
            if let Err(why) = rawts::start(&mut dev, tuning, ts_sink(feed)) {
                break Err(anyhow!("raw-TS: the restart failed: {why}"));
            }
        }
    };
    // none running: a no-op; one that would not stop above: its error at once
    let stopped = dev.stream_stop_raw();
    Ok(end.and(stopped.map(drop).context("raw stream stop")))
}

pub fn run(a: &CaptureArgs) -> Result<()> {
    let ini = a.bondriver.as_deref().map(BonDriver::load).transpose()?;
    let (dev, raw) = device_args(a, ini.as_ref())?;
    let (tuning, sid) = target(a, ini.as_ref())?;
    let sid = sid.filter(|_| !a.all_services);
    if raw && let Some(sid) = sid {
        // a --channel line's ServiceID (an explicit --sid is refused)
        eprintln!("raw-TS: every service of the TS is streamed, not only service {sid}");
    }

    // The output first: no point booting the device if the TS cannot be written.
    let out: Box<dyn Write + Send> = match &a.output {
        Some(p) => Box::new(BufWriter::new(
            File::create(p).with_context(|| format!("cannot create {}", p.display()))?,
        )),
        None if io::stdout().is_terminal() => {
            bail!("refusing to write a TS to the terminal: use -o FILE or redirect stdout")
        }
        None => Box::new(BufWriter::new(io::stdout())),
    };
    tuner::install_ctrl_c();

    let sink = Arc::new(Mutex::new(Sink::new(out)));
    let (feed, queue) = Feed::new();
    let writer = write_ts(Arc::clone(&sink), queue);
    // -t 0: until Ctrl-C / SIGTERM, or until the reader closes the output
    let limit = (a.seconds > 0).then(|| Duration::from_secs(a.seconds));
    let tty = io::stderr().is_terminal();
    let captured = if raw {
        capture_raw(&dev, tuning, (&feed, &sink), limit, tty)
    } else {
        capture_system1(&dev, tuning, sid, (&feed, &sink), limit, tty)
    };
    // what was queued is still written, also after a failure
    feed.finish();
    let _ = writer.join();
    let stopped = captured?;
    if tty {
        eprintln!();
    }

    let mut s = lock(&sink);
    let flushed = s.out.flush();
    let dropped = feed.dropped.load(Ordering::Relaxed);
    eprintln!(
        "total {} bytes; TS sync {}{}",
        s.bytes,
        s.sync_text(),
        if dropped > 0 {
            format!("; {dropped} bytes dropped (the output did not keep up)")
        } else {
            String::new()
        }
    );
    match s.error.take() {
        // the reader is gone (a recorder stopping a recording): the normal end
        Some(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            eprintln!("the reader closed the output");
        }
        Some(e) => return Err(e).context("writing the TS"),
        None => match flushed {
            Err(e) if e.kind() != io::ErrorKind::BrokenPipe => {
                return Err(e).context("writing the TS");
            }
            _ => {}
        },
    }
    stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that fails after `ok` bytes.
    struct Failing {
        ok: usize,
    }

    impl Write for Failing {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.ok == 0 {
                return Err(io::Error::other("disk full"));
            }
            let n = buf.len().min(self.ok);
            self.ok -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sink_counts_and_checks_sync() {
        let mut s = Sink::new(Box::new(io::sink()));
        s.write(&[0x47; 100]);
        assert_eq!(s.sync, None, "too short to judge");
        let mut ts = vec![0u8; 188 * 3];
        ts[0] = 0x47;
        ts[188] = 0x47;
        s.write(&ts);
        assert_eq!(s.sync, Some(true));
        s.write(&[0u8; 188 * 2]);
        assert_eq!(s.sync, Some(true), "judged once");
        assert_eq!(s.bytes, 100 + 188 * 5);

        let mut s = Sink::new(Box::new(io::sink()));
        s.write(&[0u8; 188 * 2]);
        assert_eq!(s.sync_text(), "NOT aligned (check scramble/packet mode)");
    }

    #[test]
    fn sink_keeps_the_first_error() {
        let mut s = Sink::new(Box::new(Failing { ok: 10 }));
        s.write(&[0; 8]);
        assert!(s.error.is_none());
        s.write(&[0; 8]);
        assert_eq!(
            s.error.as_ref().map(ToString::to_string).as_deref(),
            Some("disk full")
        );
        s.write(&[0; 8]);
        assert_eq!(s.bytes, 24, "still counted");
    }

    #[test]
    fn feed_never_waits_for_the_output() {
        // a full queue drops (and counts) instead of holding the worker up
        let (feed, queue) = Feed::new();
        for _ in 0..QUEUE_READS + 3 {
            feed.push(&[0x47; 188]);
        }
        assert_eq!(feed.bytes(), (QUEUE_READS as u64 + 3) * 188);
        assert_eq!(feed.dropped.load(Ordering::Relaxed), 3 * 188);
        // the writer writes what was queued, and ends once the feed is finished
        let sink = Arc::new(Mutex::new(Sink::new(Box::new(io::sink()))));
        let writer = write_ts(Arc::clone(&sink), queue);
        feed.finish();
        feed.push(&[0x47; 188]); // a worker that would not stop: counted, not queued
        writer.join().unwrap();
        assert_eq!(lock(&sink).bytes, QUEUE_READS as u64 * 188);
        assert_eq!(feed.bytes(), (QUEUE_READS as u64 + 4) * 188);
    }

    fn parse(cli: &[&str]) -> Result<crate::Cli, clap::Error> {
        use clap::Parser;
        crate::Cli::try_parse_from(["dt295", "capture"].iter().chain(cli))
    }

    fn args(cli: &[&str]) -> CaptureArgs {
        match parse(cli).unwrap().command {
            crate::Command::Capture(a) => a,
            _ => panic!("not capture"),
        }
    }

    #[test]
    fn targets() {
        let ini = BonDriver::from_ini(
            std::path::Path::new("d.ini"),
            "[BS]\nBS15/101=1318000,0x40F1,101\n",
        )
        .unwrap();
        let sat = Tuning::Satellite {
            hz: 1_318_000_000,
            tsid: 0x40F1,
        };
        let a = args(&["--bondriver", "d.ini", "--channel", "bs15/101", "-t", "0"]);
        assert_eq!(a.seconds, 0);
        assert_eq!(target(&a, Some(&ini)).unwrap(), (sat, Some(101)));
        let a = args(&[
            "--bondriver",
            "d.ini",
            "--channel",
            "BS15/101",
            "--sid",
            "102",
        ]);
        assert_eq!(
            target(&a, Some(&ini)).unwrap(),
            (sat, Some(102)),
            "--sid wins"
        );
        let a = args(&["--fw", "f", "--channel", "27ch"]);
        assert_eq!(
            target(&a, None).unwrap(),
            (Tuning::Terrestrial { hz: 557_142_857 }, None)
        );
        let a = args(&["--bondriver", "d.ini", "--channel", "BS99/1"]);
        assert!(target(&a, Some(&ini)).is_err());
        let a = args(&["--fw", "f", "--sat", "1318000000"]);
        assert_eq!(
            target(&a, None).unwrap().0,
            Tuning::Satellite {
                hz: 1_318_000_000,
                tsid: NO_TSID
            }
        );
    }

    #[test]
    fn the_daemon_configuration() {
        use std::path::Path;
        let a = args(&["-c", "/etc/dt295.conf", "--channel", "27ch"]);
        assert_eq!(a.config.as_deref(), Some(Path::new("/etc/dt295.conf")));
        let one = Conf::parse("[dt295]\nfirmware-dir = /fw\n[unit AB12345678]\n").unwrap();
        let (dev, raw) = conf_device_args(&one, &a.dev).unwrap();
        assert_eq!(dev.serial.as_deref(), Some("AB12345678"), "its only unit");
        assert!(!raw);
        assert_eq!(
            dev.fw.as_deref(),
            Some(Path::new("/fw/mb86m01_assp_osec_idle.bin"))
        );
        // the default state folder: /var/lib/dt295 (on Windows the configuration's)
        assert_eq!(dev.state, Some(one.state_file("AB12345678")));
        let two = Conf::parse("[unit A]\n[unit B]\n").unwrap();
        assert!(conf_device_args(&two, &a.dev).is_err(), "which one?");
        let a = args(&["-c", "x.conf", "--serial", "B", "--ter", "557142857"]);
        assert_eq!(
            conf_device_args(&two, &a.dev).unwrap().0.serial.as_deref(),
            Some("B")
        );
        assert!(parse(&["-c", "x", "--bondriver", "b.ini", "--ter", "1"]).is_err());
    }

    #[test]
    fn raw_ts_units() {
        let ini = |text: &str| BonDriver::from_ini(std::path::Path::new("d.ini"), text).unwrap();
        let raw = |cli: &[&str], text: &str| device_args(&args(cli), Some(&ini(text))).map(|d| d.1);
        let ch = ["--bondriver", "d.ini", "--channel", "27ch"];
        assert!(
            raw(&ch, "[Tuner]\nRawTS=1\n").unwrap(),
            "the ini selects it"
        );
        assert!(!raw(&ch, "[Tuner]\nRawTS=0\n").unwrap());
        let forced = ["--bondriver", "d.ini", "--channel", "27ch", "--raw-ts"];
        assert!(raw(&forced, "[Tuner]\nRawTS=0\n").unwrap(), "--raw-ts");
        // what only the System1 path uses is refused with a raw-TS ini
        let state = ["--bondriver", "d.ini", "--channel", "27ch", "--state", "s"];
        let err = raw(&state, "[Tuner]\nRawTS=1\n").unwrap_err();
        assert!(err.to_string().contains("System1"), "{err}");
        assert!(raw(&state, "[Tuner]\nRawTS=0\n").is_ok());

        let conf = Conf::parse("[unit A]\n[unit B]\nraw-ts = true\n").unwrap();
        let a = args(&["-c", "x.conf", "--serial", "b", "--ter", "557142857"]);
        assert!(conf_device_args(&conf, &a.dev).unwrap().1, "a raw-ts unit");
        let a = args(&["-c", "x.conf", "--serial", "A", "--ter", "557142857"]);
        assert!(
            !conf_device_args(&conf, &a.dev).unwrap().1,
            "the System1 unit"
        );
    }

    #[test]
    fn raw_ts_arguments() {
        // --raw-ts stands in for the firmware: no --fw, --trc, --state or --sid with it
        assert!(parse(&["--raw-ts", "--ter", "557142857"]).is_ok());
        assert!(parse(&["--raw-ts", "--serial", "AB12345678", "--channel", "27ch"]).is_ok());
        assert!(parse(&["--raw-ts", "--bondriver", "b.ini", "--channel", "BS15/TS1"]).is_ok());
        assert!(parse(&["--raw-ts", "-c", "x.conf", "--ter", "1"]).is_ok());
        for bad in [
            &["--raw-ts", "--fw", "f", "--ter", "1"][..],
            &["--raw-ts", "--trc", "t", "--ter", "1"],
            &["--raw-ts", "--state", "s", "--ter", "1"],
            &["--raw-ts", "--ter", "1", "--sid", "1024"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        // the firmware (or the ini, the configuration, --raw-ts) is still required, and
        // --fw still goes alone
        assert!(parse(&["--ter", "1"]).is_err());
        assert!(parse(&["--fw", "f", "--bondriver", "b.ini", "--ter", "1"]).is_err());
        assert!(parse(&["--fw", "f", "-c", "x.conf", "--ter", "1"]).is_err());
        assert!(parse(&["--fw", "f", "--ter", "1"]).is_ok());
    }
}
