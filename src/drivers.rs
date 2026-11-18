// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Which vfio driver claims a device, read from the kernel's own alias table,
//! and whether the device can be claimed now.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::{cc, vfio, Sysfs};

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

/// Whether a function outside the run keeps VFIO from giving its group to a
/// VM. The kernel hands a group over once no member's driver owns its DMA: a
/// vfio driver and `pci-stub` leave that to the user, and so does the port
/// driver on a bridge, which declares `driver_managed_dma`. Every GPU sits
/// behind a root port and usually a PCIe switch that share its group, so a
/// bridge is never a blocker, whatever holds it; an endpoint on any other
/// driver is.
fn blocks_group(sysfs: &Sysfs, peer: &str, driver: &str) -> Result<bool> {
    if is_vfio(driver) || driver == "pci-stub" {
        return Ok(false);
    }
    let device = sysfs
        .device(peer)
        .with_context(|| format!("{peer}: not a PCI address"))?;
    let class = pcilibs_rs::attr_hex(&device, "class")
        .with_context(|| format!("{peer}: read its PCI class"))?;
    Ok(class >> 16 != PCI_BASE_CLASS_BRIDGE)
}

const PCI_BASE_CLASS_BRIDGE: u32 = 0x06;

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
            let Some(driver) = vfio::current_driver(sysfs, &peer) else {
                continue;
            };
            if blocks_group(sysfs, &peer, &driver)? {
                held.push(format!(
                    "{peer} ({driver}) shares IOMMU group {group} with {member}"
                ));
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

    /// A GPU sits behind a root port and usually a PCIe switch that share its
    /// group and stay on `pcieport`; on a DGX Rubin NVL8 that is forty-nine
    /// bridges across eight GPUs, and the kernel gives VFIO the group anyway.
    #[rstest]
    #[case::peer_held_by_a_host_driver(0x020000, Some("mlx5_core"), false)]
    #[case::peer_unbound(0x020000, None, true)]
    #[case::peer_already_on_vfio(0x020000, Some("vfio-pci"), true)]
    #[case::peer_parked_on_pci_stub(0x020000, Some("pci-stub"), true)]
    #[case::root_port_on_pcieport(0x060400, Some("pcieport"), true)]
    #[case::host_bridge(0x060000, Some("pcieport"), true)]
    #[case::audio_companion_on_a_host_driver(0x040300, Some("snd_hda_intel"), false)]
    fn a_group_shared_with_a_host_held_function_is_refused(
        sysfs: Fake,
        #[case] peer_class: u32,
        #[case] peer_driver: Option<&str>,
        #[case] accepted: bool,
    ) {
        let gpu = node(&sysfs, &[H100])[0].address.clone();
        let peer = "0000:00:00.1";
        sysfs.add_pci_device(peer, 0x15b3, 0x1021, peer_class, peer_driver);
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
