//! The BonDriver's side of `dt295 serve`: the unit belongs to the daemon in the DLL's
//! folder (`dt295.exe serve`), which this DLL starts when none answers, and every DLL
//! instance (another TVTest, a renamed copy of the DLL with its own ini) is a client of
//! that one daemon. The relation never changes: the DLL is the client, the daemon the
//! server, whichever DLL started it; the daemon ends by itself `[Server] IdleExitSec`
//! seconds after its last client.
//!
//! Where they meet is not configurable: the socket of the DLL's folder
//! ([`Target::of_folder`]; `%TEMP%\dt295-<hash>.sock`, or the folder's loopback port
//! where a Unix domain socket cannot be used), so that every DLL of one folder meets the
//! same daemon and nothing is left in the folder. The daemon is told where to listen,
//! and that its firmware and the units' state files are in the DLL's folder; it writes
//! its log to `dt295-serve.log` there.
//!
//! A supervisor thread per DLL instance asks the daemon for the unit's C/N once a second
//! (so `GetSignalLevel` never waits) and brings the stream back when it is not running:
//! a daemon that was killed or crashed is started again and the last channel asked for
//! once more; a stream the daemon ended (the unit was unplugged, a recorder took it
//! over) or could not start yet (no unit, another TVTest streams on it) is asked for
//! again with growing pauses, so the picture comes back by itself.
//!
//! Every request carries this instance's client id and asks only for a free tuner, so
//! the daemon replaces this DLL's own stream at once on a channel change and answers
//! `busy` for a tuner another TVTest streams on.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dt295::TsSink;
use dt295::ipc::{self, Conn, Target};
use dt295::serve::{MAX_LINE, Mode, Reply, Request, StatusRequest, Tuning};
use tracing::{info, warn};

/// The daemon's executable, in the DLL's folder.
pub const SERVER_EXE: &str = "dt295.exe";
/// The daemon's log, in the DLL's folder.
pub const SERVER_LOG: &str = "dt295-serve.log";
/// A daemon log longer than this is moved to `.log.1` before the next daemon starts.
const SERVER_LOG_MAX: u64 = 1 << 20;

/// How long a failed recovery waits before the next attempt, at first; it doubles up to
/// [`RETRY_MAX`] while the attempts keep failing.
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How long a status request may take: the daemon answers it from what it has, so this
/// only catches a daemon that is stuck.
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a stream request may take: a boot (a few seconds), or the cold bring-up of
/// raw-TS, a tune and the lock wait.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// TS packet size: the sink takes whole packets.
const PACKET: usize = 188;
/// A nominal C/N reported for a flowing raw-TS stream whose daemon gives none, so the
/// host's signal meter shows the stream is up.
const RAW_NOMINAL_CN_MDB: i32 = 30_000;
/// No C/N.
const NO_CN: i32 = i32::MIN;

/// What the DLL's ini says about the daemon and the unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The DLL's folder: `dt295.exe`, the firmware and the state files.
    pub dir: PathBuf,
    /// `[Tuner] DeviceSerial`; `None` = the only unit connected.
    pub serial: Option<String>,
    /// `[Tuner] TunerNumber`: 1, or 2 (a two-tuner model's second).
    pub tuner: u8,
    /// `[Tuner] RawTS`.
    pub mode: Mode,
    /// `[Server] IdleExitSec`: the daemon ends after this long without a client (0 =
    /// never).
    pub idle_exit_secs: u64,
    /// `[Server] StartTimeoutSec`: how long to wait for a daemon this DLL started to
    /// answer. It listens before it touches a unit, so this is only a backstop; a daemon
    /// that ends at once is noticed by its exit.
    pub start_timeout_secs: u64,
    /// The daemon's log level: `-v` (debug) or `-vv` (trace), from `[Tuner] LogLevel`.
    pub verbose: u8,
}

/// The channel asked for: what a recovery asks for again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub name: String,
    pub tuning: Tuning,
    pub sid: Option<u16>,
}

/// How the daemon is started.
#[derive(Debug, Clone)]
struct Launch {
    exe: PathBuf,
    args: Vec<OsString>,
    log: PathBuf,
}

