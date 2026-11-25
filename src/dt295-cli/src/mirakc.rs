//! `dt295 mirakc-channels`: the `channels:` of a mirakc configuration from a TVTest
//! channel scan (`.ch2`) and the BonDriver_DT295 ini it was made with, or (Linux,
//! `--scan`) from a scan through `dt295 serve` (`mirakc/scan.rs`).
//!
//! One mirakc channel per TS: `extra-args` carries the tune for `dt295 tune` (`--ter
//! HZ` or `--sat HZ --tsid HEX`), which streams every service of the TS, and
//! `services` lists the TS's services (terrestrial sub-channels too), so mirakc
//! serves several services of a TS from one tune and `dt295 serve` needs no channel
//! table. `--per-service`: one channel per service instead, each tune with `--sid`.
//! The `channel` strings only need to be unique per type (mirakc merges equal type
//! + channel).
//!
//! `--logos DIR` adds mirakc's `resource.logos` for the services with a logo there
//! (`<service id>.png`, as `dt295 logo --mirakc` writes them): mirakc does not collect
//! logos itself, and its configuration is a single file.
//!
//! A `.ch2` numbers the tuning spaces the way the BonDriver lists them, and the ini
//! gives them with the BonDriver's own reading ([`dt295::chan::ini_spaces`]; the spaces of
//! its `ChannelFile` too, whose channels are GR, BS or CS by what they tune).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use dt295::System;
use dt295::chan::{NO_TSID, Space, SpaceKind};

use crate::bonini::BonDriver;
use dt295::ch2;

mod psi;
mod scan;

#[derive(Debug, Args)]
pub struct MirakcArgs {
    /// The BonDriver_DT295 ini of a TVTest channel scan: its [BS] / [CS110] lines and
    /// its [Tuner] UHF / CATV switches.
    #[arg(
        long,
        value_name = "INI",
        required_unless_present = "scan",
        conflicts_with = "scan"
    )]
    pub bondriver: Option<PathBuf>,

    /// TVTest's channel file from that scan (default: the ini's name with .ch2).
    #[arg(long, value_name = "FILE", conflicts_with = "scan")]
    pub ch2: Option<PathBuf>,

    /// (Linux) Scan with the unit --serial through `dt295 serve` instead (about 6
    /// minutes, 10 with --on-air; only while nothing records).
    #[arg(long, requires = "serial", hide = cfg!(not(unix)))]
    pub scan: bool,

    /// (Linux) The unit to scan with, by USB serial number.
    #[arg(long, value_name = "SERIAL", conflicts_with = "bondriver", hide = cfg!(not(unix)))]
    pub serial: Option<String>,

    /// (Linux) Keep only the satellite services on the air now (each is tuned; about 5
    /// minutes more). Without it every TV and radio service of the SDT is listed,
    /// spare channels off the air too.
    #[arg(long, conflicts_with = "bondriver", hide = cfg!(not(unix)))]
    pub on_air: bool,

    /// (Linux) Scan the CATV channels (C13-C63) too.
    #[arg(long, conflicts_with = "bondriver", hide = cfg!(not(unix)))]
    pub catv: bool,

    /// (Linux) The configuration of `dt295 serve`, for its socket (default
    /// /etc/dt295.conf if it exists).
    #[arg(short, long, value_name = "FILE", conflicts_with = "bondriver", hide = cfg!(not(unix)))]
    pub config: Option<PathBuf>,

    /// (Linux) The socket file of `dt295 serve` (default: $DT295_SOCKET, else the
    /// configuration's, else /run/dt295/dt295.sock).
    #[arg(long, value_name = "PATH", conflicts_with = "bondriver", hide = cfg!(not(unix)))]
    pub socket: Option<PathBuf>,

    /// (Linux) The TCP address of `dt295 serve` instead ($DT295_LISTEN).
    #[arg(long, value_name = "ADDR", conflicts_with_all = ["bondriver", "socket"], hide = cfg!(not(unix)))]
    pub connect: Option<String>,

    /// One channel per service, each tuning that service only (`dt295 tune --sid`);
    /// default: one channel per TS, streaming every service of it.
    #[arg(long)]
    pub per_service: bool,

    /// Also write resource.logos for the services with a `<service id>.png` in DIR
    /// (`dt295 logo --mirakc DIR`); the paths are written as DIR/<id>.png.
    #[arg(long, value_name = "DIR")]
    pub logos: Option<String>,

    /// Output file; default stdout.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

