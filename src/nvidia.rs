// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! NVIDIA in-band CC and PPCIE modes, over BAR0 and the FSP mailbox
//! (`pcilibs_rs::cc`). No NVIDIA kernel driver is involved, so an idle
//! vfio-bound device changes mode without being unbound.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use pcilibs_rs::cc::{self, CcMode, Gpu, NvSwitch, PpcieMode};

use crate::device::{DeviceState, Provisioning};
use crate::mode::Mode;

/// `None` for NVIDIA GPUs predating Hopper.
pub fn cc_chip(device_id: u16) -> Option<&'static str> {
    cc::chip_for(device_id).map(|chip| chip.name)
}

pub fn supports_ppcie(device_id: u16) -> bool {
    cc::chip_for(device_id).is_some_and(|chip| chip.hopper)
}

/// Kept out of discovery: maps BAR0, needs root, wakes suspended devices.
pub fn probe_modes(states: &mut [DeviceState]) -> HashMap<String, String> {
    let mut failures = HashMap::new();

    for state in states.iter_mut() {
        let probe = || -> Result<(Option<CcMode>, Option<PpcieMode>)> {
            match state.provisioning() {
                Some(Provisioning::NvidiaInBandCc) if state.cc_capable() => {
                    let gpu = Gpu::open(&state.address)?;
                    let ppcie = gpu.supports_ppcie().then(|| gpu.query_ppcie_mode());
                    Ok((Some(gpu.query_cc_mode()?), ppcie.transpose()?))
                }
                Some(Provisioning::NvidiaInBandPpcie) => {
                    let switch = NvSwitch::open(&state.address)?;
                    Ok((None, Some(switch.query_ppcie_mode()?)))
                }
                _ => Ok((None, None)),
            }
        };

        match probe() {
            Ok((cc, ppcie)) => {
                state.cc_mode = cc;
                state.ppcie_mode = ppcie;
            }
            Err(err) => {
                failures.insert(state.address.clone(), format!("{err:#}"));
            }
        }
    }

    failures
}

/// One knob write. CC and PPCIE clear each other, so which run and in which
/// order is not a free choice — see [`writes_for`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    Cc(CcMode),
    Ppcie(PpcieMode),
}

/// The writes that move a device from what it is running to `mode`. `None`
/// means no such mode: an NVSwitch has no CC, a Blackwell GPU no PPCIE.
///
/// Enabling either clears the other in firmware, so doing both would undo
/// half of the first. Only turning everything off takes two writes, that
/// being the one transition which leaves the other knob alone.
pub fn writes_for(cc: Option<CcMode>, ppcie: Option<PpcieMode>, mode: Mode) -> Vec<Write> {
    let target_ppcie = mode.ppcie_target();

    let (Some(cc), Some(ppcie)) = (cc, ppcie) else {
        let mut writes = Vec::new();
        if let Some(cc) = cc {
            if cc != mode.cc_target() {
                writes.push(Write::Cc(mode.cc_target()));
            }
        }
        if let Some(ppcie) = ppcie {
            if ppcie != target_ppcie {
                writes.push(Write::Ppcie(target_ppcie));
            }
        }
        return writes;
    };

    let target_cc = mode.cc_target();
    match target_ppcie {
        PpcieMode::On if ppcie == PpcieMode::On && cc == CcMode::Off => Vec::new(),
        PpcieMode::On => vec![Write::Ppcie(PpcieMode::On)],
        PpcieMode::Off if target_cc != CcMode::Off => {
            match cc == target_cc && ppcie == PpcieMode::Off {
                true => Vec::new(),
                false => vec![Write::Cc(target_cc)],
            }
        }
        PpcieMode::Off => {
            let mut writes = Vec::new();
            if ppcie == PpcieMode::On {
                writes.push(Write::Ppcie(PpcieMode::Off));
            }
            if cc != CcMode::Off {
                writes.push(Write::Cc(CcMode::Off));
            }
            writes
        }
    }
}

/// An open device. All are opened before any is written, so a node that
/// cannot be provisioned fails with its knobs untouched.
enum Handle {
    Gpu(Box<Gpu>),
    Switch(Box<NvSwitch>),
}

