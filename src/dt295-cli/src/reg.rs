//! `dt295 reg`: read or write chip registers through the stock 0xBC window
//! (24-bit addresses). A bring-up / analysis helper for the raw-TS firmware work.
//! Register and RAM writes are reset by a re-plug.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use dt295::Device;

#[derive(Debug, Clone, Args)]
pub struct RegArgs {
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

    /// The tuner the cold-* commands use: 1, or 2 on a two-tuner model (PIX-DT295W, PIX-DT195W).
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=2))]
    pub tuner: u8,

    #[command(subcommand)]
    pub command: RegCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum RegCommand {
    /// Dump a range of registers as hex.
    Read {
        /// Start address (hex, e.g. 0x1600000).
        #[arg(value_parser = parse_hex_u32)]
        addr: u32,
        /// How many bytes (a multiple of 4 reads cleanly; default 64).
        #[arg(long, default_value_t = 64)]
        len: usize,
    },
    /// Write bytes (hex, e.g. `00030000`) to an address.
    Write {
        /// Destination address (hex).
        #[arg(value_parser = parse_hex_u32)]
        addr: u32,
        /// The bytes as a hex string (even number of hex digits).
        bytes: String,
    },
    /// Read one register of an I2C slave over the host face of the Fujitsu I2C
    /// (0x00FD8FE0): blob-free, no aux core. The demod is 7-bit slave 0x10 (8-bit 0x20).
    I2c {
        /// 7-bit slave address (hex), e.g. 0x10 for the demod.
        #[arg(value_parser = parse_hex_u32)]
        slave: u32,
        /// Register number (hex).
        #[arg(value_parser = parse_hex_u32)]
        reg: u32,
    },
    /// EXPERIMENTAL (blob-free cold bring-up): run the host-side board clock/power
    /// chain, then read one demod register over the host-face I2C, with step
    /// diagnostics. Use on a freshly plugged (cold) device. The demod is 7-bit slave
    /// 0x10. See `docs/raw-ts.md`.
    ColdI2c {
        /// 7-bit slave address (hex). Default 0x10 (the terrestrial demod).
        #[arg(long, value_parser = parse_hex_u32, default_value = "0x10")]
        slave: u32,
        /// Register number to read (hex). Default 0x80 (demod status).
        #[arg(long, value_parser = parse_hex_u32, default_value = "0x80")]
        reg: u32,
        /// Leave the board registers as bricked-up instead of restoring them (so a
        /// follow-on step can keep the state). Default restores for cheap re-runs.
        #[arg(long)]
        no_restore: bool,
    },
    /// EXPERIMENTAL (blob-free cold tune): host board bring-up + host-face I2C, then run
    /// the front-end init and tune over it and report lock / C/N. Cold device. No
    /// resident, no aux core, no vendor firmware.
    ColdTune {
        /// Satellite (ISDB-S) instead of terrestrial (ISDB-T).
        #[arg(long)]
        sat: bool,
        /// Frequency in kHz: the terrestrial channel centre (e.g. 557143 for physical
        /// 27) or the satellite IF.
        #[arg(long, value_parser = parse_u32)]
        freq_khz: u32,
        /// TSID for satellite selection (hex), optional.
        #[arg(long, value_parser = parse_hex_u32)]
        tsid: Option<u32>,
        /// Leave the board registers bricked-up instead of restoring them.
        #[arg(long)]
        no_restore: bool,
    },
    /// EXPERIMENTAL (blob-free cold capture): host board bring-up + host-face tune, arm
    /// the capture engine over the host bank window, and report whether the TS ring fills.
    /// Cold device. No resident, no aux core, no vendor firmware.
    ColdCapture {
        /// Satellite (ISDB-S) instead of terrestrial (ISDB-T).
        #[arg(long)]
        sat: bool,
        /// Frequency in kHz (terrestrial channel centre, or satellite IF).
        #[arg(long, value_parser = parse_u32)]
        freq_khz: u32,
        /// TSID for satellite selection (hex), optional.
        #[arg(long, value_parser = parse_hex_u32)]
        tsid: Option<u32>,
    },
    /// EXPERIMENTAL (blob-free cold stream, end to end): host board bring-up + host-face
    /// tune + capture arm + ring->EP pump; capture TS for a few seconds and report bytes /
    /// sync. Cold device. No resident, no aux core, no vendor firmware.
    ColdStream {
        /// Satellite (ISDB-S) instead of terrestrial (ISDB-T).
        #[arg(long)]
        sat: bool,
        /// Frequency in kHz (terrestrial channel centre, or satellite IF).
        #[arg(long, value_parser = parse_u32)]
        freq_khz: u32,
        /// TSID for satellite selection (hex), optional.
        #[arg(long, value_parser = parse_hex_u32)]
        tsid: Option<u32>,
        /// Seconds to capture.
        #[arg(long, default_value_t = 3)]
        secs: u64,
        /// Write the captured TS to this file.
        #[arg(short, long, value_name = "FILE")]
        out: Option<String>,
    },
}