/// Why a status request got no answer.
#[derive(Debug)]
enum NoAnswer {
    /// Nothing listens at the target: there is no daemon.
    Unreachable(String),
    /// A daemon took the connection but did not answer in time: it is there, so no
    /// second one is started.
    Failed(String),
    /// The connection was closed without an answer: a daemon that is ending closes what
    /// it had not read yet, and nothing listens a moment later.
    Closed(String),
}

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "cannot reach dt295 serve: {e}"),
            Self::Failed(e) => write!(f, "dt295 serve does not answer: {e}"),
            Self::Closed(e) => write!(f, "dt295 serve closed the connection: {e}"),
        }
    }
}

/// An I/O error of a request: the daemon closed the connection, or it did not answer.
fn no_answer(e: &std::io::Error) -> NoAnswer {
    match e.kind() {
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            NoAnswer::Closed(e.to_string())
        }
        _ => NoAnswer::Failed(e.to_string()),
    }
}

/// Why a stream request failed.
#[derive(Debug)]
enum StartError {
    /// The daemon refused it for good (no lock, the other mode, no such unit): asking
    /// again would fail again.
    Refused(String),
    /// Refused for now (no unit there yet, another TVTest streams on it), or the daemon
    /// could not be reached or did not answer: the supervisor asks again.
    Retry(String),
}

impl StartError {
    fn of_reply(why: String) -> Self {
        if retry_later(&why) {
            Self::Retry(why)
        } else {
            Self::Refused(why)
        }
    }

    fn into_string(self) -> String {
        match self {
            Self::Refused(why) | Self::Retry(why) => why,
        }
    }
}

/// What the daemon answered `ok` to: the connection the TS comes on.
struct Accepted {
    conn: Conn,
    ts: BufReader<Conn>,
    unit: Option<String>,
    cn_mdb: Option<i32>,
    /// The request, for the log.
    line: String,
    t0: Instant,
}

/// The reader thread that copies the TS from the daemon's connection to the sink.
#[derive(Debug)]
struct Stream {
    conn: Conn,
    stop: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl Stream {
    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.conn.shutdown(Shutdown::Both);
        let _ = self.reader.join();
    }
}

