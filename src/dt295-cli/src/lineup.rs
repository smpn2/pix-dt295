//! `dt295 lineup`: regenerate the `[BS]` and `[CS110]` channel sections of
//! BonDriver_DT295.ini from the SDT sections cached in the BonDriver's state file.
//!
//! Per transport_stream_id (`[actual]` preferred over `[other]`) the services whose
//! service_descriptor type is digital TV (0x01), digital radio (0x02), 4K (0xAD) or
//! data broadcasting (0xC0), plus promo (0xA5) with `--promo`, are listed. Each TS
//! gets one line without a ServiceID: the BonDriver streams every service of it.
//! `--per-service` writes one line per service with its ServiceID instead (the
//! driver then reduces the PAT / SDT of a satellite stream to the tuned service). The service names in the comments
//! come from a `.ch2` channel file; the ARIB strings in the SDT are not decoded.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{Display, Write as _};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use dt295::PsiRecord;
use dt295::chan::{LNB_LO_KHZ, Transponder, rel_ts, transponder};
use dt295::state::State;
use dt295::ts::crc::section_crc_ok;
use dt295::ts::section::descriptors;
use similar::TextDiff;

use dt295::ch2;

#[cfg(test)]
mod tests;

/// `PsiRecord::kind` of an SDT.
const KIND_SDT: u8 = 4;
const TID_SDT_ACTUAL: u8 = 0x42;
const TID_SDT_OTHER: u8 = 0x46;
const TAG_SERVICE: u8 = 0x48;
/// Digital TV, digital radio, 4K and data broadcasting (e.g. the Gガイド service; a
/// host lists those like on any other tuner, TVTest disabled by default).
const LISTED_TYPES: [u8; 4] = [0x01, 0x02, 0xAD, 0xC0];
/// Promo: a channel scan skips one whose PMT never arrives, so off unless `--promo`.
const PROMO_TYPE: u8 = 0xA5;

const STATE_MAGIC: &str = "dt295-state";
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// `dt295 lineup` options.
#[derive(Debug, Args)]
pub struct LineupArgs {
    /// A state file (dt295 serve's <serial>.state, or an older BonDriver_DT295.state).
    pub state: PathBuf,

    /// Channel file with the service names for the comments (default: the --bondriver
    /// ini's name with the extension .ch2, else STATE's, the first present; `-` = none).
    #[arg(long, value_name = "FILE")]
    pub ch2: Option<PathBuf>,

    /// The BonDriver_DT295 ini to update (`--ini` is the same); without --write only
    /// shows the diff.
    #[arg(long, alias = "ini", value_name = "INI")]
    pub bondriver: Option<PathBuf>,

    /// Rewrite the sections of --bondriver in place (keeps INI.bak).
    #[arg(long, requires = "bondriver")]
    pub write: bool,

    /// Limit to one section.
    #[arg(long, value_enum)]
    pub only: Option<Band>,

    /// Also list promo services (service_type 0xA5).
    #[arg(long)]
    pub promo: bool,

    /// Ignore SDT sections last seen more than N days ago.
    #[arg(long, value_name = "N")]
    pub max_age_days: Option<u32>,

    /// One line per service, with its ServiceID; default: one
    /// line per transport stream.
    #[arg(long)]
    pub per_service: bool,
}

/// An ini section `--only` limits the output to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Band {
    /// BS only.
    Bs,
    /// CS110 only.
    Cs,
}

/// Exit codes: 0 ok, 1 a wanted section would be empty, 2 input / ini errors.
pub fn run(args: &LineupArgs) -> ExitCode {
    let code = execute(
        args,
        dt295::today(),
        &mut io::stdout().lock(),
        &mut io::stderr(),
    );
    ExitCode::from(code)
}

/// One status line to stderr.
fn warn(err: &mut dyn Write, msg: impl Display) {
    let _ = writeln!(err, "dt295 lineup: {msg}");
}

