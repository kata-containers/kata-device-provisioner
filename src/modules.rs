// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! The host's kernel modules: which one a device needs, loading it with the
//! host's own modprobe as kata-deploy does, and configuring the node to do the
//! same at boot. Not libkmod: whether a `.ko.zst` loads would then depend on
//! how the *container's* libkmod was built.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use pcilibs_rs::DRIVER_VFIO_PCI_TYPE;

const MODPROBE_CANDIDATES: &[&str] = &[
    "/usr/sbin/modprobe",
    "/sbin/modprobe",
    "/usr/bin/modprobe",
    "/bin/modprobe",
];

/// The running kernel's alias table on the node. Read from `/proc` rather
/// than `uname`, which in a container reports the image's idea of things.
pub fn modules_alias(host_root: &Path, proc: &Path) -> Result<PathBuf> {
    let osrelease = proc.join("sys/kernel/osrelease");
    let release = fs::read_to_string(&osrelease)
        .with_context(|| format!("read the kernel release from {}", osrelease.display()))?;

    Ok(host_root
        .join("lib/modules")
        .join(release.trim())
        .join("modules.alias"))
}

pub fn modprobe(host_root: &Path, module: &str) -> Result<()> {
    let modprobe = find_modprobe(host_root)?;

    let root = std::ffi::CString::new(host_root.as_os_str().as_bytes())
        .with_context(|| format!("{} contains a NUL", host_root.display()))?;
    let mut command = Command::new(&modprobe);
    command.arg(module);

    // SAFETY: chroot and chdir are async-signal-safe, all pre_exec allows.
    unsafe {
        command.pre_exec(move || {
            if libc::chroot(root.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chdir(c"/".as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let output = command.output().with_context(|| {
        format!(
            "run host {modprobe} for {module} after chroot to {} (needs root and CAP_SYS_CHROOT)",
            host_root.display()
        )
    })?;
    if output.status.success() {
        return Ok(());
    }

    bail!(
        "host modprobe failed for {module} ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// The stack every passthrough node needs up before anything can be claimed.
const VFIO_STACK: &[&str] = &["vfio", "vfio_iommu_type1"];

const MODULES_LOAD: &str = "etc/modules-load.d/kata-device-provisioner.conf";
const MODPROBE: &str = "etc/modprobe.d/kata-device-provisioner.conf";
const UDEV_RULES: &str = "etc/udev/rules.d/71-kata-device-provisioner.rules";

/// `driver_override` matches the driver name, modprobe the module name.
pub struct Claim {
    pub address: String,
    pub vendor: u16,
    pub device: u16,
    pub module: String,
    pub driver: String,
}

/// The variants register an `override_only` alias, so they match nothing until
/// something writes `driver_override`, which no modprobe file can do.
pub fn needs_driver_override(module: &str) -> bool {
    module.replace('-', "_") != "vfio_pci"
}

/// Keyed on the ids rather than an address, so the binding follows the card if
/// it moves slot.
///
/// A `driver_override` write neither loads the driver nor probes, and a module
/// already up when the event arrives attaches nothing, so the rule does both.
fn udev_rule(vendor: u16, device: u16, module: &str, driver: &str) -> String {
    format!(
        "ACTION==\"add\", SUBSYSTEM==\"pci\", ATTR{{vendor}}==\"0x{vendor:04x}\", \
         ATTR{{device}}==\"0x{device:04x}\", ATTR{{driver_override}}=\"{driver}\", \
         RUN{{builtin}}+=\"kmod load {module}\", \
         RUN+=\"/bin/sh -c 'echo %k > /sys/bus/pci/drivers_probe'\"\n"
    )
}

/// Make the kernel redo at boot what `bind` did by hand: `modprobe.d` hands
/// `vfio-pci` its ids as it loads, before another driver has looked at them,
/// and a udev rule carries whatever needs `driver_override`. Nothing runs as a
/// service and nothing gates the kubelet.
///
/// Returns the devices the rule is carrying.
pub fn persist(host_root: &Path, claims: &[Claim]) -> Result<Vec<String>> {
    let mut modules: Vec<&str> = VFIO_STACK.to_vec();
    let mut ids = Vec::new();
    let mut overridden = Vec::new();
    let mut rules = Vec::new();

    for claim in claims {
        // A module named twice under different punctuation is the same module.
        if !modules
            .iter()
            .any(|held| held.replace('-', "_") == claim.module.replace('-', "_"))
        {
            modules.push(&claim.module);
        }
        if needs_driver_override(&claim.module) {
            overridden.push(claim.address.clone());
            rules.push(udev_rule(
                claim.vendor,
                claim.device,
                &claim.module,
                &claim.driver,
            ));
        } else {
            ids.push(format!("{:04x}:{:04x}", claim.vendor, claim.device));
        }
    }
    ids.sort();
    ids.dedup();
    rules.sort();
    rules.dedup();

    write(
        &host_root.join(MODULES_LOAD),
        &format!("{}\n", modules.join("\n")),
    )?;

    let options = match ids.is_empty() {
        true => String::new(),
        false => format!("options {DRIVER_VFIO_PCI_TYPE} ids={}\n", ids.join(",")),
    };
    write(&host_root.join(MODPROBE), &options)?;

    let rules_path = host_root.join(UDEV_RULES);
    match rules.is_empty() {
        true => remove(&rules_path)?,
        false => write(&rules_path, &rules.concat())?,
    }

    Ok(overridden)
}

pub fn unpersist(host_root: &Path) -> Result<()> {
    for path in [MODULES_LOAD, MODPROBE, UDEV_RULES] {
        remove(&host_root.join(path))?;
    }

    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

/// Returned as the path looks *after* the chroot, not as it is mounted here.
fn find_modprobe(host_root: &Path) -> Result<String> {
    MODPROBE_CANDIDATES
        .iter()
        .find(|path| host_root.join(path.trim_start_matches('/')).is_file())
        .map(|path| (*path).to_string())
        .with_context(|| {
            format!(
                "no modprobe under {}: install kmod on the node",
                host_root.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};
    use std::fs;
    use tempfile::TempDir;

    #[fixture]
    fn host() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn add_file(root: &TempDir, path: &str) {
        let path = root.path().join(path.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
    }

    #[rstest]
    #[case::usr_sbin("/usr/sbin/modprobe")]
    #[case::sbin("/sbin/modprobe")]
    #[case::usr_bin("/usr/bin/modprobe")]
    #[case::bin("/bin/modprobe")]
    fn finds_modprobe_wherever_the_distribution_put_it(host: TempDir, #[case] path: &str) {
        add_file(&host, path);

        assert_eq!(find_modprobe(host.path()).unwrap(), path);
    }

    #[rstest]
    fn prefers_usr_sbin_when_several_exist(host: TempDir) {
        add_file(&host, "/bin/modprobe");
        add_file(&host, "/usr/sbin/modprobe");

        assert_eq!(find_modprobe(host.path()).unwrap(), "/usr/sbin/modprobe");
    }

    #[rstest]
    fn refuses_a_host_with_no_kmod(host: TempDir) {
        let err = find_modprobe(host.path()).unwrap_err().to_string();

        assert!(err.contains("install kmod"), "{err}");
    }

    #[rstest]
    fn a_directory_named_modprobe_is_not_a_modprobe(host: TempDir) {
        fs::create_dir_all(host.path().join("sbin/modprobe")).unwrap();

        assert!(find_modprobe(host.path()).is_err());
    }

    #[fixture]
    fn sysfs() -> Fake {
        testfs::fake()
    }

    #[rstest]
    fn loading_a_module_needs_a_modprobe_that_exists(host: TempDir) {
        let err = modprobe(host.path(), "vfio_pci").unwrap_err().to_string();

        assert!(err.contains("install kmod"), "{err}");
    }

    fn read(root: &TempDir, path: &str) -> String {
        fs::read_to_string(root.path().join(path)).unwrap()
    }

    fn claim(address: &str, device: u16, module: &str) -> Claim {
        Claim {
            address: address.to_string(),
            vendor: 0x10de,
            device,
            module: module.to_string(),
            driver: module.to_string(),
        }
    }

    #[rstest]
    fn persists_the_stack_and_the_ids_vfio_pci_should_claim(host: TempDir) {
        let devices = [
            claim("0000:65:00.0", 0x2330, "vfio_pci"),
            claim("0000:ca:00.0", 0x2330, "vfio_pci"),
            claim("0000:06:00.0", 0x22a3, "vfio_pci"),
        ];

        assert!(persist(host.path(), &devices).unwrap().is_empty());

        // One entry for vfio_pci, not one per spelling of it.
        assert_eq!(
            read(&host, MODULES_LOAD),
            "vfio\nvfio_iommu_type1\nvfio_pci\n"
        );
        // Two GPUs of the same model are one id, and the NVSwitch is there too.
        assert_eq!(
            read(&host, MODPROBE),
            "options vfio-pci ids=10de:22a3,10de:2330\n"
        );
    }

    /// A Grace GPU cannot be claimed from modprobe.d at all.
    #[rstest]
    fn claims_a_variant_device_through_a_udev_rule(host: TempDir) {
        let devices = [
            claim("0009:01:00.0", 0x2342, "nvgrace_gpu_vfio_pci"),
            claim("0000:65:00.0", 0x2330, "vfio_pci"),
        ];

        let overridden = persist(host.path(), &devices).unwrap();

        assert_eq!(overridden, ["0009:01:00.0"]);
        assert_eq!(
            read(&host, MODPROBE),
            "options vfio-pci ids=10de:2330\n",
            "a variant device's ids would hand it to the wrong driver"
        );

        assert_eq!(
            read(&host, MODULES_LOAD),
            "vfio\nvfio_iommu_type1\nnvgrace_gpu_vfio_pci\nvfio_pci\n",
            "the variant module has to be up for the override to land"
        );

        let rules = read(&host, UDEV_RULES);
        assert!(rules.contains(r#"ATTR{device}=="0x2342""#), "{rules}");
        assert!(
            rules.contains(r#"ATTR{driver_override}="nvgrace_gpu_vfio_pci""#),
            "{rules}"
        );
        assert!(rules.contains("drivers_probe"), "{rules}");
        assert!(
            !rules.contains("0x2330"),
            "vfio-pci needs no rule, modprobe.d claims it: {rules}"
        );
    }

    /// Pinned verbatim: this rule was validated on a GH200, so changing it
    /// means re-testing on hardware.
    #[rstest]
    fn the_rule_is_the_one_that_was_validated() {
        assert_eq!(
            udev_rule(
                0x10de,
                0x2342,
                "nvgrace_gpu_vfio_pci",
                "nvgrace_gpu_vfio_pci"
            ),
            "ACTION==\"add\", SUBSYSTEM==\"pci\", ATTR{vendor}==\"0x10de\", \
             ATTR{device}==\"0x2342\", ATTR{driver_override}=\"nvgrace_gpu_vfio_pci\", \
             RUN{builtin}+=\"kmod load nvgrace_gpu_vfio_pci\", \
             RUN+=\"/bin/sh -c 'echo %k > /sys/bus/pci/drivers_probe'\"\n"
        );
    }

    /// The two names are the same string for nvgrace, so nothing on that node
    /// would catch them being swapped.
    #[rstest]
    fn the_override_names_the_driver_and_the_load_names_the_module() {
        let rule = udev_rule(0x10de, 0x2342, "some_vfio_module", "some-vfio-driver");

        assert!(
            rule.contains(r#"ATTR{driver_override}="some-vfio-driver""#),
            "{rule}"
        );
        assert!(rule.contains("kmod load some_vfio_module"), "{rule}");
    }

    /// The rule keys on the device id rather than an address.
    #[rstest]
    fn writes_one_rule_per_device_id(host: TempDir) {
        let devices = [
            claim("0009:01:00.0", 0x2342, "nvgrace_gpu_vfio_pci"),
            claim("0019:01:00.0", 0x2342, "nvgrace_gpu_vfio_pci"),
        ];

        persist(host.path(), &devices).unwrap();

        assert_eq!(read(&host, UDEV_RULES).lines().count(), 1);
    }

    /// Listing it would load a driver at boot no device of its own can use.
    #[rstest]
    fn names_no_vfio_pci_when_no_device_asks_for_it(host: TempDir) {
        persist(
            host.path(),
            &[claim("0009:01:00.0", 0x2342, "nvgrace_gpu_vfio_pci")],
        )
        .unwrap();

        assert_eq!(
            read(&host, MODULES_LOAD),
            "vfio\nvfio_iommu_type1\nnvgrace_gpu_vfio_pci\n"
        );
        assert_eq!(
            read(&host, MODPROBE),
            "",
            "an ids= line here would hand the GPU to the wrong driver"
        );
    }

    /// No rule file at all, rather than an empty one.
    #[rstest]
    fn writes_no_rule_when_nothing_needs_an_override(host: TempDir) {
        persist(host.path(), &[claim("0000:65:00.0", 0x2330, "vfio_pci")]).unwrap();

        assert!(!host.path().join(UDEV_RULES).exists());
    }

    #[rstest]
    fn uninstall_removes_both_files_and_is_idempotent(host: TempDir) {
        let devices = [claim("0000:65:00.0", 0x2330, "vfio_pci")];
        persist(host.path(), &devices).unwrap();

        unpersist(host.path()).unwrap();
        unpersist(host.path()).unwrap();

        assert!(!host.path().join(MODULES_LOAD).exists());
        assert!(!host.path().join(MODPROBE).exists());
    }
}
