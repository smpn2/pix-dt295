//! `BonDriver_DT295.ini` (next to the DLL, same base name), read under the Win32
//! profile API's rules ([`dt295::profile`]).
//!
//! `[Tuner]`: `DeviceSerial`, `TunerNumber`, `RawTS`, `ChannelFile` ([`dt295::bonini::Tuner`]),
//! `LogFile`, `LogLevel`, `UHF`, `CATV`. `[BS]` / `[CS110]`: one
//! `name=IF_kHz,TSID[,ServiceID]` line per channel. `[Server]`: `IdleExitSec`,
//! `StartTimeoutSec` (the `dt295 serve` the DLL starts). The `[Tuner]` keys of the unit
//! and the tuning spaces ([`dt295::chan::ini_spaces`]: the ini's own, or those of the
//! `ChannelFile`) are read by the shared code the `dt295` command reads the same file
//! with.
//!
//! The unit belongs to `dt295 serve`, whose firmware and state files are those of the
//! DLL's folder: `Firmware`, `TrcFirmware`, `StateFile` and `DeviceIndex` of earlier
//! versions are no longer read ([`dt295::bonini::RETIRED_KEYS`]).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use dt295::bonini::{Tuner, retired_keys};
use dt295::chan::{self, Space};
use dt295::profile::{self, Ini};
use dt295::serve::Mode;
use tracing::level_filters::LevelFilter;

use crate::client::Settings;

/// `[Server] IdleExitSec` by default: the daemon ends this long after its last client.
pub const IDLE_EXIT_SECS: u32 = 5;
/// `[Server] StartTimeoutSec` by default.
pub const START_TIMEOUT_SECS: u32 = 5;

/// Paths derived from the DLL's own file name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModulePaths {
    /// Folder of the DLL including the trailing separator; empty
    /// if the module path is unknown.
    pub dir: OsString,
    /// The DLL path without its extension (`...\BonDriver_DT295`).
    pub stem: OsString,
}

impl ModulePaths {
    /// Split a module file name: the last `\` or `/` ends the folder, the last `.`
    /// starts the extension.
    pub fn from_module_path(path: &OsStr) -> Self {
        cfg_select! {
            windows => {
                use std::os::windows::ffi::{OsStrExt, OsStringExt};
                let w: Vec<u16> = path.encode_wide().collect();
                let dir_end = w
                    .iter()
                    .rposition(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
                    .map_or_default(|i| i + 1);
                let stem_end = w
                    .iter()
                    .rposition(|&c| c == u16::from(b'.'))
                    .unwrap_or(w.len());
                Self {
                    dir: OsString::from_wide(&w[..dir_end]),
                    stem: OsString::from_wide(&w[..stem_end]),
                }
            }
            _ => {
                let s = path.to_string_lossy();
                let dir_end = s.rfind(['\\', '/']).map_or_default(|i| i + 1);
                let stem_end = s.rfind('.').unwrap_or(s.len());
                Self {
                    dir: OsString::from(&s[..dir_end]),
                    stem: OsString::from(&s[..stem_end]),
                }
            }
        }
    }

    /// `BonDriver_DT295<ext>` next to the DLL.
    pub fn sibling(&self, ext: &str) -> PathBuf {
        let mut s = self.stem.clone();
        s.push(ext);
        PathBuf::from(s)
    }
}

/// What `LoadConfig` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The DLL's folder: `dt295.exe`, the firmware and the units' state files are there.
    pub dir: PathBuf,
    /// `[Tuner] DeviceSerial`; `None` = the only unit connected.
    pub serial: Option<String>,
    /// `[Tuner] TunerNumber`: the tuner of the unit, 1 or 2 (a two-tuner model's second).
    pub tuner: u8,
    /// `[Tuner] RawTS`: non-zero = the blob-free raw-TS path -- the scrambled TS, for the
    /// PC-side B-CAS (TVTest's CAS plugin) to descramble. Absent / 0 = the System1 path
    /// (the vendor firmware, the device's internal B-CAS reader).
    pub raw_ts: bool,
    /// `<dll>.log` with `[Tuner] LogFile=1`; `None` (absent or `0`) = OutputDebugString
    /// only.
    pub log_file: Option<PathBuf>,
    /// `[Tuner] LogLevel` (`error`, `warn`, `info`, `debug`, `trace`; absent or empty =
    /// `info`); `Err` holds a value that is none of them.
    pub log_level: Result<LevelFilter, String>,
    /// `<dll>.ch2`, TVTest's channel-scan file (next to the DLL): read in raw-TS mode for a
    /// known on-air channel to prime on at the open. `None` only for a driver that must
    /// not touch the disk (tests).
    pub ch2_file: Option<PathBuf>,
    /// UHF, CATV (unless hidden), then BS and CS110 when they have lines; or the spaces of
    /// the `[Tuner] ChannelFile` (none when it cannot be read).
    pub spaces: Vec<Space>,
    /// What to log about the `ChannelFile` once the log is open: `Ok` lines (its format
    /// and spaces), or `Err` why it could not be read.
    pub channel_file_log: Result<Vec<String>, String>,
    /// `[Server] IdleExitSec`: the daemon this DLL starts ends after this long without a
    /// client (0 = never).
    pub idle_exit_secs: u32,
    /// `[Server] StartTimeoutSec`: how long to wait for a daemon this DLL started.
    pub start_timeout_secs: u32,
    /// The `[Tuner]` keys of earlier versions the ini still has (not read).
    pub retired: Vec<&'static str>,
}

