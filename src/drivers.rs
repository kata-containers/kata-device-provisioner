// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Which vfio driver claims a device, read from the kernel's own alias table,
//! and whether the device can be claimed now.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::{cc, vfio, Sysfs, DRIVER_VFIO_PCI_TYPE};

use crate::device::{DeviceState, Provisioning};
use crate::modules;

/// Generic `vfio-pci` is right for every device no variant driver claims: an
/// SXM or PCIe GPU, an NVSwitch, a ConnectX PF. It is wrong for a coherently
/// attached GPU, which would reach a VM with its memory missing, so one of
/// those is refused rather than bound when the kernel offers nothing better.
fn verify_variant_available(state: &DeviceState, module: &str) -> Result<()> {
    if state.provisioning() != Some(Provisioning::NvidiaInBandCc)
        || !cc::is_c2c(state.device_id)
        || module.replace('-', "_") != "vfio_pci"
    {
        return Ok(());
    }

    bail!(
        "{} ({}) is coherently attached over NVLink-C2C and needs a vfio driver that can map \
         its memory, but this kernel's module alias table offers only {module}, which cannot. \
         The node needs a kernel whose nvgrace-gpu vfio driver claims this device",
        state.address,
        state.device_name
    )
}

/// Only a variant driver has a device-specific alias; `vfio_pci` registers a
/// catch-all, so the table's silence on a device means generic, not
/// unsupported. A table that cannot be read is still an error.
fn module_for(alias: &Path, state: &DeviceState) -> Result<String> {
    let module = match vfio::module_for(alias, state.vendor, state.device_id) {
        Ok(module) => module,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && alias.is_file() => {
            DRIVER_VFIO_PCI_TYPE.to_string()
        }
        Err(err) => {
            return Err(err).with_context(|| format!("{}: find its vfio module", state.address))
        }
    };

    verify_variant_available(state, &module)?;
    Ok(module)
}

/// modprobe takes the module, `driver_override` the driver, and they differ.
pub(crate) struct VfioDriver {
    pub module: String,
    pub driver: String,
}

/// Per device: a Grace GPU needs `nvgrace_gpu_vfio_pci` where the NIC beside
/// it takes plain `vfio-pci`.
pub(crate) fn resolve(
    sysfs: &Sysfs,
    host_root: &Path,
    proc: &Path,
    states: &[DeviceState],
) -> Result<HashMap<String, VfioDriver>> {
    let alias = modules::modules_alias(host_root, proc)?;
    let mut drivers = HashMap::new();

    for state in states {
        let module = module_for(&alias, state)?;

        modules::modprobe(host_root, &module)
            .with_context(|| format!("{}: load {module}", state.address))?;

        let driver = vfio::driver_for(sysfs, &module)
            .with_context(|| format!("{}: find the driver {module} registers", state.address))?;

        drivers.insert(state.address.clone(), VfioDriver { module, driver });
    }

    Ok(drivers)
}

