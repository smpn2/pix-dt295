//! `dt295` command-line tool.
//!
//! * `dt295 capture` -- boot the device, tune, and write the TS (every service, or the
//!   one of `--sid`) to a file or stdout.
//! * `dt295 list` -- the connected units with their USB serial numbers (for
//!   `[Tuner] DeviceSerial` / `--serial`).
//! * `dt295 state show <file>` -- summarize a state file (the BonDriver's or
//!   `dt295 serve`'s).
//! * `dt295 lineup <file>` -- regenerate the `[BS]` / `[CS110]` sections of
//!   BonDriver_DT295.ini from the SDTs in a state file.
//! * `dt295 logo` -- receive the station logos: BS / CS ones into TVTest's logo files,
//!   or BS / CS and terrestrial ones as PNG files for mirakc.
//! * `dt295 driver status|install|restore` (Windows) -- which driver the units use;
//!   switch them to WinUSB (no Zadig needed) or back to the vendor driver.
//! * `dt295 etw-urbs <xml> <tsv>` -- a USB ETW trace of the vendor driver as an access log.
//! * `dt295 mirakc-channels` -- the `channels:` of a mirakc configuration from a TVTest
//!   channel scan and its ini.
//! * `dt295 serve` (Linux) -- hold the configured units open and stream to `dt295 tune`.
//! * `dt295 tune` (Linux) -- ask `dt295 serve` for a stream and write its TS to stdout
//!   (a recorder's tuner command).
//!
//! Logs go to stderr (`-v` / `-vv` for debug / trace; `RUST_LOG` overrides).

mod bonini;
mod capture;
mod driver;
mod etw;
mod lineup;
mod list;
mod logo;
mod mirakc;
mod rawts;
mod reg;
mod sections;
mod serve;
mod state_show;
mod tune;
mod tuner;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, ArgGroup, Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "dt295", version = env!("DT295_VERSION"), about = "PIX-DT295 tools")]
struct Cli {
    /// More log output (-v debug, -vv trace); RUST_LOG overrides.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Boot the device, tune, and write the TS (every service, or the one of --sid) to a
    /// file or stdout; with --raw-ts the scrambled TS, booting nothing.
    Capture(CaptureArgs),
    /// List the connected units (PIX-DT295(W), PIX-DT195(W)): index, USB serial number, model,
    /// whether free.
    List,
    /// Inspect a state file (dt295 serve's <serial>.state, or an older BonDriver_DT295.state).
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    /// Regenerate the [BS] / [CS110] sections of BonDriver_DT295.ini from the SDTs in a state file.
    Lineup(lineup::LineupArgs),
    /// Receive the station logos: BS / CS ones into TVTest's logo files, or as PNG files for mirakc.
    Logo(logo::LogoArgs),
    /// (Windows) Show or switch the driver of the units: WinUSB (dt295 needs it) or the vendor's.
    #[command(hide = cfg!(not(windows)))]
    Driver {
        #[command(subcommand)]
        command: driver::DriverCommand,
    },
    /// Read or write chip registers through the 0xBC window (raw-TS bring-up helper).
    Reg(reg::RegArgs),
    /// Turn a USB ETW trace of the vendor driver (tracerpt XML) into an access log.
    EtwUrbs(etw::EtwArgs),
    /// The channels of a mirakc configuration: from a TVTest channel scan (.ch2 and its ini),
    /// or (Linux) from a scan through `dt295 serve`.
    MirakcChannels(mirakc::MirakcArgs),
    /// Hold units open and stream to `dt295 tune` clients and the BonDriver (the units of
    /// the configuration, or each unit a request asks for).
    Serve(serve::ServeArgs),
    /// Ask `dt295 serve` for a stream and write its TS to stdout (a recorder's tuner command).
    Tune(tune::TuneArgs),
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// Summary: link key, PSI sections per table and TS, seen days.
    Show {
        /// The state file.
        file: PathBuf,
    },
}

/// `dt295 capture` options (the frequencies in Hz).
#[derive(Debug, Args)]
#[command(group(ArgGroup::new("band").required(true).args(["ter", "sat", "channel"])))]
#[command(group(
    ArgGroup::new("firmware")
        .required(true)
        .multiple(true)
        .args(["fw", "bondriver", "config", "raw_ts"])
))]
pub struct CaptureArgs {
    #[command(flatten)]
    pub dev: tuner::DeviceArgs,

    /// A BonDriver_DT295 ini (on Linux a file of the same format): the firmware,
    /// the unit, the state file, RawTS and the channels for --channel. Options given
    /// here win over it.
    #[arg(long, value_name = "INI", conflicts_with = "fw")]
    pub bondriver: Option<PathBuf>,

