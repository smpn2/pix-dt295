use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use dt295::ts::crc::{crc32, set_section_crc};
use dt295::ts::packet::PACKET;

use crate::bonini::*;

use super::carousel::*;
use super::cdt::*;
use super::schedule::*;
use super::tvtest::*;
use super::*;

/// A long-form section: `table_id`, the bytes after section_length up to the CRC;
/// the length and CRC are filled in.
fn long_section(table_id: u8, body: &[u8]) -> Vec<u8> {
    let mut sec = vec![table_id, 0, 0];
    sec.extend_from_slice(body);
    sec.extend_from_slice(&[0; 4]);
    let sl = sec.len() - 3;
    sec[1] = 0xB0 | (sl >> 8) as u8;
    sec[2] = sl as u8;
    set_section_crc(&mut sec);
    sec
}

/// Packetizes sections, one continuity counter per PID.
#[derive(Default)]
struct Mux {
    cc: HashMap<u16, u8>,
}

impl Mux {
    /// One section from the start of a packet (pointer_field 0), stuffed with 0xFF.
    fn packets(&mut self, pid: u16, sec: &[u8]) -> Vec<u8> {
        let mut payload = vec![0u8];
        payload.extend_from_slice(sec);
        let mut out = Vec::new();
        for (i, chunk) in payload.chunks(184).enumerate() {
            let cc = self.cc.entry(pid).or_insert(0);
            let start = if i == 0 { 0x40 } else { 0 };
            out.extend_from_slice(&[0x47, start | (pid >> 8) as u8, pid as u8, 0x10 | *cc]);
            *cc = (*cc + 1) & 15;
            out.extend_from_slice(chunk);
            out.resize(out.len().next_multiple_of(PACKET), 0xFF);
        }
        out
    }
}

fn pat(programs: &[(u16, u16)]) -> Vec<u8> {
    let mut body = vec![0x40, 0xF1, 0xC1, 0, 0];
    for &(prog, pid) in programs {
        body.extend_from_slice(&prog.to_be_bytes());
        body.extend_from_slice(&(0xE000 | pid).to_be_bytes());
    }
    long_section(0x00, &body)
}

/// A PMT with (stream_type, pid, component_tag) ES entries.
fn pmt(program: u16, es: &[(u8, u16, u8)]) -> Vec<u8> {
    let mut body = program.to_be_bytes().to_vec();
    body.extend_from_slice(&[0xC1, 0, 0, 0xFF, 0xFF, 0xF0, 0]);
    for &(st, pid, tag) in es {
        body.push(st);
        body.extend_from_slice(&(0xE000 | pid).to_be_bytes());
        body.extend_from_slice(&[0xF0, 3, DESC_STREAM_ID, 1, tag]);
    }
    long_section(TID_PMT, &body)
}

/// A DII listing (module id, size, version, name) with `block_size`.
fn dii(download_id: u32, block_size: u16, modules: &[(u16, u32, u8, &str)]) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&download_id.to_be_bytes());
    msg.extend_from_slice(&block_size.to_be_bytes());
    msg.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // window, ack, tC x2
    msg.extend_from_slice(&[0, 0]); // compatibilityDescriptor: empty
    msg.extend_from_slice(&(modules.len() as u16).to_be_bytes());
    for &(id, size, version, name) in modules {
        msg.extend_from_slice(&id.to_be_bytes());
        msg.extend_from_slice(&size.to_be_bytes());
        msg.push(version);
        let info = [
            &[0x01, 1, b'x'][..],
            &[DESC_MODULE_NAME, name.len() as u8],
            name.as_bytes(),
        ]
        .concat();
        msg.push(info.len() as u8);
        msg.extend_from_slice(&info);
    }
    let mut body = vec![0x00, 0x02, 0xC1, 0, 0];
    body.extend_from_slice(&[0x11, 0x03, 0x10, 0x02, 0x80, 0, 0, 2, 0xFF, 0]);
    body.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    body.extend_from_slice(&msg);
    long_section(TID_DII, &body)
}

fn ddb(download_id: u32, module: u16, version: u8, block: u16, data: &[u8]) -> Vec<u8> {
    let mut body = module.to_be_bytes().to_vec();
    body.extend_from_slice(&[0xC1 | (version & 0x1F) << 1, block as u8, 0xFF]);
    body.extend_from_slice(&[0x11, 0x03, 0x10, 0x03]);
    body.extend_from_slice(&download_id.to_be_bytes());
    body.extend_from_slice(&[0xFF, 0]);
    body.extend_from_slice(&((data.len() + 6) as u16).to_be_bytes());
    body.extend_from_slice(&module.to_be_bytes());
    body.extend_from_slice(&[version, 0xFF]);
    body.extend_from_slice(&block.to_be_bytes());
    body.extend_from_slice(data);
    long_section(TID_DDB, &body)
}

/// A TDT: MJD and BCD h:m:s.
fn tdt(mjd: u16, hms: [u8; 3]) -> Vec<u8> {
    let mut s = vec![TID_TDT, 0x70, 5];
    s.extend_from_slice(&mjd.to_be_bytes());
    s.extend_from_slice(&hms);
    s
}

/// A fake PNG of `len` bytes (signature + filler `fill`).
fn png(len: usize, fill: u8) -> Vec<u8> {
    let mut p = PNG_SIGNATURE.to_vec();
    p.resize(len, fill);
    p
}

/// A logo module entry: the logo_id field, the services, the data.
type Entry<'a> = (u16, &'a [(u16, u16, u16)], &'a [u8]);

