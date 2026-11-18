// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! A variant driver declares `override_only` aliases, so the kernel's alias
//! table is the only record of which vfio driver claims a device. A list of
//! device ids here would go stale as the kernel adds ids.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::{cc, vfio, Sysfs};

use crate::device::{DeviceState, Provisioning};
use crate::modules;

/// A substring, since a variant driver is named after `vfio-pci`:
/// `nvgrace_gpu_vfio_pci`, `mlx5_vfio_pci`.
pub(crate) fn is_vfio(driver: &str) -> bool {
    driver.contains("vfio")
}

/// Generic `vfio-pci` does not map the memory of a coherently attached GPU,
/// so the VM would get the device without it, and for a GPU the catalog does
/// not list nothing establishes that a generic binding works. An NVSwitch has
/// no variant driver, and upstream's mlx5 variant claims only the SR-IOV VF,
/// which leaves generic for a management PF.
fn verify_generic_vfio(state: &DeviceState, module: &str) -> Result<()> {
    match state.provisioning() {
        Some(Provisioning::NvidiaInBandPpcie | Provisioning::FabricManagement) => Ok(()),
        Some(Provisioning::NvidiaInBandCc) if cc::is_c2c(state.device_id) => bail!(
            "{} ({}) is coherently attached over NVLink-C2C and needs a vfio driver that can \
             map its memory, but this kernel's module alias table offers only {module}, which \
             cannot. The node needs a kernel whose nvgrace-gpu vfio driver claims this device",
            state.address,
            state.device_name
        ),
        Some(Provisioning::NvidiaInBandCc) if state.cc_capable() => Ok(()),
        Some(Provisioning::NvidiaInBandCc) | None => bail!(
            "no vfio variant driver claims {:04x}:{:04x}, and generic vfio-pci is not known to \
             be safe for it",
            state.vendor,
            state.device_id
        ),
    }
}

/// `vfio_pci` registers only a catch-all alias, so a readable table with no
/// entry for the device means no variant driver claims it, not a broken table.
fn module_for(alias: &Path, state: &DeviceState) -> Result<String> {
    let module = vfio::module_for(alias, state.vendor, state.device_id)?
        .unwrap_or_else(|| "vfio_pci".to_string());

    if !modules::needs_driver_override(&module) {
        verify_generic_vfio(state, &module)?;
    }
    Ok(module)
}

/// `module` is `None` for a device the run leaves on the host. Such a device
/// stays in the list because the run releases it if vfio still holds it.
pub(crate) struct Planned<'a> {
    pub state: &'a DeviceState,
    pub module: Option<String>,
}

/// modprobe takes the module and `driver_override` the driver, and the two
/// names differ: `vfio_pci` and `vfio-pci`.
pub(crate) struct Target<'a> {
    pub state: &'a DeviceState,
    pub module: String,
    pub driver: String,
}

/// Resolved per device: a Grace GPU needs `nvgrace_gpu_vfio_pci` where the NIC
/// beside it takes plain `vfio-pci`. Nothing is loaded here: `vfio-pci` binds
/// the ids a saved `ids=` option names as it loads, which would change
/// devices before the ownership and idle checks.
pub(crate) fn plan<'a>(
    host_root: &Path,
    proc: &Path,
    states: &'a [DeviceState],
    takes: impl Fn(&DeviceState) -> bool,
) -> Result<Vec<Planned<'a>>> {
    let alias = modules::modules_alias(host_root, proc)?;

    states
        .iter()
        .map(|state| {
            let module = match takes(state) {
                true => Some(
                    module_for(&alias, state)
                        .with_context(|| format!("{}: find its vfio module", state.address))?,
                ),
                false => None,
            };
            Ok(Planned { state, module })
        })
        .collect()
}

/// Whether the device is on the driver its planned module registers. sysfs
/// names that driver only while the module is loaded, and an unloaded module
/// holds no device.
pub(crate) fn bound(sysfs: &Sysfs, state: &DeviceState, module: &str) -> bool {
    vfio::current_driver(sysfs, &state.address).is_some_and(|current| {
        vfio::driver_for(sysfs, module).is_ok_and(|driver| driver == current)
    })
}

/// `vfio::bind` refuses a device another driver holds, but it runs after the
/// mode stage, and a refusal there leaves GPUs reset into a mode the node
/// cannot use.
pub(crate) fn verify_bindable(sysfs: &Sysfs, planned: &[Planned]) -> Result<()> {
    for device in planned {
        let (Some(module), address) = (&device.module, &device.state.address) else {
            continue;
        };
        if let Some(current) = vfio::current_driver(sysfs, address) {
            if !bound(sysfs, device.state, module) {
                bail!("{address}: held by {current}; stop its host services and unbind it before provisioning");
            }
        }
    }
    Ok(())
}

/// Each module is loaded once, after the preflights, and asked for the driver
/// it registers, which sysfs names only for a loaded module.
pub(crate) fn load<'a>(
    sysfs: &Sysfs,
    host_root: &Path,
    planned: &[Planned<'a>],
) -> Result<Vec<Target<'a>>> {
    let mut drivers: BTreeMap<&str, String> = BTreeMap::new();
    let mut targets = Vec::new();
    for device in planned {
        let Some(module) = device.module.as_deref() else {
            continue;
        };
        if !drivers.contains_key(module) {
            modules::modprobe(host_root, module).with_context(|| format!("load {module}"))?;
            let driver = vfio::driver_for(sysfs, module)
                .with_context(|| format!("find the driver {module} registers"))?;
            drivers.insert(module, driver);
        }
        targets.push(Target {
            state: device.state,
            module: module.to_string(),
            driver: drivers[module].clone(),
        });
    }
    Ok(targets)
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

    /// What `plan` returns on a node whose kernel has no variant driver,
    /// without the alias table a test does not have.
    pub(crate) fn on_vfio_pci(
        states: &[DeviceState],
        takes: impl Fn(&DeviceState) -> bool,
    ) -> Vec<Planned<'_>> {
        states
            .iter()
            .map(|state| Planned {
                state,
                module: takes(state).then(|| "vfio_pci".to_string()),
            })
            .collect()
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
        alias_file: tempfile::NamedTempFile,
        #[case] device: (u16, u32),
        #[case] module: &str,
        #[case] expected: bool,
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

        assert_eq!(module_for(alias_file.path(), &states[0]).is_ok(), expected);
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
        fake.add_module("vfio_pci", "vfio-pci");
        fake.add_device("0000:05:00.0", driver);
        let states = in_scope(&fake.sysfs).unwrap().devices;
        assert_eq!(
            verify_bindable(&fake.sysfs, &on_vfio_pci(&states, |_| true)).is_ok(),
            accepted
        );
        assert_eq!(fake.driver_override("0000:05:00.0").trim(), "");
    }
}
