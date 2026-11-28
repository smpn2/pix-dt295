//! The models the driver runs, told apart by their USB product id.
//!
//! They have the same USB interface, bridge (MB86M01), firmware and front-end chips. The
//! one-tuner models (PIX-DT295, PIX-DT195) have one front end; the two-tuner ones
//! (PIX-DT295W, PIX-DT195W) have two, on the same board. The PIX-DT195(W) are
//! the Mac versions: the vendor's Windows driver runs the PIX-DT195 as a PIX-DT295.

use dt295_usb::{PID_DT195, PID_DT195W, PID_DT295, PID_DT295W};

/// A model the driver runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// PIX-DT295: one front end.
    Dt295,
    /// PIX-DT295W: two front ends.
    Dt295w,
    /// PIX-DT195 (for Mac): one front end, as the PIX-DT295.
    Dt195,
    /// PIX-DT195W (for Mac): two front ends, as the PIX-DT295W.
    Dt195w,
}

impl Model {
    /// The model of a USB product id; `None` for one the driver does not run.
    pub fn from_pid(pid: u16) -> Option<Self> {
        match pid {
            PID_DT295 => Some(Self::Dt295),
            PID_DT295W => Some(Self::Dt295w),
            PID_DT195 => Some(Self::Dt195),
            PID_DT195W => Some(Self::Dt195w),
            _ => None,
        }
    }

    /// The product name, as the USB product string has it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dt295 => "PIX-DT295",
            Self::Dt295w => "PIX-DT295W",
            Self::Dt195 => "PIX-DT195",
            Self::Dt195w => "PIX-DT195W",
        }
    }

    /// How many front ends (tuners) it has.
    pub fn tuners(self) -> usize {
        match self {
            Self::Dt295 | Self::Dt195 => 1,
            Self::Dt295w | Self::Dt195w => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pid_opened_is_a_model() {
        for pid in dt295_usb::PIDS {
            assert!(Model::from_pid(pid).is_some(), "0x{pid:04x}");
        }
        assert_eq!(Model::from_pid(0x1046), None);
        assert_eq!(Model::from_pid(0x103B).map(Model::tuners), Some(1));
        assert_eq!(Model::from_pid(0x103A).map(Model::tuners), Some(1));
        assert_eq!(Model::from_pid(0x103C).map(Model::tuners), Some(2));
        assert_eq!(Model::from_pid(0x1040).map(Model::tuners), Some(2));
    }
}