/// A logo module: type, then its entries.
fn logo_module(logo_type: u8, entries: &[Entry]) -> Vec<u8> {
    let mut b = vec![logo_type];
    b.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for &(id, services, data) in entries {
        b.extend_from_slice(&id.to_be_bytes());
        b.push(services.len() as u8);
        for &(onid, tsid, sid) in services {
            for v in [onid, tsid, sid] {
                b.extend_from_slice(&v.to_be_bytes());
            }
        }
        b.extend_from_slice(&(data.len() as u16).to_be_bytes());
        b.extend_from_slice(data);
    }
    b
}

// ---- logo modules ----

#[test]
fn logo_module_entries() {
    let a = png(120, 1);
    let body = logo_module(
        3,
        &[
            (0xFE05, &[(4, 0x4010, 151), (4, 0x4010, 152)], &a), // reserved bits set
            (6, &[], &a),                                        // no service: skipped
            (7, &[(4, 0x4011, 161)], &[]),                       // no data: skipped
            (8, &[(6, 0x6020, 296)], &a),
        ],
    );
    let logos = parse_logo_module(&body);
    assert_eq!(logos.len(), 2);
    assert_eq!(
        logos[0],
        Logo {
            network_id: 4,
            logo_id: 5,
            logo_type: 3,
            services: vec![(4, 0x4010, 151), (4, 0x4010, 152)],
            png: a.clone(),
        }
    );
    assert_eq!((logos[1].network_id, logos[1].logo_id), (6, 8));

    assert!(
        parse_logo_module(&logo_module(6, &[(1, &[(4, 1, 1)], &a)])).is_empty(),
        "type > 5"
    );
    let mut cut = body.clone();
    cut.truncate(body.len() - 10);
    assert_eq!(parse_logo_module(&cut).len(), 1, "stops at the cut entry");
    assert!(parse_logo_module(&[]).is_empty());
}

#[test]
fn module_names() {
    assert!(is_logo_module(b"LOGO-00"));
    assert!(is_logo_module(b"LOGO-05"));
    assert!(is_logo_module(b"CS_LOGO-03"));
    assert!(!is_logo_module(b"LOGO-10"));
    assert!(!is_logo_module(b"GENRE"));
    assert!(!is_logo_module(b"CS_LOGO-0"));
}

// ---- sections ----

#[test]
fn sections_across_and_within_packets() {
    let mut mux = Mux::default();
    let big = long_section(0x3C, &[0x55; 300]);
    let mut sb = SectionBuf::default();
    let pk = mux.packets(0x779, &big);
    assert_eq!(pk.len(), 2 * PACKET);
    assert!(sb.push(&pk[..PACKET]).is_empty());
    assert_eq!(sb.push(&pk[PACKET..]), vec![big.clone()]);

    // two sections in one packet, then stuffing
    let (a, b) = (long_section(0x3B, &[1; 20]), long_section(0x3B, &[2; 30]));
    let both = [a.clone(), b.clone()].concat();
    let pk = mux.packets(0x779, &both);
    assert_eq!(sb.push(&pk), vec![a, b]);
}

#[test]
fn sections_drop_a_gap_and_duplicates() {
    let mut mux = Mux::default();
    let big = long_section(0x3C, &[0x55; 300]);
    let pk = mux.packets(0x779, &big);
    let next = mux.packets(0x779, &long_section(0x3B, &[1; 10]));

    let mut sb = SectionBuf::default();
    sb.push(&pk[..PACKET]);
    assert!(sb.push(&pk[..PACKET]).is_empty(), "a duplicate is ignored");
    assert_eq!(sb.push(&pk[PACKET..]).len(), 1);

    let mut sb = SectionBuf::default();
    sb.push(&pk[..PACKET]);
    // the second packet is lost: the next section still comes through
    assert_eq!(sb.push(&next).len(), 1);
    assert!(sb.buf.is_empty() || sb.buf[0] == 0x3B);
}

// ---- the whole receive path ----

/// The TS of the engineering service with one BS and one CS logo module (the CS
/// module's blocks come before its DII), a TDT, and an unrelated carousel.
fn engineering_ts(bs: &[u8], cs: &[u8]) -> Vec<u8> {
    let mut m = Mux::default();
    let mut ts = Vec::new();
    ts.extend(m.packets(0, &pat(&[(0, 0x10), (929, 0x701)])));
    ts.extend(m.packets(
        0x701,
        &pmt(
            929,
            &[
                (0x0D, 0x771, 0x71),
                (0x0D, 0x779, TAG_BS),
                (0x0D, 0x77A, TAG_CS),
            ],
        ),
    ));
    ts.extend(m.packets(0x14, &tdt(61_334, [0x00, 0x00, 0x10])));
    ts.extend(m.packets(0x771, &dii(1, 100, &[(0, 50, 0, "LOGO-00")])));
    ts.extend(m.packets(
        0x779,
        &dii(
            0x10,
            128,
            &[(0, bs.len() as u32, 2, "LOGO-00"), (1, 10, 0, "GENRE")],
        ),
    ));
    for (i, block) in bs.chunks(128).enumerate() {
        ts.extend(m.packets(0x779, &ddb(0x10, 0, 2, i as u16, block)));
    }
    for (i, block) in cs.chunks(128).enumerate() {
        ts.extend(m.packets(0x77A, &ddb(0x20, 256, 0, i as u16, block)));
    }
    ts.extend(m.packets(
        0x77A,
        &dii(0x20, 128, &[(256, cs.len() as u32, 0, "CS_LOGO-00")]),
    ));
    ts
}

