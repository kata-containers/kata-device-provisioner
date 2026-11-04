// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Which vfio driver claims a device, read from the kernel's own alias table,
//! and whether the device can be claimed now.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::{cc, vfio, Sysfs, DRIVER_VFIO_PCI_TYPE};

use crate::device::{DeviceState, Provisioning};
use crate::mode::Mode;
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

/// A device outside an IOMMU group cannot be bound to vfio at all, and
/// `host::check` only proves the IOMMU is on somewhere on the node. A group
/// shared with a function this run leaves to a host driver is refused too:
/// VFIO gives a VM a group only when every member is bound to vfio or to
/// nothing, so the hypervisor would fail after the GPU modes changed. So is
/// a group holding devices that go to different VMs, two GPUs or a GPU and
/// a fabric function, since VFIO gives a group to one VM at a time; the
/// fabric's functions go to one VM together, and under PPCIE the whole
/// board does. All are refused here, before a GPU mode changes or a fabric
/// function is bound.
pub(crate) fn verify_groups(sysfs: &Sysfs, states: &[DeviceState], mode: Mode) -> Result<()> {
    let outside: Vec<String> = states
        .iter()
        .filter(|state| state.iommu_group < 0)
        .map(|state| format!("{} ({})", state.address, state.device_name))
        .collect();
    if !outside.is_empty() {
        bail!(
            "{} of the node's {} passthrough devices are in no IOMMU group and cannot be bound \
             to vfio: {}",
            outside.len(),
            states.len(),
            outside.join(", ")
        );
    }

    if mode != Mode::Ppcie {
        let mut by_group: BTreeMap<i64, Vec<&DeviceState>> = BTreeMap::new();
        for state in states {
            by_group.entry(state.iommu_group).or_default().push(state);
        }
        let split: Vec<String> = by_group
            .iter()
            .filter(|(_, members)| {
                let gpus = members.iter().filter(|member| !member.is_fabric()).count();
                gpus > 1 || (gpus == 1 && members.len() > 1)
            })
            .map(|(group, members)| {
                let addresses: Vec<&str> = members.iter().map(|m| m.address.as_str()).collect();
                format!("group {group}: {}", addresses.join(", "))
            })
            .collect();
        if !split.is_empty() {
            bail!(
                "{} IOMMU group(s) hold passthrough devices that go to different VMs, which VFIO \
                 gives to one VM at a time: {}",
                split.len(),
                split.join("; ")
            );
        }
    }

    let in_run: HashSet<&str> = states.iter().map(|state| state.address.as_str()).collect();
    let groups: BTreeMap<i64, &str> = states
        .iter()
        .map(|state| (state.iommu_group, state.address.as_str()))
        .collect();
    let mut held = Vec::new();
    for (group, member) in groups {
        let devices = sysfs.iommu_groups().join(group.to_string()).join("devices");
        let entries =
            fs::read_dir(&devices).with_context(|| format!("read {}", devices.display()))?;
        for entry in entries {
            let peer = entry
                .with_context(|| format!("read {}", devices.display()))?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if in_run.contains(peer.as_str()) {
                continue;
            }
            if let Some(driver) = vfio::current_driver(sysfs, &peer) {
                if !is_vfio(&driver) {
                    held.push(format!(
                        "{peer} ({driver}) shares IOMMU group {group} with {member}"
                    ));
                }
            }
        }
    }
    if !held.is_empty() {
        bail!(
            "{} function(s) outside this run are held by a host driver inside a passthrough \
             device's IOMMU group, which VFIO would refuse to give a VM: {}. Stop their host \
             services and unbind them, or leave this node out of the run's node selection",
            held.len(),
            held.join(", ")
        );
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

    #[rstest]
    #[case::grouped(true, true, true)]
    #[case::group_without_a_member_listing(true, false, false)]
    #[case::outside_any_group(false, false, false)]
    fn a_target_outside_an_iommu_group_is_refused(
        sysfs: Fake,
        #[case] grouped: bool,
        #[case] listed: bool,
        #[case] accepted: bool,
    ) {
        let mut states = node(&sysfs, &[H100]);
        if grouped {
            sysfs.set_iommu_group(&states[0].address, 7);
            states = in_scope(&sysfs.sysfs).unwrap().devices;
        }
        if listed {
            list_group_members(&sysfs, 7, &[&states[0].address]);
        }

        assert_eq!(
            verify_groups(&sysfs.sysfs, &states, Mode::Off).is_ok(),
            accepted
        );
    }

    #[rstest]
    #[case::two_gpus(Mode::Off, false)]
    #[case::two_gpus_under_ppcie(Mode::Ppcie, true)]
    fn a_group_holding_devices_for_different_vms_is_refused(
        sysfs: Fake,
        #[case] mode: Mode,
        #[case] accepted: bool,
    ) {
        let states = node(&sysfs, &[H100, H100]);
        let addresses: Vec<String> = states.iter().map(|s| s.address.clone()).collect();
        for address in &addresses {
            sysfs.set_iommu_group(address, 7);
        }
        let members: Vec<&str> = addresses.iter().map(String::as_str).collect();
        list_group_members(&sysfs, 7, &members);
        let states = in_scope(&sysfs.sysfs).unwrap().devices;

        assert_eq!(verify_groups(&sysfs.sysfs, &states, mode).is_ok(), accepted);
    }

    #[rstest]
    fn the_fabrics_functions_share_a_group_and_a_gpu_may_not_join_them(managed_fabric_node: Fake) {
        let fabric: Vec<String> = in_scope(&managed_fabric_node.sysfs)
            .unwrap()
            .devices
            .iter()
            .map(|s| s.address.clone())
            .collect();
        for address in &fabric {
            managed_fabric_node.set_iommu_group(address, 7);
        }
        let members: Vec<&str> = fabric.iter().map(String::as_str).collect();
        list_group_members(&managed_fabric_node, 7, &members);
        let states = in_scope(&managed_fabric_node.sysfs).unwrap().devices;
        assert!(verify_groups(&managed_fabric_node.sysfs, &states, Mode::Off).is_ok());

        let gpu = "0000:07:00.0";
        managed_fabric_node.add_pci_device(gpu, 0x10de, H100.0, H100.1, None);
        managed_fabric_node.set_iommu_group(gpu, 7);
        std::os::unix::fs::symlink(
            managed_fabric_node.device(gpu),
            managed_fabric_node
                .sysfs
                .iommu_groups()
                .join("7/devices")
                .join(gpu),
        )
        .unwrap();
        let states = in_scope(&managed_fabric_node.sysfs).unwrap().devices;
        assert!(verify_groups(&managed_fabric_node.sysfs, &states, Mode::Off).is_err());
    }

    /// The kernel lists a group's members under iommu_groups; the fixture
    /// only links a device to its group, so the listing is written here.
    fn list_group_members(sysfs: &Fake, group: u32, members: &[&str]) {
        let devices = sysfs
            .sysfs
            .iommu_groups()
            .join(group.to_string())
            .join("devices");
        std::fs::create_dir_all(&devices).unwrap();
        for member in members {
            std::os::unix::fs::symlink(sysfs.device(member), devices.join(member)).unwrap();
        }
    }

    #[rstest]
    #[case::peer_held_by_a_host_driver(Some("mlx5_core"), false)]
    #[case::peer_unbound(None, true)]
    #[case::peer_already_on_vfio(Some("vfio-pci"), true)]
    fn a_group_shared_with_a_host_held_function_is_refused(
        sysfs: Fake,
        #[case] peer_driver: Option<&str>,
        #[case] accepted: bool,
    ) {
        let gpu = node(&sysfs, &[H100])[0].address.clone();
        let peer = "0000:00:00.1";
        sysfs.add_pci_device(peer, 0x15b3, 0x1021, 0x020000, peer_driver);
        sysfs.set_iommu_group(&gpu, 7);
        sysfs.set_iommu_group(peer, 7);
        list_group_members(&sysfs, 7, &[&gpu, peer]);
        let states = in_scope(&sysfs.sysfs).unwrap().devices;
        assert_eq!(
            states.len(),
            1,
            "the NIC is out of scope, the GPU is the run"
        );

        assert_eq!(
            verify_groups(&sysfs.sysfs, &states, Mode::Off).is_ok(),
            accepted
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
