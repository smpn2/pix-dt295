//! `bontest`: load a BonDriver DLL the way TVTest does and exercise it (the virtual
//! calls go through `cpp/bonhost.cpp`).
//!
//! ```text
//! bontest <BonDriver.dll>                          list tuning spaces / channels
//! bontest <BonDriver.dll> <space> <ch> [seconds] [out.ts]
//!                                                  open, tune, print C/N and TS bytes;
//!                                                  with out.ts also save the TS the host got
//! bontest <BonDriver.dll> scan <space>[:<first>[-<last>]]...
//!                                                  open once and tune every channel of each
//!                                                  space / range in turn (what TVTest channel
//!                                                  scans ask of the driver), timing each call;
//!                                                  e.g. `scan 2:0 0` = BS ch 0, then all of UHF
//! bontest <BonDriver.dll> idle [seconds]
//!                                                  open, hold the tuner without a channel
//!                                                  (default 60 s), close: dt295 serve stays
//!                                                  while the tuner is open
//! bontest <BonDriver.dll> reopen <space> <ch> [seconds] [pause]
//!                                                  open, tune, stream, close, then after
//!                                                  `pause` s (default 90) all again with the
//!                                                  DLL still loaded: a host that keeps it
//!                                                  (dt295 serve ended and started again
//!                                                  meanwhile)
//! ```
//!
//! The DLL watches through `dt295 serve`: the forms that open the tuner need dt295.exe
//! next to the DLL (it starts one when none answers), and to stream, the device bound to
//! WinUSB and the firmware there too. Give the DLL as an absolute path: LoadLibrary looks
//! for a bare or relative name next to bontest.exe first.

#[cfg(all(windows, target_env = "msvc"))]
fn main() -> std::process::ExitCode {
    host::main()
}

#[cfg(not(all(windows, target_env = "msvc")))]
fn main() -> std::process::ExitCode {
    eprintln!("bontest: Windows (MSVC) only");
    std::process::ExitCode::from(2)
}

#[cfg(all(windows, target_env = "msvc"))]
mod host {
    use std::ffi::{OsString, c_void};
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type Bon = *mut c_void;
    type CreateFn = unsafe extern "C" fn() -> *mut c_void;
    const WAIT_OBJECT_0: u32 = 0;

    // cpp/bonhost.cpp: thin wrappers over the IBonDriver2 virtual calls. build.rs puts it
    // in the static `bonshim` library, whose `-l` Cargo gives the lib target only (the
    // search path reaches every target), so the bin names it itself.
    #[link(name = "bonshim", kind = "static")]
    unsafe extern "C" {
        fn bonhost_create(create: CreateFn) -> Bon;
        fn bonhost_tuner_name(b: Bon) -> *const u16;
        fn bonhost_enum_space(b: Bon, s: u32) -> *const u16;
        fn bonhost_enum_channel(b: Bon, s: u32, c: u32) -> *const u16;
        fn bonhost_open(b: Bon) -> i32;
        fn bonhost_is_opening(b: Bon) -> i32;
        fn bonhost_set_channel(b: Bon, s: u32, c: u32) -> i32;
        fn bonhost_cur_space(b: Bon) -> u32;
        fn bonhost_cur_channel(b: Bon) -> u32;
        fn bonhost_signal(b: Bon) -> f32;
        fn bonhost_wait(b: Bon, ms: u32) -> u32;
        fn bonhost_get_ts(b: Bon, p: *mut *mut u8, size: *mut u32, remain: *mut u32) -> i32;
        fn bonhost_close(b: Bon);
        fn bonhost_release(b: Bon);
    }

    /// A NUL-terminated UTF-16 string from the driver; `None` for NULL.
    fn wide(p: *const u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        // SAFETY: the driver returns NUL-terminated strings it keeps alive (TVTest
        // relies on the same); `len` stops at the NUL.
        let s = unsafe {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            std::slice::from_raw_parts(p, len)
        };
        Some(String::from_utf16_lossy(s))
    }