/// What TVTest's threads and the supervisor share.
struct Inner {
    target: Target,
    /// This DLL instance's id in requests.
    client: String,
    settings: Settings,
    launch: Launch,
    /// A sink into the TS FIFO, per stream.
    sink: Box<dyn Fn() -> TsSink + Send + Sync>,
    /// The channel TVTest asked for last.
    want: Mutex<Option<Want>>,
    stream: Mutex<Option<Stream>>,
    /// The unit the daemon streams this DLL's channel on (from its `ok`).
    unit: Mutex<Option<String>>,
    /// Serialises stopping and starting the stream between TVTest's thread and the
    /// supervisor.
    ops: Mutex<()>,
    /// Counts TVTest's channel changes and stops, so that the supervisor does not act
    /// on what it read before one.
    epoch: AtomicU64,
    /// The stream's C/N (0.001 dB; [`NO_CN`]): of its tune, then as the supervisor reads
    /// it.
    cn_mdb: AtomicI32,
    /// The stream wanted is not running: the reader saw the daemon end it, or it could
    /// not be started yet. The reader sets it holding `stream`, where a stream's `stop`
    /// is set, so a stream ended on purpose never sets it.
    lost: AtomicBool,
    closing: AtomicBool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("target", &self.target)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The command line of the daemon: `dt295.exe serve` in the DLL's folder, told where to
/// listen, when to end, and that its files are in that folder.
fn launch_spec(s: &Settings, target: &Target) -> Launch {
    let mut args: Vec<OsString> = vec!["serve".into()];
    match s.verbose {
        0 => {}
        1 => args.push("-v".into()),
        _ => args.push("-vv".into()),
    }
    match target {
        Target::Unix(p) => {
            args.push("--socket".into());
            args.push(p.into());
        }
        Target::Tcp(a) => {
            args.push("--listen".into());
            args.push(a.into());
        }
    }
    args.push("--idle-exit".into());
    args.push(s.idle_exit_secs.to_string().into());
    args.push("--firmware-dir".into());
    args.push(s.dir.clone().into());
    args.push("--state-dir".into());
    args.push(s.dir.clone().into());
    Launch {
        exe: s.dir.join(SERVER_EXE),
        args,
        log: s.dir.join(SERVER_LOG),
    }
}

/// One status request for a tuner: the unit's serial number and the C/N of the tuner's
/// stream when the daemon answered `ok`, `None` when it answered otherwise (no unit
/// there), [`NoAnswer`] when it did not.
fn status_of(
    target: &Target,
    unit: &str,
    tuner: u8,
) -> Result<Option<(String, Option<i32>)>, NoAnswer> {
    let conn = ipc::connect(target).map_err(|e| NoAnswer::Unreachable(e.to_string()))?;
    conn.set_read_timeout(Some(STATUS_TIMEOUT))
        .map_err(|e| no_answer(&e))?;
    let req = StatusRequest {
        unit: unit.to_owned(),
        tuner,
    };
    (&conn)
        .write_all(format!("{}\n", req.to_line()).as_bytes())
        .map_err(|e| no_answer(&e))?;
    let mut line = String::new();
    BufReader::new((&conn).take(MAX_LINE))
        .read_line(&mut line)
        .map_err(|e| no_answer(&e))?;
    if line.is_empty() {
        return Err(NoAnswer::Closed("no answer".into()));
    }
    match Reply::parse(&line) {
        Reply::Ok { unit, cn_mdb } => Ok(Some((unit.unwrap_or_default(), cn_mdb))),
        Reply::Busy(_) | Reply::Error(_) => Ok(None),
    }
}

/// Whether the supervisor should try to bring the stream back now: a channel is wanted,
/// the stream is not running or the daemon does not answer, and the pause after the
/// previous attempt is over.
fn recovery_due(lost: bool, alive: bool, last_try: Option<Instant>, pause: Duration) -> bool {
    (lost || !alive) && last_try.is_none_or(|t| t.elapsed() >= pause)
}

/// The pause before the next attempt after a failed one.
fn next_pause(after_failure: Duration) -> Duration {
    (after_failure * 2).min(RETRY_MAX)
}

/// An error the daemon answered that is worth asking again by itself: no unit there yet.
fn retry_later(why: &str) -> bool {
    why.contains("not connected") || why.starts_with("no PIX-DT295")
}

/// How often, 100 ms apart, a daemon that closed the status request without an answer
/// is asked again before it counts as stuck: one that is ending stops listening at once.
const CLOSING_TRIES: u32 = 10;

impl Inner {
    fn status(&self) -> Result<Option<(String, Option<i32>)>, NoAnswer> {
        let unit = lock(&self.unit).clone();
        let unit = unit.or_else(|| self.settings.serial.clone());
        status_of(
            &self.target,
            unit.as_deref().unwrap_or(""),
            self.settings.tuner,
        )
    }

    fn spawn_server(&self) -> Result<Child, String> {
        let l = &self.launch;
        // a long log moves aside (the daemon appends to it)
        if std::fs::metadata(&l.log).is_ok_and(|m| m.len() > SERVER_LOG_MAX) {
            let _ = std::fs::rename(&l.log, l.log.with_extension("log.1"));
        }
        let mut cmd = Command::new(&l.exe);
        cmd.args(&l.args)
            .current_dir(&self.settings.dir)
            .stdin(Stdio::null());
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&l.log)
        {
            Ok(f) => {
                let f2 = f.try_clone().map_err(|e| e.to_string())?;
                cmd.stdout(Stdio::from(f)).stderr(Stdio::from(f2));
            }
            Err(e) => {
                warn!("dt295 serve: cannot write its log {}: {e}", l.log.display());
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        // what an environment of the host says about the daemon does not apply here
        cmd.env_remove(ipc::ENV_SOCKET)
            .env_remove(ipc::ENV_LISTEN)
            .env_remove("RUST_LOG");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }
        info!(
            "dt295 serve: starting {} at {} (log {})",
            l.exe.display(),
            self.target,
            l.log.display()
        );
        cmd.spawn()
            .map_err(|e| format!("cannot start {}: {e}", l.exe.display()))
    }

