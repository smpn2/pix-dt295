//! `dt295 tune`: ask `dt295 serve` for a stream and write its TS to stdout -- a
//! recorder's tuner command (Mirakurun / mirakc). It owns no device: when the recorder
//! kills it, the socket closes and the daemon stops the stream properly.

use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args};

use dt295::chan::NO_TSID;
use dt295::ipc::{self, Conn};

use crate::serve::conf::client_target;
use crate::serve::proto::{Reply, Request, Tuning};
use crate::{parse_hex_u16, parse_u16, parse_u32};

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("band").required(true).args(["ter", "sat"])))]
pub struct TuneArgs {
    /// The unit, by USB serial number (`--unit` is the same); default: the only one
    /// (configured, or connected).
    #[arg(long, alias = "unit", value_name = "SERIAL")]
    pub serial: Option<String>,

    /// The tuner of the unit: 1, or 2 on a two-tuner model (its streams run beside tuner 1's).
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=2))]
    pub tuner: u8,

    /// ISDB-T: the channel centre frequency in Hz (e.g. 557142857 for UHF 27ch).
    #[arg(long, value_name = "HZ", value_parser = parse_u32)]
    pub ter: Option<u32>,

    /// ISDB-S: the IF frequency in Hz (satellite RF - 10678 MHz, e.g. 1318000000).
    #[arg(long, value_name = "HZ", value_parser = parse_u32)]
    pub sat: Option<u32>,

    /// ISDB-S: the broadcast transport_stream_id in hex (e.g. 40f1 or 0x40F1); 7 or
    /// less is a TS number, the TS whose TSID has those low 3 bits; default: the
    /// frequency's first TS.
    #[arg(long, value_name = "HEX", value_parser = parse_hex_u16, requires = "sat")]
    pub tsid: Option<u16>,

    /// The service to stream (decimal or 0x hex); default: every service of the TS
    /// (mirakc filters the services itself).
    #[arg(long, value_name = "ID", value_parser = parse_u16)]
    pub sid: Option<u16>,

    /// Only if the tuner is free: fail instead of taking it over from a running stream
    /// (for background jobs such as `dt295 logo`; a recorder's request still takes the
    /// tuner over from this one).
    #[arg(long)]
    pub if_free: bool,

    /// The configuration of `dt295 serve`, for its socket (default /etc/dt295.conf on
    /// Linux if it exists).
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// The socket file of `dt295 serve` (default: $DT295_SOCKET, else the
    /// configuration's, else /run/dt295/dt295.sock on Linux, %TEMP%\dt295-<hash of this
    /// executable's folder>.sock on Windows).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// The TCP address of `dt295 serve` instead (e.g. 127.0.0.1:41200; $DT295_LISTEN).
    #[arg(long, value_name = "ADDR", conflicts_with = "socket")]
    pub connect: Option<String>,
}

pub fn run(a: &TuneArgs) -> Result<()> {
    if io::stdout().is_terminal() {
        bail!("refusing to write a TS to the terminal: redirect stdout");
    }
    let tuning = match (a.ter, a.sat, a.tsid) {
        (Some(hz), None, None) => Tuning::Terrestrial { hz },
        (None, Some(hz), tsid) => Tuning::Satellite {
            hz,
            tsid: tsid.unwrap_or(NO_TSID),
        },
        _ => bail!("give --ter or --sat"),
    };
    let target = client_target(
        a.config.as_deref(),
        a.socket.as_deref(),
        a.connect.as_deref(),
    )?;
    let req = Request {
        unit: a.serial.clone().unwrap_or_default(),
        tuner: a.tuner,
        tuning,
        sid: a.sid,
        mode: None,
        client: None,
        if_free: a.if_free,
    };
    let mut conn = ipc::connect(&target)
        .with_context(|| format!("cannot connect to dt295 serve at {target} (is it running?)"))?;
    conn.write_all(format!("{}\n", req.to_line()).as_bytes())
        .context("cannot send the request")?;
    let mut ts = BufReader::with_capacity(1 << 16, conn);
    let mut line = String::new();
    ts.read_line(&mut line)
        .context("no reply from dt295 serve")?;
    if line.is_empty() {
        bail!("dt295 serve closed the connection");
    }
    if let Reply::Busy(why) | Reply::Error(why) = Reply::parse(&line) {
        bail!("{why}");
    }
    match pump(ts) {
        // the reader is gone (a recorder stopping a recording): the normal end
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e).context("passing the TS on"),
        _ => Ok(()),
    }
}