/// Reject host-owned devices before a GPU mode change can leave a partial run.
pub(crate) fn verify_bindable(
    sysfs: &Sysfs,
    states: &[DeviceState],
    drivers: &HashMap<String, VfioDriver>,
) -> Result<()> {
    for state in states {
        if let Some(current) = vfio::current_driver(sysfs, &state.address) {
            if current != drivers[&state.address].driver {
                bail!("{}: held by {current}; stop its host services and unbind it before provisioning", state.address);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::device::in_scope;
    use crate::device::tests::{
        managed_fabric_node, node, GB200, GH200, H100, NVSWITCH, RUBIN_C2C,
    };
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};
    use std::fs;

    #[fixture]
    fn sysfs() -> Fake {
        testfs::fake()
    }

    pub(crate) fn vfio_pci() -> VfioDriver {
        VfioDriver {
            module: "vfio_pci".to_string(),
            driver: "vfio-pci".to_string(),
        }
    }

    /// `vfio_pci` registers only a catch-all alias, so a table that is silent
    /// on a device means generic, which is right for everything but a
    /// coherent GPU. An NVIDIA 7.0 kernel's table, for one, names nvgrace for
    /// GH200, GB200 and GB300 and nothing for H100, B200 or an NVSwitch.
    #[rstest]
    #[case::pcie_gpu(H100, "", Some("vfio-pci"))]
    #[case::nvswitch(NVSWITCH, "", Some("vfio-pci"))]
    #[case::grace_hopper_with_its_variant(
        GH200,
        "alias vfio_pci:v000010DEd00002342sv*sd*bc*sc*i* nvgrace_gpu_vfio_pci\n",
        Some("nvgrace_gpu_vfio_pci")
    )]
    #[case::grace_blackwell_without_a_variant(GB200, "", None)]
    #[case::coherent_rubin_without_a_variant(RUBIN_C2C, "", None)]
    fn an_unmapped_device_takes_vfio_pci_unless_it_is_coherent(
        sysfs: Fake,
        #[case] device: (u16, u32),
        #[case] variant: &str,
        #[case] expected: Option<&str>,
    ) {
        let states = node(&sysfs, &[device]);
        let alias = sysfs.root().join("modules.alias");
        fs::write(
            &alias,
            format!("{variant}alias vfio_pci:v*d*sv*sd*bc*sc*i* vfio_pci\n"),
        )
        .unwrap();

        let module = module_for(&alias, &states[0]);

        assert_eq!(module.ok().as_deref(), expected);
    }

    /// Upstream's mlx5 variant claims only the SR-IOV VF, so a management PF
    /// is bound generically like any other NIC.
    #[rstest]
    fn a_management_pf_takes_vfio_pci_when_no_variant_claims_it(managed_fabric_node: Fake) {
        let states = in_scope(&managed_fabric_node.sysfs).unwrap().devices;
        let alias = managed_fabric_node.root().join("modules.alias");
        fs::write(&alias, "alias vfio_pci:v*d*sv*sd*bc*sc*i* vfio_pci\n").unwrap();

        assert_eq!(module_for(&alias, &states[0]).unwrap(), "vfio-pci");
    }

    #[rstest]
    fn a_missing_alias_table_is_an_error_not_a_fallback(sysfs: Fake) {
        let states = node(&sysfs, &[H100]);

        assert!(module_for(&sysfs.root().join("modules.alias"), &states[0]).is_err());
    }

    /// The kernel names the variant module with underscores and sysfs names
    /// the driver with dashes, and either can reach this.
    #[rstest]
    #[case::grace_hopper(GH200, "nvgrace_gpu_vfio_pci", true)]
    #[case::grace_blackwell(GB200, "nvgrace-gpu-vfio-pci", true)]
    #[case::rubin_c2c(RUBIN_C2C, "nvgrace_gpu_vfio_pci", true)]
    #[case::rubin_missing_variant(RUBIN_C2C, "vfio_pci", false)]
    #[case::pcie_gpu_needs_no_variant(H100, "vfio-pci", true)]
    #[case::nvswitch_needs_no_variant(NVSWITCH, "vfio_pci", true)]
    #[case::grace_on_a_kernel_too_old(GH200, "vfio-pci", false)]
    #[case::grace_on_a_kernel_too_old_underscored(GB200, "vfio_pci", false)]
    fn refuses_a_grace_gpu_the_kernel_has_no_driver_for(
        sysfs: Fake,
        #[case] device: (u16, u32),
        #[case] module: &str,
        #[case] expected: bool,
    ) {
        let states = node(&sysfs, &[device]);

        assert_eq!(
            verify_variant_available(&states[0], module).is_ok(),
            expected
        );
    }

    #[rstest]
    #[case::host_owned(Some("mlx5_core"), false)]
    #[case::unbound(None, true)]
    #[case::already_bound(Some("vfio-pci"), true)]
    fn binding_preflight_refuses_host_owned_management_pfs(
        managed_fabric_node: Fake,
        #[case] driver: Option<&str>,
        #[case] accepted: bool,
    ) {
        let fake = managed_fabric_node;
        fake.add_device("0000:05:00.0", driver);
        let states = in_scope(&fake.sysfs).unwrap().devices;
        let drivers = states
            .iter()
            .map(|s| (s.address.clone(), vfio_pci()))
            .collect();
        assert_eq!(
            verify_bindable(&fake.sysfs, &states, &drivers).is_ok(),
            accepted
        );
        assert_eq!(fake.driver_override("0000:05:00.0").trim(), "");
    }
}
