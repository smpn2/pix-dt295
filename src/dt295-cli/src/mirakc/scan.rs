//! `dt295 mirakc-channels --scan --serial <SERIAL>`: the channels from a scan through
//! `dt295 serve`, without a TVTest scan (Linux).
//!
//! Terrestrial: each UHF channel (and CATV with `--catv`) is tuned without a service,
//! so every service of it streams; every TV and radio service of the PAT is taken,
//! with its network and type from the SDT. A channel that does not lock is skipped
//! after the daemon's lock wait (3 s).
//!
//! Satellite: one network at a time (BS, and the two CS110 networks), a known TS is
//! tuned; its SDT\[actual\] lists its own services and its SDT\[other\] those of
//! every other TS of the network (a second TS of the network adds what the first
//! one's SDT\[other\] lacks). The TV and radio services among them are
//! listed, those off the air now too (a broadcaster's spare channels: mirakc finds them
//! once they air). With `--on-air` each is tuned, as a TVTest scan does, and kept only
//! if its PAT comes (dt295 sends nothing until a PAT lists the service).

// the scan itself is for Linux; the rest is tested everywhere
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::{BTreeMap, HashSet};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use anyhow::{Context, Result, bail};
use dt295::chan::{self, transponder};
#[cfg(unix)]
use dt295::ipc::{self, Target};

use super::Channel;
use super::psi::{Psi, Table};
#[cfg(unix)]
use crate::serve::proto::{Reply, Request, Tuning};

/// The service types a scan lists: digital TV and digital radio (TVTest's defaults;
/// not data, temporary or promo services, nor 4K, which the device cannot output).
const LISTED_TYPES: [u8; 2] = [0x01, 0x02];

/// A known TS of each satellite network to start from: (label, network,
/// transport_stream_id); the frequency is its transponder's ([`transponder`]).
const SEEDS: [(&str, u16, u16); 3] = [
    ("BS", 4, 0x40F1),
    ("CS110 network 6", 6, 0x6020),
    ("CS110 network 7", 7, 0x7040),
];

/// The terrestrial channels to try: (label, mirakc channel, frequency in Hz).
fn terrestrial_channels(catv: bool) -> Vec<(String, String, u32)> {
    let uhf =
        chan::UHF.filter_map(|n| Some((format!("UHF {n}ch"), n.to_string(), chan::uhf_hz(n)?)));
    let catv = chan::CATV
        .filter(|_| catv)
        .filter_map(|n| Some((format!("CATV C{n}ch"), format!("C{n}"), chan::catv_hz(n)?)));
    uhf.chain(catv).collect()
}

/// The channels of a terrestrial tune (every service streamed): the TV and radio
/// services of the PAT, their network from the SDT; an error says why there is none.
fn terrestrial(label: &str, channel: &str, hz: u32, psi: &Psi) -> Result<Vec<Channel>, String> {
    let pat = psi.pat.as_deref().ok_or("no PAT")?;
    let sids: Vec<u16> = pat.iter().map(|&(prog, _)| prog).collect();
    let tables = psi.tables();
    let in_sdt = |sid: u16| {
        tables.iter().filter(|t| t.actual).find_map(|t| {
            let stype = t.services.iter().find(|&&(s, _)| s == sid)?.1;
            Some((t.onid, stype))
        })
    };
    let mut out = Vec::new();
    let mut why = None;
    for sid in sids {
        let Some((onid, stype)) = in_sdt(sid) else {
            why.get_or_insert_with(|| format!("service {sid} is not in the SDT"));
            continue;
        };
        if !stype.is_some_and(|t| LISTED_TYPES.contains(&t)) {
            why.get_or_insert_with(|| {
                format!(
                    "service {sid} is no TV or radio service (type {})",
                    stype.map_or("-".into(), |t| format!("0x{t:02X}"))
                )
            });
            continue;
        }
        out.push(Channel {
            name: label.to_owned(),
            kind: "GR",
            channel: channel.to_owned(),
            args: format!("--ter {hz}"),
            nid: onid,
            sid,
            ts: label.to_owned(),
        });
    }
    match why {
        Some(why) if out.is_empty() => Err(why),
        _ => Ok(out),
    }
}

/// A satellite service a scan may list: (network, transport_stream_id, service_id).
type SatService = (u16, u16, u16);