    /// `wcstoul(s, NULL, 0)` for the space / channel numbers: decimal or `0x` hex.
    fn number(s: &str) -> Option<u32> {
        match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(h) => u32::from_str_radix(h, 16).ok(),
            None => s.parse().ok(),
        }
    }

    pub fn main() -> ExitCode {
        let args: Vec<OsString> = std::env::args_os().collect();
        if args.len() < 2 {
            eprintln!(
                "usage: bontest <BonDriver.dll> [<space> <channel> [seconds] [out.ts]]\n       \
                 bontest <BonDriver.dll> scan <space>[:<first>[-<last>]]...\n       \
                 bontest <BonDriver.dll> idle [seconds]\n       \
                 bontest <BonDriver.dll> reopen <space> <channel> [seconds] [pause]"
            );
            return ExitCode::from(2);
        }
        let path: Vec<u16> = args[1].encode_wide().chain([0]).collect();
        // SAFETY: a NUL-terminated path; loading runs the DLL's initialisation, which is
        // what this tool is for.
        let dll = unsafe { LoadLibraryW(path.as_ptr()) };
        if dll.is_null() {
            // SAFETY: no other call in between.
            eprintln!("LoadLibrary failed ({})", unsafe { GetLastError() });
            return ExitCode::from(1);
        }
        // SAFETY: a valid module handle and a NUL-terminated ASCII name.
        let Some(proc) = (unsafe { GetProcAddress(dll, c"CreateBonDriver".as_ptr().cast()) })
        else {
            eprintln!("CreateBonDriver not exported");
            return ExitCode::from(1);
        };
        // SAFETY: CreateBonDriver is `IBonDriver *CreateBonDriver()` (cdecl), the
        // signature TVTest calls it with.
        let create: CreateFn = unsafe { std::mem::transmute(proc) };
        // SAFETY: `create` is the DLL's export; bonhost_create calls it and casts.
        let bon = unsafe { bonhost_create(create) };
        if bon.is_null() {
            eprintln!("not an IBonDriver2");
            return ExitCode::from(1);
        }
        // SAFETY: `bon` is the live IBonDriver2 returned above, released only below; the
        // DLL stays loaded until after that.
        let rc = unsafe { run(bon, &args[2..]) };
        // SAFETY: as above; `bon` is not used after this.
        unsafe { bonhost_release(bon) };
        // SAFETY: the driver is released; nothing of the DLL is used after this.
        unsafe { FreeLibrary(dll) };
        rc
    }

    unsafe fn run(bon: Bon, args: &[OsString]) -> ExitCode {
        // SAFETY: see the caller.
        unsafe {
            println!(
                "tuner: {}",
                wide(bonhost_tuner_name(bon)).unwrap_or_default()
            );
            if args.len() < 2 {
                let mut s = 0;
                while let Some(name) = wide(bonhost_enum_space(bon, s)) {
                    let n = (0..)
                        .take_while(|&c| !bonhost_enum_channel(bon, s, c).is_null())
                        .count() as u32;
                    let ch =
                        |c| wide(bonhost_enum_channel(bon, s, c)).unwrap_or_else(|| "-".into());
                    let (first, last) = if n > 0 {
                        (ch(0), ch(n - 1))
                    } else {
                        ("-".into(), "-".into())
                    };
                    println!("space {s}: {name} ({n} ch: {first} .. {last})");
                    s += 1;
                }
                return ExitCode::SUCCESS;
            }

            let arg = |i: usize| args.get(i).map(|a| a.to_string_lossy().into_owned());
            if arg(0).as_deref() == Some("idle") {
                let secs: u64 = arg(1).and_then(|s| s.parse().ok()).unwrap_or(60);
                return idle(bon, secs);
            }
            if arg(0).as_deref() == Some("scan") {
                let ranges: Option<Vec<_>> = args[1..]
                    .iter()
                    .map(|a| scan_range(&a.to_string_lossy()))
                    .collect();
                let Some(ranges) = ranges else {
                    eprintln!("bad range (<space>[:<first>[-<last>]])");
                    return ExitCode::from(2);
                };
                return scan(bon, &ranges);
            }
            if arg(0).as_deref() == Some("reopen") {
                let (Some(space), Some(ch)) = (
                    arg(1).as_deref().and_then(number),
                    arg(2).as_deref().and_then(number),
                ) else {
                    eprintln!("bad space / channel number");
                    return ExitCode::from(2);
                };
                let secs: u32 = arg(3).and_then(|s| s.parse().ok()).unwrap_or(10);
                let pause: u64 = arg(4).and_then(|s| s.parse().ok()).unwrap_or(90);
                return reopen(bon, space, ch, secs, pause);
            }
            let (Some(space), Some(ch)) = (
                arg(0).as_deref().and_then(number),
                arg(1).as_deref().and_then(number),
            ) else {
                eprintln!("bad space / channel number");
                return ExitCode::from(2);
            };
            let secs: u32 = arg(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let out = match args.get(3) {
                Some(p) => match File::create(p) {
                    Ok(f) => Some(BufWriter::new(f)),
                    Err(e) => {
                        eprintln!("cannot write {}: {e}", p.to_string_lossy());
                        return ExitCode::from(1);
                    }
                },
                None => None,
            };

            if bonhost_open(bon) == 0 {
                eprintln!("OpenTuner failed (see DebugView)");
                return ExitCode::from(1);
            }
            println!("open: IsTunerOpening={}", bonhost_is_opening(bon));
            let out = stream(bon, space, ch, secs, out);
            bonhost_close(bon);
            if let Some(mut w) = out
                && let Err(e) = w.flush()
            {
                eprintln!("writing the TS: {e}");
            }
            ExitCode::SUCCESS
        }
    }

    /// Tune `(space, ch)` on the open tuner and read the TS for `secs`, printing C/N and
    /// the bytes each second (into `out` too, if given); gives `out` back.
    unsafe fn stream(
        bon: Bon,
        space: u32,
        ch: u32,
        secs: u32,
        mut out: Option<BufWriter<File>>,
    ) -> Option<BufWriter<File>> {
        // SAFETY: see `run`.
        unsafe {
            if bonhost_set_channel(bon, space, ch) == 0 {
                eprintln!("SetChannel({space},{ch}) failed");
                return out;
            }
            println!(
                "tuned {} / {}  (cur {}/{})",
                wide(bonhost_enum_space(bon, space)).unwrap_or_default(),
                wide(bonhost_enum_channel(bon, space, ch)).unwrap_or_default(),
                bonhost_cur_space(bon),
                bonhost_cur_channel(bon)
            );
            let t0 = Instant::now();
            let mut total: u64 = 0;
            for i in 1..=secs {
                while t0.elapsed() < Duration::from_secs(u64::from(i)) {
                    let (mut p, mut size, mut remain) = (std::ptr::null_mut(), 0u32, 0u32);
                    if bonhost_wait(bon, 100) == WAIT_OBJECT_0
                        && bonhost_get_ts(bon, &mut p, &mut size, &mut remain) != 0
                        && !p.is_null()
                    {
                        total += u64::from(size);
                        if let Some(w) = out.as_mut().filter(|_| size > 0) {
                            // the driver's buffer stays valid until the next call
                            let data = std::slice::from_raw_parts(p, size as usize);
                            if let Err(e) = w.write_all(data) {
                                eprintln!("writing the TS: {e}");
                                out = None;
                            }
                        }
                    }
                }
                println!("t={i}s  C/N={:.2} dB  TS={total} B", bonhost_signal(bon));
            }
            out
        }
    }

    /// `reopen`: open, stream, close, wait `pause` s, and all again with the DLL still
    /// loaded and the driver not released, as a host that keeps it does between viewing
    /// (TVTest releases it). The driver's log says what it read at the second open.
    unsafe fn reopen(bon: Bon, space: u32, ch: u32, secs: u32, pause: u64) -> ExitCode {
        // SAFETY: see `run`.
        unsafe {
            for pass in 1..=2 {
                if pass == 2 {
                    println!("closed; opening again in {pause} s");
                    std::thread::sleep(Duration::from_secs(pause));
                }
                if bonhost_open(bon) == 0 {
                    eprintln!("OpenTuner {pass} failed (see DebugView)");
                    return ExitCode::from(1);
                }
                println!("open {pass}: IsTunerOpening={}", bonhost_is_opening(bon));
                stream(bon, space, ch, secs, None);
                bonhost_close(bon);
            }
        }
        ExitCode::SUCCESS
    }

    /// `idle`: open the tuner and hold it `secs` without a channel, then close it: what a
    /// host does between viewing (dt295 serve stays meanwhile, and ends after the close).
    unsafe fn idle(bon: Bon, secs: u64) -> ExitCode {
        // SAFETY: see the caller.
        unsafe {
            if bonhost_open(bon) == 0 {
                eprintln!("OpenTuner failed (see DebugView)");
                return ExitCode::from(1);
            }
            println!(
                "open: IsTunerOpening={}; idle for {secs} s",
                bonhost_is_opening(bon)
            );
            std::thread::sleep(Duration::from_secs(secs));
            bonhost_close(bon);
        }
        ExitCode::SUCCESS
    }

    /// A `scan` argument: (space, first channel, last channel or `None` = to the end).
    type Range = (u32, u32, Option<u32>);

    /// `<space>[:<first>[-<last>]]`.
    fn scan_range(s: &str) -> Option<Range> {
        let (space, chans) = match s.split_once(':') {
            Some((sp, ch)) => (sp, Some(ch)),
            None => (s, None),
        };
        let space = number(space)?;
        let Some(chans) = chans else {
            return Some((space, 0, None));
        };
        Some(match chans.split_once('-') {
            Some((a, b)) => (space, number(a)?, Some(number(b)?)),
            None => {
                let c = number(chans)?;
                (space, c, Some(c))
            }
        })
    }

    /// `scan`: SetChannel on every channel of each range within one open, as TVTest
    /// channel scans do (without their waits for the PSI), timing each call.
    unsafe fn scan(bon: Bon, ranges: &[Range]) -> ExitCode {
        // SAFETY: see `run`.
        unsafe {
            if bonhost_open(bon) == 0 {
                eprintln!("OpenTuner failed (see DebugView)");
                return ExitCode::from(1);
            }
            let t_all = Instant::now();
            let (mut n, mut streams) = (0u32, 0u32);
            for &(space, first, last) in ranges {
                let mut c = first;
                while last.is_none_or(|l| c <= l) {
                    let Some(name) = wide(bonhost_enum_channel(bon, space, c)) else {
                        break;
                    };
                    let t = Instant::now();
                    let ok = bonhost_set_channel(bon, space, c) != 0;
                    let ms = t.elapsed().as_millis();
                    streams += u32::from(ok);
                    println!(
                        "{name:>10}: {:<6} {ms:>5} ms  C/N={:.2} dB",
                        if ok { "stream" } else { "-" },
                        bonhost_signal(bon)
                    );
                    n += 1;
                    c += 1;
                }
            }
            println!(
                "{n} channel(s), {streams} stream(s), {:.1} s",
                t_all.elapsed().as_secs_f64()
            );
            bonhost_close(bon);
            ExitCode::SUCCESS
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn numbers_like_wcstoul() {
            assert_eq!(number("14"), Some(14));
            assert_eq!(number("0x10"), Some(16));
            assert_eq!(number("x"), None);
        }

        #[test]
        fn scan_ranges() {
            assert_eq!(scan_range("0"), Some((0, 0, None)));
            assert_eq!(scan_range("2:3"), Some((2, 3, Some(3))));
            assert_eq!(scan_range("0:4-14"), Some((0, 4, Some(14))));
            assert_eq!(scan_range("0:x"), None);
            assert_eq!(scan_range("0:4-"), None);
        }

        #[test]
        fn wide_strings() {
            assert_eq!(wide(std::ptr::null()), None);
            let s: Vec<u16> = "UHF".encode_utf16().chain([0]).collect();
            assert_eq!(wide(s.as_ptr()).as_deref(), Some("UHF"));
        }
    }
}
