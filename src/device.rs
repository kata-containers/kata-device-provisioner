// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use pcilibs_rs::cc::{CcMode, PpcieMode};
use pcilibs_rs::{is_passthrough_capable_class, PCIDeviceManager};

use crate::nvidia;
use pcilibs_rs::Sysfs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A TDISP device (SEV-TIO, TDX Connect) needs no variant here: it locks to a
/// VM at bind time, so there is no node-level mode to set.
pub enum Provisioning {
    /// NVIDIA Hopper and Blackwell.
    NvidiaInBandCc,
    /// NVSwitch: no CC mode of its own, but carries the baseboard's PPCIE mode.
    NvidiaInBandPpcie,
}

/// `nvidia.com/cc.*` is NVIDIA's contract; kata-deploy's selectors key off it.
#[derive(Debug)]
pub struct CcLabels {
    pub mode_state: &'static str,
    pub ready_state: &'static str,
}

const NVIDIA_CC_LABELS: CcLabels = CcLabels {
    mode_state: "nvidia.com/cc.mode.state",
    ready_state: "nvidia.com/cc.ready.state",
};

/// `class_prefix` is the base+subclass, the top 16 bits of the class code.
#[derive(Debug)]
pub struct DeviceRow {
    pub vendor: u16,
    pub class_prefix: u16,
    pub provisioning: Provisioning,
    pub cc_labels: Option<&'static CcLabels>,
}

/// A device with no row is never touched. The identities match
/// kata-device-plugin's, so the two cannot disagree about what a device is.
pub const DEVICES: &[DeviceRow] = &[
    // NVIDIA GPU: 3D controller.
    DeviceRow {
        vendor: 0x10de,
        class_prefix: 0x0302,
        provisioning: Provisioning::NvidiaInBandCc,
        cc_labels: Some(&NVIDIA_CC_LABELS),
    },
    // NVIDIA NVSwitch: bridge, other.
    DeviceRow {
        vendor: 0x10de,
        class_prefix: 0x0680,
        provisioning: Provisioning::NvidiaInBandPpcie,
        cc_labels: None,
    },
];

pub fn row_for(vendor: u16, class: u32) -> Option<&'static DeviceRow> {
    let class_prefix = (class >> 8) as u16;
    DEVICES
        .iter()
        .find(|row| row.vendor == vendor && row.class_prefix == class_prefix)
}

#[derive(Clone, Debug)]
pub struct DeviceState {
    pub address: String,
    pub vendor: u16,
    pub device_id: u16,
    pub device_name: String,
    /// Full 24-bit class code. The name is not carried: `pcilibs-rs` resolves
    /// it against the 8-bit base class and always misses.
    pub class: u32,
    pub driver: Option<String>,
    pub iommu_group: i64,
    pub numa_node: i64,
    /// `None` means out of scope.
    pub row: Option<&'static DeviceRow>,
    /// `None` means no in-band CC support.
    pub cc_chip: Option<&'static str>,
    /// `None` unless the caller probed.
    pub cc_mode: Option<CcMode>,
    /// `None` unless the caller probed.
    pub ppcie_mode: Option<PpcieMode>,
}

impl DeviceState {
    pub fn provisioning(&self) -> Option<Provisioning> {
        self.row.map(|row| row.provisioning)
    }

    pub fn in_scope(&self) -> bool {
        self.row.is_some()
    }

    /// False for an NVSwitch, which has no mode of its own, and for anything
    /// older than Hopper.
    pub fn cc_capable(&self) -> bool {
        self.provisioning() == Some(Provisioning::NvidiaInBandCc) && self.cc_chip.is_some()
    }

    /// NVSwitches and Hopper GPUs; Blackwell encrypts NVLink instead and has
    /// no PPCIE. A switch's generation is only knowable from BAR0, so that
    /// check is left to `pcilibs_rs::cc` at open time.
    pub fn carries_ppcie(&self) -> bool {
        match self.provisioning() {
            Some(Provisioning::NvidiaInBandCc) => nvidia::supports_ppcie(self.device_id),
            Some(Provisioning::NvidiaInBandPpcie) => true,
            None => false,
        }
    }