/// A whole run: the result to `out`, status lines to `err`; returns the exit code.
fn execute(args: &LineupArgs, today: u32, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let records = match load_state(&args.state, err) {
        Ok(r) => r,
        Err(e) => {
            warn(err, format_args!("{}: {e}", args.state.display()));
            return 2;
        }
    };

    let ch2 = match &args.ch2 {
        Some(p) if p.as_os_str() == "-" => None,
        Some(p) => Some(p.clone()),
        None => default_ch2(&args.state, args.bondriver.as_deref()),
    };
    let names = match &ch2 {
        Some(p) => match read_ch2(p) {
            Ok(n) => n,
            Err(e) => {
                warn(err, format_args!("{}: {e}", p.display()));
                return 2;
            }
        },
        None => ChannelNames::default(),
    };

    let ts = collect_ts(&records, args.max_age_days, today, err);
    let mut wanted = Vec::new();
    if args.only != Some(Band::Cs) {
        wanted.push((
            "BS",
            bs_lines(&ts, &names, args.promo, !args.per_service, err),
        ));
    }
    if args.only != Some(Band::Bs) {
        wanted.push((
            "CS110",
            cs_lines(&ts, &names, args.promo, !args.per_service, err),
        ));
    }
    let counts: Vec<(&str, usize)> = wanted
        .iter()
        .map(|(name, body)| (*name, body.iter().filter(|l| !l.starts_with(';')).count()))
        .collect();
    let lines = counts
        .iter()
        .map(|(name, n)| format!("[{name}] {n} lines"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = match &ch2 {
        Some(p) if names.len() > 0 => format!("{} ({})", p.display(), names.len()),
        _ => "none".to_owned(),
    };
    warn(
        err,
        format_args!(
            "{}: {} sections; {lines}; names: {source}",
            args.state.display(),
            records.len()
        ),
    );
    let empty = counts.iter().any(|&(_, n)| n == 0);
    for (name, _) in counts.iter().filter(|&&(_, n)| n == 0) {
        warn(
            err,
            format_args!(
                "[{name}] would be empty: no valid SDT with a TV, radio or data service for that band"
            ),
        );
    }

    let Some(ini) = &args.bondriver else {
        return match emit(out, &render_sections(&wanted), err) {
            0 => u8::from(empty),
            rc => rc,
        };
    };
    if empty && args.write {
        warn(err, format_args!("refusing to write {}", ini.display()));
        return 1;
    }
    match update_ini(ini, &wanted, args.write, out, err) {
        0 => u8::from(empty),
        rc => rc,
    }
}

/// Write the result text; 2 if stdout fails.
fn emit(out: &mut dyn Write, text: &str, err: &mut dyn Write) -> u8 {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => 0,
        Err(e) => {
            warn(err, format_args!("cannot write the output: {e}"));
            2
        }
    }
}

// ---------------------------------------------------------------- input files

/// The PSI records of a state file.
fn load_state(path: &Path, err: &mut dyn Write) -> Result<Vec<PsiRecord>, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    parse_state(text.trim_prefix('\u{feff}'), err)
}

/// The PSI records of a state file's text; `Err` without a `dt295-state` header
/// (the first line that is not blank or a comment). Unparsable `psi` records are
/// reported and skipped.
fn parse_state(text: &str, err: &mut dyn Write) -> Result<Vec<PsiRecord>, String> {
    let mut header = false;
    for (n, line) in text.lines().enumerate() {
        let mut tok = line.split_ascii_whitespace();
        let Some(keyword) = tok.next().filter(|k| !k.starts_with('#')) else {
            continue;
        };
        if !header {
            if !keyword.eq_ignore_ascii_case(STATE_MAGIC) {
                return Err(format!(
                    "line {}: expected a '{STATE_MAGIC} 1' header",
                    n + 1
                ));
            }
            header = true;
            let version = tok.next();
            if version != Some("1") {
                warn(
                    err,
                    format_args!(
                        "state file version {} (expected 1); reading it anyway",
                        version.unwrap_or("?")
                    ),
                );
            }
        } else if keyword == "psi" && State::parse(line).psi.is_empty() {
            warn(
                err,
                format_args!("line {}: malformed psi record, skipped", n + 1),
            );
        }
    }
    if !header {
        return Err(format!("no '{STATE_MAGIC}' header (empty file?)"));
    }
    Ok(State::parse(text).psi)
}

/// Service names from a `.ch2` channel file, looked up by (tsid, sid), then by
/// (network_id, sid). The keys are the file's numbers as written.
#[derive(Debug, Default)]
struct ChannelNames {
    by_ts: HashMap<(i64, i64), String>,
    by_nid: HashMap<(i64, i64), String>,
}

impl ChannelNames {
    fn get(&self, tsid: u16, onid: u16, sid: u16) -> &str {
        let sid = i64::from(sid);
        self.by_ts
            .get(&(i64::from(tsid), sid))
            .or_else(|| self.by_nid.get(&(i64::from(onid), sid)))
            .map_or_default(String::as_str)
    }