    /// The daemon answers, or is started here and waited for. One that ends right after
    /// it was started (it cannot listen, another DLL's daemon won the race) is noticed by
    /// its exit, so TVTest does not wait out the start timeout for it. A daemon that is
    /// ending (it closes the request unanswered) is waited for to go, and one started.
    fn ensure_server(&self) -> Result<(), String> {
        let mut tries = 0;
        loop {
            match self.status() {
                Ok(_) => return Ok(()),
                Err(NoAnswer::Closed(_)) if tries < CLOSING_TRIES => {
                    tries += 1;
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e @ (NoAnswer::Failed(_) | NoAnswer::Closed(_))) => {
                    return Err(format!("{e} (at {})", self.target));
                }
                Err(NoAnswer::Unreachable(e)) => {
                    info!("dt295 serve: none answers at {} ({e})", self.target);
                    break;
                }
            }
        }
        let mut child = self.spawn_server()?;
        let t0 = Instant::now();
        let timeout = Duration::from_secs(self.settings.start_timeout_secs.max(1));
        loop {
            thread::sleep(Duration::from_millis(100));
            if self.closing.load(Ordering::Acquire) {
                return Err("closing".into());
            }
            match self.status() {
                Ok(_) => {
                    info!(
                        "dt295 serve: answers ({} ms after the start)",
                        t0.elapsed().as_millis()
                    );
                    return Ok(());
                }
                Err(e @ NoAnswer::Failed(_)) => return Err(format!("it started but {e}")),
                // the one that was ending, still closing what it had queued
                Err(NoAnswer::Unreachable(e) | NoAnswer::Closed(e)) => {
                    if let Ok(Some(exit)) = child.try_wait() {
                        // gone already: another DLL's daemon may have won the race
                        return match self.status() {
                            Ok(_) => Ok(()),
                            Err(_) => Err(format!(
                                "dt295 serve ended at once ({exit}); see {}",
                                self.launch.log.display()
                            )),
                        };
                    }
                    if t0.elapsed() >= timeout {
                        return Err(format!(
                            "dt295 serve did not answer within {} s: {e}",
                            timeout.as_secs()
                        ));
                    }
                }
            }
        }
    }

    /// Mark the running stream as ended on purpose: its reader seeing it end from now on
    /// is no stream lost.
    fn quiet_stream(&self) {
        if let Some(s) = lock(&self.stream).as_ref() {
            s.stop.store(true, Ordering::Release);
        }
    }

    /// End the running stream; its reader is gone when this returns, and with it every
    /// write of its TS to the sink.
    fn stop_stream(&self) {
        self.quiet_stream();
        let stream = lock(&self.stream).take();
        if let Some(s) = stream {
            s.stop();
        }
        self.cn_mdb.store(NO_CN, Ordering::Relaxed);
    }

    /// Ask the daemon for the channel's stream and feed the sink from it. Returns the
    /// C/N of the tune (0.001 dB) when the daemon gives it.
    fn start_stream(self: &Arc<Self>, want: &Want) -> Result<Option<i32>, StartError> {
        let accepted = self.request(want)?;
        self.attach(accepted, want)
    }

    /// Ask the daemon for the channel's stream, up to its `ok`.
    fn request(&self, want: &Want) -> Result<Accepted, StartError> {
        let t0 = Instant::now();
        let req = Request {
            unit: self.settings.serial.clone().unwrap_or_default(),
            tuner: self.settings.tuner,
            tuning: want.tuning,
            sid: want.sid,
            mode: Some(self.settings.mode),
            client: Some(self.client.clone()),
            // another TVTest streams on the tuner: fail rather than cut it off
            if_free: true,
        };
        let retry = StartError::Retry;
        let conn = ipc::connect(&self.target)
            .map_err(|e| retry(format!("cannot reach dt295 serve at {}: {e}", self.target)))?;
        conn.set_read_timeout(Some(REPLY_TIMEOUT))
            .map_err(|e| retry(e.to_string()))?;
        (&conn)
            .write_all(format!("{}\n", req.to_line()).as_bytes())
            .map_err(|e| retry(format!("cannot send the request: {e}")))?;
        let mut ts =
            BufReader::with_capacity(1 << 16, conn.try_clone().map_err(|e| retry(e.to_string()))?);
        let mut line = String::new();
        (&mut ts)
            .take(MAX_LINE)
            .read_line(&mut line)
            .map_err(|e| retry(format!("no answer from dt295 serve: {e}")))?;
        if line.is_empty() {
            return Err(retry("dt295 serve closed the connection".into()));
        }
        match Reply::parse(&line) {
            Reply::Ok { unit, cn_mdb } => Ok(Accepted {
                conn,
                ts,
                unit,
                cn_mdb,
                line: req.to_line(),
                t0,
            }),
            Reply::Busy(why) => Err(retry(format!("busy: {why}"))),
            Reply::Error(why) => Err(StartError::of_reply(why)),
        }
    }

