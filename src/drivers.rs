// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Which vfio driver claims a device, read from the kernel's own alias table,
//! and whether the device can be claimed now.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::{cc, vfio, Sysfs};

use crate::device::{DeviceState, Provisioning};
use crate::modules;

/// Whether `driver` is a vfio driver: `vfio-pci` or a variant named after it,
/// `nvgrace_gpu_vfio_pci`, `mlx5_vfio_pci`, so a substring, with the dash and
/// underscore spellings sysfs and modprobe use folded.
pub(crate) fn is_vfio(driver: &str) -> bool {
    driver.replace('-', "_").contains("vfio")
}

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
/// catch-all, so the table's silence on a device means generic for a device
/// that may take generic and an error for one that may not. NotFound is also
/// what a missing table reads as, so the table is read again before the
/// fallback and an unreadable one stays an error.
fn module_for(alias: &Path, state: &DeviceState) -> Result<String> {
    let module = match vfio::module_for(alias, state.vendor, state.device_id) {
        Ok(module) => module,
        Err(err) if err.kind() == io::ErrorKind::NotFound && generic_vfio_supported(state) => {
            fs::read_to_string(alias).with_context(|| format!("read {}", alias.display()))?;
            "vfio_pci".to_string()
        }
        Err(err) => return Err(err.into()),
    };

    verify_variant_available(state, &module)?;
    Ok(module)
}

/// A missing variant alias is expected for a CC GPU the catalog knows as
/// noncoherent, for an NVSwitch, and for a management PF, which upstream's
/// mlx5 variant leaves alone since it claims the SR-IOV VF. A GPU the catalog
/// does not know cannot establish that generic vfio-pci is safe for it.
fn generic_vfio_supported(state: &DeviceState) -> bool {
    match state.provisioning() {
        Some(Provisioning::NvidiaInBandCc) => state.cc_capable() && !cc::is_c2c(state.device_id),
        Some(Provisioning::NvidiaInBandPpcie | Provisioning::FabricManagement) => true,
        None => false,
    }
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
        let module = module_for(&alias, state)
            .with_context(|| format!("{}: find its vfio module", state.address))?;

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
        managed_fabric_node, node, B200, GB200, GB202, GH200, H100, NVSWITCH, RUBIN_C2C,
    };
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};
    use std::fs;

    #[fixture]
    fn alias_file() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "alias vfio_pci:v*d*sv*sd*bc*sc*i* vfio_pci\n").unwrap();
        file
    }

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

    /// Upstream's mlx5 variant claims only the SR-IOV VF, so a management PF
    /// is bound generically like any other NIC.
    #[rstest]
    fn a_management_pf_takes_vfio_pci_when_no_variant_claims_it(
        managed_fabric_node: Fake,
        alias_file: tempfile::NamedTempFile,
    ) {
        let states = in_scope(&managed_fabric_node.sysfs).unwrap().devices;

        assert_eq!(
            module_for(alias_file.path(), &states[0]).unwrap(),
            "vfio_pci"
        );
    }

    #[rstest]
    #[case::h100(H100)]
    #[case::b200(B200)]
    #[case::rtx_pro_6000(GB202)]
    #[case::nvswitch(NVSWITCH)]
    fn supported_noncoherent_devices_need_no_specific_vfio_alias(
        sysfs: Fake,
        alias_file: tempfile::NamedTempFile,
        #[case] device: (u16, u32),
        #[values(None, Some("vfio-pci"))] bound: Option<&str>,
    ) {
        let states = node(&sysfs, &[device]);
        sysfs.add_device(&states[0].address, bound);
        let states = in_scope(&sysfs.sysfs).unwrap().devices;

        assert_eq!(
            module_for(alias_file.path(), &states[0]).unwrap(),
            "vfio_pci"
        );
    }

    #[rstest]
    #[case::grace_hopper(GH200)]
    #[case::grace_blackwell(GB200)]
    #[case::coherent_rubin(RUBIN_C2C)]
    #[case::ambiguous_attachment((0x29bc, 0x030200))]
    #[case::unknown_gpu((0xffff, 0x030200))]
    fn missing_variant_support_is_not_hidden_by_generic_vfio(
        sysfs: Fake,
        alias_file: tempfile::NamedTempFile,
        #[case] device: (u16, u32),
    ) {
        let states = node(&sysfs, &[device]);

        assert!(module_for(alias_file.path(), &states[0]).is_err());
    }

    #[rstest]
    #[case::grace_hopper(GH200, "nvgrace_gpu_vfio_pci")]
    #[case::grace_blackwell(GB200, "nvgrace_gpu_vfio_pci")]
    #[case::device_specific_override(GB202, "custom_vfio_pci")]
    fn specific_kernel_aliases_still_choose_the_module(
        sysfs: Fake,
        alias_file: tempfile::NamedTempFile,
        #[case] device: (u16, u32),
        #[case] module: &str,
    ) {
        fs::write(
            alias_file.path(),
            format!(
                "alias vfio_pci:v000010DEd{:08X}sv*sd*bc*sc*i* {module}\n",
                device.0
            ),
        )
        .unwrap();
        let states = node(&sysfs, &[device]);

        assert_eq!(module_for(alias_file.path(), &states[0]).unwrap(), module);
    }

    #[rstest]
    #[case::grace_hopper(GH200)]
    #[case::grace_blackwell(GB200)]
    #[case::coherent_rubin(RUBIN_C2C)]
    fn a_generic_alias_cannot_replace_a_required_variant(
        sysfs: Fake,
        alias_file: tempfile::NamedTempFile,
        #[case] device: (u16, u32),
    ) {
        fs::write(
            alias_file.path(),
            format!(
                "alias vfio_pci:v000010DEd{:08X}sv*sd*bc*sc*i* vfio_pci\n",
                device.0
            ),
        )
        .unwrap();
        let states = node(&sysfs, &[device]);

        let err = module_for(alias_file.path(), &states[0]).unwrap_err();
        assert!(
            format!("{err:#}").contains("coherently attached"),
            "{err:#}"
        );
    }

    #[rstest]
    #[case::missing(false)]
    #[case::unreadable_contents(true)]
    fn a_broken_alias_table_is_not_a_generic_vfio_fallback(
        sysfs: Fake,
        alias_file: tempfile::NamedTempFile,
        #[case] invalid_data: bool,
    ) {
        let states = node(&sysfs, &[GB202]);
        if invalid_data {
            fs::write(alias_file.path(), [0xff]).unwrap();
        } else {
            fs::remove_file(alias_file.path()).unwrap();
        }

        let err = module_for(alias_file.path(), &states[0]).unwrap_err();
        assert!(format!("{err:#}").contains("read "), "{err:#}");
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
