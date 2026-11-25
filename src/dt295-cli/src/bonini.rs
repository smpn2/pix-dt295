//! A BonDriver_DT295 ini as the configuration of `dt295 capture`, `dt295 logo` and
//! `dt295 mirakc-channels`, read with the BonDriver's own code: `[Tuner]` gives the unit
//! (`DeviceSerial`) and the raw-TS switch, the firmware and the state file are those of
//! the ini's folder, as the `dt295 serve` the BonDriver starts uses them
//! ([`dt295::bonini::Tuner`]: the state file is `<serial>.state`), and the tuning spaces are
//! [`chan::ini_spaces`] (the built-in UHF `13ch`..`62ch` and CATV `C13ch`..`C63ch` plus
//! the `[BS]` / `[CS110]` lines, or those of the `ChannelFile`), so a TVTest `.ch2` made
//! with the ini numbers them the same. On Linux, where there is no DLL, a file of the
//! same format is the configuration.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dt295::bonini::{Tuner, retired_keys};
use dt295::chan::{self, Channel, NO_TSID, Space};
use dt295::{System, profile};

use crate::tuner::DeviceArgs;

/// What a BonDriver_DT295 ini says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BonDriver {
    pub ini: PathBuf,
    /// `[Tuner]`: the unit, the raw-TS switch, the folder of the firmware and the state
    /// file (with the link key the unit last used).
    pub tuner: Tuner,
    /// The tuning spaces, in the BonDriver's order.
    pub spaces: Vec<Space>,
}

impl BonDriver {
    pub fn from_ini(ini: &Path, text: &str) -> Result<Self> {
        let parsed =
            profile::parse(text).with_context(|| format!("cannot parse {}", ini.display()))?;
        let dir = ini.parent().unwrap_or(Path::new(""));
        let tuner = Tuner::from_ini(&parsed, dir);
        for key in retired_keys(&parsed) {
            tracing::warn!(
                "{}: [Tuner] {key} is no longer read (the firmware and the state file are \
                 those of the ini's folder)",
                ini.display()
            );
        }
        // As the BonDriver: a ChannelFile that cannot be read gives no tuning spaces (the
        // ini still names the unit and its state file, which `dt295 logo` needs).
        let spaces = match chan::ini_spaces(&parsed, tuner.channel_file.as_deref()) {
            Ok((spaces, log)) => {
                for line in log {
                    tracing::debug!("{line}");
                }
                spaces
            }
            Err(e) => {
                tracing::warn!("{}: {e}; no tuning spaces", ini.display());
                Vec::new()
            }
        };
        Ok(Self {
            ini: ini.to_owned(),
            tuner,
            spaces,
        })
    }

    pub fn load(ini: &Path) -> Result<Self> {
        let bytes = fs::read(ini).with_context(|| format!("cannot read {}", ini.display()))?;
        Self::from_ini(ini, &profile::decode(&bytes))
    }

    /// The `BonDriver_DT295*.ini` files in a folder, by name.
    pub fn find(dir: &Path) -> Result<Vec<Self>> {
        let entries =
            fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
        let mut out = Vec::new();
        for e in entries {
            let path = e?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if name.starts_with("bondriver_dt295") && name.ends_with(".ini") {
                out.push(Self::load(&path)?);
            }
        }
        out.sort_by(|a, b| a.ini.cmp(&b.ini));
        Ok(out)
    }

    /// The unit's serial number when the ini names one (`DeviceSerial`).
    pub fn serial(&self) -> Option<&str> {
        self.tuner.serial.as_deref()
    }

    /// The device options this ini gives, with the ones given on the command line
    /// winning (an explicit `-i` also wins over the ini's `DeviceSerial`): the firmware
    /// in the ini's folder, and the unit's state file there unless `--state` names one.
    pub fn device_args(&self, given: &DeviceArgs) -> DeviceArgs {
        let explicit_index = given.index != 0;
        DeviceArgs {
            fw: given.fw.clone().or_else(|| Some(self.tuner.firmware())),
            trc: given
                .trc
                .clone()
                .or_else(|| Some(self.tuner.trc_firmware())),
            index: given.index,
            serial: given
                .serial
                .clone()
                .or_else(|| self.serial().filter(|_| !explicit_index).map(str::to_owned)),
            state: given.state.clone(),
            state_dir: given
                .state_dir
                .clone()
                .or_else(|| Some(self.tuner.dir.clone())),
        }
    }
}

