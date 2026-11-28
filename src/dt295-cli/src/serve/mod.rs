//! `dt295 serve`: hold units open and stream to clients over a Unix domain socket, on
//! Linux and on Windows alike: `dt295 tune` (a recorder runs it as its tuner command)
//! and the BonDriver, which always watches through it (it starts one in its folder when
//! none answers, with `--idle-exit`, so the daemon ends a few seconds after the last
//! TVTest). Where it listens: [`dt295::ipc`].
//!
//! Every `dt295 capture` booted the device, authenticated a new secure session and
//! was then killed by the recorder. That was the root of most trouble on Linux: a
//! previous session timing out under the new one, keepalives that never went out,
//! streams left running with their TS queued, link keys to carry between processes.
//! Here one thread per unit owns the device; a client only asks for a stream and reads
//! it, or asks for a tuner's C/N (`status`, answered here without the unit's thread). A
//! two-tuner model's second tuner has a thread of its own, so its streams run beside the first
//! tuner's ([`tuner2`]).
//!
//! The units are those of the configuration ([`conf`]), opened at the start, or without
//! any there, each unit a request asks for (or the only one connected), opened then on
//! the path that request asks for.

pub mod conf;
/// The protocol, shared with the BonDriver (`dt295-core`).
pub use dt295::serve as proto;
mod raw;
mod tuner2;
mod unit;

pub use daemon::{ServeArgs, run};

mod daemon {
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    use std::net::Shutdown;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use clap::Args;
    use tracing::{debug, info, warn};

    use dt295::ipc::{self, Conn};

    use super::conf::{self, Conf};
    use super::proto::{Asked, MAX_LINE, Mode, Reply};
    use super::tuner2::{self, Slot};
    use super::unit::{self, Info, Job, Spec};
    use crate::tuner::{install_ctrl_c, stopping};