/// The TV and radio services of network `onid`, from the whole SDT sub-tables the
/// tunes gave: per TS the union of its sections. Only TSes of right-hand transponders,
/// in TSID order; services in id order.
fn satellite_services(onid: u16, tables: &[Table]) -> Vec<SatService> {
    let mut by_ts: BTreeMap<u16, BTreeMap<u16, Option<u8>>> = BTreeMap::new();
    for t in tables
        .iter()
        .filter(|t| t.onid == onid && transponder(t.onid, t.tsid).is_some())
    {
        let ts = by_ts.entry(t.tsid).or_default();
        for &(sid, stype) in &t.services {
            let e = ts.entry(sid).or_default();
            *e = e.or(stype);
        }
    }
    by_ts
        .into_iter()
        .flat_map(|(tsid, services)| {
            services
                .into_iter()
                .filter(|(_, t)| t.is_some_and(|t| LISTED_TYPES.contains(&t)))
                .map(move |(sid, _)| (onid, tsid, sid))
        })
        .collect()
}

/// Another TS of network `onid` than `tsid` to tune for `tsid`'s own services.
fn second_ts(onid: u16, tsid: u16, tables: &[Table]) -> Option<u16> {
    tables
        .iter()
        .filter(|t| !t.actual && t.onid == onid && t.tsid != tsid)
        .map(|t| t.tsid)
        .find(|&ts| transponder(onid, ts).is_some())
}

/// The mirakc channel of a satellite service (named like a BonDriver ini line).
fn satellite(onid: u16, tsid: u16, sid: u16) -> Option<Channel> {
    let tp = transponder(onid, tsid)?;
    let name = tp.service_name(sid);
    Some(Channel {
        channel: name.replace('/', "_"),
        name,
        kind: if tp.bs { "BS" } else { "CS" },
        args: format!("--sat {} --tsid {tsid:04x}", tp.if_khz * 1000),
        nid: onid,
        sid,
        ts: tp.ts_name(tsid),
    })
}

/// Keep the first channel of each service (a station received on two channels).
fn dedup(channels: Vec<Channel>, skipped: &mut Vec<String>) -> Vec<Channel> {
    let mut seen = HashSet::new();
    channels
        .into_iter()
        .filter(|c| {
            let first = seen.insert(c.service_id());
            if !first {
                skipped.push(format!(
                    "{}: service {} is on an earlier channel too",
                    c.name, c.sid
                ));
            }
            first
        })
        .collect()
}

/// How long a tune is read: the terrestrial PSI comes within a second; a satellite
/// SDT\[other\] goes round in about 13 s; a PAT that lists a service comes ten times a
/// second.
#[cfg(unix)]
const TERRESTRIAL_READ: Duration = Duration::from_secs(5);
#[cfg(unix)]
const SDT_READ: Duration = Duration::from_secs(16);
#[cfg(unix)]
const SERVICE_READ: Duration = Duration::from_secs(4);
/// The daemon may boot the unit before it answers.
#[cfg(unix)]
const REPLY_WAIT: Duration = Duration::from_mins(1);

/// The scan: the channels, and what was left out (and why).
#[cfg(unix)]
pub(super) fn run(
    serial: &str,
    catv: bool,
    on_air: bool,
    target: &Target,
) -> Result<(Vec<Channel>, Vec<String>)> {
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    let t0 = Instant::now();
    for (label, channel, hz) in terrestrial_channels(catv) {
        let tuning = Tuning::Terrestrial { hz };
        let done = |psi: &Psi, _: Duration| psi.sdt_actual_complete() && psi.pat.is_some();
        match tune(target, serial, tuning, None, TERRESTRIAL_READ, done)? {
            Tuned::Refused(why) if why.starts_with("no lock") => {}
            Tuned::Refused(why) => skipped.push(format!("{label}: {why}")),
            Tuned::Stream(psi) => match terrestrial(&label, &channel, hz, &psi) {
                Ok(chs) => {
                    for ch in chs {
                        eprintln!("{label}: service {} (network {})", ch.sid, ch.nid);
                        found.push(ch);
                    }
                }
                Err(why) => skipped.push(format!("{label}: {why}")),
            },
        }
    }
    eprintln!(
        "terrestrial: {} channel(s), {:.0} s",
        found.len(),
        t0.elapsed().as_secs_f64()
    );

    for (label, onid, tsid) in SEEDS {
        let hz = transponder(onid, tsid).map_or_default(|t| t.if_khz * 1000);
        let t1 = Instant::now();
        let mut tables = Vec::new();
        let tuning = Tuning::Satellite { hz, tsid };
        let whole = |_: &Psi, t: Duration| t >= SDT_READ;
        let Tuned::Stream(psi) = tune(target, serial, tuning, None, SDT_READ, whole)? else {
            skipped.push(format!("{label}: TS 0x{tsid:04X} does not lock"));
            continue;
        };
        tables.extend(psi.tables());
        if let Some(other) = second_ts(onid, tsid, &tables) {
            let hz = transponder(onid, other).map_or_default(|t| t.if_khz * 1000);
            let tuning = Tuning::Satellite { hz, tsid: other };
            if let Tuned::Stream(psi) = tune(target, serial, tuning, None, SDT_READ, whole)? {
                tables.extend(psi.tables());
            }
        }
        let services = satellite_services(onid, &tables);
        let mut listed = 0;
        for (onid, tsid, sid) in services {
            let Some(ch) = satellite(onid, tsid, sid) else {
                continue;
            };
            if on_air {
                let hz = transponder(onid, tsid).map_or_default(|t| t.if_khz * 1000);
                let tuning = Tuning::Satellite { hz, tsid };
                let streams = |psi: &Psi, _: Duration| psi.pat.is_some();
                match tune(target, serial, tuning, Some(sid), SERVICE_READ, streams)? {
                    Tuned::Stream(psi) if psi.pat.is_some() => {}
                    Tuned::Stream(_) => {
                        skipped.push(format!("{}: off the air", ch.name));
                        continue;
                    }
                    Tuned::Refused(why) => {
                        skipped.push(format!("{}: {why}", ch.name));
                        continue;
                    }
                }
            }
            listed += 1;
            found.push(ch);
        }
        eprintln!(
            "{label}: {listed} channel(s), {:.0} s",
            t1.elapsed().as_secs_f64()
        );
    }
    Ok((dedup(found, &mut skipped), skipped))
}