fn parse_u32(s: &str) -> std::result::Result<u32, std::num::ParseIntError> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => s.parse(),
    }
}

/// Parse an argument documented as hex: `0x`-prefixed or bare, always base 16 (so `10` is
/// 0x10, matching the help, not decimal ten).
fn parse_hex_u32(s: &str) -> std::result::Result<u32, std::num::ParseIntError> {
    let hex = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => h,
        None => s,
    };
    u32::from_str_radix(hex, 16)
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !s.len().is_multiple_of(2) {
        bail!("hex byte string must have an even number of digits");
    }
    if !s.is_ascii() {
        bail!("hex byte string must be ASCII hex digits");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("bad hex byte"))
        .collect()
}

pub fn run(a: &RegArgs) -> Result<()> {
    let mut dev = match &a.serial {
        Some(s) => Device::open_serial(s),
        None => Device::open(a.index),
    }
    .context("opening the PIX-DT295")?;
    dev.set_tuner(usize::from(a.tuner) - 1)
        .with_context(|| format!("tuner {} of a {}", a.tuner, dev.model().name()))?;

    match &a.command {
        RegCommand::Read { addr, len } => {
            let data = dev.reg_read(*addr, *len).context("register read")?;
            for (i, chunk) in data.chunks(16).enumerate() {
                let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
                println!("{:08x}: {}", addr + (i * 16) as u32, hex.join(" "));
            }
        }
        RegCommand::Write { addr, bytes } => {
            let data = unhex(bytes)?;
            dev.reg_write(*addr, &data).context("register write")?;
            eprintln!("wrote {} bytes at 0x{addr:08x}", data.len());
        }
        RegCommand::I2c { slave, reg } => {
            let v = dev
                .fj_demod_read(*slave as u8, *reg as u8)
                .context("host-face I2C read")?;
            println!("slave 0x{slave:02x} reg 0x{reg:02x} = 0x{v:02x}");
        }
        RegCommand::ColdI2c {
            slave,
            reg,
            no_restore,
        } => {
            let (res, report) = dev.fj_demod_read_cold(*slave as u8, *reg as u8, !*no_restore);
            for line in &report {
                println!("  {line}");
            }
            match res {
                Ok(v) => println!("=> slave 0x{slave:02x} reg 0x{reg:02x} = 0x{v:02x}"),
                Err(e) => println!("=> FAILED: {e}"),
            }
        }
        RegCommand::ColdTune {
            sat,
            freq_khz,
            tsid,
            no_restore,
        } => {
            let (res, report) = dev.fj_cold_tune(
                !*sat,
                freq_khz.saturating_mul(1000),
                tsid.map(|t| t as u16),
                !*no_restore,
            );
            for line in &report {
                println!("  {line}");
            }
            match res {
                Ok(true) => println!("=> LOCKED"),
                Ok(false) => println!("=> no lock"),
                Err(e) => println!("=> FAILED: {e}"),
            }
        }
        RegCommand::ColdCapture {
            sat,
            freq_khz,
            tsid,
        } => {
            let (res, report) =
                dev.fj_cold_capture(!*sat, freq_khz.saturating_mul(1000), tsid.map(|t| t as u16));
            for line in &report {
                println!("  {line}");
            }
            match res {
                Ok(true) => println!("=> TS FLOWING (cold, blob-free)"),
                Ok(false) => println!("=> armed but no TS / no lock"),
                Err(e) => println!("=> FAILED: {e}"),
            }
        }
        RegCommand::ColdStream {
            sat,
            freq_khz,
            tsid,
            secs,
            out,
        } => {
            let (res, report) = dev.fj_cold_stream(
                !*sat,
                freq_khz.saturating_mul(1000),
                tsid.map(|t| t as u16),
                *secs,
                out.as_deref(),
            );
            for line in &report {
                println!("  {line}");
            }
            match res {
                Ok((bytes, sync)) => println!(
                    "=> {bytes} bytes, TS sync {:.1}% {}",
                    sync * 100.0,
                    if sync > 0.9 {
                        "(cold, blob-free TS)"
                    } else {
                        "(low sync -- check framing)"
                    }
                ),
                Err(e) => println!("=> FAILED: {e}"),
            }
        }
    }
    Ok(())
}
