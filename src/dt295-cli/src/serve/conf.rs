//! The configuration of `dt295 serve` (ini syntax; on Linux `/etc/dt295.conf`):
//!
//! ```ini
//! [dt295]
//! ; the vendor's idle and trc images
//! firmware-dir = /usr/lib/firmware/dt295
//! ; <serial>.state per unit
//! state-dir = /var/lib/dt295
//! ; where clients connect (or listen = <host:port> for TCP on the loopback)
//! socket = /run/dt295/dt295.sock
//!
//! [unit AB12345678]
//! ; blob-free raw-TS: no vendor firmware, the device streams the scrambled TS and
//! ; the host descrambles it (default off, i.e. System1 with the in-device card)
//! [unit CD23456789]
//! raw-ts = true
//! ```
//!
//! Everything is optional (these are the defaults). With `[unit <serial>]` sections,
//! whose only setting is `raw-ts`, the daemon opens those units at its start and serves
//! only them; without any, it opens a unit at the first request for it, on the path the
//! request asks for. Unknown sections and settings are errors, so a typo does not go
//! unnoticed. Comments take a line of their own (`;` or `#` first): after a value they
//! would be part of it.
//!
//! Without a configuration file (on Linux when `/etc/dt295.conf` does not exist; on
//! Windows unless `-c` names one) everything is the default. On Windows the firmware and
//! state folders are by default the folder of `dt295.exe`, and those of a configuration
//! file its own folder; relative ones (and a relative `socket`) are relative to it. The
//! socket is by default the one of `dt295.exe`'s folder ([`Target::of_program`]).
//!
//! Where the daemon listens and a client connects: the command line's `--socket` /
//! `--listen` (`--connect`), else `$DT295_SOCKET` / `$DT295_LISTEN`, else the
//! configuration's, else the default.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use dt295::ipc::Target;
use dt295::{IDLE_FIRMWARE, TRC_FIRMWARE};
use ini::Ini;

/// The configuration `dt295 serve` reads when there is one and `-c` names none.
#[cfg(not(windows))]
pub const DEFAULT_CONF: Option<&str> = Some("/etc/dt295.conf");
#[cfg(windows)]
pub const DEFAULT_CONF: Option<&str> = None;
#[cfg(not(windows))]
const DEFAULT_FIRMWARE_DIR: &str = "/usr/lib/firmware/dt295";
#[cfg(windows)]
const DEFAULT_FIRMWARE_DIR: &str = ".";
#[cfg(not(windows))]
const DEFAULT_STATE_DIR: &str = "/var/lib/dt295";
#[cfg(windows)]
const DEFAULT_STATE_DIR: &str = ".";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conf {
    pub firmware_dir: PathBuf,
    pub state_dir: PathBuf,
    /// `socket` (a Unix domain socket) or `listen` (TCP); `None` = the default.
    pub target: Option<Target>,
    /// The units to serve, in file order; none = any unit a request asks for.
    pub units: Vec<Unit>,
}

/// One `[unit <serial>]` of the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The USB serial number.
    pub serial: String,
    /// Blob-free raw-TS: no vendor firmware, the device streams the scrambled TS and
    /// the host descrambles it (`raw-ts = true`); default off (System1).
    pub raw_ts: bool,
}

