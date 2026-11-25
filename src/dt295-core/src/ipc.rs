//! Where `dt295 serve` listens and its clients (`dt295 tune`, the scan of
//! `dt295 mirakc-channels`, the BonDriver in server mode) connect: a Unix domain socket,
//! on Linux and on Windows alike (AF_UNIX, Windows 10 1803 or later), or TCP on the
//! loopback. Both carry the same byte stream, so the protocol ([`crate::serve`]) does
//! not care which one it is.
//!
//! On Windows the socket is a file named after a folder, that of `dt295.exe` or of the
//! BonDriver DLL: `dt295-<hash>.sock` in the user's temporary folder (`%TEMP%`). The
//! programs of one folder meet there without being told where, those of two folders stay
//! apart, and nothing is left in the folder itself (which may be one where a socket
//! cannot be made, such as a network drive). Beside the socket file stays
//! `<socket file>.lock`, which a daemon holds while it replaces or removes the socket
//! file. Where a Unix domain socket cannot be used
//! (Windows before 1803, or a temporary folder whose path is too long for a socket
//! address), the folder's loopback port is used instead ([`Target::of_folder`]).

use std::fmt;
use std::fs;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use socket2::{Domain, SockAddr, Socket, Type};
use tracing::warn;

/// A connection between the daemon and a client, either kind.
pub type Conn = Socket;

/// The daemon's listening socket, either kind, and the socket file it made.
#[derive(Debug)]
pub struct Listener {
    sock: Socket,
    /// The Unix domain socket's file, and which file it is: [`unlisten`] removes it
    /// only while it is still this one.
    file: Option<(PathBuf, FileId)>,
}

/// The socket on Linux, whatever the folder.
#[cfg(not(windows))]
pub const DEFAULT_SOCKET: &str = "/run/dt295/dt295.sock";
/// A socket file to use instead of the default (before the configuration's).
pub const ENV_SOCKET: &str = "DT295_SOCKET";
/// A TCP address (`host:port`) to use instead of the default (before the configuration's).
pub const ENV_LISTEN: &str = "DT295_LISTEN";

/// The loopback ports of the TCP fallback: one of a hundred, picked by the folder.
const FALLBACK_PORT_BASE: u16 = 41200;
/// A refused loopback connection can take a second or two on Windows (the SYN is
/// retried); a daemon that is up accepts within milliseconds.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const BACKLOG: i32 = 128;

/// A number for the folder `dir`: FNV-1a of the path, case-folded, without a trailing
/// separator. Other spellings of the same folder (`/` for `\`, a `\\?\` prefix, an 8.3
/// name) give other numbers; the BonDriver and `dt295.exe` take the path of their own
/// file as Windows gives it.
pub fn folder_hash(dir: &Path) -> u64 {
    let path = dir.to_string_lossy().to_lowercase();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.trim_end_matches(['\\', '/']).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The socket file of the folder `dir`: `dt295-<hash>.sock` in the temporary folder.
pub fn folder_socket(dir: &Path) -> PathBuf {
    std::env::temp_dir().join(format!("dt295-{:016x}.sock", folder_hash(dir)))
}

/// The loopback address of the TCP fallback for the folder `dir`. Two folders can get
/// the same port (one in a hundred); only Windows before 1803 uses it.
pub fn folder_address(dir: &Path) -> String {
    let port = FALLBACK_PORT_BASE + (folder_hash(dir) % 100) as u16;
    format!("127.0.0.1:{port}")
}

/// Where the daemon listens and a client connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A Unix domain socket (its file).
    Unix(PathBuf),
    /// TCP, `host:port`.
    Tcp(String),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Unix(p) => write!(f, "{}", p.display()),
            Target::Tcp(a) => write!(f, "tcp {a}"),
        }
    }
}

impl Target {
    /// `$DT295_SOCKET`, else `$DT295_LISTEN`; `None` when neither is set (or empty).
    pub fn from_env() -> Option<Self> {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        if let Some(p) = var(ENV_SOCKET) {
            return Some(Target::Unix(PathBuf::from(p)));
        }
        var(ENV_LISTEN).map(|a| Target::Tcp(a.to_string_lossy().into_owned()))
    }