    /// Feed the sink from the stream the daemon accepted. Returns the C/N of the tune
    /// (0.001 dB) when the daemon gives it.
    fn attach(self: &Arc<Self>, a: Accepted, want: &Want) -> Result<Option<i32>, StartError> {
        let Accepted {
            conn,
            ts,
            unit,
            cn_mdb,
            line,
            t0,
        } = a;
        info!(
            "{line}: streaming from dt295 serve ({} ms){}",
            t0.elapsed().as_millis(),
            cn_mdb.map_or_default(|cn| format!(", C/N {:.1} dB", f64::from(cn) / 1000.0))
        );
        if unit.is_some() {
            *lock(&self.unit) = unit;
        }
        let cn = cn_mdb.or((self.settings.mode == Mode::RawTs).then_some(RAW_NOMINAL_CN_MDB));
        self.cn_mdb.store(cn.unwrap_or(NO_CN), Ordering::Relaxed);
        let _ = conn.set_read_timeout(None);
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (inner, stop) = (Arc::clone(self), Arc::clone(&stop));
            let mut sink = (self.sink)();
            let name = want.name.clone();
            thread::Builder::new()
                .name("BonDriver_DT295 stream".into())
                .spawn(move || {
                    let bytes = read_ts(ts, &mut sink);
                    // where `stop` is set: ended on purpose or not, never in between
                    let _stream = lock(&inner.stream);
                    if !stop.load(Ordering::Acquire) {
                        warn!(
                            "dt295 serve ended the stream of {name} ({bytes} bytes); it is \
                             asked for again"
                        );
                        inner.lost.store(true, Ordering::Release);
                    }
                })
                .map_err(|e| StartError::Retry(format!("cannot start the reader: {e}")))?
        };
        *lock(&self.stream) = Some(Stream { conn, stop, reader });
        Ok(cn_mdb)
    }
}

/// Copy the TS to the sink in whole packets until the connection ends. Returns the bytes
/// read.
fn read_ts(mut ts: BufReader<impl Read>, sink: &mut TsSink) -> u64 {
    let mut buf = vec![0u8; 1 << 16];
    let mut have = 0;
    // what the reply line's read took in already
    let early = ts.buffer().len().min(buf.len());
    buf[..early].copy_from_slice(&ts.buffer()[..early]);
    ts.consume(early);
    have += early;
    let mut total = early as u64;
    loop {
        let whole = have / PACKET * PACKET;
        if whole > 0 {
            sink(&buf[..whole]);
            buf.copy_within(whole..have, 0);
            have -= whole;
        }
        let n = match ts.get_mut().read(&mut buf[have..]) {
            Ok(0) => return total,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return total,
        };
        have += n;
        total += n as u64;
    }
}

