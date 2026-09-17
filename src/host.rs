// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Whether this node can do VFIO passthrough at all.
//!
//! Checked before anything is touched, because none of it is fixable from a
//! pod once a mode transition is half done.

use std::fs;

use anyhow::{bail, Result};

use pcilibs_rs::{normalize_bdf, Sysfs};

/// Their absence is why `modprobe.d` alone is enough at boot: nothing races
/// `vfio-pci` for the device, so it needs no initramfs ordering games.
const CONFLICTING_DRIVERS: &[&str] = &["nvidia", "nouveau"];

/// Says nothing about vfio drivers: requiring `vfio-pci` here would load it
/// on nodes whose GPUs want a variant driver instead.
pub fn check(sysfs: &Sysfs) -> Result<()> {
    check_iommu(sysfs)?;
    check_no_conflicting_driver(sysfs)
}

/// Probing reads GPU registers over BAR0, which is not ours to do on a device
/// another driver is driving, and returns values that cannot be trusted
/// anyway. Plain `status` reads sysfs only and still answers.
pub fn check_probeable(sysfs: &Sysfs) -> Result<()> {
    check_no_conflicting_driver(sysfs)
}

fn check_iommu(sysfs: &Sysfs) -> Result<()> {
    let groups = sysfs.iommu_groups();
    let populated = fs::read_dir(&groups)
        .map(|entries| entries.flatten().next().is_some())
        .unwrap_or(false);

    if !populated {
        bail!(
            "no IOMMU groups under {}: the IOMMU is off. Enable it on the kernel \
             command line (intel_iommu=on / amd_iommu=on) and reboot",
            groups.display()
        );
    }

    Ok(())
}

fn check_no_conflicting_driver(sysfs: &Sysfs) -> Result<()> {
    for name in CONFLICTING_DRIVERS {
        let held = devices_held_by(sysfs, name);
        if !held.is_empty() {
            bail!(
                "the {name} driver holds {}: a passthrough node must not run a \
                 host GPU driver. Blacklist {name} in /etc/modprobe.d, rebuild \
                 the initramfs, and reboot",
                held.join(", ")
            );
        }
    }

    Ok(())
}

/// A driver directory holds a symlink per bound device, named by address, and
/// a `module` symlink that is not a device at all.
fn devices_held_by(sysfs: &Sysfs, driver: &str) -> Vec<String> {
    let Some(entries) = sysfs.driver(driver).and_then(|dir| fs::read_dir(dir).ok()) else {
        return Vec::new();
    };

    let mut held: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| normalize_bdf(name).is_some())
        .collect();
    held.sort();
    held
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};

    #[fixture]
    fn healthy_node() -> Fake {
        let fake = testfs::fake();
        fake.add_iommu_group(14);
        fake.add_driver("vfio-pci");
        fake.add_pci_device("0000:65:00.0", 0x10de, 0x2330, 0x030200, Some("vfio-pci"));
        fake
    }

    #[rstest]
    fn accepts_a_healthy_node(healthy_node: Fake) {
        check(&healthy_node.sysfs).unwrap();
    }

    #[rstest]
    fn rejects_a_node_with_the_iommu_off() {
        let fake = testfs::fake();
        fake.add_driver("vfio-pci");

        let err = check(&fake.sysfs).unwrap_err().to_string();

        assert!(err.contains("IOMMU is off"), "{err}");
    }

    /// A node whose GPU wants a variant driver has no `vfio-pci` registered.
    #[rstest]
    fn accepts_a_node_with_no_vfio_driver_registered_at_all() {
        let fake = testfs::fake();
        fake.add_iommu_group(14);
        fake.add_pci_device("0009:01:00.0", 0x10de, 0x2342, 0x030200, None);

        check(&fake.sysfs).unwrap();
    }

    #[rstest]
    #[case::proprietary("nvidia")]
    #[case::open_source("nouveau")]
    fn rejects_a_node_whose_host_gpu_driver_holds_a_device(
        healthy_node: Fake,
        #[case] driver: &str,
    ) {
        healthy_node.add_pci_device("0000:0a:00.0", 0x10de, 0x2330, 0x030200, Some(driver));

        let err = check(&healthy_node.sysfs).unwrap_err().to_string();

        assert!(err.contains(driver), "{err}");
        assert!(err.contains("0000:0a:00.0"), "{err}");
    }

    #[rstest]
    #[case::proprietary("nvidia")]
    #[case::open_source("nouveau")]
    fn tolerates_a_loaded_host_driver_that_holds_nothing(healthy_node: Fake, #[case] driver: &str) {
        healthy_node.add_driver(driver);

        check(&healthy_node.sysfs).unwrap();
    }

    /// Every loaded driver has one, and it is not a device.
    #[rstest]
    fn does_not_take_a_drivers_module_link_for_a_device(healthy_node: Fake) {
        healthy_node.add_driver("nouveau");
        let module = healthy_node.driver("nouveau").join("module");
        std::os::unix::fs::symlink("../../../../module/nouveau", module).unwrap();

        assert!(devices_held_by(&healthy_node.sysfs, "nouveau").is_empty());
        check(&healthy_node.sysfs).unwrap();
    }
}
