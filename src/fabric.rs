// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! kata-device-plugin advertises `nvidia.com/nvswitch-fabric` only once every
//! function pcilibs-rs names as the fabric is bound, so a fabric bound in part
//! is a node without the resource.

use std::collections::BTreeSet;

use anyhow::{bail, Result};

use crate::device::{DeviceState, Node};
use crate::mode::Mode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    Host,
    /// The fabric manager runs in a ServiceVM and needs the switches whether
    /// the workload GPUs run CC or not.
    ServiceVm,
    /// PPCIE gives one confidential VM the whole baseboard, switches included.
    PpcieVm,
}

impl Destination {
    pub(crate) fn of(mode: Mode, bind_fabric: bool) -> Self {
        match (mode, bind_fabric) {
            (Mode::Ppcie, _) => Self::PpcieVm,
            (_, true) => Self::ServiceVm,
            (_, false) => Self::Host,
        }
    }

    /// Whether the run binds `state` to vfio. A GPU always is; a fabric
    /// function follows the fabric.
    pub(crate) fn takes(self, state: &DeviceState) -> bool {
        self != Self::Host || !state.is_fabric()
    }
}

/// The scan can name a function no `DEVICES` row admits, a management PF that
/// enumerates as Ethernet for one. Binding the rest would leave the device
/// plugin advertising no fabric, so the run is refused before any GPU mode
/// changes.
pub(crate) fn verify(node: &Node, fabric: Destination) -> Result<()> {
    if fabric == Destination::Host {
        return Ok(());
    }
    let bound: BTreeSet<&str> = node
        .devices
        .iter()
        .filter(|state| state.is_fabric())
        .map(|state| state.address.as_str())
        .collect();
    if fabric == Destination::ServiceVm && bound.is_empty() {
        bail!("fabric binding requested but no NVSwitch or qualified management PF was found");
    }
    let missing: Vec<&str> = node
        .fabric
        .iter()
        .map(String::as_str)
        .filter(|bdf| !bound.contains(bdf))
        .collect();
    if !bound.is_empty() && !missing.is_empty() {
        bail!(
            "the NVSwitch fabric is passed through whole or not at all, and {} of its {} \
             functions would stay on the host: {}",
            missing.len(),
            node.fabric.len(),
            missing.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::in_scope;
    use crate::device::tests::{managed_fabric_node, node, B200, H100, NVSWITCH};
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};

    #[fixture]
    fn sysfs() -> Fake {
        testfs::fake()
    }

    fn taken(states: &[DeviceState], fabric: Destination) -> usize {
        states.iter().filter(|state| fabric.takes(state)).count()
    }

    #[rstest]
    #[case::off_leaves_the_switches_on_the_host(Mode::Off, 8)]
    #[case::on_leaves_the_switches_on_the_host(Mode::On, 8)]
    #[case::devtools_leaves_the_switches_on_the_host(Mode::DevTools, 8)]
    #[case::ppcie_claims_the_whole_baseboard(Mode::Ppcie, 12)]
    fn passes_the_switches_through_only_under_ppcie(
        sysfs: Fake,
        #[case] mode: Mode,
        #[case] expected: usize,
    ) {
        let board = [H100; 8]
            .into_iter()
            .chain([NVSWITCH; 4])
            .collect::<Vec<_>>();
        let states = node(&sysfs, &board);

        assert_eq!(states.len(), 12);
        assert_eq!(taken(&states, Destination::of(mode, false)), expected);
    }

    #[rstest]
    #[case::off(Mode::Off)]
    #[case::on(Mode::On)]
    #[case::devtools(Mode::DevTools)]
    fn service_vm_binding_is_independent_of_cc_mode(sysfs: Fake, #[case] mode: Mode) {
        let _ = node(&sysfs, &[H100, NVSWITCH]);
        let node = in_scope(&sysfs.sysfs).unwrap();
        let fabric = Destination::of(mode, true);
        verify(&node, fabric).unwrap();
        assert_eq!(taken(&node.devices, fabric), 2);
    }

    #[rstest]
    fn a_candidate_label_cannot_authorize_an_ordinary_nic(sysfs: Fake) {
        let _ = node(&sysfs, &[B200]);
        sysfs.add_pci_device("0000:06:00.0", 0x15b3, 0x1021, 0x020700, None);
        let node = in_scope(&sysfs.sysfs).unwrap();
        assert!(verify(&node, Destination::ServiceVm).is_err());
        assert!(verify(&node, Destination::Host).is_ok());
    }

    #[rstest]
    fn a_fabric_that_would_be_bound_in_part_is_refused(managed_fabric_node: Fake) {
        let fake = managed_fabric_node;
        fake.add_pci_device("0000:05:00.3", 0x15b3, 0x1021, 0x020000, None);
        let node = in_scope(&fake.sysfs).unwrap();
        assert_eq!(node.fabric.len(), 3);

        let err = verify(&node, Destination::ServiceVm)
            .unwrap_err()
            .to_string();

        assert!(err.contains("0000:05:00.3"), "{err}");
        assert!(err.contains("1 of its 3"), "{err}");
    }

    /// An incomplete fabric matters only once part of it is passed through.
    #[rstest]
    fn a_fabric_nobody_asked_for_stays_on_the_host_whole(sysfs: Fake) {
        let _ = node(&sysfs, &[H100, NVSWITCH, NVSWITCH]);
        let node = in_scope(&sysfs.sysfs).unwrap();
        assert_eq!(node.fabric.len(), 2);

        verify(&node, Destination::Host).unwrap();
        assert_eq!(taken(&node.devices, Destination::Host), 1);
    }
}
