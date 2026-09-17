// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Whether anything is currently using a device.
//!
//! A CC mode change resets the GPU, taking it from any VM mid-computation. A
//! VM holding one always has its VFIO node open, so that is the evidence.

use std::fs;
use std::path::{Path, PathBuf};

use crate::device::DeviceState;
use pcilibs_rs::Sysfs;

pub const PROC: &str = "/proc";
pub const DEV_VFIO: &str = "/dev/vfio";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub comm: String,
    pub node: PathBuf,
}

/// Unreadable `/proc` entries are skipped: the caller is root, so one it
/// cannot see is one that exited mid-scan.
pub fn holders(
    proc_root: &Path,
    dev_vfio: &Path,
    sysfs: &Sysfs,
    state: &DeviceState,
) -> Vec<Holder> {
    let nodes = vfio_nodes(dev_vfio, sysfs, state);
    if nodes.is_empty() {
        return Vec::new();
    }

    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for process in entries.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };

        let Ok(fds) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };

        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if nodes.contains(&target) {
                found.push(Holder {
                    pid,
                    comm: comm(&process.path()),
                    node: target,
                });
            }
        }
    }

    found.sort_by_key(|holder| (holder.pid, holder.node.clone()));
    found.dedup();
    found
}

/// Both the legacy group node and the newer cdev: which one a VMM opens is up
/// to the VMM and the kernel.
fn vfio_nodes(dev_vfio: &Path, sysfs: &Sysfs, state: &DeviceState) -> Vec<PathBuf> {
    let mut nodes = Vec::new();

    if state.iommu_group >= 0 {
        nodes.push(dev_vfio.join(state.iommu_group.to_string()));
    }

    let cdevs = sysfs
        .device(&state.address)
        .map(|device| device.join("vfio-dev"));

    if let Ok(entries) = fs::read_dir(cdevs.unwrap_or_default()) {
        for entry in entries.flatten() {
            nodes.push(dev_vfio.join("devices").join(entry.file_name()));
        }
    }

    nodes
}

fn comm(process: &Path) -> String {
    fs::read_to_string(process.join("comm"))
        .map(|comm| comm.trim().to_string())
        .unwrap_or_else(|_| "?".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::discover;
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};
    use tempfile::TempDir;

    struct Node {
        fake: Fake,
        proc_root: TempDir,
        dev_vfio: TempDir,
    }

    impl Node {
        fn state(&self) -> DeviceState {
            discover(&self.fake.sysfs)
                .unwrap()
                .into_iter()
                .find(|state| state.address == "0000:65:00.0")
                .unwrap()
        }

        fn add_process(&self, pid: u32, comm: &str, node: &str) {
            let fd_dir = self.proc_root.path().join(pid.to_string()).join("fd");
            fs::create_dir_all(&fd_dir).unwrap();
            fs::write(
                self.proc_root.path().join(pid.to_string()).join("comm"),
                format!("{comm}\n"),
            )
            .unwrap();

            let target = self.dev_vfio.path().join(node);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, "").unwrap();
            std::os::unix::fs::symlink(&target, fd_dir.join("3")).unwrap();
        }

        fn holders(&self) -> Vec<Holder> {
            holders(
                self.proc_root.path(),
                self.dev_vfio.path(),
                &self.fake.sysfs,
                &self.state(),
            )
        }
    }

    #[fixture]
    fn node() -> Node {
        let fake = testfs::fake();
        fake.add_pci_device("0000:65:00.0", 0x10de, 0x2330, 0x030200, Some("vfio-pci"));
        fake.set_iommu_group("0000:65:00.0", 14);
        fs::create_dir_all(fake.device("0000:65:00.0").join("vfio-dev").join("vfio0")).unwrap();

        Node {
            fake,
            proc_root: tempfile::tempdir().unwrap(),
            dev_vfio: tempfile::tempdir().unwrap(),
        }
    }

    #[rstest]
    fn an_untouched_device_has_no_holders(node: Node) {
        assert!(node.holders().is_empty());
    }

    #[rstest]
    #[case::legacy_group_node("14")]
    #[case::cdev("devices/vfio0")]
    fn finds_a_vm_holding_the_device(node: Node, #[case] vfio_node: &str) {
        node.add_process(4242, "qemu-system-x86", vfio_node);

        let holders = node.holders();

        assert_eq!(holders.len(), 1, "{holders:?}");
        assert_eq!(holders[0].pid, 4242);
        assert_eq!(holders[0].comm, "qemu-system-x86");
    }

    #[rstest]
    fn ignores_fds_on_other_devices(node: Node) {
        node.add_process(4242, "qemu-system-x86", "devices/vfio9");
        node.add_process(4243, "cloud-hypervisor", "77");

        assert!(node.holders().is_empty());
    }

    #[rstest]
    fn reports_every_holder(node: Node) {
        node.add_process(4242, "qemu-system-x86", "devices/vfio0");
        node.add_process(17, "cloud-hypervisor", "14");

        let pids: Vec<_> = node.holders().iter().map(|holder| holder.pid).collect();

        assert_eq!(pids, [17, 4242]);
    }

    #[rstest]
    fn survives_a_process_that_exits_mid_scan(node: Node) {
        node.add_process(4242, "qemu-system-x86", "devices/vfio0");
        fs::remove_dir_all(node.proc_root.path().join("4242").join("fd")).unwrap();

        assert!(node.holders().is_empty());
    }

    #[rstest]
    fn ignores_non_numeric_proc_entries(node: Node) {
        fs::create_dir_all(node.proc_root.path().join("self").join("fd")).unwrap();

        assert!(node.holders().is_empty());
    }
}