/// How one tune of a scan went.
#[cfg(unix)]
enum Tuned {
    /// The stream ran; what it gave.
    Stream(Psi),
    /// The daemon could not stream it (no lock, ...).
    Refused(String),
}

/// Stream `tuning` on `unit` if it is free, and read it until `enough` says so,
/// `limit` passes or the stream ends (another client took the unit, or it went away).
#[cfg(unix)]
fn tune(
    target: &Target,
    serial: &str,
    tuning: Tuning,
    sid: Option<u16>,
    limit: Duration,
    enough: impl Fn(&Psi, Duration) -> bool,
) -> Result<Tuned> {
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};

    let req = Request {
        unit: serial.to_owned(),
        tuner: 1,
        tuning,
        sid,
        mode: None,
        client: None,
        if_free: true,
    };
    let conn = ipc::connect(target)
        .with_context(|| format!("cannot connect to dt295 serve at {target} (is it running?)"))?;
    conn.set_read_timeout(Some(REPLY_WAIT))?;
    (&conn)
        .write_all(format!("{}\n", req.to_line()).as_bytes())
        .context("cannot send the request")?;
    let mut ts = BufReader::with_capacity(1 << 16, conn);
    let mut line = String::new();
    ts.read_line(&mut line)
        .context("no reply from dt295 serve")?;
    match Reply::parse(&line) {
        _ if line.is_empty() => bail!("dt295 serve closed the connection"),
        Reply::Ok { .. } => {}
        Reply::Busy(why) => bail!("{why}: scan when nothing records"),
        Reply::Error(why) => return Ok(Tuned::Refused(why)),
    }
    let t0 = Instant::now();
    let mut psi = Psi::default();
    psi.feed(ts.buffer());
    let sock = ts.into_inner();
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut buf = vec![0u8; 1 << 16];
    while !enough(&psi, t0.elapsed()) && t0.elapsed() < limit {
        match (&sock).read(&mut buf) {
            Ok(0) => break,
            Ok(n) => psi.feed(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e).context("reading the TS"),
        }
    }
    Ok(Tuned::Stream(psi))
}

#[cfg(test)]
mod tests {
    use super::super::psi::tests::{Ts, pat, sdt};
    use super::*;

    #[test]
    fn seeds_are_receivable_transponders() {
        let hz: Vec<_> = SEEDS
            .iter()
            .map(|&(_, onid, tsid)| transponder(onid, tsid).map(|t| t.if_khz * 1000))
            .collect();
        assert_eq!(
            hz,
            [
                Some(1_318_000_000),
                Some(1_613_000_000),
                Some(1_653_000_000)
            ]
        );
    }

    #[test]
    fn terrestrial_takes_every_tv_service() {
        let mut ts = Ts::default();
        ts.add(
            0,
            &pat(0x7FE0, &[(1024, 0x1F0), (1025, 0x3F0), (1408, 0x1FC8)]),
        )
        .add(
            0x11,
            &sdt(
                true,
                0x7FE0,
                32736,
                (0, 0),
                &[(1024, 1), (1025, 1), (1408, 0xC0)],
            ),
        );
        let mut psi = Psi::default();
        psi.feed(&ts.bytes);
        let all = terrestrial("UHF 27ch", "27", 557_142_857, &psi).unwrap();
        // the TV services of the PAT (not the one-seg)
        let sids: Vec<u16> = all.iter().map(|c| c.sid).collect();
        assert_eq!(sids, [1024, 1025]);
        let ch = &all[0];
        assert_eq!(
            (
                ch.kind,
                ch.channel.as_str(),
                ch.args.as_str(),
                ch.nid,
                ch.sid
            ),
            ("GR", "27", "--ter 557142857", 32736, 1024)
        );
        assert_eq!(ch.service_id(), 3_273_601_024);
    }

