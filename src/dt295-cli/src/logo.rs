//! `dt295 logo`: receive the BS / CS station logos and add them to TVTest's logo files.
//!
//! The channels do not carry their logos. Both sets come in the data carousels of
//! the BS engineering service (service 929 on BS-15): component_tag 0x79 carries the
//! modules LOGO-00..05 (BS) and 0x7A carries CS_LOGO-00..05 (CS). They are sent for
//! under a minute about every 20 minutes at irregular times, not always all of them,
//! and in some periods not at all, so a viewer hardly ever receives them. This command
//! takes a free unit (with the firmware and state file of its BonDriver ini in TVTest's
//! BonDriver folder; on the raw-TS path of a `RawTS=1` ini, booting nothing, so the unit
//! stays usable for raw-TS), holds it on the engineering service until the logos are in, and
//! merges them into TVTest's logo data file, its logo ID map and its logo folder, the
//! way TVTest stores logos it receives itself. `--schedule` makes it a daily task, like
//! the background download of the vendor's application.
//!
//! `--mirakc` writes them for mirakc instead (it does not collect logos itself): one PNG
//! per service, named by mirakc's service id, for its `resource.logos`. With
//! `dt295 serve` holding the units, the TS comes from `dt295 tune` on stdin (`--ts -`).
//! A terrestrial channel's TS gives its station's logos from its CDT (TVTest receives
//! those itself, so only `--mirakc` writes them).

mod carousel;
mod cdt;
mod schedule;
mod tvtest;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use dt295::System;

use crate::bonini::BonDriver;
use crate::rawts;
use crate::serve::proto::Tuning;
use crate::tuner::{self, DeviceArgs, Unit, stopping};
use carousel::{Collector, Logo};
use tvtest::{Store, TRANSPARENT_MAX, TvTest};

/// The engineering service that carries the logos: BS-15 (IF 1318.000 MHz).
const ENG_IF_HZ: u32 = dt295::chan::bs_if_khz(15).unwrap() * 1000;
const ENG_SID: u16 = 929;
/// Seconds from 1601-01-01 (TVTest's time base) to 1970-01-01.
const SECS_1601_TO_UNIX: u64 = 11_644_473_600;
const JST_OFFSET_SECS: u64 = 9 * 3600;
/// Seconds between tries for a free unit or after a failed one.
const RETRY_SECS: u64 = 30;
/// A unit whose stream shows no valid PAT for this long is given up (and tried again).
const STALL: Duration = Duration::from_secs(15);

/// `dt295 logo` options.
#[derive(Debug, Args)]
pub struct LogoArgs {
    /// Without --fw the unit, firmware and state file come from the BonDriver_DT295*.ini
    /// files in TVTest's BonDriver folder (any free unit).
    #[command(flatten)]
    pub dev: DeviceArgs,

    /// TVTest's folder (with TVTest.ini): its logo files are updated.
    #[arg(long, value_name = "DIR", required_unless_present_any = ["unschedule", "mirakc"])]
    pub tvtest: Option<PathBuf>,

    /// Write the logos for mirakc instead: one PNG per service in DIR, named by mirakc's
    /// service id (network_id * 100000 + service_id), for its resource.logos (see
    /// `dt295 mirakc-channels --logos`). Reads the TS given with --ts.
    #[arg(long, value_name = "DIR", requires = "ts",
          conflicts_with_all = ["tvtest", "schedule", "unschedule", "log"])]
    pub mirakc: Option<PathBuf>,

    /// Give up after this many minutes. When they are sent, the logos come about every 20
    /// minutes; the default also covers one missed transmission or a while without a free
    /// unit. A terrestrial CDT goes round in about 10 minutes.
    #[arg(long, value_name = "MINUTES", default_value_t = 45,
          value_parser = clap::value_parser!(u64).range(1..=600))]
    pub wait: u64,

    /// The engineering service's IF in Hz.
    #[arg(long, value_name = "HZ", value_parser = crate::parse_u32, default_value_t = ENG_IF_HZ)]
    pub sat: u32,

    /// The engineering service's transport_stream_id in hex.
    #[arg(long, value_name = "HEX", value_parser = crate::parse_hex_u16, default_value = "40F1")]
    pub tsid: u16,

    /// The engineering service's service id.
    #[arg(long, value_name = "ID", value_parser = crate::parse_u16, default_value_t = ENG_SID)]
    pub sid: u16,

    /// Read a TS of the engineering service instead of tuning (no device): a captured
    /// file, or `-` for stdin as it comes (on Linux from `dt295 tune ... --sid 929`;
    /// until the logos are in, --wait passes or the stream ends). With --mirakc it can
    /// also be a terrestrial channel's (its CDT, until it is in).
    #[arg(long, value_name = "FILE",
          conflicts_with_all = ["fw", "trc", "index", "serial", "state"])]
    pub ts: Option<PathBuf>,

    /// Show what would change; write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// (Windows) Append all output to FILE and leave the console (for Task Scheduler).
    #[arg(long, value_name = "FILE", hide = cfg!(not(windows)))]
    pub log: Option<PathBuf>,

    /// (Windows) Register a daily Task Scheduler task running this command for
    /// --tvtest (at HH:MM, default 03:55; it logs to dt295_logo.log in that folder).
    #[arg(long, value_name = "HH:MM", num_args = 0..=1,
          default_missing_value = schedule::DEFAULT_TIME, hide = cfg!(not(windows)),
          conflicts_with_all = ["fw", "trc", "index", "serial", "state", "ts", "wait",
                                "dry_run", "log"])]
    pub schedule: Option<String>,

    /// (Windows) Remove the Task Scheduler task.
    #[arg(long, conflicts_with = "schedule", hide = cfg!(not(windows)))]
    pub unschedule: bool,
}