#[test]
fn collects_bs_and_cs_logos() {
    let bs = logo_module(
        0,
        &[(1, &[(4, 0x40F1, 101), (4, 0x40F1, 102)], &png(300, 7))],
    );
    let cs = logo_module(0, &[(2, &[(6, 0x6020, 296)], &png(150, 9))]);
    let ts = engineering_ts(&bs, &cs);

    let mut c = Collector::new(929);
    assert_eq!(c.progress(), "waiting for service 929");
    // odd chunks: packets are cut between calls
    let (first, rest) = ts.split_at(PACKET * 2 + 100);
    c.feed(first);
    assert_eq!(c.progress(), "logo data: BS -, CS - modules");
    for chunk in rest.chunks(1000) {
        c.feed(chunk);
    }
    assert!(c.done());
    assert_eq!(c.progress(), "logo data: BS 1/1, CS 1/1 modules");
    assert_eq!(c.time, Some(13_437_014_410), "2026-10-21 00:00:10");

    let logos = c.logos();
    assert_eq!(logos.len(), 2);
    assert_eq!((logos[0].network_id, logos[0].logo_id), (4, 1));
    assert_eq!(logos[0].png, png(300, 7));
    assert_eq!((logos[1].network_id, logos[1].logo_id), (6, 2));
}

#[test]
fn incomplete_and_resynced_streams() {
    let bs = logo_module(0, &[(1, &[(4, 0x40F1, 101)], &png(300, 7))]);
    let cs = logo_module(0, &[(2, &[(6, 0x6020, 296)], &png(150, 9))]);
    let ts = engineering_ts(&bs, &cs);

    // without the CS DII (the last section) the CS module is not counted yet
    let mut c = Collector::new(929);
    c.feed(&ts[..ts.len() - PACKET]);
    assert_eq!(c.progress(), "logo data: BS 1/1, CS - modules");
    assert!(!c.done());
    assert_eq!(c.logos().len(), 1);

    // garbage before the stream: found by the sync search
    let mut c = Collector::new(929);
    c.feed(&[0x12; 77]);
    c.feed(&ts);
    assert!(c.done());

    // another program number: nothing
    let mut c = Collector::new(930);
    c.feed(&ts);
    assert!(!c.done());
    assert!(c.logos().is_empty());
}

#[test]
fn a_new_module_version_starts_over() {
    let mut m = Mux::default();
    let mut c = Collector::new(929);
    c.feed(&m.packets(0, &pat(&[(929, 0x701)])));
    c.feed(&m.packets(0x701, &pmt(929, &[(0x0D, 0x779, TAG_BS)])));
    let v1 = logo_module(0, &[(1, &[(4, 1, 101)], &png(200, 1))]);
    c.feed(&m.packets(0x779, &dii(5, 128, &[(0, v1.len() as u32, 1, "LOGO-00")])));
    c.feed(&m.packets(0x779, &ddb(5, 0, 1, 0, &v1[..128])));
    // version 2 announced: the version 1 block no longer counts
    let v2 = logo_module(0, &[(1, &[(4, 1, 101)], &png(200, 2))]);
    c.feed(&m.packets(0x779, &dii(5, 128, &[(0, v2.len() as u32, 2, "LOGO-00")])));
    c.feed(&m.packets(0x779, &ddb(5, 0, 1, 1, &v1[128..])));
    assert!(!c.done());
    c.feed(&m.packets(0x779, &ddb(5, 0, 2, 0, &v2[..128])));
    c.feed(&m.packets(0x779, &ddb(5, 0, 2, 1, &v2[128..])));
    assert!(c.done());
    assert_eq!(c.logos()[0].png, png(200, 2));
}

#[test]
fn broadcast_time() {
    assert_eq!(
        linear_seconds(&[0xC9, 0x58, 0, 0, 0]),
        Some(12_591_158_400),
        "2000-01-01"
    );
    assert_eq!(
        linear_seconds(&[0xEF, 0x96, 0x23, 0x59, 0x59]),
        Some(13_437_100_799)
    );
    assert_eq!(linear_seconds(&[0xEF, 0x96, 0x24, 0, 0]), None, "hour 24");
    assert_eq!(linear_seconds(&[0xEF, 0x96, 0x1A, 0, 0]), None, "not BCD");
    assert_eq!(linear_seconds(&[0xEF]), None);
    let now = now_jst();
    assert!(
        now > 13_437_014_400 && now < 15_746_918_400,
        "TVTest's 2000-2099 range"
    );
}

// ---- TVTest's logo data file ----

fn record(version: u16, time: u64, png: Vec<u8>) -> Record {
    Record { version, time, png }
}

#[test]
fn logo_data_round_trip() {
    let mut d = LogoData::default();
    d.records
        .insert((4, 1, 0), record(0, 13_437_014_400, png(300, 1)));
    d.records
        .insert((7, 0x1FF, 5), record(26, 13_000_000_000, png(1296, 2)));
    let b = d.to_bytes();
    assert_eq!(&b[..8], b"LogoData");
    assert_eq!(&b[8..16], &[1, 0, 0, 0, 2, 0, 0, 0]);
    // the first record: header, data, CRC over both
    assert_eq!(&b[16..26], &[4, 0, 1, 0, 0, 0, 0, 0, 0x2C, 0x01]);
    assert_eq!(crc32(&b[16..16 + 24 + 300]).to_le_bytes(), b[340..344]);
    assert_eq!(LogoData::parse(&b), Ok(d));

    let mut bad = b.clone();
    bad[16 + 24 + 20] ^= 1;
    assert_eq!(LogoData::parse(&bad), Err("logo 0: CRC error".into()));
    assert_eq!(
        LogoData::parse(&b[..b.len() - 1]),
        Err("logo 1: cut short".into())
    );
    assert!(LogoData::parse(b"LogoDat").is_err());
    let mut v2 = b.clone();
    v2[8] = 2;
    assert!(LogoData::parse(&v2).is_err());
}