    /// The configuration of `dt295 serve` (/etc/dt295.conf): the firmware, the unit's
    /// state file and raw-ts from it, so the link key carries over between the daemon
    /// and a capture (stop the daemon first: it holds its units).
    #[arg(short = 'c', long, value_name = "FILE", conflicts_with_all = ["bondriver", "fw"])]
    pub config: Option<PathBuf>,

    /// Stream on the blob-free raw-TS path: no vendor firmware and no state file, the
    /// TS stays scrambled for the host to descramble with a real card, every service of
    /// it. A RawTS=1 ini or a raw-ts = true unit selects it too. The first tune after a
    /// plug brings the chip up (some seconds); later ones, in this process or the next,
    /// only retune. A unit booted with the vendor firmware since its plug needs a
    /// re-plug first.
    #[arg(long, conflicts_with_all = ["fw", "trc", "state", "sid"])]
    pub raw_ts: bool,

    /// A channel by name: one of --bondriver (a [BS] / [CS110] line, e.g. BS15/TS1, or
    /// a channel of its ChannelFile), or a built-in terrestrial one (27ch, C13ch).
    #[arg(long, value_name = "NAME", conflicts_with_all = ["ter", "sat", "tsid"])]
    pub channel: Option<String>,

    /// ISDB-T: the channel centre frequency in Hz (e.g. 557142857 for UHF 27ch).
    #[arg(long, value_name = "HZ", value_parser = parse_u32)]
    pub ter: Option<u32>,

    /// ISDB-S: the IF frequency in Hz (satellite RF - 10678 MHz, e.g. 1318000000).
    #[arg(long, value_name = "HZ", value_parser = parse_u32)]
    pub sat: Option<u32>,

    /// ISDB-S: the broadcast transport_stream_id in hex (e.g. 40f1 or 0x40F1); 7 or
    /// less is a TS number, the TS whose TSID has those low 3 bits.
    #[arg(long, value_name = "HEX", value_parser = parse_hex_u16, conflicts_with = "ter")]
    pub tsid: Option<u16>,

    /// The service to stream (decimal or 0x hex); default: every service of the TS.
    #[arg(long, value_name = "ID", value_parser = parse_u16)]
    pub sid: Option<u16>,

    /// Every service of the TS even when the --channel line names a service.
    #[arg(long, conflicts_with = "sid")]
    pub all_services: bool,

    /// Capture duration in seconds (Ctrl-C stops earlier); 0 = until Ctrl-C /
    /// SIGTERM, or until the reader closes the output.
    #[arg(short = 't', long, value_name = "SECONDS", default_value_t = 10)]
    pub seconds: u64,

    /// Output file; default stdout (which must not be a terminal).
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

/// A number in decimal or `0x` hex.
fn parse_u64_auto(s: &str) -> Result<u64, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => s.parse(),
    };
    r.map_err(|e| format!("{s:?}: {e}"))
}

pub(crate) fn parse_u32(s: &str) -> Result<u32, String> {
    u32::try_from(parse_u64_auto(s)?).map_err(|_| format!("{s:?}: out of range"))
}

pub(crate) fn parse_u16(s: &str) -> Result<u16, String> {
    u16::try_from(parse_u64_auto(s)?).map_err(|_| format!("{s:?}: out of range"))
}

