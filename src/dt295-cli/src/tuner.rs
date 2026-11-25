//! What `dt295 capture`, `dt295 logo` and `dt295 serve` share: the device options,
//! opening and booting a unit, waiting for lock, Ctrl-C, and carrying the link key in
//! the state file (by the rules of [`dt295::state`]).

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use dt295::bonini::DeviceSel;
use dt295::state::{State, key_journal};
use dt295::{Device, System};

/// Which unit to open and the firmware to boot it with.
#[derive(Debug, Clone, Args)]
pub struct DeviceArgs {
    /// Idle firmware image (mb86m01_assp_osec_idle.bin from the vendor package).
    #[arg(long, value_name = "FILE")]
    pub fw: Option<PathBuf>,

    /// Transcoder firmware (mb86m01_assp_osec_trc_m_h.bin) for the System1 (EP 0x81)
    /// viewing path -- required to stream (there is no System2 path).
    #[arg(long, value_name = "FILE")]
    pub trc: Option<PathBuf>,

    /// Which PIX-DT295, by number (0 = the first free one; see `dt295 list`).
    #[arg(
        short,
        long,
        value_name = "N",
        default_value_t = 0,
        conflicts_with = "serial"
    )]
    pub index: usize,

    /// The PIX-DT295 with this USB serial number (see `dt295 list`).
    #[arg(long, value_name = "SERIAL")]
    pub serial: Option<String>,

    /// A state file (the unit's, <serial>.state next to the BonDriver or in /var/lib/dt295):
    /// use its link key and save the key this stream proves, so the next program to open
    /// the unit (dt295 serve, or dt295 again) can still decrypt its TS.
    #[arg(long, value_name = "FILE")]
    pub state: Option<PathBuf>,

    /// Without `state`: the folder of the unit's state file, `<serial>.state` (a
    /// BonDriver ini's folder).
    #[arg(skip)]
    pub state_dir: Option<PathBuf>,
}

/// Set by Ctrl-C: stop cleanly (the stream is stopped, the link key saved).
static STOP: AtomicBool = AtomicBool::new(false);

pub fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

#[cfg(windows)]
pub fn install_ctrl_c() {
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::core::BOOL;

    unsafe extern "system" fn on_ctrl(ctrl: u32) -> BOOL {
        if ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT {
            STOP.store(true, Ordering::Relaxed);
            TRUE
        } else {
            FALSE
        }
    }
    // SAFETY: registers a handler that only stores an atomic; it lives for the
    // whole process.
    if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), TRUE) } == 0 {
        tracing::warn!("cannot install the Ctrl-C handler");
    }
}

/// SIGINT, SIGTERM (what a recorder sends to stop a tuner command) and SIGHUP.
#[cfg(unix)]
pub fn install_ctrl_c() {
    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores an atomic, which is async-signal-safe;
        // it lives for the whole process.
        if unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            tracing::warn!("cannot install the handler of signal {sig}");
        }
    }
}

#[cfg(not(any(windows, unix)))]
pub fn install_ctrl_c() {}

/// Open the unit `--serial`, else `-i`, without booting anything (the raw-TS path boots
/// no firmware), trying again for a moment while it cannot be opened, as the BonDriver
/// does ([`Device::open_sel`]).
pub fn open_device(a: &DeviceArgs) -> Result<Device> {
    let sel = match &a.serial {
        Some(s) => DeviceSel::Serial(s.clone()),
        None => DeviceSel::Index(a.index),
    };
    let dev = Device::open_sel(&sel).with_context(|| format!("open device ({sel})"))?;
    if let Some(s) = dev.serial() {
        tracing::info!("device serial {s}");
    }
    Ok(dev)
}

/// An open, booted unit and the state file it takes its link key from.
pub struct Unit {
    pub dev: Device,
    /// The idle firmware, for a restart.
    fw: PathBuf,
    state: Option<(PathBuf, State)>,
    /// The journal wrote an unproved key to the state file.
    pending_written: Arc<AtomicBool>,
}