#[test]
fn logo_data_version_0() {
    let p = png(100, 3);
    let mut b = b"LogoData".to_vec();
    b.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0]);
    let start = b.len();
    b.extend_from_slice(&[6, 0, 3, 0, 1, 0, 2, 0, 100, 0]);
    b.extend_from_slice(&p);
    let crc = crc32(&b[start..]);
    b.extend_from_slice(&crc.to_le_bytes());
    let d = LogoData::parse(&b).unwrap();
    assert_eq!(d.records[&(6, 3, 2)], record(1, 0, p));
}

fn logo(network_id: u16, logo_id: u16, logo_type: u8, sids: &[u16], data: Vec<u8>) -> Logo {
    Logo {
        network_id,
        logo_id,
        logo_type,
        services: sids.iter().map(|&s| (network_id, 0x40F1, s)).collect(),
        png: data,
    }
}

#[test]
fn merge_like_tvtest() {
    let mut d = LogoData::default();
    d.records.insert((4, 1, 0), record(0, 100, png(200, 1))); // same data
    d.records.insert((4, 2, 0), record(0, 100, png(200, 1))); // data changed
    d.records.insert((4, 3, 0), record(0, 900, png(200, 1))); // stored later
    d.records.insert((4, 4, 0), record(5, 100, png(200, 1))); // newer version
    let logos = [
        logo(4, 1, 0, &[101], png(200, 1)),
        logo(4, 2, 0, &[102], png(200, 2)),
        logo(4, 3, 0, &[103], png(200, 2)),
        logo(4, 4, 0, &[104], png(200, 2)),
        logo(4, 5, 0, &[105, 106], png(200, 2)),
        logo(4, 6, 0, &[107], png(93, 0)),
        logo(4, 7, 0, &[108], png(1297, 0)),
    ];
    let r = d.merge(&logos, 500);
    assert_eq!(r.added, [(4, 5, 0)]);
    assert_eq!(r.updated, [(4, 2, 0)]);
    assert_eq!((r.unchanged, r.transparent, r.too_large), (3, 1, 1));
    assert_eq!(d.records[&(4, 2, 0)], record(0, 500, png(200, 2)));
    assert_eq!(d.records[&(4, 3, 0)].time, 900);
    assert_eq!(d.records[&(4, 4, 0)].version, 5);
    assert!(!d.records.contains_key(&(4, 6, 0)));

    let key = |sid: u32| (4 << 16) | sid;
    assert_eq!(r.id_map[&key(101)], (1, false));
    assert_eq!(r.id_map[&key(102)], (2, true));
    assert_eq!(r.id_map[&key(105)], (5, true));
    assert_eq!(r.id_map[&key(106)], (5, true));
    assert!(!r.id_map.contains_key(&key(107)), "transparent: not mapped");
    assert!(
        r.summary(&logos)
            .contains("1 added, 1 updated, 3 unchanged")
    );
}

#[test]
fn version_order_wraps() {
    use std::cmp::Ordering::*;
    assert_eq!(compare_version(3, 3), Equal);
    assert_eq!(compare_version(4, 3), Greater);
    assert_eq!(compare_version(3000, 2048), Greater);
    assert_eq!(compare_version(0, 4095), Greater, "wrapped");
    assert_eq!(compare_version(4095, 0), Less);
}

// ---- the logo ID map ----

fn ids(entries: &[(u32, u16, bool)]) -> BTreeMap<u32, (u16, bool)> {
    entries.iter().map(|&(k, id, r)| (k, (id, r))).collect()
}

#[test]
fn id_map_updates_in_place() {
    let text = "[Other]\r\nA=1\r\n[LogoIDMap]\r\n7FE10408=8\r\n00040065=1\r\n00040066=2\r\n\r\n[Next]\r\nB=2\r\n";
    let (out, added, changed) = set_logo_ids(
        text,
        &ids(&[
            (0x0004_0065, 9, false), // present, its logo not updated: kept
            (0x0004_0066, 3, true),  // replaced
            (0x0004_0067, 4, true),  // added
            (0x0006_0128, 5, false), // added
        ]),
    );
    assert_eq!((added, changed), (2, 1));
    assert_eq!(
        out,
        "[Other]\r\nA=1\r\n[LogoIDMap]\r\n7FE10408=8\r\n00040065=1\r\n00040066=3\r\n\
         00040067=4\r\n00060128=5\r\n\r\n[Next]\r\nB=2\r\n"
    );
}

#[test]
fn id_map_created() {
    let (out, added, _) = set_logo_ids("", &ids(&[(0x0004_0065, 1, true)]));
    assert_eq!((out.as_str(), added), ("[LogoIDMap]\r\n00040065=1\r\n", 1));
    let (out, ..) = set_logo_ids("[X]\nA=1\n\n", &ids(&[(0x0004_0065, 1, true)]));
    assert_eq!(out, "[X]\nA=1\n\n[LogoIDMap]\n00040065=1\n");
    let (out, added, changed) =
        set_logo_ids("[logoidmap]\n00040065=1\n", &ids(&[(0x0004_0065, 1, true)]));
    assert_eq!(
        (out.as_str(), added, changed),
        ("[logoidmap]\n00040065=1\n", 0, 0)
    );
}

// ---- TVTest's files ----

/// A scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("dt295-logo-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn text_encodings() {
    let utf16 = encode_text("[Settings]\r\nA=あ\r\n", true);
    assert_eq!(&utf16[..4], &[0xFF, 0xFE, b'[', 0]);
    assert_eq!(decode_text(&utf16), "[Settings]\r\nA=あ\r\n");
    assert_eq!(decode_text(b"\xEF\xBB\xBFA=1"), "A=1");
    let s = ini_section(
        "[Settings]\r\nLogoDirectory = .\\Logo \r\n[Other]\r\nLogoDirectory=x\r\n",
        "settings",
    );
    assert_eq!(s.get("logodirectory").map(String::as_str), Some(".\\Logo"));
    let t = "[A]\nk=1\nK=2\n# c\n;x=1\nnoeq\n[ b ] tail\nv = \"q\" \n";
    assert_eq!(ini_section(t, "a").get("k").map(String::as_str), Some("1"));
    assert_eq!(ini_section(t, "a").len(), 1);
    assert_eq!(ini_section(t, "B").get("v").map(String::as_str), Some("q"));
}