pub fn run(a: &MirakcArgs) -> Result<()> {
    let (channels, skipped, source) = match (&a.bondriver, &a.serial) {
        (Some(ini), _) => from_tvtest(ini, a.ch2.as_deref())?,
        (None, Some(serial)) => from_scan(a, serial)?,
        (None, None) => bail!("give --bondriver, or --scan with --serial"),
    };
    for s in &skipped {
        eprintln!("skipped: {s}");
    }
    if channels.is_empty() {
        bail!("no channel from {source}");
    }
    let groups = if a.per_service {
        unique(&channels)
            .into_iter()
            .map(|c| {
                let sid = c.sid;
                (c, vec![sid])
            })
            .collect()
    } else {
        by_ts(&channels)
    };
    let mut yaml = render(&groups);
    if let Some(dir) = &a.logos {
        let have = |id: u64| Path::new(dir).join(format!("{id}.png")).is_file();
        let logos = render_logos(&channels, dir, have);
        if logos.is_empty() {
            eprintln!("no logo of these services in {dir}");
        }
        yaml.push_str(&logos);
    }
    match &a.output {
        Some(p) => fs::write(p, yaml).with_context(|| format!("cannot write {}", p.display()))?,
        None => io::stdout().write_all(yaml.as_bytes())?,
    }
    eprintln!("{} channel(s) from {source}", groups.len());
    Ok(())
}

/// The channels of a TVTest scan, what was left out, and the source's name.
fn from_tvtest(ini: &Path, ch2: Option<&Path>) -> Result<(Vec<Channel>, Vec<String>, String)> {
    let spaces = BonDriver::load(ini)?.spaces;
    let ch2_path = ch2.map_or_else(|| ini.with_extension("ch2"), Path::to_path_buf);
    let services =
        ch2::read(&ch2_path).with_context(|| format!("cannot read {}", ch2_path.display()))?;
    let (channels, skipped) = channels(&spaces, &services);
    Ok((channels, skipped, ch2_path.display().to_string()))
}

#[cfg(unix)]
fn from_scan(a: &MirakcArgs, serial: &str) -> Result<(Vec<Channel>, Vec<String>, String)> {
    let target = crate::serve::conf::client_target(
        a.config.as_deref(),
        a.socket.as_deref(),
        a.connect.as_deref(),
    )?;
    let (channels, skipped) = scan::run(serial, a.catv, a.on_air, &target)?;
    Ok((channels, skipped, format!("a scan with {serial}")))
}