pub fn run(a: &LogoArgs) -> Result<()> {
    if cfg!(not(windows)) {
        for (given, name) in [
            (a.log.is_some(), "--log"),
            (a.schedule.is_some(), "--schedule"),
            (a.unschedule, "--unschedule"),
        ] {
            if given {
                bail!(
                    "{name} is only on Windows (Task Scheduler); on Linux see the systemd timer in INSTALL.md"
                );
            }
        }
    }
    if a.unschedule {
        return schedule::remove();
    }
    if let Some(dir) = &a.mirakc {
        return to_mirakc(a, dir);
    }
    let dir = a.tvtest.as_deref().context("--tvtest is required")?;
    let tv = TvTest::open(dir)?;
    if let Some(at) = &a.schedule {
        return schedule::create(&tv, at);
    }
    if let Some(p) = &a.log {
        log_to(p)?;
        println!("== dt295 logo, {} (JST)", jst_text(unix_now(), true));
    }
    fetch(a, &tv)
}

fn fetch(a: &LogoArgs, tv: &TvTest) -> Result<()> {
    // TVTest's files first: no point waiting for logos that cannot be stored.
    let store = Store::locate(tv)?;
    store.load_data()?;

    let collector = match &a.ts {
        Some(p) => read_ts(p, a)?,
        None => receive(a, tv)?,
    };
    let logos = collector.logos();
    if logos.is_empty() {
        bail!("no complete logo data ({})", collector.progress());
    }
    let time = collector.time.unwrap_or_else(now_jst);
    // read again: TVTest may have saved logos of its own while this one waited
    let mut data = store.load_data()?;
    let report = data.merge(&logos, time);
    println!("{}", report.summary(&logos));

    if a.dry_run {
        println!("dry run: nothing written");
        return Ok(());
    }
    store.save(&data, &report)?;
    if !report.updated.is_empty() {
        // TVTest re-reads a newer file when it exits but keeps its own copy of a logo
        // of the same version (and of an ID map entry)
        println!(
            "note: a running TVTest puts its own copy of the {} updated logos back when \
             it exits; run with TVTest closed to keep them",
            report.updated.len()
        );
    }
    if !report.added.is_empty() || !report.updated.is_empty() {
        println!("TVTest reads its logo files when it starts: new logos show from its next start");
    }
    Ok(())
}