    fn len(&self) -> usize {
        self.by_ts.len()
    }
}

/// The names of a `.ch2` file's services; the first name of a key wins.
fn channel_names(services: &[ch2::Service]) -> ChannelNames {
    let mut names = ChannelNames::default();
    for s in services.iter().filter(|s| !s.name.is_empty()) {
        let sid = i64::from(s.sid);
        names
            .by_ts
            .entry((i64::from(s.tsid), sid))
            .or_insert_with(|| s.name.clone());
        names
            .by_nid
            .entry((i64::from(s.nid), sid))
            .or_insert_with(|| s.name.clone());
    }
    names
}

#[cfg(test)]
fn parse_ch2(text: &str) -> ChannelNames {
    channel_names(&ch2::parse(text))
}

fn read_ch2(path: &Path) -> io::Result<ChannelNames> {
    Ok(channel_names(&ch2::read(path)?))
}

/// The default `.ch2`, the first that exists: TVTest writes it next to the DLL with the
/// DLL's name, as the ini has (`--bondriver`), which `dt295 serve`'s `<serial>.state` does
/// not; else the state file's name with `.ch2`.
fn default_ch2(state: &Path, bondriver: Option<&Path>) -> Option<PathBuf> {
    bondriver
        .into_iter()
        .chain([state])
        .map(|p| p.with_extension("ch2"))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------- SDT -> lineup

/// One SDT section.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Sdt {
    pub(crate) actual: bool,
    pub(crate) tsid: u16,
    pub(crate) onid: u16,
    pub(crate) version: u8,
    pub(crate) section_number: u8,
    pub(crate) last_section_number: u8,
    /// (service_id, service_type of its first service_descriptor), in section order.
    pub(crate) services: Vec<(u16, Option<u8>)>,
}

/// An [`Sdt`] for a CRC-valid, well-formed, current SDT section; else `None`.
pub(crate) fn parse_sdt(sec: &[u8]) -> Option<Sdt> {
    let (mut sdt, loops) = sdt_section(sec)?;
    sdt.services = loops
        .into_iter()
        .map(|(sid, desc)| (sid, service_type(desc)))
        .collect();
    Some(sdt)
}

/// (service_id, its descriptor loop) of each service of an SDT section.
pub(crate) type ServiceLoops<'a> = Vec<(u16, &'a [u8])>;

/// A CRC-valid, well-formed, current SDT section: its [`Sdt`] (without services) and
/// each service's descriptor loop, in section order.
pub(crate) fn sdt_section(sec: &[u8]) -> Option<(Sdt, ServiceLoops<'_>)> {
    if sec.len() < 15 || !matches!(sec[0], TID_SDT_ACTUAL | TID_SDT_OTHER) {
        return None;
    }
    let sl = (usize::from(sec[1] & 0x0F) << 8) | usize::from(sec[2]);
    if sl < 12
        || sec.len() < 3 + sl
        || !section_crc_ok(&sec[..3 + sl])
        || sec[1] & 0x80 == 0
        || sec[5] & 0x01 == 0
    {
        return None;
    }
    let end = 3 + sl - 4;
    let mut loops = Vec::new();
    let mut p = 11;
    while p + 5 <= end {
        let sid = u16::from_be_bytes([sec[p], sec[p + 1]]);
        let dend = p + 5 + ((usize::from(sec[p + 3] & 0x0F) << 8) | usize::from(sec[p + 4]));
        if dend > end {
            return None;
        }
        loops.push((sid, &sec[p + 5..dend]));
        p = dend;
    }
    let sdt = Sdt {
        actual: sec[0] == TID_SDT_ACTUAL,
        tsid: u16::from_be_bytes([sec[3], sec[4]]),
        onid: u16::from_be_bytes([sec[8], sec[9]]),
        version: (sec[5] >> 1) & 0x1F,
        section_number: sec[6],
        last_section_number: sec[7],
        services: Vec::new(),
    };
    Some((sdt, loops))
}

/// The service_type of the first service_descriptor of a descriptor loop.
fn service_type(d: &[u8]) -> Option<u8> {
    descriptors(d).find_map(|(tag, body)| body.first().copied().filter(|_| tag == TAG_SERVICE))
}

/// The services of one transport stream.
#[derive(Debug)]
struct TsInfo {
    onid: u16,
    actual: bool,
    /// service_id -> service_type (`None`: no service_descriptor).
    services: BTreeMap<u16, Option<u8>>,
}