/// Once a second: the C/N, and the stream brought back when it is not running.
fn supervise(inner: &Arc<Inner>) {
    let mut last_try: Option<Instant> = None;
    let mut pause = RETRY_FIRST;
    while !inner.closing.load(Ordering::Acquire) {
        let epoch = inner.epoch.load(Ordering::Acquire);
        let alive = match inner.status() {
            Ok(st) => {
                let streaming = lock(&inner.stream).is_some();
                if let Some((_, Some(cn))) = st
                    && streaming
                {
                    inner.cn_mdb.store(cn, Ordering::Relaxed);
                }
                true
            }
            // a daemon that is there but slow is not replaced by a second one
            Err(e) => matches!(e, NoAnswer::Failed(_)),
        };
        let due = lock(&inner.want).is_some()
            && recovery_due(inner.lost.load(Ordering::Acquire), alive, last_try, pause);
        if due {
            let ops = lock(&inner.ops);
            if inner.closing.load(Ordering::Acquire) {
                break;
            }
            // read again under `ops`: TVTest's thread may have changed the channel or
            // stopped the stream since, and then what was read above no longer holds
            let want = lock(&inner.want).clone();
            let lost = inner.lost.load(Ordering::Acquire);
            let Some(want) = want.filter(|_| {
                inner.epoch.load(Ordering::Acquire) == epoch
                    && recovery_due(lost, alive, None, pause)
            }) else {
                drop(ops);
                nap(inner);
                continue;
            };
            last_try = Some(Instant::now());
            info!(
                "{}; asking for {} again",
                if alive {
                    "the stream is not running"
                } else {
                    "dt295 serve does not answer"
                },
                want.name
            );
            inner.stop_stream();
            inner.lost.store(false, Ordering::Release);
            match inner
                .ensure_server()
                .map_err(StartError::Retry)
                .and_then(|()| inner.start_stream(&want))
            {
                Ok(_) => {
                    info!("the stream of {} is back", want.name);
                    pause = RETRY_FIRST;
                }
                Err(e) => {
                    info!(
                        "the stream of {} is not back yet ({}); next try in {} s",
                        want.name,
                        e.into_string(),
                        pause.as_secs()
                    );
                    inner.lost.store(true, Ordering::Release);
                    pause = next_pause(pause);
                }
            }
        }
        nap(inner);
    }
}

/// This DLL instance's name to the daemon: `<process id>-<module>-<n>`. A renamed copy
/// of the DLL loaded into the same process (BonDriverProxyEx opening two) is a module of
/// its own with its own count, so the module is part of it: where this module's counter
/// is (each module has its own).
fn client_id() -> String {
    static INSTANCE: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{:x}-{}",
        std::process::id(),
        std::ptr::addr_of!(INSTANCE) as usize,
        INSTANCE.fetch_add(1, Ordering::Relaxed) + 1
    )
}

