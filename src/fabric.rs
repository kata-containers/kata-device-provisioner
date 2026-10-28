// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! The NVSwitch fabric a Service VM is given, whole or not at all.

use std::collections::BTreeSet;

use anyhow::{bail, Result};

use crate::device::{DeviceState, Node};
use crate::mode::Mode;

/// A ServiceVM needs the fabric even when workload GPUs use per-GPU CC.
pub(crate) fn passthrough_set(
    states: &[DeviceState],
    mode: Mode,
    bind_fabric: bool,
) -> Vec<DeviceState> {
    states
        .iter()
        .filter(|state| bind_fabric || mode == Mode::Ppcie || !state.is_fabric())
        .cloned()
        .collect()
}

/// The fabric is passed through whole or not at all.  The device plugin
/// advertises nvidia.com/nvswitch-fabric only once every function pcilibs-rs
/// names is bound, so binding part of it would leave the node without the
/// resource and the Service VM without a fabric it can bring up.  Refused
/// before any GPU is touched.
pub(crate) fn verify(node: &Node, passthrough: &[DeviceState], bind_fabric: bool) -> Result<()> {
    let bound: BTreeSet<&str> = passthrough
        .iter()
        .filter(|state| state.is_fabric())
        .map(|state| state.address.as_str())
        .collect();
    if bind_fabric && bound.is_empty() {
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

        let passthrough = passthrough_set(&states, mode, false);

        assert_eq!(states.len(), 12);
        assert_eq!(passthrough.len(), expected);
    }

    #[rstest]
    #[case::off(Mode::Off)]
    #[case::on(Mode::On)]
    #[case::devtools(Mode::DevTools)]
    fn service_vm_binding_is_independent_of_cc_mode(sysfs: Fake, #[case] mode: Mode) {
        let _ = node(&sysfs, &[H100, NVSWITCH]);
        let node = in_scope(&sysfs.sysfs).unwrap();
        let passthrough = passthrough_set(&node.devices, mode, true);
        verify(&node, &passthrough, true).unwrap();
        assert_eq!(passthrough.len(), 2);
    }

    #[rstest]
    fn a_candidate_label_cannot_authorize_an_ordinary_nic(sysfs: Fake) {
        let _ = node(&sysfs, &[B200]);
        sysfs.add_pci_device("0000:06:00.0", 0x15b3, 0x1021, 0x020700, None);
        let node = in_scope(&sysfs.sysfs).unwrap();
        let passthrough = passthrough_set(&node.devices, Mode::Off, true);
        assert!(verify(&node, &passthrough, true).is_err());
        assert!(verify(&node, &passthrough, false).is_ok());
    }

    /// A management PF the scan names but no row admits, here one that
    /// came up as an Ethernet function, would stay on the host; the device
    /// plugin would then advertise no fabric at all.
    #[rstest]
    fn a_fabric_that_would_be_bound_in_part_is_refused(managed_fabric_node: Fake) {
        let fake = managed_fabric_node;
        fake.add_pci_device("0000:05:00.3", 0x15b3, 0x1021, 0x020000, None);
        let node = in_scope(&fake.sysfs).unwrap();
        assert_eq!(node.fabric.len(), 3);

        let passthrough = passthrough_set(&node.devices, Mode::Off, true);
        let err = verify(&node, &passthrough, true).unwrap_err().to_string();

        assert!(err.contains("0000:05:00.3"), "{err}");
        assert!(err.contains("1 of its 3"), "{err}");
    }

    /// Left on the host whole when nothing asks for it: an incomplete
    /// fabric is only a problem once part of it is being passed through.
    #[rstest]
    fn a_fabric_nobody_asked_for_stays_on_the_host_whole(sysfs: Fake) {
        let _ = node(&sysfs, &[H100, NVSWITCH, NVSWITCH]);
        let node = in_scope(&sysfs.sysfs).unwrap();
        assert_eq!(node.fabric.len(), 2);

        let passthrough = passthrough_set(&node.devices, Mode::On, false);
        verify(&node, &passthrough, false).unwrap();
        assert_eq!(passthrough.len(), 1);
    }
}