/// The channel a name stands for: a channel of one of the tuning spaces (names compare
/// case-insensitively, the first one to tune wins), else a built-in `NNch` (UHF) or
/// `CNNch` (CATV) name, also when the ini turns that space off or there is no ini.
pub fn tune(name: &str, spaces: &[Space]) -> Option<Channel> {
    let mut listed = spaces.iter().flat_map(|s| &s.channels);
    if let Some(c) = listed.find(|c| c.untunable.is_none() && c.name.eq_ignore_ascii_case(name)) {
        return Some(c.clone());
    }
    let number = name.trim().to_ascii_lowercase();
    let number = number.strip_suffix("ch")?;
    let hz = match number.strip_prefix('c') {
        Some(n) => chan::catv_hz(n.parse().ok()?),
        None => chan::uhf_hz(number.parse().ok()?),
    };
    Some(Channel {
        name: name.to_owned(),
        sys: System::Terrestrial,
        freq_hz: hz?,
        tsid: NO_TSID,
        sid: None,
        untunable: None,
    })
}

/// Why the channel of a name does not tune, when [`tune`] finds none: the first listed
/// channel of that name is one of the placeholders (another modulation, left-hand, a
/// number without a definition).
pub fn untunable<'a>(name: &str, spaces: &'a [Space]) -> Option<&'a str> {
    spaces
        .iter()
        .flat_map(|s| &s.channels)
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .and_then(|c| c.untunable.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dt295::chan::SpaceKind;

    const INI: &str = "[Tuner]\r\n\
        DeviceIndex=2\r\n\
        TrcFirmware=\"fw\\trc.bin\"\r\n\
        ; DeviceSerial=x\r\n\
        [BS]  ; satellite\r\n\
        BS15/101=1318000,0x40F1,101\r\n\
        BS01/151=1049480,0x4010\r\n\
        Bad=900000,0x4010\r\n\
        [CS110]\r\n\
        ND02/296=1613000,0x6020,296\r\n\
        [UHF]\r\n\
        27ch=nothing\r\n";

    fn ini() -> BonDriver {
        BonDriver::from_ini(&Path::new("cfg").join("dt295.ini"), INI).unwrap()
    }

    fn kinds(b: &BonDriver) -> Vec<SpaceKind> {
        b.spaces.iter().map(|s| s.kind).collect()
    }

    #[test]
    fn tuner_section() {
        let b = ini();
        let dir = Path::new("cfg");
        assert_eq!(b.serial(), None);
        assert_eq!(b.tuner.firmware(), dir.join("mb86m01_assp_osec_idle.bin"));
        assert_eq!(
            b.tuner.trc_firmware(),
            dir.join("mb86m01_assp_osec_trc_m_h.bin"),
            "TrcFirmware is no longer read"
        );
        assert_eq!(b.tuner.state_file("ab1"), dir.join("AB1.state"));
        use SpaceKind::*;
        assert_eq!(kinds(&b), [Uhf, Catv, Bs, Cs110]);
        assert_eq!(
            b.spaces[2].channels.len(),
            2,
            "the line out of range is none"
        );
    }

    #[test]
    fn spaces_as_the_bondriver_lists_them() {
        let b = |text: &str| BonDriver::from_ini(Path::new("d.ini"), text).unwrap();
        use SpaceKind::*;
        // GetPrivateProfileInt: an empty value is the default (on), as is no key.
        assert_eq!(kinds(&b("[Tuner]\nUHF=\nCATV=0\n")), [Uhf]);
        // Only the first [BS] section counts.
        let two = b("[BS]\nA=1049480,0x4010\n[BS]\nB=1087840,0x4030\n");
        let names: Vec<_> = two.spaces[2].channels.iter().map(|c| &c.name).collect();
        assert_eq!(names, ["A"]);
        // A ChannelFile that is not there: no spaces, but the ini still reads.
        let gone = b("[Tuner]\nDeviceSerial=WI01\nChannelFile=missing.ChSet.txt\n");
        assert_eq!((gone.serial(), gone.spaces.len()), (Some("WI01"), 0));
    }

    #[test]
    fn channel_names() {
        let b = ini();
        let tuned = |name: &str| tune(name, &b.spaces).map(|c| (c.sys, c.freq_hz, c.tsid, c.sid));
        let sat = |hz, tsid, sid| Some((System::Satellite, hz, tsid, sid));
        let ter = |hz| Some((System::Terrestrial, hz, NO_TSID, None));
        assert_eq!(tuned("BS15/101"), sat(1_318_000_000, 0x40F1, Some(101)));
        assert_eq!(tuned("bs01/151"), sat(1_049_480_000, 0x4010, None));
        assert_eq!(tuned("ND02/296"), sat(1_613_000_000, 0x6020, Some(296)));
        assert_eq!(tuned("Bad"), None, "IF out of range");
        assert_eq!(tuned("27ch"), ter(557_142_857));
        assert_eq!(tuned("C13CH"), ter(111_142_857));
        assert_eq!(tuned("12ch"), None);
        assert_eq!(tuned("BS99/1"), None);
        assert_eq!(
            tune(" 62ch", &[]).map(|c| (c.sys, c.freq_hz)),
            Some((System::Terrestrial, 767_142_857)),
            "built in without an ini"
        );
        // why a placeholder of a channel file does not tune; none for a channel that
        // tunes or is not there
        let file = [Space {
            name: "S".into(),
            kind: SpaceKind::File,
            channels: vec![Channel {
                untunable: Some("left-hand".into()),
                ..tune("BS15/101", &b.spaces).unwrap()
            }],
        }];
        assert_eq!(tune("bs15/101", &file), None);
        assert_eq!(untunable("bs15/101", &file), Some("left-hand"));
        assert_eq!(untunable("BS15/101", &b.spaces), None);
        assert_eq!(untunable("BS99/1", &b.spaces), None);
    }

    #[test]
    fn options_given_win() {
        let b = BonDriver::from_ini(
            Path::new("dt295.ini"),
            "[Tuner]\nDeviceSerial=EF34567890\nStateFile=s.state\n",
        )
        .unwrap();
        let none = DeviceArgs {
            fw: None,
            trc: None,
            index: 0,
            serial: None,
            state: None,
            state_dir: None,
        };
        let d = b.device_args(&none);
        assert_eq!(d.serial.as_deref(), Some("EF34567890"));
        assert_eq!(d.state, None, "StateFile is no longer read");
        assert_eq!(d.state_dir, Some(PathBuf::new()), "the ini's folder");
        assert_eq!(d.fw, Some(PathBuf::from("mb86m01_assp_osec_idle.bin")));
        let d = b.device_args(&DeviceArgs {
            state: Some("given.state".into()),
            ..none.clone()
        });
        assert_eq!(d.state, Some(PathBuf::from("given.state")));

        let given = DeviceArgs {
            fw: Some("f".into()),
            serial: Some("X".into()),
            ..none.clone()
        };
        let d = b.device_args(&given);
        assert_eq!((d.fw, d.serial.as_deref()), (Some("f".into()), Some("X")));
        // -i given: the ini's serial no longer applies
        let d = b.device_args(&DeviceArgs { index: 1, ..none });
        assert_eq!((d.index, d.serial), (1, None));
    }

    #[test]
    fn tuner_keys_as_the_bondriver_reads_them() {
        let b = |text: &str| BonDriver::from_ini(Path::new("d.ini"), text).unwrap();
        let none = DeviceArgs {
            fw: None,
            trc: None,
            index: 0,
            serial: None,
            state: None,
            state_dir: None,
        };
        // TrcFirmware and DeviceIndex are no longer read
        assert_eq!(
            b("[Tuner]\nTrcFirmware=\n").device_args(&none).trc,
            Some(PathBuf::from("mb86m01_assp_osec_trc_m_h.bin"))
        );
        assert_eq!(b("[Tuner]\nDeviceIndex=3\n").device_args(&none).index, 0);
        // DeviceSerial: blanks trimmed (inside quotes too), upper case.
        let d = b("[Tuner]\nDeviceSerial=\" ef34567890 \"\n").device_args(&none);
        assert_eq!(d.serial.as_deref(), Some("EF34567890"));
    }
}