/// Hold a free unit on the engineering service until every logo module is in, or
/// until `--wait` minutes have passed (waiting for a free unit, and trying again after
/// a failed one, within that time).
fn receive(a: &LogoArgs, tv: &TvTest) -> Result<Collector> {
    tuner::install_ctrl_c();
    if a.dev.fw.is_some() && a.dev.trc.is_none() {
        bail!("--fw needs --trc (the transcoder firmware) to stream");
    }
    if a.dev.fw.is_none() && (a.dev.trc.is_some() || a.dev.state.is_some() || a.dev.index != 0) {
        bail!(
            "--trc, --state and -i go with --fw; without it the BonDriver inis give the \
             unit (--serial picks one)"
        );
    }
    let bons = if a.dev.fw.is_some() {
        Vec::new()
    } else {
        let dir = tv.driver_dir();
        let bons = BonDriver::find(&dir)?;
        if bons.is_empty() {
            bail!(
                "no BonDriver_DT295*.ini in {}: give the unit with --fw and --trc",
                dir.display()
            );
        }
        bons
    };
    let console = a.log.is_none();
    let deadline = unix_now() + a.wait * 60;
    println!(
        "waiting for the logos until {} (JST) at most (in some periods none are sent)",
        jst_text(deadline, false)
    );
    let shared = Arc::new(Mutex::new(Collector::new(a.sid)));
    let mut said_busy = false;
    while !stopping() && unix_now() < deadline && !lock(&shared).done() {
        let (dev, raw) = if bons.is_empty() {
            (a.dev.clone(), false)
        } else {
            match choose(&bons, a.dev.serial.as_deref())? {
                Some(d) => d,
                None => {
                    if !said_busy {
                        println!("{}: no free PIX-DT295 yet", jst_text(unix_now(), false));
                        said_busy = true;
                    }
                    pause(RETRY_SECS, deadline);
                    continue;
                }
            }
        };
        let held = if raw {
            hold_raw(a, &dev, &shared, deadline, console)
        } else {
            hold(a, &dev, &shared, deadline, console)
        };
        match held {
            Ok(()) => break, // the logos are in, the time is up, or Ctrl-C
            Err(e) => {
                println!("{}: {e:#}", jst_text(unix_now(), false));
                pause(RETRY_SECS, deadline);
            }
        }
    }
    Ok(std::mem::take(&mut *lock(&shared)))
}

/// A free unit (only `only` if given) with the firmware and the state file of its
/// BonDriver's folder, and whether that ini is on the raw-TS path (`RawTS=1`: boot
/// nothing, as the BonDriver).
fn choose(bons: &[BonDriver], only: Option<&str>) -> Result<Option<(DeviceArgs, bool)>> {
    let units = dt295::list().context("enumerate USB devices")?;
    let free: Vec<String> = units
        .iter()
        .filter(|u| u.available)
        .filter_map(|u| u.serial.clone())
        .filter(|s| only.is_none_or(|o| s.eq_ignore_ascii_case(o)))
        .collect();
    let Some((serial, bon)) = tvtest::pick_unit(&free, units.len(), bons) else {
        return Ok(None);
    };
    let files = bon.unwrap_or(&bons[0]);
    let raw = files.tuner.raw_ts;
    let path = if raw { ", raw-TS" } else { "" };
    match bon {
        Some(b) => println!("using unit {serial} ({}{path})", b.ini.display()),
        None => println!("using unit {serial} (no BonDriver ini names it{path})"),
    }
    let dev = DeviceArgs {
        fw: Some(files.tuner.firmware()),
        trc: Some(files.tuner.trc_firmware()),
        index: 0,
        tuner: 1,
        state: Some(files.tuner.state_file(&serial)),
        state_dir: None,
        serial: Some(serial),
    };
    Ok(Some((dev, raw)))
}

/// Tune the engineering service and collect until the logos are in or until `end`.
fn hold(
    a: &LogoArgs,
    dev: &DeviceArgs,
    shared: &Arc<Mutex<Collector>>,
    end: u64,
    console: bool,
) -> Result<()> {
    let mut unit = Unit::open(dev)?;
    unit.dev.tune_s(a.sat, a.tsid).context("tune (ISDB-S)")?;
    unit.wait_lock(System::Satellite)?;
    unit.dev.set_service_id(Some(a.sid));
    let worker = Arc::clone(shared);
    unit.dev
        .stream_start(Box::new(move |ts: &[u8]| lock(&worker).feed(ts)))
        .context("stream start")?;
    let stalled = collect(shared, end, console);
    let stopped = unit.dev.stream_stop(); // the worker no longer feeds after this
    unit.save_link_key();
    if stalled {
        bail!(
            "no decodable PAT for {} s: the stream stopped, or the link key does not match",
            STALL.as_secs()
        );
    }
    stopped.context("stream stop")
}