    /// How long a client may take to send its request line.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    /// How many connections' request lines are read at once. A client sends its line as
    /// it connects, so only connections that send nothing pile up; one more is closed.
    const READERS_MAX: usize = 32;
    /// A client that reads nothing for this long counts as gone.
    const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(Debug, Args)]
    pub struct ServeArgs {
        /// The configuration file (default: /etc/dt295.conf on Linux when it exists; on
        /// Windows none).
        #[arg(short, long, value_name = "FILE")]
        pub config: Option<PathBuf>,

        /// Listen on this socket file (default: $DT295_SOCKET, else the configuration's,
        /// else /run/dt295/dt295.sock on Linux, %TEMP%\dt295-<hash of this executable's
        /// folder>.sock on Windows).
        #[arg(long, value_name = "PATH")]
        pub socket: Option<PathBuf>,

        /// Listen on this TCP address instead (e.g. 127.0.0.1:41200; $DT295_LISTEN).
        #[arg(long, value_name = "ADDR", conflicts_with = "socket")]
        pub listen: Option<String>,

        /// The folder of the vendor's firmware images, instead of the configuration's.
        #[arg(long, value_name = "DIR")]
        pub firmware_dir: Option<PathBuf>,

        /// The folder of the units' state files (<serial>.state), instead of the
        /// configuration's.
        #[arg(long, value_name = "DIR")]
        pub state_dir: Option<PathBuf>,

        /// End when no client has been there for this many seconds (0 = run until
        /// stopped).
        #[arg(long, value_name = "SECONDS", default_value_t = 0)]
        pub idle_exit: u64,
    }

    pub fn run(a: &ServeArgs) -> Result<()> {
        let mut conf = Conf::find(a.config.as_deref())?;
        if let Some(dir) = &a.firmware_dir {
            conf.firmware_dir.clone_from(dir);
        }
        if let Some(dir) = &a.state_dir {
            conf.state_dir.clone_from(dir);
        }
        let conf = Arc::new(conf);
        install_ctrl_c();
        let target = conf::server_target(&conf, a.socket.as_deref(), a.listen.as_deref());
        let listener =
            ipc::listen(&target).with_context(|| format!("cannot listen on {target}"))?;
        let mut units = Units::new(Arc::clone(&conf));
        for u in &conf.units {
            let mode = if u.raw_ts { Mode::RawTs } else { Mode::System1 };
            units
                .spawn(&u.serial, mode)
                .with_context(|| format!("cannot start the thread of unit {}", u.serial))?;
        }
        let idle_exit = (a.idle_exit > 0).then(|| Duration::from_secs(a.idle_exit));
        info!(
            "dt295 {}: listening on {target}; units: {}{}; firmware {}, state {}",
            env!("DT295_VERSION"),
            if conf.units.is_empty() {
                "any, as asked".to_owned()
            } else {
                units.serials().join(", ")
            },
            idle_exit
                .map_or_default(|d| format!("; ends after {} s without a client", d.as_secs())),
            conf.firmware_dir.display(),
            conf.state_dir.display(),
        );
        // each connection's request line is read on a thread of its own, so a client that
        // sends nothing holds up nobody else
        let (read_tx, read) = mpsc::channel();
        let mut readers = Readers::default();
        let mut last_client = Instant::now();
        while !stopping() {
            loop {
                match ipc::accept(&listener) {
                    Ok(conn) => {
                        readers.read(conn, read_tx.clone());
                        last_client = Instant::now();
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        warn!("accept: {e}");
                        break;
                    }
                }
            }
            match read.recv_timeout(Duration::from_millis(100)) {
                Ok(incoming) => {
                    dispatch(incoming, &mut units);
                    last_client = Instant::now();
                }
                Err(_) => {
                    if let Some(idle) = idle_exit {
                        if units.busy() {
                            last_client = Instant::now();
                        } else if last_client.elapsed() >= idle {
                            info!("no client for {} s", idle.as_secs());
                            break;
                        }
                    }
                }
            }
        }
        info!("stopping");
        // nothing listens from here on: a client that comes while the units close starts
        // another daemon, instead of waiting on this one for an answer that never comes
        ipc::unlisten(listener);
        units.close();
        Ok(())
    }

    /// A connection and its request line, as read.
    struct Incoming {
        conn: Conn,
        line: String,
        read: std::io::Result<usize>,
    }

    /// The threads that read request lines, at most [`READERS_MAX`].
    #[derive(Default)]
    struct Readers {
        running: Arc<AtomicUsize>,
        /// Whether the connections closed for being too many were logged (once until
        /// there are fewer again).
        full_told: bool,
    }

    /// One reader thread running, until it is dropped.
    struct Reader(Arc<AtomicUsize>);

    impl Drop for Reader {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    impl Readers {
        /// Read the request line of `conn` on a thread of its own, and pass it on; or
        /// answer it `busy` at once when [`READERS_MAX`] read already.
        fn read(&mut self, conn: Conn, to: Sender<Incoming>) {
            if self.running.load(Ordering::Acquire) >= READERS_MAX {
                if !self.full_told {
                    warn!("{READERS_MAX} connections sent no request yet; answering more busy");
                    self.full_told = true;
                }
                refuse(&conn);
                return;
            }
            self.full_told = false;
            self.running.fetch_add(1, Ordering::AcqRel);
            let reader = Reader(Arc::clone(&self.running));
            let spawned = thread::Builder::new()
                .name("dt295 serve request".into())
                .spawn(move || {
                    let _reader = reader;
                    let _ = conn.set_nonblocking(false);
                    let _ = conn.set_read_timeout(Some(REQUEST_TIMEOUT));
                    let mut line = String::new();
                    let read = BufReader::new((&conn).take(MAX_LINE)).read_line(&mut line);
                    let _ = to.send(Incoming { conn, line, read });
                });
            if let Err(e) = spawned {
                warn!("cannot read a request: {e}");
            }
        }
    }

    /// Answer `conn`, whose request is not read, `busy`, and close it: a client takes that
    /// for a daemon that is there (the BonDriver keeps its stream), where a close without
    /// an answer would be one that is ending.
    fn refuse(conn: &Conn) {
        let busy = Reply::Busy(format!("{READERS_MAX} other requests are being read"));
        let _ = conn.set_write_timeout(Some(Duration::from_millis(100)));
        let _ = (&*conn).write_all(format!("{}\n", busy.to_line()).as_bytes());
        let _ = conn.shutdown(Shutdown::Write);
        // what the client sent already is read: closed with it unread, the socket would
        // be reset, and the answer might never be read
        let _ = conn.set_nonblocking(true);
        let mut sent = [0u8; 512];
        while matches!((&*conn).read(&mut sent), Ok(n) if n > 0) {}
    }

    /// A unit's threads (the first tuner's, which keeps the unit, and a two-tuner model's
    /// second tuner's): where their requests go and what they tell about themselves.
    struct Handle {
        serial: String,
        mode: Mode,
        jobs: Sender<Job>,
        thread: JoinHandle<()>,
        info: Arc<Info>,
        jobs2: Sender<Job>,
        thread2: JoinHandle<()>,
        info2: Arc<Info>,
        /// Where the unit's thread lends the second tuner (to a second tuner's thread
        /// started again, too).
        slot: Arc<Slot>,
    }

    impl Handle {
        /// Where the requests for tuner `n` go, and what it tells (`None`: its thread
        /// ended).
        fn tuner(&self, n: u8) -> Option<(&Sender<Job>, &Info)> {
            let (jobs, thread, info) = match n {
                2 => (&self.jobs2, &self.thread2, &self.info2),
                _ => (&self.jobs, &self.thread, &self.info),
            };
            (!thread.is_finished()).then_some((jobs, &**info))
        }
    }

    /// The units being served.
    struct Units {
        conf: Arc<Conf>,
        list: Vec<Handle>,
    }

    impl Units {
        fn new(conf: Arc<Conf>) -> Self {
            Self {
                conf,
                list: Vec::new(),
            }
        }

        fn serials(&self) -> Vec<&str> {
            self.list.iter().map(|u| u.serial.as_str()).collect()
        }

        /// Whether a unit serves a request (a thread that ended serves none).
        fn busy(&self) -> bool {
            self.list.iter().any(|u| {
                (!u.thread.is_finished() && u.info.busy())
                    || (!u.thread2.is_finished() && u.info2.busy())
            })
        }

        fn find(&self, serial: &str) -> Option<&Handle> {
            self.list
                .iter()
                .find(|u| u.serial.eq_ignore_ascii_case(serial))
        }

        fn spawn(&mut self, serial: &str, mode: Mode) -> std::io::Result<&Handle> {
            let handle = self.start(serial, mode)?;
            self.list.push(handle);
            Ok(&self.list[self.list.len() - 1])
        }

        /// Start the threads of unit `serial`, on `mode`.
        fn start(&self, serial: &str, mode: Mode) -> std::io::Result<Handle> {
            let slot = Arc::new(Slot::default());
            let spec = Spec {
                serial: serial.to_owned(),
                mode,
                fixed: !self.conf.units.is_empty(),
                slot: Arc::clone(&slot),
            };
            let info = Arc::new(Info::default());
            let (jobs, thread) = unit::spawn(spec, Arc::clone(&self.conf), Arc::clone(&info))?;
            // a two-tuner model's second tuner streams beside the first (on either path)
            let info2 = Arc::new(Info::default());
            let (jobs2, thread2) =
                tuner2::spawn(serial.to_owned(), Arc::clone(&slot), Arc::clone(&info2))?;
            Ok(Handle {
                serial: serial.to_owned(),
                mode,
                jobs,
                thread,
                info,
                jobs2,
                thread2,
                info2,
                slot,
            })
        }

        /// Start the second tuner's thread of the unit at `i` again (the one before
        /// ended); it borrows the tuner from the same unit's thread.
        fn start_tuner2(&mut self, i: usize) -> Result<(), String> {
            let serial = self.list[i].serial.clone();
            let slot = Arc::clone(&self.list[i].slot);
            let info2 = Arc::new(Info::default());
            let (jobs2, thread2) = tuner2::spawn(serial.clone(), slot, Arc::clone(&info2))
                .map_err(|e| {
                    format!("cannot start the thread of unit {serial} tuner 2 again: {e}")
                })?;
            let u = &mut self.list[i];
            u.jobs2 = jobs2;
            u.info2 = info2;
            let gone = std::mem::replace(&mut u.thread2, thread2);
            let how = if gone.join().is_err() {
                "panicked"
            } else {
                "ended"
            };
            warn!("unit {serial}: the thread of tuner 2 {how}; started again");
            Ok(())
        }

        /// The serial number of the unit a request names, or of the only one there is
        /// (configured, or else served or connected).
        fn resolve(&self, unit: &str) -> Result<String, String> {
            if !self.conf.units.is_empty() {
                return match (unit, self.list.as_slice()) {
                    ("", [only]) => Ok(only.serial.clone()),
                    ("", _) => Err(format!(
                        "several units are configured ({}); name one",
                        self.serials().join(", ")
                    )),
                    (unit, _) => self
                        .find(unit)
                        .map(|u| u.serial.clone())
                        .ok_or_else(|| format!("no [unit {unit}] in the configuration")),
                };
            }
            let served: Vec<String> = self.list.iter().map(|u| u.serial.clone()).collect();
            pick(unit, &served, dt295::present_serials().ok())
        }

        /// Where the requests for `serial` go: its threads, started now (on `mode`) if it
        /// has none yet. A thread that ended while the daemon runs (a bug that panicked)
        /// is started again: the unit's (the first tuner's) closed the unit as it unwound,
        /// so it starts on the request's mode, or a configured unit's, and the second
        /// tuner's thread with it (the one before ends on its own, its requests' channel
        /// gone); the second tuner's alone, for a request of `tuner` 2. Until one is
        /// started, the one that ended stays.
        fn route(&mut self, serial: &str, mode: Mode, tuner: u8) -> Result<&Handle, String> {
            if let Some(i) = self
                .list
                .iter()
                .position(|u| u.serial.eq_ignore_ascii_case(serial))
            {
                if stopping() {
                    return Ok(&self.list[i]);
                }
                if !self.list[i].thread.is_finished() {
                    if tuner == 2 && self.list[i].thread2.is_finished() {
                        self.start_tuner2(i)?;
                    }
                    return Ok(&self.list[i]);
                }
                let serial = self.list[i].serial.clone();
                let mode = if self.conf.units.is_empty() {
                    mode
                } else {
                    self.list[i].mode
                };
                let again = self
                    .start(&serial, mode)
                    .map_err(|e| format!("cannot start the thread of unit {serial} again: {e}"))?;
                let gone = std::mem::replace(&mut self.list[i], again);
                let how = if gone.thread.join().is_err() {
                    "panicked"
                } else {
                    "ended"
                };
                warn!(
                    "unit {serial}: its thread {how}; started again ({})",
                    mode.word()
                );
                return Ok(&self.list[i]);
            }
            info!("unit {serial}: serving it ({})", mode.word());
            self.spawn(serial, mode)
                .map_err(|e| format!("cannot start the thread of unit {serial}: {e}"))
        }

        /// End the threads (each ends once it is not streaming).
        fn close(self) {
            let threads: Vec<_> = self
                .list
                .into_iter()
                .flat_map(|u| [u.thread, u.thread2])
                .collect();
            for thread in threads {
                let _ = thread.join();
            }
        }
    }

    /// Without a configuration, the serial number of the unit a request names (`unit`),
    /// or of the only one connected now (`present`; a unit served before and unplugged
    /// since does not count). A unit named that is served is taken connected or not (its
    /// thread waits for it). When the units cannot be listed, those served stand in.
    fn pick(unit: &str, served: &[String], present: Option<Vec<String>>) -> Result<String, String> {
        let same = |a: &String| a.eq_ignore_ascii_case(unit);
        if !unit.is_empty()
            && let Some(s) = served.iter().find(|s| same(s))
        {
            return Ok(s.clone());
        }
        let there = present.unwrap_or_else(|| served.to_vec());
        match (unit, there.as_slice()) {
            ("", [only]) => Ok(only.clone()),
            ("", []) => Err("no PIX-DT295(W) / PIX-DT195(W) is connected".into()),
            ("", _) => Err(format!(
                "several units are connected ({}); name one",
                there.join(", ")
            )),
            (unit, _) => there
                .into_iter()
                .find(same)
                .ok_or_else(|| format!("unit {unit} is not connected")),
        }
    }

    /// Act on a client's request: answer a status, or hand a tune to its unit's thread.
    fn dispatch(incoming: Incoming, units: &mut Units) {
        let Incoming {
            mut conn,
            line,
            read,
        } = incoming;
        let _ = conn.set_write_timeout(Some(WRITE_TIMEOUT));
        if matches!(read, Ok(0)) {
            return; // connected and closed without a request: a client checking it is up
        }
        let asked = read
            .map_err(|e| format!("cannot read the request: {e}"))
            .and_then(|_| Asked::parse(&line))
            .and_then(|a| units.resolve(a.unit()).map(|serial| (a, serial)));
        let why = match asked {
            Ok((Asked::Status(s), serial)) => {
                let cn_mdb = units
                    .find(&serial)
                    .and_then(|u| u.tuner(s.tuner))
                    .and_then(|(_, info)| info.cn_mdb());
                let ok = Reply::Ok {
                    unit: Some(serial),
                    cn_mdb,
                };
                let _ = conn.write_all(format!("{}\n", ok.to_line()).as_bytes());
                return;
            }
            Ok((Asked::Tune(req), serial)) => {
                match units.route(&serial, req.mode.unwrap_or(Mode::System1), req.tuner) {
                    Ok(unit) => {
                        let _ = conn.set_read_timeout(None);
                        // a thread that ended is not started again only as the daemon stops
                        if let Some((jobs, _)) = unit.tuner(req.tuner) {
                            match jobs.send(Job { req, conn }) {
                                Ok(()) => return,
                                Err(e) => conn = e.0.conn,
                            }
                        }
                        "the daemon is stopping".to_owned()
                    }
                    Err(why) => why,
                }
            }
            Err(why) => why,
        };
        // a BonDriver asks for the status once a second, also while no unit is there
        if line.starts_with("status") {
            debug!("request {:?}: {why}", line.trim_end());
        } else {
            warn!("request {:?}: {why}", line.trim_end());
        }
        let _ = conn.write_all(format!("{}\n", Reply::Error(why).to_line()).as_bytes());
    }

    #[cfg(test)]
    mod tests {
        use super::{READERS_MAX, Readers, pick};
        use dt295::ipc::{self, Target};
        use std::io::{ErrorKind, Read, Write};
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;
        use std::thread::sleep;
        use std::time::Duration;

        #[test]
        fn connections_beyond_the_readers_are_closed() {
            let path = std::env::temp_dir()
                .join(format!("dt295-readers-test-{}.sock", std::process::id()));
            let target = Target::Unix(path.clone());
            let listener = ipc::listen(&target).expect("listen");
            let (tx, rx) = mpsc::channel();
            let mut readers = Readers::default();
            let mut clients = Vec::new();
            for n in 0..=READERS_MAX {
                let mut client = ipc::connect(&target).expect("connect");
                if n == READERS_MAX {
                    // its request comes, but is not read
                    client.write_all(b"status\n").unwrap();
                }
                clients.push(client);
                let conn = loop {
                    match ipc::accept(&listener) {
                        Ok(c) => break c,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                readers.read(conn, tx.clone());
            }
            // one too many: answered busy at once, and closed
            let mut last = clients.pop().unwrap();
            last.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut answer = String::new();
            last.read_to_string(&mut answer).expect("the answer");
            assert!(answer.starts_with("busy "), "{answer:?}");
            // a client that sends its line frees its reader
            clients[0].write_all(b"status\n").unwrap();
            let got = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the request");
            assert_eq!(got.line, "status\n");
            for _ in 0..100 {
                if readers.running.load(Ordering::Acquire) < READERS_MAX {
                    break;
                }
                sleep(Duration::from_millis(10));
            }
            // fewer (all of them, were the test slower than the requests' timeout)
            assert!(readers.running.load(Ordering::Acquire) < READERS_MAX);
            drop(clients);
            ipc::unlisten(listener);
            let mut lock = path.into_os_string();
            lock.push(".lock");
            let _ = std::fs::remove_file(lock);
        }

        fn list(s: &[&str]) -> Vec<String> {
            s.iter().map(|&s| s.to_owned()).collect()
        }

        #[test]
        fn the_unit_connected_now() {
            // A was served and unplugged, B is plugged in: B, not "several"
            assert_eq!(pick("", &list(&["A"]), Some(list(&["B"]))), Ok("B".into()));
            assert_eq!(pick("", &list(&["A"]), Some(list(&["A"]))), Ok("A".into()));
            assert!(pick("", &[], Some(list(&["A", "B"]))).is_err_and(|e| e.contains("several")));
            assert_eq!(
                pick("", &list(&["A"]), Some(vec![])),
                Err("no PIX-DT295(W) / PIX-DT195(W) is connected".into())
            );
            // the units cannot be listed: those served
            assert_eq!(pick("", &list(&["A"]), None), Ok("A".into()));
            // a unit named: served (unplugged or not), or connected
            assert_eq!(pick("a", &list(&["A"]), Some(vec![])), Ok("A".into()));
            assert_eq!(pick("b", &list(&["A"]), Some(list(&["B"]))), Ok("B".into()));
            assert!(
                pick("C", &list(&["A"]), Some(list(&["B"])))
                    .is_err_and(|e| e.contains("not connected"))
            );
        }
    }
}
