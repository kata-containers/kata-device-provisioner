// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Which vfio driver claims a device, read from the kernel's own alias table,
//! and whether the device can be claimed now.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::{cc, vfio, Sysfs};

use crate::device::{DeviceState, Provisioning};
use crate::modules;

/// A generic mapping must not leave a known coherent GPU's memory inaccessible.
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
        let module = vfio::module_for(&alias, state.vendor, state.device_id)
            .with_context(|| format!("{}: find its vfio module", state.address))?;

        verify_variant_available(state, &module)?;

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

    #[rstest]
    #[case::pcie_gpu(H100)]
    #[case::coherent_gpu(GB200)]
    #[case::rubin_gpu(RUBIN_C2C)]
    #[case::switch(NVSWITCH)]
    fn missing_vfio_mapping_stops_before_module_loading(sysfs: Fake, #[case] device: (u16, u32)) {
        let states = node(&sysfs, &[device]);
        let proc_root = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        fs::create_dir_all(proc_root.path().join("sys/kernel")).unwrap();
        fs::write(
            proc_root.path().join("sys/kernel/osrelease"),
            "test-kernel\n",
        )
        .unwrap();
        let modules = host.path().join("lib/modules/test-kernel");
        fs::create_dir_all(&modules).unwrap();
        fs::write(
            modules.join("modules.alias"),
            "alias vfio_pci:v*d*sv*sd*bc*sc*i* vfio_pci\n",
        )
        .unwrap();

        let err = resolve(&sysfs.sysfs, host.path(), proc_root.path(), &states)
            .err()
            .expect("an unmapped device must fail before looking for modprobe");

        let message = format!("{err:#}");
        assert!(message.contains(&states[0].address), "{message}");
        assert!(
            message.contains("no device-specific VFIO module alias"),
            "{message}"
        );
        assert_eq!(
            err.root_cause()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::NotFound
        );
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
