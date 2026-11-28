//! `dt295 list`: the connected units (PIX-DT295(W), PIX-DT195(W)), to find the serial number
//! to put on a unit's label and into `[Tuner] DeviceSerial` (plug them in one at a time).

use anyhow::{Context, Result};
use dt295::{Listed, Model};

pub fn run() -> Result<()> {
    let units = dt295::list().context("enumerate USB devices")?;
    if units.is_empty() {
        println!("no PIX-DT295(W) / PIX-DT195(W) connected");
        return Ok(());
    }
    print!("{}", render(&units));
    Ok(())
}

/// One line per unit: the index it has now (free units only, as `-i` counts them), the
/// serial, the model, whether it is free, and where it is.
fn render(units: &[Listed]) -> String {
    let mut out = String::from("index  serial        model       status  location\n");
    let mut index = 0;
    for u in units {
        let idx = if u.available {
            index += 1;
            (index - 1).to_string()
        } else {
            "-".to_owned()
        };
        out.push_str(&format!(
            "{idx:<5}  {:<12}  {:<10}  {:<6}  {}\n",
            u.serial.as_deref().unwrap_or("(none)"),
            Model::from_pid(u.pid).map_or("?", Model::name),
            if u.available { "free" } else { "busy" },
            u.location
        ));
    }
    if units.iter().any(|u| !u.available) {
        out.push_str(BUSY_NOTE);
    }
    out
}

#[cfg(windows)]
const BUSY_NOTE: &str = "busy = in use by another program (dt295 serve, for one), or not bound \
                         to WinUSB; it has no index now, so the free units' numbers shift -- \
                         use the serial\n";
#[cfg(not(windows))]
const BUSY_NOTE: &str = "busy = in use by another program (dt295 serve, for one); it has no \
                         index now, so the free units' numbers shift -- use the serial\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(serial: Option<&str>, available: bool, location: &str) -> Listed {
        Listed {
            pid: if location == "c" { 0x1040 } else { 0x103B },
            serial: serial.map(str::to_owned),
            location: location.to_owned(),
            available,
        }
    }

    #[test]
    fn busy_units_get_no_index() {
        let s = render(&[
            unit(Some("CD23456789"), false, "a"),
            unit(Some("EF34567890"), true, "b"),
            unit(None, true, "c"),
        ]);
        let lines: Vec<_> = s.lines().collect();
        assert_eq!(
            lines[0],
            "index  serial        model       status  location"
        );
        assert_eq!(lines[1], "-      CD23456789    PIX-DT295   busy    a");
        assert_eq!(lines[2], "0      EF34567890    PIX-DT295   free    b");
        assert_eq!(lines[3], "1      (none)        PIX-DT195W  free    c");
        assert!(lines[4].starts_with("busy = "));
    }

    #[test]
    fn all_free_has_no_note() {
        let s = render(&[unit(Some("CD23456789"), true, "a")]);
        assert_eq!(s.lines().count(), 2);
    }
}