impl Handle {
    fn open(state: &DeviceState) -> Result<Option<Self>> {
        let handle = match state.provisioning() {
            Some(Provisioning::NvidiaInBandCc) if state.cc_capable() => {
                Self::Gpu(Box::new(Gpu::open(&state.address)?))
            }
            Some(Provisioning::NvidiaInBandPpcie) => {
                Self::Switch(Box::new(NvSwitch::open(&state.address)?))
            }
            _ => return Ok(None),
        };
        Ok(Some(handle))
    }

    fn current(&self) -> Result<(Option<CcMode>, Option<PpcieMode>)> {
        match self {
            Self::Gpu(gpu) => {
                let ppcie = gpu.supports_ppcie().then(|| gpu.query_ppcie_mode());
                Ok((Some(gpu.query_cc_mode()?), ppcie.transpose()?))
            }
            Self::Switch(switch) => Ok((None, Some(switch.query_ppcie_mode()?))),
        }
    }

    fn write(&self, write: Write) -> Result<()> {
        match (self, write) {
            (Self::Gpu(gpu), Write::Cc(mode)) => gpu.set_cc_mode(mode),
            (Self::Gpu(gpu), Write::Ppcie(mode)) => gpu.set_ppcie_mode(mode),
            (Self::Switch(switch), Write::Ppcie(mode)) => switch.set_ppcie_mode(mode),
            (Self::Switch(_), Write::Cc(mode)) => {
                bail!("an NVSwitch has no CC mode to set to {mode}")
            }
        }
    }