    /// The default for the programs of the folder `dir`: on Linux [`DEFAULT_SOCKET`]
    /// (whatever the folder); on Windows the folder's socket file, or, where a Unix
    /// domain socket cannot be used there, the folder's loopback port.
    #[cfg_attr(not(windows), allow(unused_variables))]
    pub fn of_folder(dir: &Path) -> Self {
        #[cfg(not(windows))]
        return Target::Unix(PathBuf::from(DEFAULT_SOCKET));
        #[cfg(windows)]
        {
            let path = folder_socket(dir);
            if unix_socket_usable(&path) {
                return Target::Unix(path);
            }
            let t = Target::Tcp(folder_address(dir));
            warn!(
                "a Unix domain socket at {} cannot be used here (Windows before 1803, or \
                 the path is too long for one); using {t} instead",
                path.display()
            );
            t
        }
    }

    /// The default of this program: [`Target::of_folder`] of its executable's folder.
    pub fn of_program() -> Self {
        let exe = std::env::current_exe().ok();
        let dir = exe
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(Path::new(""));
        Self::of_folder(dir)
    }
}

fn unix_socket() -> io::Result<Socket> {
    Socket::new(Domain::UNIX, Type::STREAM, None)
}

/// Whether a Unix domain socket at `path` can be used here: the address family exists
/// (on Windows, 10 1803 or later) and the path fits a socket address.
pub fn unix_socket_usable(path: &Path) -> bool {
    SockAddr::unix(path).is_ok() && unix_socket().is_ok()
}

fn addresses(addr: &str) -> io::Result<Vec<SocketAddr>> {
    let all: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
    if all.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{addr}: no address"),
        ));
    }
    Ok(all)
}

/// Whether the file at `path` (not followed) is a Unix domain socket's, so that no other
/// file is ever removed in its place (a connection to a plain file fails as one to a
/// socket that nobody serves does): on Linux by its type, on Windows by its reparse
/// tag. A symbolic link, a junction or a cloud file is a reparse point too.
#[cfg(unix)]
fn is_socket_file(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::FileTypeExt;
    Ok(fs::symlink_metadata(path)?.file_type().is_socket())
}

#[cfg(windows)]
fn is_socket_file(path: &Path) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FindClose, FindFirstFileW, WIN32_FIND_DATAW,
    };
    /// The reparse tag of an AF_UNIX socket's file.
    const IO_REPARSE_TAG_AF_UNIX: u32 = 0x8000_0023;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: plain data, all zeroes is a valid value.
    let mut found: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a writable WIN32_FIND_DATAW.
    let h = unsafe { FindFirstFileW(wide.as_ptr(), &mut found) };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the search handle FindFirstFileW returned, closed once.
    unsafe { FindClose(h) };
    // dwReserved0 holds the reparse tag of a reparse point
    Ok(found.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && found.dwReserved0 == IO_REPARSE_TAG_AF_UNIX)
}

/// Which file a path names: removed and made again, it is another one.
#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(windows)]
type FileId = (u32, u64);

/// Which file `path` (not followed) is: its device and inode.
#[cfg(unix)]
fn file_id(path: &Path) -> io::Result<FileId> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(path)?;
    Ok((m.dev(), m.ino()))
}

/// Which file `path` (not followed) is: its volume and file index. Not its creation
/// time: a file made again under the same name soon after can take the old one's
/// (NTFS tunnelling).
#[cfg(windows)]
fn file_id(path: &Path) -> io::Result<FileId> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };
    let f = fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: plain data, all zeroes is a valid value.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: an open handle and a writable BY_HANDLE_FILE_INFORMATION.
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Hold the lock of the socket file `path` (`<path>.lock`, left in place) until the
/// file returned is dropped: one daemon at a time replaces the socket file or removes
/// it, so that two starting together do not each remove the other's.
fn lock_socket_file(path: &Path) -> io::Result<fs::File> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock = PathBuf::from(name);
    let f = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
    {
        // made by another user: reading is enough to lock it
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => fs::File::open(&lock)?,
        r => r?,
    };
    f.lock()?;
    Ok(f)
}

