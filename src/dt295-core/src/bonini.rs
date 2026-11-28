//! The `[Tuner]` section of a `BonDriver_DT295.ini` as the BonDriver reads it, shared
//! with the `dt295` command, which takes the same file as its configuration: the unit and
//! its tuner, the raw-TS switch and the channel file of another BonDriver. The unit's firmware and
//! state file are in the ini's folder, where the `dt295 serve` the BonDriver starts keeps
//! them. Relative paths are under the ini's folder. The tuning spaces of the file are
//! [`chan::ini_spaces`](crate::chan::ini_spaces).

use std::fmt;
use std::path::{self, Path, PathBuf};

use crate::fwdl::{IDLE_FIRMWARE, TRC_FIRMWARE};
use crate::profile::{self, Ini};

/// `[Tuner]` keys of earlier versions, no longer read: the firmware and the state file
/// are those of the ini's folder, and `dt295 serve` knows its units by serial number.
pub const RETIRED_KEYS: [&str; 4] = ["Firmware", "TrcFirmware", "DeviceIndex", "StateFile"];

/// Which device to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSel {
    /// `DeviceIndex`: the n-th PIX-DT295 this process can open (one in use by another
    /// host is skipped, so with several units the numbers shift).
    Index(usize),
    /// `DeviceSerial`: the unit with this USB serial number (upper case; the device is
    /// matched case-insensitively). Fixed to one unit whatever the order.
    Serial(String),
}

impl fmt::Display for DeviceSel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceSel::Index(n) => write!(f, "DeviceIndex {n}"),
            DeviceSel::Serial(s) => write!(f, "serial {s}"),
        }
    }
}

/// What `[Tuner]` says about the unit and its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tuner {
    /// The ini's folder (empty = the working directory): the unit's firmware and its
    /// state file are there.
    pub dir: PathBuf,
    /// `DeviceSerial` (blanks trimmed, upper case); `None` = the only unit connected.
    pub serial: Option<String>,
    /// `TunerNumber`: the tuner of the unit, 1 (absent, or anything but 2) or 2 (a
    /// two-tuner model's second).
    pub tuner_number: u8,
    /// `RawTS`: non-zero = the blob-free raw-TS path (absent = the System1 path).
    pub raw_ts: bool,
    /// `ChannelFile`: the channel file of another BonDriver whose tuning spaces to list
    /// instead of the ini's own ([`dt295_chdef::load`]); absent or empty = none.
    pub channel_file: Option<PathBuf>,
}

impl Tuner {
    /// `[Tuner]` of `ini`, a file in the folder `dir` (the base of relative paths; empty
    /// = the working directory).
    pub fn from_ini(ini: &Ini, dir: &Path) -> Self {
        let string = |key: &str| profile::get(ini, "Tuner", key);
        let serial = string("DeviceSerial").unwrap_or("").trim();
        let channel_file = match string("ChannelFile").unwrap_or("").trim() {
            "" => None,
            p => Some(resolve(dir, p)),
        };
        Self {
            dir: dir.to_owned(),
            serial: (!serial.is_empty()).then(|| serial.to_ascii_uppercase()),
            tuner_number: if profile::get_int(ini, "Tuner", "TunerNumber", 1) == 2 {
                2
            } else {
                1
            },
            raw_ts: profile::get_int(ini, "Tuner", "RawTS", 0) != 0,
            channel_file,
        }
    }

    /// The idle firmware, in the ini's folder.
    pub fn firmware(&self) -> PathBuf {
        resolve(&self.dir, IDLE_FIRMWARE)
    }

    /// The System1 transcoder firmware, in the ini's folder.
    pub fn trc_firmware(&self) -> PathBuf {
        resolve(&self.dir, TRC_FIRMWARE)
    }

    /// The state file of the unit `serial` (link key, PSI): `<serial>.state` in the
    /// ini's folder, as `dt295 serve` names it.
    pub fn state_file(&self, serial: &str) -> PathBuf {
        resolve(&self.dir, &format!("{}.state", serial.to_ascii_uppercase()))
    }
}

/// The [`RETIRED_KEYS`] an ini still has.
pub fn retired_keys(ini: &Ini) -> Vec<&'static str> {
    RETIRED_KEYS
        .into_iter()
        .filter(|k| profile::get(ini, "Tuner", k).is_some())
        .collect()
}

/// An ini path: absolute (`X:...`, `\\...`, or absolute on this OS) as is, else
/// appended to `dir` as text, so a `\x` stays under it as in the BonDriver. A `dir`
/// ending in `\` or `/` (an ini is a Windows file) gets no second separator.
pub fn resolve(dir: &Path, value: &str) -> PathBuf {
    let dir = dir.as_os_str();
    if dir.is_empty() || is_absolute(value) || Path::new(value).is_absolute() {
        return PathBuf::from(value);
    }
    let mut s = dir.to_owned();
    if !s.to_string_lossy().ends_with(['\\', '/']) {
        s.push(path::MAIN_SEPARATOR_STR);
    }
    s.push(value);
    PathBuf::from(s)
}