impl TsInfo {
    fn new(onid: u16, actual: bool) -> Self {
        TsInfo {
            onid,
            actual,
            services: BTreeMap::new(),
        }
    }

    /// The TV, radio and data service ids, ascending (with the promo ones if `promo`).
    fn listed(&self, promo: bool) -> Vec<u16> {
        let wanted = |t: u8| LISTED_TYPES.contains(&t) || (promo && t == PROMO_TYPE);
        self.services
            .iter()
            .filter(|(_, t)| t.is_some_and(wanted))
            .map(|(&sid, _)| sid)
            .collect()
    }
}

/// The transport streams of the SDT records, by tsid. Per (tsid, actual,
/// section_number) the most recently seen copy wins (the later one on a tie); a TS
/// with any `[actual]` section ignores its `[other]` ones. Records with a zero
/// `seen_day` count as seen long ago and are never too old.
fn collect_ts(
    records: &[PsiRecord],
    max_age_days: Option<u32>,
    today: u32,
    err: &mut dyn Write,
) -> BTreeMap<u16, TsInfo> {
    let mut best: BTreeMap<(u16, bool, u8), (u32, Sdt)> = BTreeMap::new();
    let (mut bad, mut stale) = (0, 0);
    for r in records.iter().filter(|r| r.kind == KIND_SDT) {
        let Some(sdt) = parse_sdt(&r.section) else {
            bad += 1;
            continue;
        };
        let too_old = max_age_days.is_some_and(|max| {
            r.seen_day != 0 && i64::from(today) - i64::from(r.seen_day) > i64::from(max)
        });
        if too_old {
            stale += 1;
            continue;
        }
        let key = (sdt.tsid, sdt.actual, sdt.section_number);
        if best.get(&key).is_none_or(|&(day, _)| r.seen_day >= day) {
            best.insert(key, (r.seen_day, sdt));
        }
    }
    if bad > 0 {
        warn(
            err,
            format_args!("{bad} SDT section(s) failed the CRC/format check, skipped"),
        );
    }
    if let Some(max) = max_age_days.filter(|_| stale > 0) {
        warn(
            err,
            format_args!("{stale} SDT section(s) older than {max} days, skipped"),
        );
    }

    let mut ts: BTreeMap<u16, TsInfo> = BTreeMap::new();
    // Key order: a TS's [other] sections come before its [actual] ones.
    for ((tsid, actual, _), (_, sdt)) in best {
        let info = ts
            .entry(tsid)
            .or_insert_with(|| TsInfo::new(sdt.onid, actual));
        if actual && !info.actual {
            *info = TsInfo::new(sdt.onid, actual);
        } else if info.actual && !actual {
            continue;
        }
        for (sid, stype) in sdt.services {
            if stype.is_some() || !info.services.contains_key(&sid) {
                info.services.insert(sid, stype);
            }
        }
    }
    ts
}

fn label(sid: u16, name: &str) -> String {
    if name.is_empty() {
        sid.to_string()
    } else {
        format!("{sid} {name}")
    }
}

fn labels(items: &[(u16, u16, &str)]) -> String {
    items
        .iter()
        .map(|&(_, sid, name)| label(sid, name))
        .collect::<Vec<_>>()
        .join(" / ")
}

fn warn_dups(lines: &[String], section: &str, err: &mut dyn Write) {
    let mut seen = HashSet::new();
    for line in lines.iter().filter(|l| !l.starts_with(';')) {
        let key = line.split_once('=').map_or(line.as_str(), |(k, _)| k);
        if !seen.insert(key) {
            warn(
                err,
                format_args!("[{section}]: duplicate channel name {key}"),
            );
        }
    }
}