#[test]
fn store_follows_tvtest_ini() {
    let s = Scratch::new("store");
    let ini = "[Settings]\r\nLogoDataFileName=Logos\r\nSaveRawLogo=yes\r\nLogoDirectory=Pics\r\n";
    fs::write(s.0.join("TVTest.ini"), encode_text(ini, true)).unwrap();
    let store = Store::locate(&TvTest::open(&s.0).unwrap()).unwrap();
    assert_eq!(store.data, s.0.join("Logos"));
    assert_eq!(store.id_map, s.0.join("Logos.ini"));
    assert_eq!(store.dir, Some(s.0.join("Pics")));

    // defaults; no logo folder unless TVTest saves logos there or it exists
    let s2 = Scratch::new("store-defaults");
    let store = Store::locate(&TvTest::open(&s2.0).unwrap()).unwrap();
    assert_eq!(store.data, s2.0.join("LogoData"));
    assert_eq!(store.dir, None);
    assert!(TvTest::open(&s2.0.join("missing")).is_err());
}

#[test]
fn save_and_merge_again() {
    let s = Scratch::new("save");
    fs::write(s.0.join("TVTest.ini"), "[Settings]\r\nSaveRawLogo=yes\r\n").unwrap();
    fs::write(s.0.join("LogoData.ini"), "[LogoIDMap]\r\n7FE10408=8\r\n").unwrap();
    let store = Store::locate(&TvTest::open(&s.0).unwrap()).unwrap();
    let logos = [
        logo(4, 1, 0, &[101], png(200, 1)),
        logo(6, 2, 3, &[296], png(150, 2)),
    ];

    let mut d = store.load_data().unwrap();
    let r = d.merge(&logos, 1000);
    store.save(&d, &r).unwrap();
    assert_eq!(store.load_data().unwrap(), d);
    assert_eq!(
        fs::read_to_string(s.0.join("LogoData.ini")).unwrap(),
        "[LogoIDMap]\r\n7FE10408=8\r\n00040065=1\r\n00060128=2\r\n"
    );
    assert_eq!(
        fs::read(s.0.join("Logo").join("0004_001_000_00")).unwrap(),
        png(200, 1)
    );
    assert_eq!(
        fs::read(s.0.join("Logo").join("0006_002_000_03")).unwrap(),
        png(150, 2)
    );
    assert!(
        !s.0.join("LogoData.bak").exists(),
        "nothing to back up the first time"
    );

    // the same logos again: nothing changes
    let mut d2 = store.load_data().unwrap();
    let r2 = d2.merge(&logos, 2000);
    assert_eq!((r2.added.len(), r2.updated.len(), r2.unchanged), (0, 0, 2));
    store.save(&d2, &r2).unwrap();
    assert!(!s.0.join("LogoData.bak").exists(), "not rewritten");

    // a changed logo: rewritten, the old file kept
    let mut d3 = store.load_data().unwrap();
    let r3 = d3.merge(&[logo(4, 1, 0, &[101], png(200, 9))], 3000);
    store.save(&d3, &r3).unwrap();
    assert_eq!(
        store.load_data().unwrap().records[&(4, 1, 0)].png,
        png(200, 9)
    );
    assert_eq!(
        LogoData::parse(&fs::read(s.0.join("LogoData.bak")).unwrap()).unwrap(),
        d
    );
}

#[test]
fn logo_args() {
    use clap::Parser;
    let cli = crate::Cli::try_parse_from(["dt295", "logo", "--tvtest", "D:\\TVTest"]).unwrap();
    let crate::Command::Logo(a) = cli.command else {
        panic!("not logo")
    };
    assert_eq!(
        (a.sat, a.tsid, a.sid, a.wait),
        (1_318_000_000, 0x40F1, 929, 45)
    );
    assert_eq!((a.dev.fw.as_ref(), a.ts.as_ref()), (None, None));
    assert_eq!(a.schedule, None);

    let parse = |args: &[&str]| crate::Cli::try_parse_from(["dt295", "logo"].iter().chain(args));
    let logo = |args: &[&str]| match parse(args).unwrap().command {
        crate::Command::Logo(a) => a,
        _ => panic!("not logo"),
    };
    assert!(parse(&["--fw", "f"]).is_err(), "--tvtest is required");
    assert!(parse(&["--ts", "a.ts", "--tvtest", "x"]).is_ok());
    assert!(parse(&["--ts", "a.ts", "--fw", "f", "--tvtest", "x"]).is_err());
    // --wait bounds reading --ts - (stdin)
    assert!(parse(&["--ts", "-", "--wait", "5", "--tvtest", "x"]).is_ok());
    // --mirakc instead of --tvtest, with --ts
    assert!(parse(&["--ts", "-", "--mirakc", "/var/lib/dt295/logos"]).is_ok());
    assert!(parse(&["--mirakc", "d"]).is_err(), "--mirakc needs --ts");
    assert!(parse(&["--ts", "-", "--mirakc", "d", "--tvtest", "x"]).is_err());
    assert!(parse(&["--wait", "0", "--tvtest", "x"]).is_err());
    assert_eq!(logo(&["--wait", "20", "--tvtest", "x"]).wait, 20);
    assert_eq!(
        logo(&["--schedule", "--tvtest", "x"]).schedule.as_deref(),
        Some("03:55")
    );
    assert_eq!(
        logo(&["--schedule", "5:10", "--tvtest", "x"])
            .schedule
            .as_deref(),
        Some("5:10")
    );
    assert!(parse(&["--schedule", "--log", "l", "--tvtest", "x"]).is_err());
    assert!(parse(&["--schedule", "--unschedule", "--tvtest", "x"]).is_err());
}