    #[test]
    fn terrestrial_failures_say_why() {
        let why = |ts: &Ts| {
            let mut psi = Psi::default();
            psi.feed(&ts.bytes);
            terrestrial("UHF 13ch", "13", 473_142_857, &psi).unwrap_err()
        };
        assert_eq!(why(&Ts::default()), "no PAT");
        let mut ts = Ts::default();
        ts.add(0, &pat(1, &[(1024, 0x1F0)]));
        assert_eq!(why(&ts), "service 1024 is not in the SDT");
        ts.add(0x11, &sdt(true, 1, 32736, (0, 0), &[(1024, 0xC0)]));
        assert_eq!(
            why(&ts),
            "service 1024 is no TV or radio service (type 0xC0)"
        );
    }

    fn table(actual: bool, tsid: u16, onid: u16, services: &[(u16, u8)]) -> Table {
        Table {
            actual,
            tsid,
            onid,
            services: services.iter().map(|&(s, t)| (s, Some(t))).collect(),
        }
    }

    #[test]
    fn satellite_services_from_the_sdts() {
        let tables = [
            // tuned BS-15: [actual] reduced to 101, [other] of the rest
            table(true, 0x40F1, 4, &[(101, 1)]),
            table(false, 0x4010, 4, &[(151, 1), (753, 0xC0)]),
            table(false, 0x4011, 4, &[(161, 1), (169, 0xA1)]),
            table(false, 0x46D2, 4, &[(231, 1), (531, 2)]),
            // BS-2 is left-hand
            table(false, 0x4020, 4, &[(999, 1)]),
            // then BS-1: BS-15's [other]
            table(true, 0x4010, 4, &[(151, 1)]),
            table(false, 0x40F1, 4, &[(101, 1), (102, 1), (700, 0xC0)]),
            // another network
            table(false, 0x6080, 6, &[(55, 1)]),
        ];
        assert_eq!(second_ts(4, 0x40F1, &tables), Some(0x4010));
        assert_eq!(
            satellite_services(4, &tables),
            [
                (4, 0x4010, 151),
                (4, 0x4011, 161),
                (4, 0x40F1, 101),
                (4, 0x40F1, 102),
                (4, 0x46D2, 231),
                (4, 0x46D2, 531),
            ]
        );
        let ch = satellite(4, 0x46D2, 531).unwrap();
        assert_eq!(
            (
                ch.name.as_str(),
                ch.kind,
                ch.channel.as_str(),
                ch.args.as_str(),
                ch.ts.as_str()
            ),
            (
                "BS13/531",
                "BS",
                "BS13_531",
                "--sat 1279640000 --tsid 46d2",
                "BS13/TS2"
            )
        );
        let ch = satellite(7, 0x7040, 250).unwrap();
        assert_eq!(
            (
                ch.name.as_str(),
                ch.kind,
                ch.args.as_str(),
                ch.service_id(),
                ch.ts.as_str()
            ),
            (
                "ND04/250",
                "CS",
                "--sat 1653000000 --tsid 7040",
                700_250,
                "ND04/TS0"
            )
        );
    }

    #[test]
    fn the_channel_lists() {
        let uhf = terrestrial_channels(false);
        assert_eq!(uhf.len(), 50);
        assert_eq!(uhf[14], ("UHF 27ch".into(), "27".into(), 557_142_857));
        let all = terrestrial_channels(true);
        assert_eq!(all.len(), 101);
        assert_eq!(all[50], ("CATV C13ch".into(), "C13".into(), 111_142_857));
    }

    #[test]
    fn one_channel_per_service() {
        let ch = |name: &str, sid| Channel {
            name: name.into(),
            kind: "GR",
            channel: name.into(),
            args: String::new(),
            nid: 32736,
            sid,
            ts: name.into(),
        };
        let mut skipped = Vec::new();
        let kept = dedup(
            vec![ch("27", 1024), ch("30", 1024), ch("26", 1032)],
            &mut skipped,
        );
        assert_eq!(kept.len(), 2);
        assert_eq!(skipped, ["30: service 1024 is on an earlier channel too"]);
    }
}