/// `[BS]`: one line per TS with a comment listing its services (`per_ts`), or one
/// line per TV / radio / data service with a comment per transponder.
fn bs_lines(
    ts: &BTreeMap<u16, TsInfo>,
    names: &ChannelNames,
    promo: bool,
    per_ts: bool,
    err: &mut dyn Write,
) -> Vec<String> {
    let mut groups: BTreeMap<Transponder, Vec<(u16, u16, &str)>> = BTreeMap::new();
    for (&tsid, info) in ts {
        if tsid >> 12 != 0x4 {
            continue;
        }
        let Some(tp) = transponder(info.onid, tsid) else {
            let tp = (tsid >> 4) & 0x1F;
            warn(
                err,
                format_args!(
                    "BS TSID 0x{tsid:04X}: transponder {tp} is not a right-hand BS transponder, skipped"
                ),
            );
            continue;
        };
        let listed = info.listed(promo);
        if listed.is_empty() {
            warn(
                err,
                format_args!("BS TSID 0x{tsid:04X} has no TV, radio or data service, skipped"),
            );
            continue;
        }
        groups.entry(tp).or_default().extend(
            listed
                .into_iter()
                .map(|sid| (tsid, sid, names.get(tsid, info.onid, sid))),
        );
    }
    let mut out = Vec::new();
    for (tp, items) in groups {
        let num = tp.num;
        let mhz = f64::from(tp.if_khz + LNB_LO_KHZ) / 1000.0;
        if per_ts {
            for one in items.chunk_by(|a, b| a.0 == b.0) {
                let (tsid, rel) = (one[0].0, rel_ts(one[0].0));
                out.push(format!(
                    "; BS-{num:<2} TS{rel} ({mhz:.2} MHz): {}",
                    labels(one)
                ));
                out.push(tp.ts_line(tsid));
            }
            continue;
        }
        out.push(format!("; BS-{num:<2} ({mhz:.2} MHz): {}", labels(&items)));
        for (tsid, sid, _) in items {
            out.push(tp.service_line(tsid, sid));
        }
    }
    warn_dups(&out, "BS", err);
    out
}

/// `[CS110]`: one line per TS with a comment listing its services (`per_ts`), or one
/// line per TV / radio / data service with a comment per ND.
fn cs_lines(
    ts: &BTreeMap<u16, TsInfo>,
    names: &ChannelNames,
    promo: bool,
    per_ts: bool,
    err: &mut dyn Write,
) -> Vec<String> {
    let mut groups: BTreeMap<Transponder, Vec<(u16, u16, &str)>> = BTreeMap::new();
    for (&tsid, info) in ts {
        if !matches!(tsid >> 12, 0x6 | 0x7) {
            continue;
        }
        let Some(tp) = transponder(info.onid, tsid) else {
            let nd = (tsid >> 4) & 0x1F;
            warn(
                err,
                format_args!(
                    "CS TSID 0x{tsid:04X}: ND{nd:02} is not a right-hand CS110 transponder, skipped"
                ),
            );
            continue;
        };
        let listed = info.listed(promo);
        if listed.is_empty() {
            warn(
                err,
                format_args!("CS TSID 0x{tsid:04X} has no TV, radio or data service, skipped"),
            );
            continue;
        }
        groups.entry(tp).or_default().extend(
            listed
                .into_iter()
                .map(|sid| (tsid, sid, names.get(tsid, info.onid, sid))),
        );
    }
    let mut out = Vec::new();
    for (tp, items) in groups {
        let nd = tp.num;
        if per_ts {
            for one in items.chunk_by(|a, b| a.0 == b.0) {
                let (tsid, rel) = (one[0].0, rel_ts(one[0].0));
                out.push(format!("; ND{nd:02} TS{rel}: {}", labels(one)));
                out.push(tp.ts_line(tsid));
            }
            continue;
        }
        out.push(format!("; ND{nd:02}: {}", labels(&items)));
        for (tsid, sid, _) in items {
            out.push(tp.service_line(tsid, sid));
        }
    }
    warn_dups(&out, "CS110", err);
    out
}

/// The sections as printed without `--bondriver`.
fn render_sections(sections: &[(&str, Vec<String>)]) -> String {
    let mut s = String::new();
    for (i, (name, body)) in sections.iter().enumerate() {
        if i > 0 {
            s.push('\n');
        }
        let _ = writeln!(s, "[{name}]");
        for line in body {
            s.push_str(line);
            s.push('\n');
        }
    }
    s
}

// ---------------------------------------------------------------- ini rewrite