    fn reset(&self) -> Result<()> {
        match self {
            Self::Gpu(gpu) => gpu.reset(),
            Self::Switch(switch) => switch.reset(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    /// Not reset: doing so to a device that needs nothing is a free outage.
    AlreadySet,
    Applied,
}

/// PPCIE is a property of a whole baseboard: no device reports it active until
/// all of them carry it. So the knobs go to every device first and the resets
/// come after, which also means nothing is reset for a mode the rest of the
/// board is not getting.
pub fn apply(states: &[DeviceState], mode: Mode) -> Result<Vec<(String, Transition)>> {
    let mut devices = Vec::new();
    for state in states {
        if let Some(handle) = Handle::open(state)
            .with_context(|| format!("{}: open for mode change", state.address))?
        {
            devices.push((state, handle));
        }
    }

    let mut pending = Vec::new();
    let mut outcomes = Vec::new();
    for (state, handle) in &devices {
        let (cc, ppcie) = handle
            .current()
            .with_context(|| format!("{}: read current mode", state.address))?;
        let writes = writes_for(cc, ppcie, mode);
        outcomes.push((
            state.address.clone(),
            match writes.is_empty() {
                true => Transition::AlreadySet,
                false => Transition::Applied,
            },
        ));
        if !writes.is_empty() {
            pending.push((state, handle, writes));
        }
    }

    for (state, handle, writes) in &pending {
        for write in writes {
            handle
                .write(*write)
                .with_context(|| format!("{}: set {mode}", state.address))?;
        }
    }

    // Without this the device reports the new mode and behaves like the old.
    for (state, handle, _) in &pending {
        handle
            .reset()
            .with_context(|| format!("{}: reset to activate {mode}", state.address))?;
    }

    for (state, handle) in &devices {
        verify(handle, state, mode)?;
    }

    Ok(outcomes)
}

/// Re-read the hardware rather than trusting that the writes took.
pub fn verify_mode(state: &DeviceState, mode: Mode) -> Result<()> {
    let Some(handle) =
        Handle::open(state).with_context(|| format!("{}: open to verify", state.address))?
    else {
        return Ok(());
    };
    verify(&handle, state, mode)
}

fn verify(handle: &Handle, state: &DeviceState, mode: Mode) -> Result<()> {
    let (cc, ppcie) = handle
        .current()
        .with_context(|| format!("{}: verify mode", state.address))?;

    if let Some(cc) = cc {
        if cc != mode.cc_target() {
            bail!(
                "{}: CC mode is {cc}, expected {} for {mode}",
                state.address,
                mode.cc_target()
            );
        }
    }

    if let Some(ppcie) = ppcie {
        if ppcie != mode.ppcie_target() {
            bail!(
                "{}: PPCIE mode is {ppcie}, expected {} for {mode}",
                state.address,
                mode.ppcie_target()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const BLACKWELL: Option<PpcieMode> = None;

    #[rstest]
    #[case::already_off(CcMode::Off, Mode::Off, &[])]
    #[case::already_on(CcMode::On, Mode::On, &[])]
    #[case::enable(CcMode::Off, Mode::On, &[Write::Cc(CcMode::On)])]
    #[case::disable(CcMode::On, Mode::Off, &[Write::Cc(CcMode::Off)])]
    #[case::devtools(CcMode::On, Mode::DevTools, &[Write::Cc(CcMode::DevTools)])]
    // Refused before this point; CC off is what PPCIE means without PPCIE.
    #[case::ppcie(CcMode::On, Mode::Ppcie, &[Write::Cc(CcMode::Off)])]
    fn a_gpu_without_ppcie_only_ever_gets_cc_writes(
        #[case] cc: CcMode,
        #[case] mode: Mode,
        #[case] expected: &[Write],
    ) {
        assert_eq!(writes_for(Some(cc), BLACKWELL, mode), expected);
    }

    /// `set_ppcie_mode(on)` clears the CC knobs itself.
    #[rstest]
    #[case::from_off(CcMode::Off, PpcieMode::Off)]
    #[case::from_cc(CcMode::On, PpcieMode::Off)]
    #[case::from_devtools(CcMode::DevTools, PpcieMode::Off)]
    fn enabling_ppcie_is_a_single_write(#[case] cc: CcMode, #[case] ppcie: PpcieMode) {
        assert_eq!(
            writes_for(Some(cc), Some(ppcie), Mode::Ppcie),
            [Write::Ppcie(PpcieMode::On)]
        );
    }

    /// Likewise, enabling CC clears the PPCIE knob.
    #[rstest]
    #[case::on(Mode::On, CcMode::On)]
    #[case::devtools(Mode::DevTools, CcMode::DevTools)]
    fn enabling_cc_out_of_ppcie_is_a_single_write(#[case] mode: Mode, #[case] target: CcMode) {
        assert_eq!(
            writes_for(Some(CcMode::Off), Some(PpcieMode::On), mode),
            [Write::Cc(target)]
        );
    }

    /// The one transition needing two: disabling CC leaves PPCIE alone.
    #[rstest]
    fn turning_everything_off_clears_ppcie_first() {
        assert_eq!(
            writes_for(Some(CcMode::On), Some(PpcieMode::On), Mode::Off),
            [Write::Ppcie(PpcieMode::Off), Write::Cc(CcMode::Off)]
        );
        assert_eq!(
            writes_for(Some(CcMode::Off), Some(PpcieMode::On), Mode::Off),
            [Write::Ppcie(PpcieMode::Off)]
        );
    }

    #[rstest]
    #[case::ppcie_board(CcMode::Off, PpcieMode::On, Mode::Ppcie)]
    #[case::cc_board(CcMode::On, PpcieMode::Off, Mode::On)]
    #[case::off_board(CcMode::Off, PpcieMode::Off, Mode::Off)]
    fn a_device_already_in_the_mode_is_left_alone(
        #[case] cc: CcMode,
        #[case] ppcie: PpcieMode,
        #[case] mode: Mode,
    ) {
        assert!(writes_for(Some(cc), Some(ppcie), mode).is_empty());
    }

    /// A switch has no CC write to clear its PPCIE knob implicitly.
    #[rstest]
    #[case::off(Mode::Off)]
    #[case::on(Mode::On)]
    #[case::devtools(Mode::DevTools)]
    fn a_switch_leaving_ppcie_gets_one_ppcie_write(#[case] mode: Mode) {
        assert_eq!(
            writes_for(None, Some(PpcieMode::On), mode),
            [Write::Ppcie(PpcieMode::Off)]
        );
    }

    #[rstest]
    fn a_switch_already_in_the_boards_mode_is_left_alone() {
        assert!(writes_for(None, Some(PpcieMode::On), Mode::Ppcie).is_empty());
        assert!(writes_for(None, Some(PpcieMode::Off), Mode::Off).is_empty());
    }
}