/// A second between the supervisor's rounds, cut short by the close.
fn nap(inner: &Inner) {
    for _ in 0..10 {
        if inner.closing.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// This DLL instance's connection to `dt295 serve`.
#[derive(Debug)]
pub struct Client {
    inner: Arc<Inner>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

impl Client {
    /// Make sure `dt295 serve` answers at the DLL's folder (starting it if none does),
    /// and start the supervisor. `sink` makes a sink into the TS FIFO.
    pub fn open(
        settings: Settings,
        sink: Box<dyn Fn() -> TsSink + Send + Sync>,
    ) -> Result<Self, String> {
        let target = Target::of_folder(&settings.dir);
        let launch = launch_spec(&settings, &target);
        let inner = Arc::new(Inner {
            target,
            client: client_id(),
            settings,
            launch,
            sink,
            want: Mutex::new(None),
            stream: Mutex::new(None),
            unit: Mutex::new(None),
            ops: Mutex::new(()),
            epoch: AtomicU64::new(0),
            cn_mdb: AtomicI32::new(NO_CN),
            lost: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        });
        inner.ensure_server()?;
        info!(
            "using dt295 serve at {} as client {}",
            inner.target, inner.client
        );
        let supervisor = {
            let inner = Arc::clone(&inner);
            thread::Builder::new()
                .name("BonDriver_DT295 supervisor".into())
                .spawn(move || supervise(&inner))
                .map_err(|e| warn!("cannot start the supervisor: {e}"))
                .ok()
        };
        Ok(Self {
            inner,
            supervisor: Mutex::new(supervisor),
        })
    }

    /// Ask the daemon for `want`'s stream (starting the daemon again first when it has
    /// gone away). Returns the C/N of the tune (0.001 dB) when the daemon gives it.
    ///
    /// The request goes out while the running stream still runs: the daemon replaces
    /// this DLL's stream with it on the same tuner, so no other TVTest takes the tuner in
    /// between. Once the daemon answered, the old stream's reader is gone, and `purge`
    /// empties what it left in the FIFO before the new stream's TS comes.
    ///
    /// A request refused for now (no unit, another TVTest on the tuner), or one the
    /// daemon did not answer, fails at once, and the supervisor keeps asking for the
    /// channel; one refused for good (no lock) is not asked for again.
    pub fn set_channel(&self, want: Want, purge: impl FnOnce()) -> Result<Option<i32>, String> {
        let inner = &self.inner;
        let _ops = lock(&inner.ops);
        inner.epoch.fetch_add(1, Ordering::AcqRel);
        *lock(&inner.want) = Some(want.clone());
        inner.quiet_stream();
        inner.lost.store(false, Ordering::Release);
        let asked = inner
            .ensure_server()
            .map_err(StartError::Retry)
            .and_then(|()| inner.request(&want));
        inner.stop_stream();
        purge();
        let started = asked.and_then(|a| inner.attach(a, &want));
        started.map_err(|e| {
            if matches!(e, StartError::Retry(_)) {
                inner.lost.store(true, Ordering::Release);
            }
            e.into_string()
        })
    }

    /// End the stream and forget the channel (nothing is asked for again).
    pub fn stop(&self) {
        let inner = &self.inner;
        let _ops = lock(&inner.ops);
        inner.epoch.fetch_add(1, Ordering::AcqRel);
        *lock(&inner.want) = None;
        inner.stop_stream();
        inner.lost.store(false, Ordering::Release);
    }

    /// The stream's C/N in dB, as last read (never waits); 0 without a stream.
    pub fn signal_level(&self) -> f32 {
        match self.inner.cn_mdb.load(Ordering::Relaxed) {
            NO_CN => 0.0,
            cn => cn as f32 / 1000.0,
        }
    }

    /// End the stream and the supervisor; the daemon ends the stream (and itself when it
    /// has no client left).
    pub fn close(&self) {
        let inner = &self.inner;
        inner.closing.store(true, Ordering::Release);
        {
            let _ops = lock(&inner.ops);
            *lock(&inner.want) = None;
            inner.stop_stream();
        }
        let supervisor = lock(&self.supervisor).take();
        if let Some(h) = supervisor {
            let _ = h.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn settings(dir: &Path) -> Settings {
        Settings {
            dir: dir.to_owned(),
            serial: None,
            tuner: 1,
            mode: Mode::System1,
            idle_exit_secs: 5,
            start_timeout_secs: 5,
            verbose: 0,
        }
    }

    /// A folder standing in for the DLL's, removed afterwards by the caller.
    fn dll_folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dt295-client-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn args_of(l: &Launch) -> Vec<String> {
        l.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_daemon_is_the_folders() {
        let dir = Path::new(r"C:\TVTest");
        let socket = ipc::folder_socket(dir);
        let l = launch_spec(&settings(dir), &Target::Unix(socket.clone()));
        assert_eq!(l.exe, dir.join(SERVER_EXE));
        assert_eq!(l.log, dir.join(SERVER_LOG));
        let args = args_of(&l);
        assert_eq!(args[0], "serve");
        let has = |k: &str, v: &str| args.windows(2).any(|w| w[0] == k && w[1] == v);
        assert!(has("--socket", &socket.to_string_lossy()), "{args:?}");
        assert!(has("--idle-exit", "5"), "{args:?}");
        assert!(has("--firmware-dir", r"C:\TVTest"), "{args:?}");
        assert!(has("--state-dir", r"C:\TVTest"), "{args:?}");
        assert!(!args.iter().any(|a| a.starts_with("-v")), "{args:?}");
        // the unit and the mode go into the requests, not the daemon's command line
        assert!(
            !args.iter().any(|a| a == "--serial" || a.contains("raw")),
            "{args:?}"
        );
        let l = launch_spec(
            &Settings {
                verbose: 2,
                ..settings(dir)
            },
            &Target::Tcp("127.0.0.1:41234".into()),
        );
        let args = args_of(&l);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--listen" && w[1] == "127.0.0.1:41234")
        );
        assert!(args.iter().any(|a| a == "-vv"));
    }

    #[test]
    fn recovery_timing() {
        let p = Duration::from_secs(2);
        assert!(
            !recovery_due(false, true, None, p),
            "stream fine, daemon there"
        );
        assert!(recovery_due(true, true, None, p), "stream lost");
        assert!(recovery_due(false, false, None, p), "daemon gone");
        assert!(
            !recovery_due(true, true, Some(Instant::now()), p),
            "too soon"
        );
        assert!(recovery_due(
            true,
            true,
            Some(Instant::now() - Duration::from_secs(3)),
            p
        ));
        assert_eq!(next_pause(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(next_pause(Duration::from_secs(20)), RETRY_MAX);
        let retried = |why: &str| matches!(StartError::of_reply(why.into()), StartError::Retry(_));
        assert!(retried("unit AB12345678 is not connected"));
        assert!(retried("no PIX-DT295(W) / PIX-DT195(W) is connected"));
        assert!(!retried("no lock (C/N 3.2 dB)"));
        assert!(!retried(
            "unit AB12345678 streams raw-TS (the path it was opened on), not System1"
        ));
        // a daemon that ends closes the request; one that is stuck lets it time out
        let closed = std::io::Error::from(ErrorKind::ConnectionReset);
        assert!(matches!(no_answer(&closed), NoAnswer::Closed(_)));
        let slow = std::io::Error::from(ErrorKind::TimedOut);
        assert!(matches!(no_answer(&slow), NoAnswer::Failed(_)));
    }

    #[test]
    fn packets_only() {
        use std::sync::Mutex;
        let got = Arc::new(Mutex::new(Vec::new()));
        let mut sink: TsSink = {
            let got = Arc::clone(&got);
            Box::new(move |ts: &[u8]| {
                assert_eq!(ts.len() % PACKET, 0);
                got.lock().unwrap().extend_from_slice(ts);
            })
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            c.write_all(b"ok\n").unwrap();
            let data: Vec<u8> = (0..PACKET * 3 + 50).map(|i| i as u8).collect();
            for piece in data.chunks(100) {
                c.write_all(piece).unwrap();
            }
        });
        let sock = std::net::TcpStream::connect(addr).unwrap();
        writer.join().unwrap();
        let mut ts = BufReader::new(sock);
        let mut line = String::new();
        ts.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
        let bytes = read_ts(ts, &mut sink);
        assert_eq!(bytes, PACKET as u64 * 3 + 50);
        let got = got.lock().unwrap();
        assert_eq!(got.len(), PACKET * 3, "the partial packet stays out");
        assert_eq!(got[0], 0, "the TS read with the reply line comes first");
        assert_eq!(got[PACKET * 3 - 1], (PACKET * 3 - 1) as u8);
    }

    #[test]
    fn no_daemon_fails_at_once() {
        let dir = dll_folder("noexe");
        let t0 = Instant::now();
        let err = Client::open(settings(&dir), Box::new(|| Box::new(|_: &[u8]| {})))
            .expect_err("no dt295.exe");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        assert!(err.contains("cannot start"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in dt295.exe that exits at once (cmd.exe given arguments it does not run):
    /// the open fails well within the start timeout, by its exit.
    #[cfg(windows)]
    #[test]
    fn a_daemon_that_ends_at_once_is_not_waited_for() {
        let dir = dll_folder("ends");
        let system32 =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()))
                .join("System32");
        std::fs::copy(system32.join("cmd.exe"), dir.join(SERVER_EXE)).expect("copy cmd.exe");
        let t0 = Instant::now();
        let err = Client::open(
            Settings {
                start_timeout_secs: 20,
                ..settings(&dir)
            },
            Box::new(|| Box::new(|_: &[u8]| {})),
        )
        .expect_err("the daemon ended");
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert!(err.contains("ended at once"), "{err}");
        assert!(dir.join(SERVER_LOG).exists(), "its output went to the log");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_ids_differ() {
        let (a, b) = (client_id(), client_id());
        assert_ne!(a, b);
        assert!(a.starts_with(&format!("{}-", std::process::id())), "{a}");
        assert!(!a.contains(char::is_whitespace), "{a}");
    }
}