impl Unit {
    /// Open the unit, hand it the state file's link key, and boot the firmware.
    pub fn open(a: &DeviceArgs) -> Result<Self> {
        let fw =
            a.fw.as_ref()
                .context("--fw is required to use the device")?;
        let mut dev = open_device(a)?;
        // The device keeps applying the link key it handed out first until its secure
        // session lapses; a later process gets a fresh key it never applies. The state
        // file carries the applied key between processes (dt295 serve's and ours).
        let path = a.state.clone().or_else(|| {
            let dir = a.state_dir.as_ref()?;
            let serial = dev.serial()?;
            Some(dt295::bonini::resolve(
                dir,
                &format!("{}.state", serial.to_ascii_uppercase()),
            ))
        });
        let state = match path {
            Some(p) => {
                let st = State::load_settled(&p)
                    .with_context(|| format!("cannot read {}", p.display()))?
                    .unwrap_or_default();
                Some((p, st))
            }
            None => None,
        };
        // --trc selects the System1 (EP 0x81) viewing path. Set it BEFORE start: the
        // front-end power-on sequence depends on it.
        if let Some(trc) = &a.trc {
            dev.set_trc_firmware(Some(trc.clone()));
        }
        let pending_written = Arc::new(AtomicBool::new(false));
        if let Some((path, st)) = &state {
            let keys = st.prior_keys();
            if !keys.is_empty() {
                dev.set_prior_link_keys(&keys);
            }
            dev.set_link_key_journal(key_journal(path.clone(), Arc::clone(&pending_written)));
        }
        dev.start(fw).context("start")?;
        let mut unit = Self {
            dev,
            fw: fw.clone(),
            state,
            pending_written,
        };
        if unit.dev.needs_recovery() {
            tracing::warn!("the firmware stopped acking during the start; starting once more");
            unit.restart()?;
        }
        Ok(unit)
    }

    /// Start the device once more, after the firmware faulted (0xFFFF) or stopped
    /// acking. That happens within about a minute of an earlier process: its secure
    /// session times out under this one. The BonDriver recovers the same way before a
    /// tune.
    pub fn restart(&mut self) -> Result<()> {
        self.dev.start(&self.fw).context("start (recovery)")
    }

    /// Wait up to 3 s for lock, showing the C/N on stderr; without it an error
    /// (`dt295 serve` refuses such a tune the same way), unless Ctrl-C came.
    pub fn wait_lock(&mut self, sys: System) -> Result<()> {
        // on a terminal the line is redrawn; into a log (a recorder) only the last one
        let tty = std::io::stderr().is_terminal();
        let mut line = String::new();
        let mut cn = None;
        for _ in 0..30 {
            if stopping() {
                break;
            }
            if let Ok(st) = self.dev.read_status(sys) {
                line = format!(
                    "lock={} C/N={:.2} dB (raw 0x{:x})",
                    u8::from(st.locked),
                    f64::from(st.cn_mdb) / 1000.0,
                    st.raw
                );
                if tty {
                    eprint!("{line}\r");
                }
                if st.locked {
                    cn = None;
                    break;
                }
                cn = Some(st.cn_mdb);
            }
            thread::sleep(Duration::from_millis(100));
        }
        if tty {
            eprintln!();
        } else if !line.is_empty() {
            eprintln!("{line}");
        }
        match cn {
            Some(cn) if !stopping() => bail!("no lock (C/N {:.1} dB)", f64::from(cn) / 1000.0),
            None if line.is_empty() && !stopping() => bail!("no lock (no status)"),
            _ => Ok(()),
        }
    }

    /// Save the link key a stream proved to the state file, if it changed. The unproved
    /// keys listed give way to the one this process was handed, while it is not the
    /// proved one (the device may still switch to it).
    pub fn save_link_key(&mut self) {
        let Some((p, st)) = &mut self.state else {
            return;
        };
        if self.dev.link_key().is_none() {
            return;
        }
        let pending = self.pending_written.swap(false, Ordering::Relaxed);
        let taken = st.take_link_keys(self.dev.link_key(), self.dev.unproved_link_key());
        if taken || pending {
            match st.save(p) {
                Ok(()) => eprintln!(
                    "link key: saved the key this stream proved to {}",
                    p.display()
                ),
                Err(e) => eprintln!("link key: cannot save {}: {e}", p.display()),
            }
        }
    }
}