/// The lines of `text`, each with its line end (CRLF, LF or CR).
fn split_lines_keepends(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        let eol = match b[i] {
            b'\n' => 1,
            b'\r' if b.get(i + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => 0,
        };
        if eol == 0 {
            i += 1;
        } else {
            i += eol;
            out.push(&text[start..i]);
            start = i;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn has_eol(line: &str) -> bool {
    line.ends_with(['\n', '\r'])
}

/// The lower-cased name of a `[name]` section header; what follows the `]` is
/// ignored, as the BonDriver (and the Win32 profile API) does.
fn header_name(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix('[')?;
    let end = rest.find(']')?;
    Some(rest[..end].trim().to_lowercase())
}

fn is_header(line: &str) -> bool {
    header_name(line).is_some()
}

fn is_comment_or_blank(line: &str) -> bool {
    let s = line.trim();
    s.is_empty() || s.starts_with([';', '#'])
}

/// `text` with what follows the `[name]` header up to the next section header (or
/// the end) replaced by `body` (lines without line ends). The comment / blank lines
/// right before the next header belong to that section and are kept; everything
/// else is untouched. A missing section is appended. New lines get the file's line
/// end (CRLF if it has any).
fn replace_section(text: &str, name: &str, body: &[String]) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = split_lines_keepends(text)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut new_body: Vec<String> = body.iter().map(|b| format!("{b}{nl}")).collect();
    let want = name.to_lowercase();
    let Some(hdr) = lines
        .iter()
        .position(|l| header_name(l).as_deref() == Some(&want[..]))
    else {
        if let Some(last) = lines.last_mut() {
            if !has_eol(last) {
                last.push_str(nl);
            }
            if !last.trim().is_empty() {
                lines.push(nl.to_owned());
            }
        }
        lines.push(format!("[{name}]{nl}"));
        lines.append(&mut new_body);
        return lines.concat();
    };
    if !has_eol(&lines[hdr]) {
        lines[hdr].push_str(nl);
    }
    let end = (hdr + 1..lines.len())
        .find(|&i| is_header(&lines[i]))
        .unwrap_or(lines.len());
    let mut keep = end;
    if end < lines.len() {
        while keep > hdr + 1 && is_comment_or_blank(&lines[keep - 1]) {
            keep -= 1;
        }
        if keep == end {
            new_body.push(nl.to_owned());
        }
    }
    lines.splice(hdr + 1..keep, new_body);
    lines.concat()
}

/// Unified diff of the ini's lines (line ends ignored), headers `PATH` and
/// `PATH (regenerated)`; empty if only line ends differ.
fn ini_diff(old: &str, new: &str, path: &str) -> String {
    fn strip(t: &str) -> Vec<&str> {
        split_lines_keepends(t)
            .into_iter()
            .map(|l| l.trim_end_matches(['\r', '\n']))
            .collect()
    }
    let (old, new) = (strip(old), strip(new));
    TextDiff::from_slices(&old, &new)
        .unified_diff()
        .header(path, &format!("{path} (regenerated)"))
        .to_string()
}

/// `<path><suffix>` (the suffix is appended to the full file name).
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Copy `path` to `PATH.bak`, then replace it with `text` via `PATH.tmp` and a
/// rename. Returns the backup's path.
fn write_with_backup(path: &Path, bom: bool, text: &str) -> io::Result<PathBuf> {
    let bak = suffixed(path, ".bak");
    fs::copy(path, &bak)?;
    let tmp = suffixed(path, ".tmp");
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        if bom {
            f.write_all(UTF8_BOM)?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(|()| bak)
}

/// Replace `sections` in the ini at `path`: the unified diff to `out` (dry run) or,
/// with `write`, the file rewritten in place (keeping its UTF-8 BOM if any). Returns
/// the exit code.
fn update_ini(
    path: &Path,
    sections: &[(&str, Vec<String>)],
    write: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let shown = path.display();
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(e) => {
            warn(err, format_args!("{shown}: {e}"));
            return 2;
        }
    };
    let (bom, body) = match raw.strip_prefix(UTF8_BOM) {
        Some(body) => (true, body),
        None => (false, raw.as_slice()),
    };
    let text = match std::str::from_utf8(body) {
        Ok(text) => text,
        Err(e) => {
            warn(
                err,
                format_args!("{shown} is not UTF-8 ({e}); not touching it"),
            );
            return 2;
        }
    };
    let new = sections.iter().fold(text.to_owned(), |t, (name, body)| {
        replace_section(&t, name, body)
    });
    if new == text {
        warn(err, format_args!("{shown}: already up to date"));
        return 0;
    }
    if !write {
        let rc = emit(out, &ini_diff(text, &new, &shown.to_string()), err);
        warn(err, "dry run; add --write to apply");
        return rc;
    }
    match write_with_backup(path, bom, &new) {
        Ok(bak) => {
            warn(
                err,
                format_args!("{shown} updated (backup: {})", bak.display()),
            );
            0
        }
        Err(e) => {
            warn(err, format_args!("{shown}: {e}"));
            2
        }
    }
}