#[cfg(not(unix))]
fn from_scan(_: &MirakcArgs, _: &str) -> Result<(Vec<Channel>, Vec<String>, String)> {
    bail!("--scan is only on Linux (it scans through dt295 serve)")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Channel {
    name: String,
    kind: &'static str,
    channel: String,
    /// The tune of the TS (`extra-args` of a channel per TS).
    args: String,
    nid: u16,
    sid: u16,
    /// The name of the TS's channel when one channel streams the whole TS (`UHF 27ch`,
    /// `BS15/TS1`).
    ts: String,
}

impl Channel {
    fn service_id(&self) -> u64 {
        service_id(self.nid, self.sid)
    }
}

/// mirakc's id of a service (`dt295 logo --mirakc` names its files by it).
pub(crate) fn service_id(nid: u16, sid: u16) -> u64 {
    u64::from(nid) * 100_000 + u64::from(sid)
}

/// The channels of the enabled services, and why the others that are enabled were
/// left out.
fn channels(spaces: &[Space], services: &[ch2::Service]) -> (Vec<Channel>, Vec<String>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for s in services.iter().filter(|s| s.enabled) {
        let what = format!("{} (service {})", s.name, s.sid);
        let ch = s.channel;
        let space = spaces.get(s.space as usize);
        let line = space.and_then(|sp| Some((sp, sp.channels.get(ch as usize)?)));
        if let Some((sp, c)) = line
            && let Some(why) = &c.untunable
        {
            skipped.push(format!(
                "{what}: {} {} is no channel to tune ({why})",
                sp.name, c.name
            ));
            continue;
        }
        // a channel file's: by what it tunes (a BS IF is below 1.6 GHz, a CS110 one above)
        let kind = |sp: &Space, c: &dt295::chan::Channel| match sp.kind {
            SpaceKind::File if c.sys == System::Terrestrial => SpaceKind::Uhf,
            SpaceKind::File if c.freq_hz < 1_600_000_000 => SpaceKind::Bs,
            SpaceKind::File => SpaceKind::Cs110,
            k => k,
        };
        let mapped = line.map(|(sp, c)| match kind(sp, c) {
            // 27ch: "27", C27ch: "C27"
            SpaceKind::Uhf | SpaceKind::Catv | SpaceKind::File => (
                "GR",
                c.name.strip_suffix("ch").unwrap_or(&c.name).to_owned(),
                format!("--ter {}", c.freq_hz),
                format!("{} {}", sp.name, c.name),
            ),
            k @ (SpaceKind::Bs | SpaceKind::Cs110) => {
                // without a TSID, or with a TS number: the service's
                let tsid = if c.tsid == NO_TSID || c.ts_number().is_some() {
                    s.tsid
                } else {
                    c.tsid
                };
                (
                    if k == SpaceKind::Bs { "BS" } else { "CS" },
                    c.name.replace('/', "_"),
                    format!("--sat {} --tsid {tsid:04x}", c.freq_hz),
                    c.name.clone(),
                )
            }
        });
        let Some((kind, channel, args, ts)) = mapped else {
            skipped.push(format!(
                "{what}: space {} channel {ch} is not in the ini",
                s.space
            ));
            continue;
        };
        if !seen.insert((kind, channel.clone(), s.sid)) {
            continue; // listed twice
        }
        out.push(Channel {
            name: s.name.clone(),
            kind,
            channel,
            args,
            nid: s.nid,
            sid: s.sid,
            ts,
        });
    }
    (out, skipped)
}

/// A YAML double-quoted scalar.
fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One channel per service, each tuning that service only: a channel name already taken
/// (another service of the same terrestrial channel or one-TS ini line) gets the service
/// id appended.
fn unique(channels: &[Channel]) -> Vec<Channel> {
    let mut seen = HashSet::new();
    channels
        .iter()
        .map(|c| {
            let mut c = c.clone();
            c.args = format!("{} --sid {}", c.args, c.sid);
            if !seen.insert((c.kind, c.channel.clone())) {
                c.channel = format!("{}_{}", c.channel, c.sid);
                seen.insert((c.kind, c.channel.clone()));
            }
            c
        })
        .collect()
}

/// One channel per TS: the services' channels with the same tune become the first
/// one's, streaming every service, with all their service ids. It takes the TS's name
/// (a satellite one as its channel too: `BS15_TS1`).
fn by_ts(channels: &[Channel]) -> Vec<(Channel, Vec<u16>)> {
    let mut out: Vec<(Channel, Vec<u16>)> = Vec::new();
    for c in channels {
        let mut c = c.clone();
        c.name.clone_from(&c.ts);
        if c.kind != "GR" {
            c.channel = c.ts.replace('/', "_");
        }
        match out
            .iter_mut()
            .find(|(g, _)| g.kind == c.kind && g.args == c.args)
        {
            Some((_, sids)) if sids.contains(&c.sid) => {}
            Some((_, sids)) => sids.push(c.sid),
            None => {
                let sid = c.sid;
                out.push((c, vec![sid]));
            }
        }
    }
    out
}

/// The channels with their service ids.
fn render(channels: &[(Channel, Vec<u16>)]) -> String {
    let mut out = String::from("channels:\n");
    for (c, sids) in channels {
        let sids: Vec<String> = sids.iter().map(u16::to_string).collect();
        let _ = write!(
            out,
            "  - name: {}\n    type: {}\n    channel: {}\n    extra-args: {}\n    services: [{}]\n",
            quoted(&c.name),
            c.kind,
            quoted(&c.channel),
            quoted(&c.args),
            sids.join(", ")
        );
    }
    out
}

/// `resource.logos` for the channels `have` a logo for (empty if none), each service
/// once.
fn render_logos(channels: &[Channel], dir: &str, have: impl Fn(u64) -> bool) -> String {
    let dir = dir.trim_end_matches(['/', '\\']);
    let mut out = String::new();
    let mut seen = HashSet::new();
    for id in channels
        .iter()
        .map(Channel::service_id)
        .filter(|&id| seen.insert(id) && have(id))
    {
        if out.is_empty() {
            out.push_str("resource:\n  logos:\n");
        }
        let _ = write!(
            out,
            "    - service-id: {id}\n      image: {}\n",
            quoted(&format!("{dir}/{id}.png"))
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "[Tuner]\nCATV=0\n[BS]\nBS15/101=1318000,0x40F1,101\nbad=12,1\n\
                       BS15/102=1318000,0x40F1,102\n[CS110]\nND02/296=1613000,0x6020,296\n";

    fn spaces(ini: &str) -> Vec<Space> {
        BonDriver::from_ini(Path::new("d.ini"), ini).unwrap().spaces
    }

    fn svc(name: &str, space: u32, channel: u32, sid: u16, enabled: bool) -> ch2::Service {
        ch2::Service {
            name: name.into(),
            space,
            channel,
            service_type: 1,
            sid,
            nid: match space {
                0 => 32736,
                1 => 4,
                _ => 6,
            },
            tsid: 0,
            enabled,
        }
    }

    #[test]
    fn spaces_follow_the_bondriver() {
        let s = spaces(INI);
        let kinds: Vec<_> = s.iter().map(|s| s.kind).collect();
        use SpaceKind::*;
        assert_eq!(kinds, [Uhf, Bs, Cs110], "CATV off");
        assert_eq!(
            s[1].channels.len(),
            2,
            "the line that does not parse is not a channel"
        );
        // An empty UHF= is on, as GetPrivateProfileInt reads it: a scan made with this
        // ini numbers BS as space 1.
        let s = spaces("[Tuner]\nUHF=\nCATV=0\n[BS]\nBS15/TS1=1318000,0x40F1\n");
        let kinds: Vec<_> = s.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, [Uhf, Bs]);
    }

    #[test]
    fn channels_of_a_channel_file() {
        let dir = std::env::temp_dir().join(format!("dt295-mirakc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("BonDriver_PT3-S.ChSet.txt"),
            "$BS\t0\n$CS110\t1\n\
             BS15/TS1\t0\t0\t7\t1\nBS15/TS0\t0\t1\t7\t16624\n\
             ND02\t1\t0\t12\t0\nBS99\t1\t1\t99\t0\n",
        )
        .unwrap();
        let ini = "[Tuner]\nChannelFile=BonDriver_PT3-S.ChSet.txt\n[BS]\nA=1049480\n";
        let b = BonDriver::from_ini(&dir.join("BonDriver_DT295.ini"), ini);
        fs::remove_dir_all(&dir).unwrap();
        let mut services = [
            svc("NHK BS", 0, 0, 101, true),
            svc("BSx", 0, 1, 103, true),
            svc("TBS1", 1, 0, 296, true),
            svc("none", 1, 1, 1, true),
        ];
        services[0].tsid = 0x40F1;
        services[2].tsid = 0x6020;
        let (ch, skipped) = channels(&b.unwrap().spaces, &services);
        let got: Vec<_> = ch
            .iter()
            .map(|c| (c.kind, c.channel.as_str(), c.args.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("BS", "BS15_TS1", "--sat 1318000000 --tsid 40f1"),
                ("BS", "BS15_TS0", "--sat 1318000000 --tsid 40f0"),
                ("CS", "ND02", "--sat 1613000000 --tsid 6020"),
            ],
            "a TS number takes the service's TSID"
        );
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].contains("BS99 is no channel to tune"),
            "{skipped:?}"
        );
    }

    #[test]
    fn channels_from_a_scan() {
        let services = [
            svc("NHK G", 0, 14, 1024, true),
            svc("NHK G2", 0, 14, 1025, true), // same frequency: a channel name of its own
            svc("off", 0, 13, 1032, false),
            svc("NHK BS", 1, 0, 101, true),
            svc("BS2", 1, 1, 102, true),
            svc("TBS1", 2, 0, 296, true),
            svc("lost", 3, 0, 1, true),
        ];
        let (ch, skipped) = channels(&spaces(INI), &services);
        let ch = unique(&ch);
        assert_eq!(ch.len(), 5);
        assert_eq!(skipped.len(), 1);
        assert_eq!((ch[1].channel.as_str(), ch[1].sid), ("27_1025", 1025));
        let ch: Vec<Channel> = ch.into_iter().filter(|c| c.sid != 1025).collect();
        assert_eq!(
            ch[0],
            Channel {
                name: "NHK G".into(),
                kind: "GR",
                channel: "27".into(),
                args: "--ter 557142857 --sid 1024".into(),
                nid: 32736,
                sid: 1024,
                ts: "UHF 27ch".into(),
            }
        );
        assert_eq!(ch[1].channel, "BS15_101");
        assert_eq!(ch[1].args, "--sat 1318000000 --tsid 40f1 --sid 101");
        assert_eq!((ch[3].kind, ch[3].channel.as_str()), ("CS", "ND02_296"));
        let yaml = render(&[(ch[0].clone(), vec![1024])]);
        assert_eq!(
            yaml,
            "channels:\n  - name: \"NHK G\"\n    type: GR\n    channel: \"27\"\n    \
             extra-args: \"--ter 557142857 --sid 1024\"\n    services: [1024]\n"
        );
    }

    #[test]
    fn one_channel_per_ts() {
        let services = [
            svc("NHK G", 0, 14, 1024, true),
            svc("NHK G2", 0, 14, 1025, true),
            svc("NHK BS", 1, 0, 101, true),
            svc("BS2", 1, 1, 102, true), // another line of the same TS
            svc("TBS1", 2, 0, 296, true),
        ];
        let (ch, skipped) = channels(&spaces(INI), &services);
        assert_eq!((ch.len(), skipped.len()), (5, 0));
        let g = by_ts(&ch);
        let got: Vec<(&str, &str, &str, &[u16])> = g
            .iter()
            .map(|(c, sids)| {
                let c = (c.name.as_str(), c.channel.as_str(), c.args.as_str());
                (c.0, c.1, c.2, sids.as_slice())
            })
            .collect();
        // a TS of service lines is named after its first line
        assert_eq!(
            got,
            [
                ("UHF 27ch", "27", "--ter 557142857", &[1024, 1025][..]),
                (
                    "BS15/101",
                    "BS15_101",
                    "--sat 1318000000 --tsid 40f1",
                    &[101, 102][..]
                ),
                (
                    "ND02/296",
                    "ND02_296",
                    "--sat 1613000000 --tsid 6020",
                    &[296][..]
                ),
            ]
        );
        assert!(render(&g).contains("    services: [1024, 1025]\n"));
    }

    #[test]
    fn lines_of_a_whole_ts() {
        const PER_TS: &str = "[Tuner]\nCATV=0\n[BS]\nBS15/TS1=1318000,0x40F1\n";
        let services = [svc("NHK BS", 1, 0, 101, true), svc("BS2", 1, 0, 102, true)];
        let (ch, skipped) = channels(&spaces(PER_TS), &services);
        assert!(skipped.is_empty());
        let one = unique(&ch);
        let got: Vec<(&str, &str)> = one
            .iter()
            .map(|c| (c.channel.as_str(), c.args.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("BS15_TS1", "--sat 1318000000 --tsid 40f1 --sid 101"),
                ("BS15_TS1_102", "--sat 1318000000 --tsid 40f1 --sid 102"),
            ]
        );
        let g = by_ts(&ch);
        assert_eq!(g.len(), 1);
        assert_eq!(
            (
                g[0].0.name.as_str(),
                g[0].0.channel.as_str(),
                g[0].0.args.as_str(),
                g[0].1.as_slice()
            ),
            (
                "BS15/TS1",
                "BS15_TS1",
                "--sat 1318000000 --tsid 40f1",
                &[101, 102][..]
            )
        );
    }

    #[test]
    fn logos_for_the_channels_that_have_one() {
        let services = [
            svc("NHK G", 0, 14, 1024, true),
            svc("NHK BS", 1, 0, 101, true),
        ];
        let (ch, _) = channels(&spaces(INI), &services);
        assert_eq!(ch[0].service_id(), 3_273_601_024);
        assert_eq!(render_logos(&ch, "/l/", |_| false), "");
        assert_eq!(
            render_logos(&ch, "/var/lib/dt295/logos/", |id| id == 400_101),
            "resource:\n  logos:\n    - service-id: 400101\n      \
             image: \"/var/lib/dt295/logos/400101.png\"\n"
        );
    }

    #[test]
    fn arguments() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            crate::Cli::try_parse_from(["dt295", "mirakc-channels"].iter().chain(args))
        };
        assert!(parse(&["--bondriver", "b.ini"]).is_ok());
        assert!(parse(&["--scan", "--serial", "AB12345678", "--catv"]).is_ok());
        assert!(parse(&[]).is_err(), "--bondriver or --scan");
        assert!(parse(&["--scan"]).is_err(), "--scan needs --serial");
        assert!(parse(&["--serial", "X", "--bondriver", "b.ini"]).is_err());
        assert!(parse(&["--scan", "--serial", "X", "--bondriver", "b.ini"]).is_err());
        assert!(parse(&["--scan", "--serial", "X", "--ch2", "c"]).is_err());
        assert!(parse(&["--bondriver", "b.ini", "--catv"]).is_err());
    }

    #[test]
    fn yaml_quoting() {
        assert_eq!(quoted(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(quoted("x\ty"), "\"x\\u0009y\"");
    }
}