/// A drive letter path or a UNC path.
fn is_absolute(p: &str) -> bool {
    let mut c = p.chars();
    matches!(
        (c.next(), c.next()),
        (Some(_), Some(':')) | (Some('\\'), Some('\\'))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        Path::new("TVTest").join("BonDriver")
    }

    fn tuner(text: &str) -> Tuner {
        Tuner::from_ini(&profile::parse(text).expect("parses"), &dir())
    }

    #[test]
    fn defaults() {
        let t = tuner("");
        assert_eq!(t.firmware(), dir().join("mb86m01_assp_osec_idle.bin"));
        assert_eq!(
            t.trc_firmware(),
            dir().join("mb86m01_assp_osec_trc_m_h.bin")
        );
        assert_eq!(t.serial, None);
        assert_eq!(t.state_file("cd23456789"), dir().join("CD23456789.state"));
        assert_eq!(t.tuner_number, 1);
        assert!(!t.raw_ts);
        assert_eq!(t.channel_file, None);
    }

    #[test]
    fn the_tuner() {
        assert_eq!(tuner("[Tuner]\nTunerNumber=2\n").tuner_number, 2);
        assert_eq!(tuner("[Tuner]\nTunerNumber=1\n").tuner_number, 1);
        assert_eq!(tuner("[Tuner]\nTunerNumber=3\n").tuner_number, 1);
        assert_eq!(tuner("[Tuner]\nTunerNumber=\n").tuner_number, 1);
    }

    #[test]
    fn channel_file() {
        assert_eq!(
            tuner("[Tuner]\nChannelFile=BonDriver_PT3-S.ChSet.txt\n").channel_file,
            Some(dir().join("BonDriver_PT3-S.ChSet.txt"))
        );
        assert_eq!(
            tuner("[Tuner]\nChannelFile=\"D:\\BDA\\BonDriver_BDA.ini\"\n").channel_file,
            Some(PathBuf::from(r"D:\BDA\BonDriver_BDA.ini"))
        );
        assert_eq!(tuner("[Tuner]\nChannelFile= \n").channel_file, None);
    }

    #[test]
    fn values() {
        let t = tuner("[tuner]\r\nRawTS=1\r\n");
        assert!(t.raw_ts);
        assert!(!tuner("[Tuner]\nRawTS=0\n").raw_ts);
    }

    #[test]
    fn the_unit() {
        let serial = |text: &str| tuner(text).serial;
        assert_eq!(
            serial("[Tuner]\nDeviceSerial= cd23456789 \n"),
            Some("CD23456789".into())
        );
        assert_eq!(
            serial("[Tuner]\nDeviceSerial=\" cd23456789 \"\n"),
            Some("CD23456789".into()),
            "blanks inside quotes too"
        );
        assert_eq!(
            serial("[Tuner]\nDeviceSerial=\" \"\n"),
            None,
            "blank = the only one"
        );
        assert_eq!(DeviceSel::Index(3).to_string(), "DeviceIndex 3");
        assert_eq!(
            DeviceSel::Serial("CD23456789".into()).to_string(),
            "serial CD23456789"
        );
    }

    #[test]
    fn retired() {
        let ini = profile::parse(
            "[Tuner]\nFirmware=D:\\fw\\idle.bin\nTrcFirmware=\nDeviceIndex=1\nStateFile=x\n",
        )
        .unwrap();
        assert_eq!(retired_keys(&ini), RETIRED_KEYS);
        let t = Tuner::from_ini(&ini, &dir());
        assert_eq!(t.firmware(), dir().join(IDLE_FIRMWARE), "not read");
        assert_eq!(t.serial, None);
        assert!(retired_keys(&profile::parse("[Tuner]\nRawTS=1\n").unwrap()).is_empty());
    }

    #[test]
    fn relative_paths() {
        let d = dir();
        assert_eq!(resolve(&d, r"D:\fw\a.bin"), PathBuf::from(r"D:\fw\a.bin"));
        assert_eq!(
            resolve(&d, r"\\nas\fw\a.bin"),
            PathBuf::from(r"\\nas\fw\a.bin")
        );
        assert_eq!(resolve(&d, "a.bin"), d.join("a.bin"), "a separator added");
        assert_eq!(resolve(Path::new(""), "a.bin"), PathBuf::from("a.bin"));
        let t = Tuner::from_ini(&profile::parse("").unwrap(), Path::new(""));
        assert_eq!(
            (t.firmware(), t.state_file("a")),
            (PathBuf::from(IDLE_FIRMWARE), PathBuf::from("A.state")),
            "an ini in the working directory"
        );
    }

    #[test]
    fn windows_paths() {
        let d = Path::new(r"C:\TVTest\BonDriver\");
        assert_eq!(
            resolve(d, r"fw\a.bin"),
            PathBuf::from(r"C:\TVTest\BonDriver\fw\a.bin"),
            "no second separator"
        );
        assert_eq!(
            resolve(d, r"\fw\a.bin"),
            PathBuf::from(r"C:\TVTest\BonDriver\\fw\a.bin"),
            "a root-relative path stays under the folder"
        );
    }
}