/// [`hold`] on the raw-TS path ([`rawts`]): open the unit without booting anything, bring
/// the chip up (once per plug) or only retune, and collect from the scrambled TS of the
/// whole transponder (the tables and the engineering service are not scrambled). No
/// state file.
fn hold_raw(
    a: &LogoArgs,
    dev: &DeviceArgs,
    shared: &Arc<Mutex<Collector>>,
    end: u64,
    console: bool,
) -> Result<()> {
    let mut d = tuner::open_device(dev)?;
    let bytes = Arc::new(AtomicU64::new(0));
    let (worker, count) = (Arc::clone(shared), Arc::clone(&bytes));
    let sink = Box::new(move |ts: &[u8]| {
        count.fetch_add(ts.len() as u64, Ordering::Relaxed);
        lock(&worker).feed(ts);
    });
    let tuning = Tuning::Satellite {
        hz: a.sat,
        tsid: a.tsid,
    };
    let full = rawts::start(&mut d, tuning, sink).map_err(|why| anyhow!("raw-TS: {why}"))?;
    if !rawts::wait_first_ts(&d, || bytes.load(Ordering::Relaxed)) {
        if stopping() {
            return d.stream_stop_raw().map(drop).context("raw stream stop");
        }
        bail!("raw-TS: {}", rawts::why_no_ts(&mut d, full));
    }
    let stalled = collect(shared, end, console);
    let stopped = d.stream_stop_raw();
    if stalled {
        bail!(
            "no PAT for {} s: the stream stopped (the worker ended or the signal went)",
            STALL.as_secs()
        );
    }
    stopped.map(drop).context("raw stream stop")
}

/// While a stream feeds `shared`: show the progress until the logos are in, `end` comes,
/// a stop signal or a stream without a valid PAT for [`STALL`] (true).
fn collect(shared: &Arc<Mutex<Collector>>, end: u64, console: bool) -> bool {
    let mut seen = (lock(shared).pats, Instant::now());
    let mut stalled = false;
    loop {
        thread::sleep(Duration::from_millis(500));
        let (done, progress, pats) = {
            let c = lock(shared);
            (c.done(), c.progress(), c.pats)
        };
        if pats != seen.0 {
            seen = (pats, Instant::now());
        }
        let now = unix_now();
        if console {
            eprint!("{progress}; {}  \r", jst_text(now, false));
        }
        if done || stopping() || now >= end {
            break;
        }
        if seen.1.elapsed() >= STALL {
            stalled = true;
            break;
        }
    }
    if console {
        eprintln!();
    }
    println!(
        "{}: {}",
        jst_text(unix_now(), false),
        lock(shared).progress()
    );
    stalled
}

/// The logos of the TS given with --ts: a file, or `-` read as it comes until the logos
/// are in, --wait passes or the stream ends.
fn read_ts(path: &Path, a: &LogoArgs) -> Result<Collector> {
    let mut c = Collector::new(a.sid);
    if path.as_os_str() != "-" {
        c.feed(&fs::read(path).with_context(|| format!("cannot read {}", path.display()))?);
        return Ok(c);
    }
    let deadline = unix_now() + a.wait * 60;
    // Read on a thread: a pipe that goes silent (a service off the air, which
    // `dt295 tune` never starts) must not outlast --wait.
    let (tx, rx) = mpsc::sync_channel::<io::Result<Vec<u8>>>(64);
    thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let mut buf = vec![0u8; 1 << 16];
            let sent = match input.read(&mut buf) {
                Ok(0) => break, // the stream ended
                Ok(n) => {
                    buf.truncate(n);
                    tx.send(Ok(buf))
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => tx.send(Err(e)),
            };
            if sent.is_err() {
                break;
            }
        }
    });
    let mut told = unix_now();
    loop {
        let now = unix_now();
        if c.done() || now >= deadline {
            break;
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(data)) => c.feed(&data),
            Ok(Err(e)) => return Err(e).context("reading the TS"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // the stream ended
        }
        if now >= told + 60 {
            eprintln!("{}: {}", jst_text(now, false), c.progress());
            told = now;
        }
    }
    eprintln!("{}: {}", jst_text(unix_now(), false), c.progress());
    Ok(c)
}

/// Logo types for mirakc, best first: HD (16:9) large and small, SD 16:9 large and
/// small, SD 4:3 large and small.
const MIRAKC_TYPES: [u8; 6] = [5, 2, 4, 1, 3, 0];