// ---- time ----

#[test]
fn japan_time_text() {
    // 2026-10-20T17:04:44Z
    assert_eq!(jst_text(1_792_515_884, false), "02:04:44");
    assert_eq!(jst_text(1_792_515_884, true), "2026-10-21 02:04:44");
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    assert_eq!(civil_from_days(20_747), (2026, 10, 21));
}

// ---- the BonDriver units ----

#[test]
fn bondriver_ini() {
    let dir = Path::new("D:\\TV\\BonDriver");
    let ini = dir.join("BonDriver_DT295-1.ini");
    let b = BonDriver::from_ini(
        &ini,
        "[Tuner]\r\nDeviceSerial=EF34567890\r\n; StateFile=x\r\nTrcFirmware=\"fw\\trc.bin\"\r\n\
         [BS]\r\nFirmware=no\r\n",
    )
    .unwrap();
    assert_eq!(b.serial(), Some("EF34567890"));
    assert_eq!(b.tuner.firmware(), dir.join("mb86m01_assp_osec_idle.bin"));
    assert_eq!(
        b.tuner.trc_firmware(),
        dir.join("mb86m01_assp_osec_trc_m_h.bin"),
        "the folder's, whatever TrcFirmware says"
    );
    assert_eq!(
        b.tuner.state_file("EF34567890"),
        dir.join("EF34567890.state")
    );

    let b = BonDriver::from_ini(&ini, "[tuner]\nDeviceSerial=\nStateFile=shared.state\n").unwrap();
    assert_eq!(b.serial(), None);
}

#[test]
fn finds_the_bondriver_inis() {
    let s = Scratch::new("find");
    let tv = TvTest::open(&s.0).unwrap();
    assert_eq!(tv.driver_dir(), s.0.join("."));
    fs::write(
        s.0.join("TVTest.ini"),
        encode_text("[Settings]\r\nDriverDirectory=BonDriver\r\n", true),
    )
    .unwrap();
    let tv = TvTest::open(&s.0).unwrap();
    let dir = s.0.join("BonDriver");
    assert_eq!(tv.driver_dir(), dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("BonDriver_DT295-1.ini"),
        "[Tuner]\r\nDeviceSerial=B\r\n",
    )
    .unwrap();
    fs::write(
        dir.join("BonDriver_DT295.ini"),
        "[Tuner]\r\nDeviceSerial=A\r\n",
    )
    .unwrap();
    fs::write(dir.join("BonDriver_Other.ini"), "[Tuner]\r\n").unwrap();
    fs::write(dir.join("BonDriver_DT295.dll"), "").unwrap();
    let found = BonDriver::find(&tv.driver_dir()).unwrap();
    let serials: Vec<_> = found.iter().map(BonDriver::serial).collect();
    assert_eq!(serials, [Some("B"), Some("A")], "by file name");
}

fn bon(serial: Option<&str>, name: &str) -> BonDriver {
    BonDriver::from_ini(
        &Path::new("BD").join(name),
        &serial.map_or_default(|s| format!("[Tuner]\nDeviceSerial={s}\n")),
    )
    .unwrap()
}

#[test]
fn picks_a_free_unit() {
    let bons = [bon(Some("WA1"), "a.ini"), bon(Some("ub2"), "b.ini")];
    let free = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // the first free unit a BonDriver names (serials compare case-insensitively)
    let (s, b) = pick_unit(&free(&["X9", "UB2"]), 4, &bons).unwrap();
    assert_eq!((s.as_str(), b), ("UB2", Some(&bons[1])));
    // none named: the first free one, without a state file
    let (s, b) = pick_unit(&free(&["X9"]), 4, &bons).unwrap();
    assert_eq!((s.as_str(), b), ("X9", None));
    assert_eq!(pick_unit(&[], 4, &bons), None);
    // one unit and one ini without a serial: that is its ini
    let single = [bon(None, "c.ini")];
    let (_, b) = pick_unit(&free(&["X9"]), 1, &single).unwrap();
    assert_eq!(b, Some(&single[0]));
    assert_eq!(pick_unit(&free(&["X9"]), 2, &single).unwrap().1, None);
}

// ---- the scheduled task ----

#[test]
fn task_pieces() {
    assert_eq!(parse_hhmm("03:55").unwrap(), (3, 55));
    assert_eq!(parse_hhmm(" 5:07").unwrap(), (5, 7));
    assert!(parse_hhmm("24:00").is_err());
    assert!(parse_hhmm("0355").is_err());

    // 2026-10-21 02:04:44 JST
    let now = 1_792_515_884;
    assert_eq!(
        start_boundary(now, 3, 55),
        "2026-10-21T03:55:00",
        "still ahead today"
    );
    assert_eq!(
        start_boundary(now, 2, 4),
        "2026-10-22T02:04:00",
        "passed: tomorrow"
    );

    assert_eq!(quote_arg("D:\\TV Test"), "\"D:\\TV Test\"");
    assert_eq!(
        quote_arg("D:\\TV\\"),
        "\"D:\\TV\\\\\"",
        "a trailing backslash is doubled"
    );
    assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");

    let xml = task_xml(
        "C:\\x&y\\dt295.exe",
        "logo --tvtest \"D:\\TV\"",
        "2026-10-22T03:55:00",
    );
    assert!(xml.contains("<StartBoundary>2026-10-22T03:55:00</StartBoundary>"));
    assert!(xml.contains("<Command>C:\\x&amp;y\\dt295.exe</Command>"));
    assert!(xml.contains("<Arguments>logo --tvtest &quot;D:\\TV&quot;</Arguments>"));
    assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
    assert!(!xml.contains("<Principals>"), "the registering user");
}

