// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Error};
use pcilibs_rs::cc::{CcMode, PpcieMode};

/// Not `cc::CcMode`: that is per-GPU and has no PPCIE variant, while PPCIE is
/// node-wide and covers the NVSwitches. All four are mutually exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    On,
    DevTools,
    /// Protected PCIe: Hopper multi-GPU CC, switches claimed by a single CVM.
    Ppcie,
}

impl Mode {
    /// Per-GPU CC is off under PPCIE: the board is the trust boundary there.
    pub fn cc_target(self) -> CcMode {
        match self {
            Mode::Off | Mode::Ppcie => CcMode::Off,
            Mode::On => CcMode::On,
            Mode::DevTools => CcMode::DevTools,
        }
    }

    pub fn ppcie_target(self) -> PpcieMode {
        match self {
            Mode::Ppcie => PpcieMode::On,
            Mode::Off | Mode::On | Mode::DevTools => PpcieMode::Off,
        }
    }

    pub fn cc_ready(self) -> bool {
        self != Mode::Off
    }
}

impl FromStr for Mode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Mode::Off),
            "on" => Ok(Mode::On),
            "devtools" => Ok(Mode::DevTools),
            "ppcie" => Ok(Mode::Ppcie),
            other => bail!("unknown mode {other:?}: expected off, on, devtools or ppcie"),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Mode::Off => "off",
            Mode::On => "on",
            Mode::DevTools => "devtools",
            Mode::Ppcie => "ppcie",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::off("off", Mode::Off)]
    #[case::on("on", Mode::On)]
    #[case::devtools("devtools", Mode::DevTools)]
    #[case::ppcie("ppcie", Mode::Ppcie)]
    fn parses_and_renders_every_mode(#[case] text: &str, #[case] mode: Mode) {
        assert_eq!(text.parse::<Mode>().unwrap(), mode);
        assert_eq!(mode.to_string(), text);
    }

    #[rstest]
    #[case::empty("")]
    #[case::gpu_operator_spelling("enabled")]
    #[case::wrong_case("On")]
    fn rejects_anything_else(#[case] text: &str) {
        assert!(text.parse::<Mode>().is_err());
    }

    #[rstest]
    #[case::off(Mode::Off, CcMode::Off, PpcieMode::Off, false)]
    #[case::on(Mode::On, CcMode::On, PpcieMode::Off, true)]
    #[case::devtools(Mode::DevTools, CcMode::DevTools, PpcieMode::Off, true)]
    #[case::ppcie_turns_per_gpu_cc_off(Mode::Ppcie, CcMode::Off, PpcieMode::On, true)]
    fn maps_to_hardware_targets(
        #[case] mode: Mode,
        #[case] cc: CcMode,
        #[case] ppcie: PpcieMode,
        #[case] ready: bool,
    ) {
        assert_eq!(mode.cc_target(), cc);
        assert_eq!(mode.ppcie_target(), ppcie);
        assert_eq!(mode.cc_ready(), ready);
    }
}