/// One PNG per service, keyed by mirakc's service id: the most preferred type it has.
/// `None` for a service with only transparent dummies (no logo).
fn mirakc_logos(logos: &[Logo]) -> BTreeMap<u64, Option<&[u8]>> {
    let rank = |t: u8| {
        MIRAKC_TYPES
            .iter()
            .position(|&x| x == t)
            .unwrap_or(usize::MAX)
    };
    let mut best: BTreeMap<u64, Option<(usize, &[u8])>> = BTreeMap::new();
    for logo in logos {
        let real = logo.png.len() > TRANSPARENT_MAX;
        let r = rank(logo.logo_type);
        for &(onid, _, sid) in &logo.services {
            let id = crate::mirakc::service_id(onid, sid);
            let held = best.entry(id).or_default();
            if real && held.is_none_or(|(h, _)| r < h) {
                *held = Some((r, &logo.png));
            }
        }
    }
    best.into_iter()
        .map(|(id, b)| (id, b.map(|(_, png)| png)))
        .collect()
}

/// `--mirakc`: the logos of the TS as `<service id>.png` files in `dir`.
fn to_mirakc(a: &LogoArgs, dir: &Path) -> Result<()> {
    let ts = a.ts.as_deref().context("--mirakc needs --ts")?;
    let collector = read_ts(ts, a)?;
    let mut logos = collector.logos();
    logos.extend(collector.cdt_logos());
    if logos.is_empty() {
        bail!("no complete logo data ({})", collector.progress());
    }
    let files = mirakc_logos(&logos);
    let count = files.values().flatten().count();
    if a.dry_run {
        println!("{count} service logo(s); dry run: nothing written");
        return Ok(());
    }
    let complete = collector.done();
    let (written, removed) = write_mirakc(dir, &files, complete)?;
    println!(
        "{count} service logo(s) in {}: {written} written, {} kept, {removed} removed{}",
        dir.display(),
        count - written,
        if complete {
            ""
        } else {
            " (incomplete: only the missing ones written)"
        }
    );
    Ok(())
}

/// Write the changed `<service id>.png` files and remove those of the services that
/// have no logo; returns (written, removed). From `incomplete` data (a stream cut short:
/// some types or modules missing) only the missing files are written, so a better type
/// from an earlier run is not replaced.
fn write_mirakc(
    dir: &Path,
    files: &BTreeMap<u64, Option<&[u8]>>,
    complete: bool,
) -> Result<(usize, usize)> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let (mut written, mut removed) = (0, 0);
    for (id, png) in files {
        let path = dir.join(format!("{id}.png"));
        let Some(png) = png else {
            if complete && path.is_file() {
                fs::remove_file(&path)
                    .with_context(|| format!("cannot remove {}", path.display()))?;
                removed += 1;
            }
            continue;
        };
        let keep = if complete {
            fs::read(&path).is_ok_and(|old| old == *png)
        } else {
            path.exists()
        };
        if keep {
            continue;
        }
        let tmp = dir.join(format!("{id}.png.tmp"));
        fs::write(&tmp, png)
            .and_then(|()| fs::rename(&tmp, &path))
            .with_context(|| format!("cannot write {}", path.display()))?;
        written += 1;
    }
    Ok((written, removed))
}

fn lock(c: &Mutex<Collector>) -> MutexGuard<'_, Collector> {
    c.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sleep `secs`, but not past `deadline` (Unix seconds) and not after Ctrl-C.
fn pause(secs: u64, deadline: u64) {
    let until = (unix_now() + secs).min(deadline);
    while unix_now() < until && !stopping() {
        thread::sleep(Duration::from_millis(500));
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or_default(|d| d.as_secs())
}

/// Now as TVTest's time: seconds since 1601-01-01 in Japan time.
fn now_jst() -> u64 {
    SECS_1601_TO_UNIX + unix_now() + JST_OFFSET_SECS
}

/// Unix seconds as Japan time: "HH:MM:SS", with the date first if `date`.
fn jst_text(unix: u64, date: bool) -> String {
    let t = unix + JST_OFFSET_SECS;
    let s = t % 86_400;
    let hms = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    if date {
        let (y, m, d) = schedule::civil_from_days(i64::try_from(t / 86_400).unwrap_or_default());
        format!("{y:04}-{m:02}-{d:02} {hms}")
    } else {
        hms
    }
}

/// `--log`: from here on stdout and stderr append to `path`, and the console (the
/// window Task Scheduler opens) is let go.
#[cfg(windows)]
fn log_to(path: &Path) -> Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        FreeConsole, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    // kept open for the rest of the process
    let handle = file.into_raw_handle();
    // SAFETY: plain Win32 calls; `handle` is an open file handle that is never closed.
    // std looks the standard handles up on every write, so the output follows.
    unsafe {
        FreeConsole();
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    Ok(())
}

#[cfg(not(windows))]
fn log_to(_: &Path) -> Result<()> {
    bail!("--log is only on Windows (Task Scheduler)")
}