/// Listen on `target`, non-blocking. A socket file that a daemon now gone left behind
/// is replaced; one where a daemon answers is an error, so that one daemon serves a
/// target, and so is any other file there. On Linux the socket's group may use it too
/// (0660).
pub fn listen(target: &Target) -> io::Result<Listener> {
    let (l, file) = match target {
        Target::Unix(path) => {
            if let Some(dir) = path.parent()
                && !dir.as_os_str().is_empty()
            {
                fs::create_dir_all(dir)?;
            }
            let addr = SockAddr::unix(path)?;
            // a lock file that cannot be had (another user's, a file system without
            // locks) leaves only the race of daemons starting together
            let _lock = lock_socket_file(path)
                .inspect_err(|e| warn!("{}.lock: {e}; going on without the lock", path.display()))
                .ok();
            // not followed: on Windows the socket file is a reparse point
            if fs::symlink_metadata(path).is_ok() {
                if !is_socket_file(path)? {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    ));
                }
                if unix_socket()?.connect(&addr).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another dt295 serve answers at {}", path.display()),
                    ));
                }
                fs::remove_file(path)?;
            }
            let l = unix_socket()?;
            l.bind(&addr)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;
            }
            // listening before the lock goes: the next daemon finds this one answering
            l.listen(BACKLOG)?;
            let id = file_id(path)?;
            (l, Some((path.clone(), id)))
        }
        Target::Tcp(addr) => {
            let sa = addresses(addr)?[0];
            let l = Socket::new(Domain::for_address(sa), Type::STREAM, None)?;
            // as std's TcpListener: not on Windows, where it would let a second daemon
            // take the port
            #[cfg(unix)]
            l.set_reuse_address(true)?;
            l.bind(&sa.into()).map_err(|e| {
                if e.kind() == io::ErrorKind::AddrInUse {
                    io::Error::new(e.kind(), format!("another program listens at {sa}"))
                } else {
                    e
                }
            })?;
            l.listen(BACKLOG)?;
            (l, None)
        }
    };
    l.set_nonblocking(true)?;
    Ok(Listener { sock: l, file })
}

/// The next connection (blocking), or `WouldBlock`.
pub fn accept(listener: &Listener) -> io::Result<Conn> {
    let (c, peer) = listener.sock.accept()?;
    c.set_nonblocking(false)?;
    if peer.as_socket().is_some() {
        c.set_tcp_nodelay(true)?;
    }
    Ok(c)
}

