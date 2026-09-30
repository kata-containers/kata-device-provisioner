// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Keep privileged discovery out of NFD's long-lived worker.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

const DIRECTORY: &str = "etc/kubernetes/node-feature-discovery/features.d";
pub(crate) const FILE: &str =
    "etc/kubernetes/node-feature-discovery/features.d/kata-device-provisioner";
const FABRIC: &str = "feature.node.kubernetes.io/managed-fabric=true\n";

pub fn remove(host_root: &Path) -> Result<()> {
    let path = host_root.join(FILE);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove NFD facts {}", path.display())),
    }
}

pub fn publish(host_root: &Path, managed_fabric: bool) -> Result<()> {
    if !managed_fabric {
        return remove(host_root);
    }

    let directory = host_root.join(DIRECTORY);
    fs::create_dir_all(&directory)
        .with_context(|| format!("create NFD directory {}", directory.display()))?;
    // NFD ignores dotfiles, so it cannot observe a partially written result.
    // Jobs often reuse PID 1; a crashed predecessor must not block publication.
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = directory.join(format!(
        ".kata-device-provisioner-{}-{nonce}",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("create NFD temporary file {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(FABRIC.as_bytes())?;
        // The non-root worker needs read access, never write access.
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        file.sync_all()?;
        fs::rename(&temporary, host_root.join(FILE))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.context("publish verified NFD fabric fact")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::{fixture, rstest};
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[fixture]
    fn host() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[rstest]
    fn replaces_a_complete_snapshot_readable_by_non_root(host: TempDir) {
        publish(host.path(), true).unwrap();
        let path = host.path().join(FILE);
        let old = fs::File::open(&path).unwrap();
        fs::write(&path, "old-result=true\n").unwrap();
        publish(host.path(), true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), FABRIC);
        assert_eq!(std::io::read_to_string(old).unwrap(), "old-result=true\n");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::read_dir(host.path().join(DIRECTORY)).unwrap().count(),
            1
        );
    }

    #[rstest]
    #[case::no_fabric(false)]
    #[case::uninstall(true)]
    fn removes_only_our_snapshot(host: TempDir, #[case] uninstall: bool) {
        publish(host.path(), true).unwrap();
        let other = host.path().join(DIRECTORY).join("another-producer");
        fs::write(&other, "another-feature=true\n").unwrap();
        for _ in 0..2 {
            if uninstall {
                remove(host.path()).unwrap();
            } else {
                publish(host.path(), false).unwrap();
            }
        }
        assert!(!host.path().join(FILE).exists());
        assert!(other.exists());
    }

    #[rstest]
    fn replaces_a_destination_symlink_without_following_it(host: TempDir) {
        fs::create_dir_all(host.path().join(DIRECTORY)).unwrap();
        let victim = host.path().join("unrelated");
        fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, host.path().join(FILE)).unwrap();
        publish(host.path(), true).unwrap();
        assert_eq!(fs::read_to_string(victim).unwrap(), "unchanged");
        assert_eq!(fs::read_to_string(host.path().join(FILE)).unwrap(), FABRIC);
    }

    #[rstest]
    fn an_interrupted_job_does_not_block_its_successor(host: TempDir) {
        let directory = host.path().join(DIRECTORY);
        fs::create_dir_all(&directory).unwrap();
        let stale = directory.join(format!(".kata-device-provisioner-{}-0", std::process::id()));
        fs::write(&stale, "partial").unwrap();
        publish(host.path(), true).unwrap();
        assert_eq!(fs::read_to_string(host.path().join(FILE)).unwrap(), FABRIC);
        assert_eq!(fs::read_to_string(stale).unwrap(), "partial");
    }

    #[rstest]
    fn publication_failure_cleans_up_the_temporary_file(host: TempDir) {
        fs::create_dir_all(host.path().join(FILE)).unwrap();
        assert!(publish(host.path(), true).is_err());
        assert_eq!(
            fs::read_dir(host.path().join(DIRECTORY)).unwrap().count(),
            1
        );
        assert!(remove(host.path()).is_err());
    }
}