impl Conf {
    /// No configuration file: the defaults (on Windows the folders are `dt295.exe`'s).
    pub fn builtin() -> Self {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut conf = Self::empty();
        #[cfg(windows)]
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            conf.firmware_dir.clone_from(&dir);
            conf.state_dir = dir;
        }
        conf
    }

    /// The configuration `config`, else the default one if it exists, else the
    /// defaults.
    pub fn find(config: Option<&Path>) -> Result<Self> {
        match config.or(DEFAULT_CONF.map(Path::new)) {
            Some(path) if config.is_some() || path.exists() => Self::load(path),
            _ => Ok(Self::builtin()),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut conf = Self::parse(&text).with_context(|| format!("in {}", path.display()))?;
        // the folders, relative to the configuration's (on Windows it sits with them)
        #[cfg(windows)]
        if let Some(dir) = path.parent() {
            conf.firmware_dir = dir.join(&conf.firmware_dir);
            conf.state_dir = dir.join(&conf.state_dir);
            if let Some(Target::Unix(socket)) = &mut conf.target {
                *socket = dir.join(&*socket);
            }
        }
        Ok(conf)
    }

    fn empty() -> Self {
        Self {
            firmware_dir: DEFAULT_FIRMWARE_DIR.into(),
            state_dir: DEFAULT_STATE_DIR.into(),
            target: None,
            units: Vec::new(),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let ini = Ini::load_from_str(text)?;
        let mut conf = Self::empty();
        for (section, props) in &ini {
            let words: Vec<&str> = section.unwrap_or_default().split_whitespace().collect();
            match words.as_slice() {
                [] if props.is_empty() => {}
                [] => bail!("settings before the first section"),
                [name] if name.eq_ignore_ascii_case("dt295") => {
                    for (key, value) in props {
                        let path = PathBuf::from(value);
                        let target = match key.to_ascii_lowercase().as_str() {
                            "firmware-dir" => {
                                conf.firmware_dir = path;
                                continue;
                            }
                            "state-dir" => {
                                conf.state_dir = path;
                                continue;
                            }
                            "socket" => Target::Unix(path),
                            "listen" => Target::Tcp(value.to_owned()),
                            _ => bail!("[dt295]: unknown setting {key:?}"),
                        };
                        if conf.target.replace(target).is_some() {
                            bail!("[dt295]: give socket or listen, once");
                        }
                    }
                }
                [kind, serial] if kind.eq_ignore_ascii_case("unit") => {
                    if conf
                        .units
                        .iter()
                        .any(|u| u.serial.eq_ignore_ascii_case(serial))
                    {
                        bail!("[unit {serial}] appears twice");
                    }
                    let mut raw_ts = false;
                    for (key, value) in props {
                        match key.to_ascii_lowercase().as_str() {
                            "raw-ts" => raw_ts = parse_bool(value)?,
                            _ => bail!("[unit {serial}]: unknown setting {key:?}"),
                        }
                    }
                    conf.units.push(Unit {
                        serial: (*serial).to_owned(),
                        raw_ts,
                    });
                }
                _ => bail!(
                    "unknown section [{}] (expected [dt295] or [unit <serial>])",
                    section.unwrap_or_default()
                ),
            }
        }
        Ok(conf)
    }

    pub fn idle_firmware(&self) -> PathBuf {
        self.firmware_dir.join(IDLE_FIRMWARE)
    }

    pub fn trc_firmware(&self) -> PathBuf {
        self.firmware_dir.join(TRC_FIRMWARE)
    }

    /// The state file of a unit (link key, PSI), named after its serial.
    pub fn state_file(&self, serial: &str) -> PathBuf {
        self.state_dir
            .join(format!("{}.state", serial.to_ascii_uppercase()))
    }
}

/// A boolean setting value (`true`/`false`, also yes/no, on/off, 1/0).
fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => bail!("raw-ts must be true or false, not {other:?}"),
    }
}

/// The command line's choice of where to listen or connect: `--socket <path>`, or
/// `--listen` / `--connect <host:port>`.
fn given(socket: Option<&Path>, tcp: Option<&str>) -> Option<Target> {
    match (socket, tcp) {
        (Some(p), _) => Some(Target::Unix(p.to_owned())),
        (None, Some(a)) => Some(Target::Tcp(a.to_owned())),
        (None, None) => None,
    }
}

/// Where `dt295 serve` listens: the command line's, else the environment's, else the
/// configuration's, else the default.
pub fn server_target(conf: &Conf, socket: Option<&Path>, listen: Option<&str>) -> Target {
    given(socket, listen)
        .or_else(Target::from_env)
        .or_else(|| conf.target.clone())
        .unwrap_or_else(Target::of_program)
}