    /// Empty until the mode is known: an unprobed run must not claim CC-ready.
    pub fn cc_label_values(&self) -> Vec<(&'static str, String)> {
        let Some(labels) = self.row.and_then(|row| row.cc_labels) else {
            return Vec::new();
        };

        // A PPCIE board runs with per-GPU CC off, so reporting the CC mode
        // alone would label a protected node as not ready.
        let (mode, ready) = match (self.ppcie_mode, self.cc_mode) {
            (Some(PpcieMode::On), _) => ("ppcie".to_string(), true),
            (_, Some(cc)) => (cc.to_string(), cc != CcMode::Off),
            (_, None) => return Vec::new(),
        };

        vec![
            (labels.mode_state, mode),
            (labels.ready_state, ready.to_string()),
        ]
    }
}

/// Sorted by PCI address. Pure sysfs: no root, no BAR0, no device wake-up.
pub fn discover(sysfs: &Sysfs) -> Result<Vec<DeviceState>> {
    let manager = PCIDeviceManager::new(sysfs.clone());
    let devices = manager
        .get_all_devices(None)
        .with_context(|| format!("enumerate PCI devices under {}", sysfs.devices().display()))?;

    Ok(devices
        .into_iter()
        .filter(|dev| is_passthrough_capable_class(dev.class))
        .map(|dev| {
            let row = row_for(dev.vendor, dev.class);
            DeviceState {
                vendor: dev.vendor,
                device_id: dev.device,
                device_name: dev.device_name,
                class: dev.class,
                driver: (!dev.driver.is_empty()).then_some(dev.driver),
                iommu_group: dev.iommu_group,
                numa_node: dev.numa_node,
                row,
                cc_chip: match row.map(|row| row.provisioning) {
                    Some(Provisioning::NvidiaInBandCc) => nvidia::cc_chip(dev.device),
                    Some(Provisioning::NvidiaInBandPpcie) | None => None,
                },
                cc_mode: None,
                ppcie_mode: None,
                address: dev.address,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};

    #[fixture]
    fn sysfs() -> Fake {
        testfs::fake()
    }

    #[fixture]
    fn hopper_node(sysfs: Fake) -> Fake {
        sysfs.add_pci_device("0000:65:00.0", 0x10de, 0x2330, 0x030200, Some("vfio-pci"));
        sysfs.add_pci_device("0000:0a:00.0", 0x10de, 0x2330, 0x030200, Some("vfio-pci"));
        sysfs.add_pci_device("0000:06:00.0", 0x10de, 0x22a3, 0x068000, None);
        sysfs.add_pci_device("0000:64:00.0", 0x10de, 0x22a4, 0x060400, Some("pcieport"));
        sysfs.add_pci_device("0000:01:00.0", 0x8086, 0x1521, 0x020000, Some("igb"));
        sysfs
    }

    #[rstest]
    #[case::h100(
        0x10de,
        0x2330,
        0x030200,
        Some(Provisioning::NvidiaInBandCc),
        Some("GH100")
    )]
    #[case::b200(
        0x10de,
        0x2901,
        0x030200,
        Some(Provisioning::NvidiaInBandCc),
        Some("GB100")
    )]
    #[case::pre_cc_gpu(0x10de, 0x20b0, 0x030200, Some(Provisioning::NvidiaInBandCc), None)]
    #[case::nvswitch(0x10de, 0x22a3, 0x068000, Some(Provisioning::NvidiaInBandPpcie), None)]
    #[case::display_gpu(0x10de, 0x25b2, 0x030000, None, None)]
    #[case::amd_gpu(0x1002, 0x74a1, 0x030200, None, None)]
    #[case::intel_nic(0x8086, 0x1521, 0x020000, None, None)]
    fn table_decides_what_is_in_scope(
        sysfs: Fake,
        #[case] vendor: u16,
        #[case] device: u16,
        #[case] class: u32,
        #[case] provisioning: Option<Provisioning>,
        #[case] cc_chip: Option<&str>,
    ) {
        sysfs.add_pci_device("0000:65:00.0", vendor, device, class, None);

        let states = discover(&sysfs.sysfs).unwrap();

        assert_eq!(states[0].provisioning(), provisioning);
        assert_eq!(states[0].cc_chip, cc_chip);
        assert!(states[0].cc_mode.is_none(), "discover must not touch BAR0");
    }

    #[rstest]
    #[case::on(CcMode::On, "on", "true")]
    #[case::devtools(CcMode::DevTools, "devtools", "true")]
    #[case::off(CcMode::Off, "off", "false")]
    fn publishes_cc_labels_for_a_probed_gpu(
        sysfs: Fake,
        #[case] mode: CcMode,
        #[case] mode_state: &str,
        #[case] ready_state: &str,
    ) {
        sysfs.add_pci_device("0000:65:00.0", 0x10de, 0x2330, 0x030200, None);
        let mut states = discover(&sysfs.sysfs).unwrap();
        states[0].cc_mode = Some(mode);

        assert_eq!(
            states[0].cc_label_values(),
            [
                ("nvidia.com/cc.mode.state", mode_state.to_string()),
                ("nvidia.com/cc.ready.state", ready_state.to_string()),
            ]
        );
    }

    /// Labelling this node "cc off, not ready" would hide a protected board
    /// from every selector looking for one.
    #[rstest]
    fn a_ppcie_gpu_is_labelled_ppcie_and_ready(sysfs: Fake) {
        sysfs.add_pci_device("0000:65:00.0", 0x10de, 0x2330, 0x030200, None);
        let mut states = discover(&sysfs.sysfs).unwrap();
        states[0].cc_mode = Some(CcMode::Off);
        states[0].ppcie_mode = Some(PpcieMode::On);

        assert_eq!(
            states[0].cc_label_values(),
            [
                ("nvidia.com/cc.mode.state", "ppcie".to_string()),
                ("nvidia.com/cc.ready.state", "true".to_string()),
            ]
        );
    }

    #[rstest]
    #[case::unprobed_gpu(0x10de, 0x2330, 0x030200)]
    #[case::nvswitch_has_no_cc_labels(0x10de, 0x22a3, 0x068000)]
    fn publishes_no_cc_labels_without_a_known_mode(
        sysfs: Fake,
        #[case] vendor: u16,
        #[case] device: u16,
        #[case] class: u32,
    ) {
        sysfs.add_pci_device("0000:65:00.0", vendor, device, class, None);

        let states = discover(&sysfs.sysfs).unwrap();

        assert!(states[0].cc_label_values().is_empty());
    }

    #[rstest]
    #[case::pci_bridge(0x10de, 0x22a4, 0x060400)]
    #[case::host_bridge(0x10de, 0x1af1, 0x060000)]
    #[case::audio_companion(0x10de, 0x22ba, 0x040300)]
    fn devices_that_cannot_be_passed_through_are_not_reported(
        sysfs: Fake,
        #[case] vendor: u16,
        #[case] device: u16,
        #[case] class: u32,
    ) {
        sysfs.add_pci_device("0000:64:00.0", vendor, device, class, None);

        assert!(discover(&sysfs.sysfs).unwrap().is_empty());
    }

    #[rstest]
    #[case::vfio_bound(Some("vfio-pci"))]
    #[case::host_driver(Some("nvidia"))]
    #[case::unbound(None)]
    fn reports_current_driver(sysfs: Fake, #[case] driver: Option<&str>) {
        sysfs.add_pci_device("0000:65:00.0", 0x10de, 0x2330, 0x030200, driver);

        let states = discover(&sysfs.sysfs).unwrap();

        assert_eq!(states[0].driver.as_deref(), driver);
    }

    #[rstest]
    fn discovers_in_pci_address_order(hopper_node: Fake) {
        let states = discover(&hopper_node.sysfs).unwrap();

        let in_scope: Vec<_> = states
            .iter()
            .filter(|s| s.in_scope())
            .map(|s| s.address.as_str())
            .collect();
        assert_eq!(in_scope, ["0000:06:00.0", "0000:0a:00.0", "0000:65:00.0"]);
    }

    #[rstest]
    fn out_of_scope_devices_are_reported_but_untagged(hopper_node: Fake) {
        let states = discover(&hopper_node.sysfs).unwrap();

        let nic = states
            .iter()
            .find(|s| s.address == "0000:01:00.0")
            .expect("a passthrough-capable NIC is still reported");
        assert!(!nic.in_scope());
    }

    #[rstest]
    fn empty_node_discovers_nothing(sysfs: Fake) {
        assert!(discover(&sysfs.sysfs).unwrap().is_empty());
    }
}