/// Stop listening: remove the socket file, while it is still the one [`listen`] made
/// (another daemon may have replaced it), then close the socket. A client that comes
/// now finds nothing listening rather than a connection nobody answers.
pub fn unlisten(listener: Listener) {
    if let Some((path, id)) = &listener.file {
        // without the lock, as `listen` went on without it
        let _lock = lock_socket_file(path).ok();
        if file_id(path).is_ok_and(|now| now == *id) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Connect to the daemon at `target` (blocking).
pub fn connect(target: &Target) -> io::Result<Conn> {
    match target {
        Target::Unix(p) => {
            let s = unix_socket()?;
            s.connect(&SockAddr::unix(p)?)?;
            Ok(s)
        }
        Target::Tcp(addr) => {
            let mut last = None;
            for sa in addresses(addr)? {
                let s = Socket::new(Domain::for_address(sa), Type::STREAM, None)?;
                match s.connect_timeout(&sa.into(), CONNECT_TIMEOUT) {
                    Ok(()) => {
                        s.set_tcp_nodelay(true)?;
                        return Ok(s);
                    }
                    Err(e) => last = Some(e),
                }
            }
            Err(last.unwrap_or_else(|| io::Error::other("no address")))
        }
    }
}

/// Whether the client closed its end (without reading or writing anything): a read
/// that would not block and finds the end of the stream. The writer thread shares the
/// socket in blocking mode, so the socket's mode is left alone.
#[cfg(unix)]
pub fn peer_closed(conn: &Conn) -> bool {
    use std::os::fd::AsRawFd;
    let mut b = 0u8;
    // SAFETY: a valid socket and a 1-byte buffer; MSG_DONTWAIT makes only this call
    // non-blocking.
    let n = unsafe {
        libc::recv(
            conn.as_raw_fd(),
            (&raw mut b).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    match n {
        0 => true, // end of stream
        n if n > 0 => false,
        _ => !matches!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ),
    }
}

/// Whether the client closed its end (without reading or writing anything): a socket
/// that is readable and peeks the end of the stream. The writer thread shares the
/// socket in blocking mode, so the socket's mode is left alone.
#[cfg(windows)]
pub fn peer_closed(conn: &Conn) -> bool {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        FD_SET, MSG_PEEK, SOCKET, TIMEVAL, recv, select,
    };

    let s = conn.as_raw_socket() as SOCKET;
    let mut readable = FD_SET {
        fd_count: 1,
        fd_array: [0; 64],
    };
    readable.fd_array[0] = s;
    let now = TIMEVAL {
        tv_sec: 0,
        tv_usec: 0,
    };
    // SAFETY: one valid socket in the set; a zero timeout polls.
    let n = unsafe {
        select(
            0,
            &mut readable,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &now,
        )
    };
    if n <= 0 {
        return n < 0; // 0: nothing to read, the peer is there
    }
    let mut b = 0u8;
    // SAFETY: a readable socket, so this does not block; a 1-byte buffer.
    let r = unsafe { recv(s, &mut b, 1, MSG_PEEK) };
    r <= 0 // 0: the end of the stream; < 0: the connection failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::thread::sleep;

    #[test]
    fn the_folder_names_the_socket_and_the_port() {
        let a = folder_socket(Path::new(r"C:\TVTest\BonDriver"));
        assert_eq!(a.parent(), Some(std::env::temp_dir().as_path()));
        let name = a.file_name().and_then(|n| n.to_str()).expect("a name");
        assert!(
            name.starts_with("dt295-") && name.ends_with(".sock") && name.len() == 6 + 16 + 5,
            "{name}"
        );
        // the same folder however it is spelt, another folder elsewhere
        for same in [r"c:\tvtest\bondriver", r"C:\TVTest\BonDriver\"] {
            assert_eq!(a, folder_socket(Path::new(same)), "{same}");
        }
        assert_ne!(a, folder_socket(Path::new(r"C:\TVTest2\BonDriver")));
        let port = |dir: &str| {
            let addr = folder_address(Path::new(dir));
            let (host, port) = addr.split_once(':').expect("host:port");
            assert_eq!(host, "127.0.0.1");
            port.parse::<u16>().expect("a port")
        };
        assert!((41200..41300).contains(&port(r"C:\TVTest\BonDriver")));
        assert_eq!(port(r"C:\TVTest\BonDriver"), port(r"c:\tvtest\bondriver\"));
    }

    #[cfg(windows)]
    #[test]
    fn the_folder_socket_fits_a_socket_address() {
        // a usual temporary folder: the Unix domain socket, not the fallback
        let dir = Path::new(r"C:\TVTest\BonDriver");
        assert_eq!(Target::of_folder(dir), Target::Unix(folder_socket(dir)));
    }

    fn accept_one(l: &Listener) -> Conn {
        for _ in 0..200 {
            match accept(l) {
                Ok(c) => return c,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
        panic!("no connection");
    }

    /// A request line, a reply line and the stream after it, then the client goes:
    /// what the daemon and its clients do over either kind of socket.
    fn round_trip(target: &Target, listener: &Listener) {
        let client = connect(target).expect("connect");
        (&client).write_all(b"tune\n").unwrap();
        let server = accept_one(listener);
        let mut line = String::new();
        BufReader::new(&server).read_line(&mut line).unwrap();
        assert_eq!(line, "tune\n");
        assert!(!peer_closed(&server), "the client is there");
        let writer = server.try_clone().expect("a second handle for the writer");
        (&writer).write_all(b"ok\nTS").unwrap();
        let mut got = [0u8; 5];
        (&client).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ok\nTS");
        drop(client);
        sleep(Duration::from_millis(50));
        assert!(peer_closed(&server), "the client went");
    }

    #[test]
    fn unix_socket_round_trip() {
        let path = std::env::temp_dir().join(format!("dt295-ipc-test-{}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        let target = Target::Unix(path.clone());
        let listener = listen(&target).expect("listen");
        round_trip(&target, &listener);
        let Err(err) = listen(&target) else {
            panic!("a live daemon is not replaced");
        };
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        // a daemon that crashed: the file stays
        let gone = listener;
        let crashed = gone.file.clone();
        drop(gone);
        assert!(fs::symlink_metadata(&path).is_ok(), "the file stays");
        // the crashed daemon's file kept under another name, as a live daemon's is: ext4
        // gives a socket file removed after its socket closed the same inode at once
        let mut kept = path.clone().into_os_string();
        kept.push(".kept");
        let kept = PathBuf::from(kept);
        let _ = fs::remove_file(&kept);
        fs::hard_link(&path, &kept).expect("a link to the file");
        // left behind by a daemon that is gone: replaced
        let listener = listen(&target).expect("listen again");
        assert_ne!(listener.file, crashed, "another file");
        // the daemon that crashed, ending only now, leaves the new one's file alone
        unlisten(Listener {
            sock: unix_socket().unwrap(),
            file: crashed,
        });
        assert!(fs::symlink_metadata(&path).is_ok(), "the new file stays");
        let client = connect(&target).expect("the new daemon still answers");
        drop(client);
        unlisten(listener);
        assert!(fs::symlink_metadata(&path).is_err(), "removed");
        let _ = fs::remove_file(&kept);
        let mut lock = path.into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }

    /// Daemons that start together where one crashed: one listens, the others find it.
    #[test]
    fn one_of_daemons_starting_together_listens() {
        let path = std::env::temp_dir().join(format!("dt295-ipc-race-{}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        let target = Target::Unix(path.clone());
        drop(listen(&target).expect("a daemon that crashes"));
        // the file is replaced only under its lock
        let held = lock_socket_file(&path).expect("the lock");
        let waits = {
            let target = target.clone();
            std::thread::spawn(move || listen(&target))
        };
        sleep(Duration::from_millis(200));
        assert!(!waits.is_finished(), "waits for the lock");
        drop(held);
        unlisten(
            waits
                .join()
                .unwrap()
                .expect("listens once the lock is free"),
        );
        drop(listen(&target).expect("a daemon that crashes"));
        let tries: Vec<_> = (0..4)
            .map(|_| {
                let target = target.clone();
                std::thread::spawn(move || listen(&target))
            })
            .collect();
        let listeners: Vec<Listener> = tries
            .into_iter()
            .filter_map(|t| t.join().unwrap().ok())
            .collect();
        assert_eq!(listeners.len(), 1, "one listens");
        assert!(connect(&target).is_ok(), "and answers");
        for l in listeners {
            unlisten(l);
        }
        let mut lock = path.into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }

    #[test]
    fn tcp_round_trip() {
        // a free port, then the same through listen()
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let target = Target::Tcp(format!("127.0.0.1:{port}"));
        let listener = listen(&target).expect("listen");
        round_trip(&target, &listener);
        assert!(listen(&target).is_err(), "the port is taken");
    }

    #[test]
    fn a_file_that_is_not_a_socket_stays() {
        let dir = std::env::temp_dir().join(format!("dt295-ipc-file-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("important.txt");
        fs::write(&file, b"keep me").unwrap();
        let Err(err) = listen(&Target::Unix(file.clone())) else {
            panic!("listened in the place of a plain file");
        };
        let kept = fs::read(&file);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(kept.unwrap(), b"keep me", "the file stays");
    }

    #[test]
    fn nothing_listens() {
        let path = std::env::temp_dir().join(format!("dt295-ipc-none-{}.sock", std::process::id()));
        assert!(connect(&Target::Unix(path)).is_err());
    }
}
