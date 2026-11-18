// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! A variant driver declares `override_only` aliases, so the kernel's alias
//! table is the only record of which vfio driver claims a device. A list of
//! device ids here would go stale as the kernel adds ids.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::{cc, vfio, Sysfs};

use crate::device::{DeviceState, Provisioning};
use crate::fabric::Destination;
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
    fabric: Destination,
) -> Result<Vec<Planned<'a>>> {
    let alias = modules::modules_alias(host_root, proc)?;

    states
        .iter()
        .map(|state| {
            let module = match fabric.takes(state) {
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

type Groups<'a> = BTreeMap<u32, Vec<&'a DeviceState>>;

/// A device outside an IOMMU group cannot be bound to vfio at all, and
/// `host::check` only proves the IOMMU is on somewhere on the node.
fn by_group<'a>(states: &[&'a DeviceState]) -> Result<Groups<'a>> {
    let mut groups = Groups::new();
    let mut outside = Vec::new();
    for state in states {
        match state.iommu_group {
            Some(group) => groups.entry(group).or_default().push(*state),
            None => outside.push(format!("{} ({})", state.address, state.device_name)),
        }
    }
    if !outside.is_empty() {
        bail!(
            "{} of the node's {} passthrough devices are in no IOMMU group and cannot be bound \
             to vfio: {}",
            outside.len(),
            states.len(),
            outside.join(", ")
        );
    }
    Ok(groups)
}

/// VFIO gives a group to one VM at a time, so a group holding two GPUs, or a
/// GPU and a fabric function, would have the hypervisor fail after the GPU
/// modes changed. The fabric's functions go to one VM together.
fn verify_one_vm_per_group(groups: &Groups) -> Result<()> {
    let split: Vec<String> = groups
        .iter()
        .filter(|(_, members)| {
            members.len() > 1 && members.iter().any(|member| !member.is_fabric())
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
    Ok(())
}

const PCI_BASE_CLASS_BRIDGE: u32 = 0x06;

/// The kernel hands a group to VFIO once no member's driver owns its DMA. A
/// vfio driver and `pci-stub` leave DMA to the user, and so does the port
/// driver on a bridge, which declares `driver_managed_dma`. Every GPU shares
/// its group with the root port and usually a PCIe switch above it, so
/// counting bridges would refuse every GPU.
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

/// VFIO gives a VM a group only when every member is bound to vfio or to
/// nothing, so a member this run leaves to a host driver would have the
/// hypervisor fail after the GPU modes changed.
fn verify_peers(sysfs: &Sysfs, groups: &Groups) -> Result<()> {
    let in_run: HashSet<&str> = groups
        .values()
        .flatten()
        .map(|state| state.address.as_str())
        .collect();
    let mut held = Vec::new();
    for (group, members) in groups {
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
                    "{peer} ({driver}) shares IOMMU group {group} with {}",
                    members[0].address
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

/// A group VFIO cannot hand to a VM fails in the hypervisor, after the GPU
/// modes changed, so it is refused before the mode stage.
pub(crate) fn verify_groups(
    sysfs: &Sysfs,
    states: &[DeviceState],
    fabric: Destination,
) -> Result<()> {
    let run: Vec<&DeviceState> = states.iter().filter(|state| fabric.takes(state)).collect();
    let groups = by_group(&run)?;
    // Under PPCIE the whole board goes to one VM, shared groups included.
    if fabric != Destination::PpcieVm {
        verify_one_vm_per_group(&groups)?;
    }
    verify_peers(sysfs, &groups)
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
    pub(crate) fn on_vfio_pci(states: &[DeviceState], fabric: Destination) -> Vec<Planned<'_>> {
        states
            .iter()
            .map(|state| Planned {
                state,
                module: fabric.takes(state).then(|| "vfio_pci".to_string()),
            })
            .collect()
    }

    /// What `load` returns for that plan, without the modprobe a test cannot
    /// run.
    pub(crate) fn loaded<'a>(planned: &[Planned<'a>]) -> Vec<Target<'a>> {
        planned
            .iter()
            .filter(|device| device.module.is_some())
            .map(|device| Target {
                state: device.state,
                module: "vfio_pci".to_string(),
                driver: "vfio-pci".to_string(),
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
        if grouped && !listed {
            fs::remove_dir_all(sysfs.sysfs.iommu_groups().join("7/devices")).unwrap();
        }

        assert_eq!(
            verify_groups(&sysfs.sysfs, &states, Destination::Host).is_ok(),
            accepted
        );
    }

    #[rstest]
    #[case::two_gpus(Destination::Host, false)]
    #[case::two_gpus_under_ppcie(Destination::PpcieVm, true)]
    fn a_group_holding_devices_for_different_vms_is_refused(
        sysfs: Fake,
        #[case] fabric: Destination,
        #[case] accepted: bool,
    ) {
        for state in node(&sysfs, &[H100, H100]) {
            sysfs.set_iommu_group(&state.address, 7);
        }
        let states = in_scope(&sysfs.sysfs).unwrap().devices;

        assert_eq!(
            verify_groups(&sysfs.sysfs, &states, fabric).is_ok(),
            accepted
        );
    }

    #[rstest]
    fn the_fabrics_functions_share_a_group_and_a_gpu_may_not_join_them(managed_fabric_node: Fake) {
        let fake = managed_fabric_node;
        for state in in_scope(&fake.sysfs).unwrap().devices {
            fake.set_iommu_group(&state.address, 7);
        }
        let states = in_scope(&fake.sysfs).unwrap().devices;
        assert!(verify_groups(&fake.sysfs, &states, Destination::ServiceVm).is_ok());

        let gpu = "0000:07:00.0";
        fake.add_pci_device(gpu, 0x10de, H100.0, H100.1, None);
        fake.set_iommu_group(gpu, 7);
        let states = in_scope(&fake.sysfs).unwrap().devices;
        assert!(verify_groups(&fake.sysfs, &states, Destination::ServiceVm).is_err());
    }

    /// The root port and the PCIe switch above a GPU share its group and stay
    /// on `pcieport`, and the kernel gives VFIO the group anyway.
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
        let states = in_scope(&sysfs.sysfs).unwrap().devices;
        assert_eq!(
            states.len(),
            1,
            "the NIC is out of scope, the GPU is the run"
        );

        assert_eq!(
            verify_groups(&sysfs.sysfs, &states, Destination::Host).is_ok(),
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
            verify_bindable(&fake.sysfs, &on_vfio_pci(&states, Destination::ServiceVm)).is_ok(),
            accepted
        );
        assert_eq!(fake.driver_override("0000:05:00.0").trim(), "");
    }
}