// ---- review fixes ----

#[test]
fn a_new_download_replaces_the_old() {
    let mut m = Mux::default();
    let mut c = Collector::new(929);
    c.feed(&m.packets(0, &pat(&[(929, 0x701)])));
    c.feed(&m.packets(0x701, &pmt(929, &[(0x0D, 0x779, TAG_BS)])));
    let body = logo_module(0, &[(1, &[(4, 1, 101)], &png(200, 1))]);
    let size = body.len() as u32;
    // download 5: only the first block comes
    c.feed(&m.packets(0x779, &dii(5, 128, &[(0, size, 0, "LOGO-00")])));
    c.feed(&m.packets(0x779, &ddb(5, 0, 0, 0, &body[..128])));
    assert_eq!(c.progress(), "logo data: BS 0/1 modules");
    // download 6 takes over: the stale half module of 5 must not hold it back
    c.feed(&m.packets(0x779, &dii(6, 128, &[(0, size, 0, "LOGO-00")])));
    c.feed(&m.packets(0x779, &ddb(6, 0, 0, 0, &body[..128])));
    c.feed(&m.packets(0x779, &ddb(6, 0, 0, 1, &body[128..])));
    assert!(c.done());
    assert_eq!(c.logos().len(), 1);
    // the empty DII sent between transmissions keeps what was received
    c.feed(&m.packets(0x779, &dii(0, 0, &[])));
    assert!(c.done());
    assert_eq!(c.logos().len(), 1);
}

#[test]
fn counts_valid_pats_only() {
    let mut m = Mux::default();
    let mut c = Collector::new(929);
    let good = pat(&[(929, 0x701)]);
    c.feed(&m.packets(0, &good));
    c.feed(&m.packets(0, &good));
    let mut bad = good.clone();
    bad[9] ^= 0x55; // a stream that does not decrypt: the CRC fails
    c.feed(&m.packets(0, &bad));
    assert_eq!(c.pats, 2);
}

#[test]
fn unschedule_needs_no_folder() {
    use clap::Parser;
    let parse = |args: &[&str]| crate::Cli::try_parse_from(["dt295", "logo"].iter().chain(args));
    assert!(parse(&["--unschedule"]).is_ok());
    assert!(parse(&[]).is_err(), "--tvtest otherwise");
    assert!(parse(&["--serial", "EF34567890", "--tvtest", "x"]).is_ok());
}

#[test]
fn mirakc_takes_the_best_type_per_service() {
    let logos = [
        logo(4, 1, 0, &[101, 102], png(200, 0)),
        logo(4, 1, 5, &[101, 102], png(205, 0)),
        logo(4, 1, 2, &[101, 102], png(202, 0)),
        logo(6, 9, 3, &[296], png(303, 0)),
        logo(6, 9, 1, &[296], png(301, 0)),
        // transparent dummies are no logo
        logo(6, 9, 5, &[296], png(93, 0)),
        logo(6, 8, 5, &[297], png(93, 0)),
    ];
    let files = mirakc_logos(&logos);
    let got: Vec<_> = files
        .iter()
        .map(|(id, p)| (*id, p.map(<[u8]>::len)))
        .collect();
    // HD large for BS; SD 16:9 small beats SD 4:3 large (and a dummy HD large); 297
    // has no logo
    assert_eq!(
        got,
        [
            (400_101, Some(205)),
            (400_102, Some(205)),
            (600_296, Some(301)),
            (600_297, None)
        ]
    );
}

#[test]
fn mirakc_files_from_complete_and_cut_short_data() {
    let s = Scratch::new("mirakc");
    let dir = s.0.join("logos");
    let read = |id: u64| fs::read(dir.join(format!("{id}.png"))).ok();
    let (hd, sd, other) = (png(205, 5), png(200, 0), png(150, 1));

    // a complete run writes every logo
    let files = BTreeMap::from([(1, Some(&hd[..])), (2, Some(&other[..])), (3, None)]);
    assert_eq!(write_mirakc(&dir, &files, true).unwrap(), (2, 0));
    assert_eq!(read(1), Some(hd.clone()));
    assert_eq!(
        write_mirakc(&dir, &files, true).unwrap(),
        (0, 0),
        "unchanged"
    );

    // a run cut short (only a smaller type of 1 came) adds 4 but keeps the rest
    let files = BTreeMap::from([(1, Some(&sd[..])), (2, None), (4, Some(&other[..]))]);
    assert_eq!(write_mirakc(&dir, &files, false).unwrap(), (1, 0));
    assert_eq!((read(1), read(2)), (Some(hd.clone()), Some(other.clone())));
    assert_eq!(read(4), Some(other.clone()));

    // a complete one replaces a changed logo and removes one that is gone
    assert_eq!(write_mirakc(&dir, &files, true).unwrap(), (1, 1));
    assert_eq!((read(1), read(2)), (Some(sd), None));
    assert!(!dir.join("1.png.tmp").exists());
}

// ---- terrestrial: the CDT ----