/// Where a client (`dt295 tune`, a scan) connects: the command line's, else the
/// environment's, else the configuration's (`config`, or the default one if it exists),
/// else the default.
pub fn client_target(
    config: Option<&Path>,
    socket: Option<&Path>,
    connect: Option<&str>,
) -> Result<Target> {
    if let Some(t) = given(socket, connect).or_else(Target::from_env) {
        return Ok(t);
    }
    Ok(Conf::find(config)?
        .target
        .unwrap_or_else(Target::of_program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_units() {
        let c = Conf::parse("[unit AB12345678]\n[unit cd23456789]\n").unwrap();
        assert_eq!(c.firmware_dir, Path::new(DEFAULT_FIRMWARE_DIR));
        assert_eq!(c.target, None, "the default");
        assert_eq!(
            c.units
                .iter()
                .map(|u| u.serial.as_str())
                .collect::<Vec<_>>(),
            ["AB12345678", "cd23456789"]
        );
        assert!(c.units.iter().all(|u| !u.raw_ts), "default off");
        #[cfg(not(windows))]
        assert_eq!(
            c.idle_firmware(),
            Path::new("/usr/lib/firmware/dt295/mb86m01_assp_osec_idle.bin")
        );
        assert_eq!(
            c.state_file("cd23456789"),
            Path::new(DEFAULT_STATE_DIR).join("CD23456789.state")
        );
    }

    #[test]
    fn no_units_serves_any() {
        for text in ["", "[dt295]\nstate-dir = /srv/dt295\n"] {
            assert!(Conf::parse(text).unwrap().units.is_empty(), "{text:?}");
        }
    }

    #[test]
    fn builtin() {
        let c = Conf::builtin();
        assert!(c.units.is_empty());
        assert_eq!(c.target, None);
        #[cfg(windows)]
        {
            let exe = std::env::current_exe().unwrap();
            assert_eq!(c.firmware_dir, exe.parent().unwrap(), "dt295.exe's folder");
            assert_eq!(c.state_dir, c.firmware_dir);
        }
        #[cfg(not(windows))]
        assert_eq!(c.state_dir, Path::new(DEFAULT_STATE_DIR));
    }

    #[test]
    fn settings() {
        let c = Conf::parse(
            "; comment\n[dt295]\nfirmware-dir = /opt/fw\nState-Dir=/srv/dt295\n\
             socket = /tmp/dt295.sock\n\n[unit AB12345678]\n",
        )
        .unwrap();
        assert_eq!(c.firmware_dir, Path::new("/opt/fw"));
        assert_eq!(c.state_dir, Path::new("/srv/dt295"));
        assert_eq!(c.target, Some(Target::Unix("/tmp/dt295.sock".into())));
        assert_eq!(
            c.trc_firmware(),
            Path::new("/opt/fw/mb86m01_assp_osec_trc_m_h.bin")
        );
    }

    #[test]
    fn mistakes_are_errors() {
        let err = |text: &str| Conf::parse(text).unwrap_err().to_string();
        assert!(err("[dt295]\nfirmware = /x\n[unit A]\n").contains("unknown setting"));
        assert!(err("[unit A]\nname = x\n").contains("unknown setting"));
        assert!(err("[unit A]\nraw-ts = maybe\n").contains("true or false"));
        assert!(err("[units A]\n").contains("unknown section"));
        assert!(err("[unit A B]\n").contains("unknown section"));
        assert!(err("[unit A]\n[unit a]\n").contains("twice"));
        assert!(err("socket = /x\n[unit A]\n").contains("before the first section"));
        assert!(err("[dt295]\nsocket = /x\nlisten = 127.0.0.1:1\n[unit A]\n").contains("once"));
    }

    #[test]
    fn per_unit_raw_ts() {
        let c = Conf::parse("[unit A]\n[unit B]\nraw-ts = true\n").unwrap();
        assert_eq!(c.units[0].serial, "A");
        assert!(!c.units[0].raw_ts, "default off");
        assert!(c.units[1].raw_ts, "opted in");
    }

    #[test]
    fn the_documented_example_parses() {
        // the example above (and in docs/usage.md): comments on lines of their own
        let c = Conf::parse(
            "[dt295]\n; the vendor's idle and trc images\nfirmware-dir = /usr/lib/firmware/dt295\n\
             ; <serial>.state per unit\nstate-dir = /var/lib/dt295\n\
             socket = /run/dt295/dt295.sock\n\n[unit AB12345678]\n[unit CD23456789]\n",
        )
        .unwrap();
        assert_eq!(c.firmware_dir, Path::new("/usr/lib/firmware/dt295"));
        assert_eq!(c.units.len(), 2);
    }

    #[test]
    fn tcp() {
        let c = Conf::parse("[dt295]\nlisten = 127.0.0.1:41295\n[unit A]\n").unwrap();
        assert_eq!(c.target, Some(Target::Tcp("127.0.0.1:41295".into())));
    }

    #[test]
    fn a_named_configuration_must_be_there() {
        assert!(Conf::find(Some(Path::new("/no/such.conf"))).is_err());
    }

    // The environment is the process's: these tests assume that DT295_SOCKET and
    // DT295_LISTEN are not set.
    #[test]
    fn the_client_target() {
        let given = Path::new("/tmp/x.sock");
        assert_eq!(
            client_target(None, Some(given), None).unwrap(),
            Target::Unix(given.into())
        );
        assert_eq!(
            client_target(None, None, Some("127.0.0.1:1")).unwrap(),
            Target::Tcp("127.0.0.1:1".into())
        );
        let dir = std::env::temp_dir().join(format!("dt295-conf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("dt295.conf");
        std::fs::write(&conf, "[dt295]\nsocket = s.sock\n[unit A]\n").unwrap();
        let got = client_target(Some(&conf), None, None);
        let _ = std::fs::remove_dir_all(&dir);
        let want = if cfg!(windows) {
            dir.join("s.sock")
        } else {
            "s.sock".into()
        };
        assert_eq!(
            got.unwrap(),
            Target::Unix(want),
            "relative to it on Windows"
        );
        assert!(client_target(Some(Path::new("/no/such.conf")), None, None).is_err());
        #[cfg(windows)]
        assert_eq!(
            client_target(None, None, None).unwrap(),
            Target::of_program(),
            "no configuration file on Windows unless named"
        );
    }

    #[test]
    fn the_server_target() {
        let c = Conf::parse("[dt295]\nsocket = /srv/s.sock\n[unit A]\n").unwrap();
        assert_eq!(
            server_target(&c, None, None),
            Target::Unix("/srv/s.sock".into())
        );
        assert_eq!(
            server_target(&c, None, Some("127.0.0.1:1")),
            Target::Tcp("127.0.0.1:1".into()),
            "the command line first"
        );
        let c = Conf::parse("[unit A]\n").unwrap();
        assert_eq!(server_target(&c, None, None), Target::of_program());
    }
}