/// Hex with or without `0x`.
pub(crate) fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u16::from_str_radix(hex, 16).map_err(|e| format!("{s:?}: {e}"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let default_level = match cli.verbose {
        // `logo` runs unattended for up to hours: its own lines, not the stream's.
        // nusb logs as errors what dt295 expects (a busy unit while `list` looks for
        // free ones) and reports itself.
        0 if matches!(cli.command, Command::Logo(_)) => "warn,nusb=off",
        0 => "info,nusb=off",
        1 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // no colour codes in a log file or the journal
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .init();

    let result = match cli.command {
        Command::Capture(args) => capture::run(&args),
        Command::List => list::run(),
        Command::State {
            command: StateCommand::Show { file },
        } => state_show::run(&file),
        Command::Lineup(args) => return lineup::run(&args),
        Command::Logo(args) => logo::run(&args),
        Command::Driver { command } => driver::run(&command),
        Command::Reg(args) => reg::run(&args),
        Command::EtwUrbs(args) => etw::run(&args),
        Command::MirakcChannels(args) => mirakc::run(&args),
        Command::Serve(args) => serve::run(&args),
        Command::Tune(args) => tune::run(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("dt295").chain(args.iter().copied()))
    }

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn capture_args() {
        let cli = parse(&[
            "capture",
            "--fw",
            "idle.bin",
            "--trc",
            "trc.bin",
            "--sat",
            "1318000000",
            "--tsid",
            "40f1",
            "--sid",
            "0x65",
            "-t",
            "5",
            "-o",
            "out.ts",
        ])
        .unwrap();
        let Command::Capture(a) = cli.command else {
            panic!("not capture")
        };
        assert_eq!(a.sat, Some(1_318_000_000));
        assert_eq!(a.tsid, Some(0x40F1));
        assert_eq!(a.sid, Some(101));
        assert_eq!(a.seconds, 5);
        assert_eq!(a.output, Some(PathBuf::from("out.ts")));
        assert_eq!(a.dev.fw, Some(PathBuf::from("idle.bin")));
        assert_eq!(a.dev.trc, Some(PathBuf::from("trc.bin")));
        assert_eq!(a.dev.index, 0);
        assert_eq!(a.dev.serial, None);

        let cli = parse(&["capture", "--fw", "f", "--ter", "557142857"]).unwrap();
        let Command::Capture(a) = cli.command else {
            panic!("not capture")
        };
        assert_eq!(
            (a.ter, a.sat, a.tsid, a.sid, a.seconds),
            (Some(557_142_857), None, None, None, 10)
        );
    }

    #[test]
    fn capture_arg_errors() {
        // One of --ter / --sat, not both; --tsid only with --sat; --fw required.
        assert!(parse(&["capture", "--fw", "f"]).is_err());
        assert!(parse(&["capture", "--fw", "f", "--ter", "1", "--sat", "2"]).is_err());
        assert!(parse(&["capture", "--fw", "f", "--ter", "1", "--tsid", "4010"]).is_err());
        assert!(parse(&["capture", "--ter", "1"]).is_err());
        assert!(parse(&["capture", "--fw", "f", "--sat", "1", "--tsid", "zz"]).is_err());
        assert!(parse(&["capture", "--fw", "f", "--ter", "1", "--sid", "70000"]).is_err());
        assert!(
            parse(&[
                "capture", "--fw", "f", "--ter", "1", "-i", "1", "--serial", "X"
            ])
            .is_err(),
            "--index or --serial"
        );
    }

    #[test]
    fn list_and_serial_args() {
        assert_matches!(parse(&["list"]).unwrap().command, Command::List);
        let cli = parse(&[
            "capture",
            "--fw",
            "f",
            "--ter",
            "1",
            "--serial",
            "CD23456789",
        ])
        .unwrap();
        let Command::Capture(a) = cli.command else {
            panic!("not capture")
        };
        assert_eq!(a.dev.serial.as_deref(), Some("CD23456789"));
    }

    #[test]
    fn state_show_args() {
        let cli = parse(&["state", "show", "BonDriver_DT295.state"]).unwrap();
        assert_matches!(
            cli.command,
            Command::State { command: StateCommand::Show { file } }
                if file == std::path::Path::new("BonDriver_DT295.state")
        );
    }

    #[test]
    fn number_parsers() {
        assert_eq!(parse_u32("0x10"), Ok(16));
        assert_eq!(parse_u32("16"), Ok(16));
        assert!(parse_u32("4294967296").is_err());
        assert_eq!(parse_hex_u16("4010"), Ok(0x4010));
        assert_eq!(parse_hex_u16("0X4010"), Ok(0x4010));
        assert!(parse_hex_u16("10000").is_err());
        assert_eq!(parse_u16("1024"), Ok(1024));
    }

    #[test]
    fn capture_with_an_ini() {
        assert!(
            parse(&[
                "capture",
                "--bondriver",
                "d.ini",
                "--channel",
                "BS15/101",
                "-t",
                "0"
            ])
            .is_ok()
        );
        assert!(parse(&["capture", "--bondriver", "d.ini", "--ter", "557142857"]).is_ok());
        assert!(parse(&["capture", "--fw", "f", "--channel", "27ch"]).is_ok());
        assert!(
            parse(&["capture", "--bondriver", "d.ini"]).is_err(),
            "a band is needed"
        );
        assert!(
            parse(&[
                "capture",
                "--bondriver",
                "d.ini",
                "--channel",
                "27ch",
                "--ter",
                "1"
            ])
            .is_err(),
            "--channel or --ter"
        );
        assert!(
            parse(&["capture", "--channel", "27ch"]).is_err(),
            "--fw or --bondriver"
        );
    }
}