/// An SDT[actual] with (service_id, logo_transmission_descriptor body) entries; an
/// empty body leaves the descriptor out.
fn sdt(onid: u16, tsid: u16, services: &[(u16, &[u8])]) -> Vec<u8> {
    let mut body = tsid.to_be_bytes().to_vec();
    body.extend_from_slice(&[0xC1, 0, 0]);
    body.extend_from_slice(&onid.to_be_bytes());
    body.push(0xFF);
    for &(sid, logo) in services {
        body.extend_from_slice(&sid.to_be_bytes());
        body.push(0xFC);
        let mut desc = vec![0xC1, 1, 0x84]; // digital_copy_control stand-in
        if !logo.is_empty() {
            desc.extend_from_slice(&[DESC_LOGO_TRANSMISSION, logo.len() as u8]);
            desc.extend_from_slice(logo);
        }
        body.extend_from_slice(&(0x8000 | desc.len() as u16).to_be_bytes());
        body.extend_from_slice(&desc);
    }
    long_section(TID_SDT, &body)
}

/// A CDT section with one logo (download_data_id 1024, logo_version 2).
fn cdt(onid: u16, logo_type: u8, logo_id: u16, data: &[u8]) -> Vec<u8> {
    let mut body = vec![0x04, 0x00, 0xC1, 0, 0];
    body.extend_from_slice(&onid.to_be_bytes());
    body.extend_from_slice(&[DATA_TYPE_LOGO, 0xF0, 0]);
    body.push(logo_type);
    body.extend_from_slice(&(0xFE00 | logo_id).to_be_bytes());
    body.extend_from_slice(&0xF002u16.to_be_bytes());
    body.extend_from_slice(&(data.len() as u16).to_be_bytes());
    body.extend_from_slice(data);
    long_section(TID_CDT, &body)
}

#[test]
fn terrestrial_logos_from_the_cdt() {
    let mut m = Mux::default();
    let mut c = Collector::new(929);
    c.feed(&m.packets(0, &pat(&[(1024, 0x101)])));
    assert_eq!(c.progress(), "no service 929 in this TS, no CDT logo yet");
    // logo_transmission types 1 (CDT, with download_data_id) and 2 (logo_id only) name
    // logo 0; type 3 (a logo string) names none
    let by_cdt = [0x01, 0xFE, 0x00, 0xF0, 0x02, 0x04, 0x00];
    let services: [(u16, &[u8]); 4] = [
        (1024, &by_cdt),
        (1025, &[0x02, 0xFE, 0x00]),
        (1408, &[0x03, b'N']),
        (1032, &[]),
    ];
    c.feed(&m.packets(0x11, &sdt(32736, 0x7FE0, &services)));
    for t in [3, 4, 5, 0] {
        c.feed(&m.packets(0x29, &cdt(32736, t, 0, &png(900 + usize::from(t), t))));
    }
    // other data in the CDT, and a logo no service shows
    let mut other = cdt(32736, 1, 0, &png(901, 1));
    other[10] = 0x02;
    set_section_crc(&mut other);
    c.feed(&m.packets(0x29, &other));
    c.feed(&m.packets(0x29, &cdt(32736, 2, 7, &png(500, 2))));
    assert!(!c.done());
    assert_eq!(
        c.progress(),
        "CDT logos: 32736/0 types [0, 3, 4, 5], 32736/7 types [2]"
    );
    // a type sent more often than the others is no round
    c.feed(&m.packets(0x29, &cdt(32736, 4, 0, &png(904, 4))));
    assert!(!c.done());
    // the first one again: the round is over
    c.feed(&m.packets(0x29, &cdt(32736, 3, 0, &png(903, 3))));
    assert!(c.done());
    assert!(c.progress().ends_with("(all in)"));
    assert!(c.logos().is_empty(), "nothing for TVTest");
    let logos = c.cdt_logos();
    assert_eq!(logos.len(), 4, "logo 7 has no service");
    assert_eq!(
        logos[0].services,
        [(32736, 0x7FE0, 1024), (32736, 0x7FE0, 1025)]
    );
    let files = mirakc_logos(&logos);
    let got: Vec<_> = files
        .iter()
        .map(|(id, p)| (*id, p.map(<[u8]>::len)))
        .collect();
    assert_eq!(
        got,
        [(3_273_601_024, Some(905)), (3_273_601_025, Some(905))]
    );
}

#[test]
fn cdt_done_once_every_named_logo_has_all_types() {
    let mut m = Mux::default();
    let mut c = Collector::new(929);
    c.feed(&m.packets(0, &pat(&[(1032, 0x101)])));
    let named = [0x02, 0xFE, 0x08];
    c.feed(&m.packets(0x11, &sdt(32737, 0x7FE1, &[(1032, &named)])));
    for t in 0..5 {
        c.feed(&m.packets(0x29, &cdt(32737, t, 8, &png(300, t))));
        assert!(!c.done());
    }
    c.feed(&m.packets(0x29, &cdt(32737, 5, 8, &png(300, 5))));
    assert!(c.done(), "all six types, no need to wait for the round");
}

#[test]
fn the_engineering_service_ignores_a_cdt() {
    let bs = logo_module(0, &[(1, &[(4, 0x40F1, 101)], &png(300, 7))]);
    let cs = logo_module(0, &[(2, &[(6, 0x6020, 296)], &png(150, 9))]);
    let ts = engineering_ts(&bs, &cs);
    let mut m = Mux::default();
    let mut c = Collector::new(929);
    // the PAT first, then a whole CDT round, without the carousels' modules
    c.feed(&ts[..PACKET]);
    let named = [0x02, 0xFE, 0x01];
    c.feed(&m.packets(0x11, &sdt(4, 0x40F1, &[(101, &named)])));
    for t in [0, 1, 0] {
        c.feed(&m.packets(0x29, &cdt(4, t, 1, &png(300, t))));
    }
    assert!(!c.done(), "the carousels decide for service 929");
    c.feed(&ts[PACKET..]);
    assert!(c.done());
}