/// Copy the TS to stdout until either end goes away. A recorder that gives up on a
/// silent stream only closes its end of the pipe (mirakc after its 30 s limit), and
/// the daemon sends nothing until the service's PAT, so waiting for a write to fail
/// could take for ever (45 min for a station off the air): poll sees the closed pipe
/// at once. Written unbuffered: stdout's own buffer flushes at every 0x0A.
#[cfg(unix)]
fn pump(ts: BufReader<Conn>) -> io::Result<()> {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsFd, AsRawFd};

    let mut out = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    out.write_all(ts.buffer())?;
    let sock = ts.into_inner();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            // no events asked for: POLLERR / POLLHUP come anyway when the reader goes
            libc::pollfd {
                fd: out.as_raw_fd(),
                events: 0,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfd entries for open descriptors.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fds[1].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return Ok(());
        }
        if fds[0].revents != 0 {
            let n = match (&sock).read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if n == 0 {
                return Ok(()); // the daemon ended the stream
            }
            out.write_all(&buf[..n])?;
        }
    }
}

/// Copy the TS to stdout until either end goes away. As on Unix, a reader that goes
/// while the daemon sends nothing is not left waiting for a write to fail: while the
/// socket is quiet, a write of no bytes to the pipe (which the reader of a byte pipe
/// never sees) fails once it is gone.
#[cfg(windows)]
fn pump(ts: BufReader<Conn>) -> io::Result<()> {
    use std::io::Read;

    let mut out = io::stdout().lock();
    out.write_all(ts.buffer())?;
    out.flush()?;
    let mut sock = ts.into_inner();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if !readable(&sock, 1)? {
            if stdout_reader_gone() {
                return Ok(());
            }
            continue;
        }
        let n = match sock.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(()); // the daemon ended the stream
        }
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
}

/// Whether `sock` has something to read (or its end) within `secs` seconds.
#[cfg(windows)]
fn readable(sock: &Conn, secs: i32) -> io::Result<bool> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        FD_SET, SOCKET, TIMEVAL, WSAGetLastError, select,
    };

    let mut set = FD_SET {
        fd_count: 1,
        fd_array: [0; 64],
    };
    set.fd_array[0] = sock.as_raw_socket() as SOCKET;
    let wait = TIMEVAL {
        tv_sec: secs,
        tv_usec: 0,
    };
    // SAFETY: one valid socket in the set, and a timeout.
    let n = unsafe {
        select(
            0,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &wait,
        )
    };
    if n < 0 {
        // SAFETY: no arguments; the error of the call above.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(n > 0)
}

/// Whether the reader of stdout, a pipe, is gone: a write of no bytes fails then
/// (`ERROR_NO_DATA`, `ERROR_BROKEN_PIPE`, `ERROR_PIPE_NOT_CONNECTED` once the server of a
/// named pipe disconnected). Not a pipe, or a message pipe: never (the reader of a
/// message pipe reads the write of no bytes as a message, and may take it for the end).
/// A pipe whose kind cannot be had (the server end of an outbound named pipe) is taken
/// for a byte pipe.
#[cfg(windows)]
fn stdout_reader_gone() -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, GetLastError,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType, WriteFile};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    use windows_sys::Win32::System::Pipes::{GetNamedPipeInfo, PIPE_TYPE_MESSAGE};

    // SAFETY: no arguments but the constant.
    let h = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    // SAFETY: a handle (or an invalid one, which is no pipe).
    if unsafe { GetFileType(h) } != FILE_TYPE_PIPE {
        return false;
    }
    let mut flags = 0u32;
    let none = std::ptr::null_mut();
    // SAFETY: a pipe handle, a place for its flags, and no other places asked for.
    if unsafe { GetNamedPipeInfo(h, &mut flags, none, none, none) } != 0
        && flags & PIPE_TYPE_MESSAGE != 0
    {
        return false;
    }
    let none = [0u8; 1];
    let mut written = 0u32;
    // SAFETY: a valid buffer of which no bytes are written, and a place for the count.
    let ok = unsafe { WriteFile(h, none.as_ptr(), 0, &mut written, std::ptr::null_mut()) };
    // SAFETY: no arguments; the error of the call above.
    ok == 0
        && matches!(
            unsafe { GetLastError() },
            ERROR_NO_DATA | ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED
        )
}