impl Config {
    /// Read the configuration from a parsed ini (an empty one gives the defaults).
    pub fn from_ini(ini: &Ini, paths: &ModulePaths) -> Self {
        // The unit, RawTS and the ChannelFile: the dt295 command reads them from the same
        // file with the same code.
        let tuner = Tuner::from_ini(ini, Path::new(&paths.dir));

        // LogFile=1: the core log to <dll>.log, a fixed name -- one per DLL copy. Absent
        // or 0 = no file.
        let log_file = (tuner_int(ini, "LogFile", 0) != 0).then(|| paths.sibling(".log"));
        let log_level = match profile::get(ini, "Tuner", "LogLevel").unwrap_or("").trim() {
            "" => Ok(LevelFilter::INFO),
            v => v.parse::<LevelFilter>().map_err(|_| v.to_owned()),
        };

        // A ChannelFile that cannot be read lists no spaces: the ini's own would number
        // the channels unlike the .ch2 made with that file.
        let (spaces, channel_file_log) = match chan::ini_spaces(ini, tuner.channel_file.as_deref())
        {
            Ok((spaces, log)) => (spaces, Ok(log)),
            Err(e) => (Vec::new(), Err(e)),
        };

        Self {
            dir: folder(&paths.dir),
            serial: tuner.serial,
            tuner: tuner.tuner_number,
            raw_ts: tuner.raw_ts,
            log_file,
            log_level,
            // TVTest writes <dll>.ch2 next to the DLL
            ch2_file: Some(paths.sibling(".ch2")),
            spaces,
            channel_file_log,
            idle_exit_secs: profile::get_int(ini, "Server", "IdleExitSec", IDLE_EXIT_SECS),
            start_timeout_secs: profile::get_int(
                ini,
                "Server",
                "StartTimeoutSec",
                START_TIMEOUT_SECS,
            ),
            retired: retired_keys(ini),
        }
    }

    /// What the client of `dt295 serve` needs.
    pub fn settings(&self) -> Settings {
        Settings {
            dir: self.dir.clone(),
            serial: self.serial.clone(),
            tuner: self.tuner,
            mode: if self.raw_ts {
                Mode::RawTs
            } else {
                Mode::System1
            },
            idle_exit_secs: u64::from(self.idle_exit_secs),
            start_timeout_secs: u64::from(self.start_timeout_secs),
            verbose: match self.log_level {
                Ok(LevelFilter::TRACE) => 2,
                Ok(LevelFilter::DEBUG) => 1,
                _ => 0,
            },
        }
    }
}

/// The folder of a module path's folder part (`...\BonDriver\`): without the trailing
/// separator, unless it is a root.
fn folder(dir: &OsStr) -> PathBuf {
    Path::new(dir).components().collect()
}

/// `GetPrivateProfileIntW(L"Tuner", key, default)` ([`profile::get_int`]).
fn tuner_int(ini: &Ini, key: &str, default: u32) -> u32 {
    profile::get_int(ini, "Tuner", key, default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ModulePaths {
        ModulePaths::from_module_path(OsStr::new(r"C:\TVTest\BonDriver\BonDriver_DT295.dll"))
    }

    fn config(text: &str) -> Config {
        Config::from_ini(&profile::parse(text).expect("parses"), &paths())
    }

    #[test]
    fn log_level() {
        let level = |text: &str| config(text).log_level;
        assert_eq!(level(""), Ok(LevelFilter::INFO), "no key");
        assert_eq!(level("[Tuner]\r\nLogLevel=\r\n"), Ok(LevelFilter::INFO));
        assert_eq!(
            level("[Tuner]\r\nLogLevel=debug\r\n"),
            Ok(LevelFilter::DEBUG)
        );
        assert_eq!(
            level("[Tuner]\r\nLogLevel= Trace \r\n"),
            Ok(LevelFilter::TRACE)
        );
        assert_eq!(level("[Tuner]\r\nLogLevel=warn\r\n"), Ok(LevelFilter::WARN));
        assert_eq!(level("[Tuner]\r\nLogLevel=loud\r\n"), Err("loud".into()));
    }

    #[test]
    fn module_paths() {
        let p = paths();
        assert_eq!(p.dir, OsString::from(r"C:\TVTest\BonDriver\"));
        assert_eq!(
            p.stem,
            OsString::from(r"C:\TVTest\BonDriver\BonDriver_DT295")
        );
        assert_eq!(
            p.sibling(".ini"),
            PathBuf::from(r"C:\TVTest\BonDriver\BonDriver_DT295.ini")
        );
        let none = ModulePaths::from_module_path(OsStr::new(""));
        assert_eq!(none.sibling(".ini"), PathBuf::from(".ini"));
        assert_eq!(folder(&none.dir), PathBuf::new());
    }

    #[cfg(windows)]
    #[test]
    fn the_folder() {
        assert_eq!(config("").dir, PathBuf::from(r"C:\TVTest\BonDriver"));
        let root = ModulePaths::from_module_path(OsStr::new(r"D:\BonDriver_DT295.dll"));
        assert_eq!(
            folder(&root.dir),
            PathBuf::from(r"D:\"),
            "a root keeps its separator"
        );
    }

    #[test]
    fn defaults_without_ini() {
        let c = config("");
        assert_eq!(c.serial, None, "the only unit");
        assert_eq!(c.tuner, 1);
        assert!(!c.raw_ts);
        assert_eq!(c.log_file, None, "no LogFile key = no log file");
        assert_eq!(
            (c.idle_exit_secs, c.start_timeout_secs),
            (IDLE_EXIT_SECS, START_TIMEOUT_SECS)
        );
        assert!(c.retired.is_empty());
        let names: Vec<_> = c.spaces.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["UHF", "CATV"]);
        let s = c.settings();
        assert_eq!((s.mode, s.verbose, s.idle_exit_secs), (Mode::System1, 0, 5));
        assert_eq!(s.dir, c.dir);
    }

    #[test]
    fn tuner_section() {
        let c = config(
            "\u{FEFF}; comment\r\n\
             [tuner]\r\n\
             DeviceSerial= cd23456789 \r\n\
             LogFile=logs\\dt295core.log\r\n\
             RawTS=1\r\n\
             UHF=0\r\n\
             CATV=yes\r\n",
        );
        assert_eq!(c.serial.as_deref(), Some("CD23456789"));
        assert!(c.raw_ts);
        assert_eq!(c.settings().mode, Mode::RawTs);
        // A path is not a flag: LogFile reads as 0 (as GetPrivateProfileInt would).
        assert_eq!(c.log_file, None);
        // "yes" is not a number: GetPrivateProfileInt reads 0.
        assert!(c.spaces.is_empty());
    }

    #[test]
    fn log_flag() {
        let copy =
            ModulePaths::from_module_path(OsStr::new(r"C:\TVTest\BonDriver\BonDriver_DT295-1.dll"));
        let log =
            |text: &str| Config::from_ini(&profile::parse(text).expect("parses"), &copy).log_file;
        // LogFile: 1 = <dll>.log (named after the DLL copy); absent, empty or 0 = off.
        assert_eq!(
            log("[Tuner]\nLogFile=1\n"),
            Some(PathBuf::from(r"C:\TVTest\BonDriver\BonDriver_DT295-1.log"))
        );
        for off in [
            "",
            "[Tuner]\nLogFile=\n",
            "[Tuner]\nLogFile=0\n",
            "[Tuner]\n;LogFile=1\n",
            "[BS]\nLogFile=1\n",
        ] {
            assert_eq!(log(off), None, "{off:?}");
        }
        let verbose = |text: &str| config(text).settings().verbose;
        assert_eq!(verbose("[Tuner]\nLogLevel=debug\n"), 1);
        assert_eq!(verbose("[Tuner]\nLogLevel=trace\n"), 2);
        assert_eq!(verbose("[Tuner]\nLogLevel=warn\n"), 0);
    }

    #[test]
    fn server_section() {
        let c = config("[Server]\r\nIdleExitSec=30\r\nStartTimeoutSec=9\r\n");
        assert_eq!((c.idle_exit_secs, c.start_timeout_secs), (30, 9));
        let s = c.settings();
        assert_eq!((s.idle_exit_secs, s.start_timeout_secs), (30, 9));
        assert_eq!(
            config("[Server]\nIdleExitSec=0\n").idle_exit_secs,
            0,
            "never"
        );
        assert_eq!(
            config("[Tuner]\nIdleExitSec=30\n").idle_exit_secs,
            IDLE_EXIT_SECS,
            "[Server] only"
        );
    }

    #[test]
    fn retired_keys_are_not_read() {
        let c = config(
            "[Tuner]\nFirmware=D:\\fw\\idle.bin\nTrcFirmware=\nDeviceIndex=2\nStateFile=x.state\n",
        );
        assert_eq!(
            c.retired,
            ["Firmware", "TrcFirmware", "DeviceIndex", "StateFile"]
        );
        assert_eq!(c.serial, None, "DeviceIndex picks nothing");
        assert_eq!(
            c.settings().dir,
            config("").dir,
            "the firmware is the folder's"
        );
    }

    #[test]
    fn channel_file() {
        let c = config("");
        assert_eq!(
            c.channel_file_log,
            Ok(Vec::new()),
            "none: the ini's own spaces"
        );
        // One that cannot be read: no spaces (the ini's own would number them otherwise)
        let c =
            config("[Tuner]\nChannelFile=Z:\\absent\\BonDriver_PT3-S.ChSet.txt\n[BS]\nA=1049480\n");
        assert!(c.spaces.is_empty());
        let e = c.channel_file_log.unwrap_err();
        assert!(
            e.starts_with(r"ChannelFile Z:\absent\BonDriver_PT3-S.ChSet.txt: "),
            "{e}"
        );
    }

    #[test]
    fn raw_ts_mode() {
        // Absent / 0 = the System1 path; RawTS=1 = the blob-free raw-TS path.
        assert!(!config("").raw_ts);
        assert!(!config("[Tuner]\nRawTS=0\n").raw_ts);
        assert!(config("[Tuner]\nRawTS=1\n").raw_ts);
    }

    #[test]
    fn sample_ini_from_repo() {
        let text = include_str!("../BonDriver_DT295.ini");
        let c = config(text);
        let summary: Vec<_> = c
            .spaces
            .iter()
            .map(|s| (s.name.as_str(), s.channels.len()))
            .collect();
        assert_eq!(
            summary,
            [("UHF", 50), ("CATV", 51), ("BS", 26), ("CS110", 12)]
        );
        // one line per TS without a ServiceID: every service of it
        assert!(
            c.spaces[2..]
                .iter()
                .all(|s| s.channels.iter().all(|ch| ch.sid.is_none()))
        );
        let line = |space: usize, name: &str| {
            let ch = c.spaces[space]
                .channels
                .iter()
                .find(|ch| ch.name == name)
                .unwrap();
            (ch.freq_hz, ch.tsid)
        };
        assert_eq!(line(2, "BS15/TS1"), (1_318_000_000, 0x40F1));
        assert_eq!(line(2, "BS13/TS2"), (1_279_640_000, 0x46D2));
        assert_eq!(line(2, "BS01/TS1"), (1_049_480_000, 0x4011));
        assert_eq!(line(3, "ND06/TS0"), (1_693_000_000, 0x7060));
    }
    #[test]
    fn the_sample_ini_has_no_retired_key() {
        assert!(
            config(include_str!("../BonDriver_DT295.ini"))
                .retired
                .is_empty()
        );
    }
}
