// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! NFD's worker runs non-root with a read-only mount and cannot read VPD, so
//! the Job, which has verified the binding as root, writes the result where
//! NFD's `local` source reads it.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::modules;

const DIRECTORY: &str = "etc/kubernetes/node-feature-discovery/features.d";
const NAME: &str = "kata-device-provisioner";
const FABRIC: &str = "kata.feature.node.kubernetes.io/managed-fabric=true\n";

pub(crate) fn file(host_root: &Path) -> PathBuf {
    host_root.join(DIRECTORY).join(NAME)
}

pub fn remove(host_root: &Path) -> Result<()> {
    modules::remove(&file(host_root))
}

pub fn publish(host_root: &Path) -> Result<()> {
    // NFD re-reads a file whose inode or mtime changed, so a file that already
    // holds the fact is not rewritten. Only a regular file counts, so a
    // symlink at the path is replaced below and never followed.
    let path = file(host_root);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file())
        && fs::read(&path).is_ok_and(|current| current == FABRIC.as_bytes())
    {
        return Ok(());
    }

    let directory = host_root.join(DIRECTORY);
    fs::create_dir_all(&directory)
        .with_context(|| format!("create NFD directory {}", directory.display()))?;
    // The temporary name starts with a dot, which NFD skips, so NFD never
    // reads a partial file. It is removed on drop if any step below fails.
    let mut temporary = NamedTempFile::new_in(&directory)
        .with_context(|| format!("create a temporary file in {}", directory.display()))?;
    (|| -> Result<()> {
        temporary.write_all(FABRIC.as_bytes())?;
        // NFD's worker is not root and has to read the file.
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path)?;
        Ok(())
    })()
    .context("publish verified NFD fabric fact")
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
        publish(host.path()).unwrap();
        let path = file(host.path());
        let old = fs::File::open(&path).unwrap();
        fs::write(&path, "old-result=true\n").unwrap();
        publish(host.path()).unwrap();
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
    fn a_converged_run_leaves_the_snapshot_alone(host: TempDir) {
        use std::os::unix::fs::MetadataExt;
        publish(host.path()).unwrap();
        let path = file(host.path());
        let before = fs::metadata(&path).unwrap();
        publish(host.path()).unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(fs::read_to_string(path).unwrap(), FABRIC);
    }

    #[rstest]
    fn removes_only_our_snapshot(host: TempDir) {
        publish(host.path()).unwrap();
        let other = host.path().join(DIRECTORY).join("another-producer");
        fs::write(&other, "another-feature=true\n").unwrap();
        for _ in 0..2 {
            remove(host.path()).unwrap();
        }
        assert!(!file(host.path()).exists());
        assert!(other.exists());
    }

    #[rstest]
    fn replaces_a_destination_symlink_without_following_it(host: TempDir) {
        fs::create_dir_all(host.path().join(DIRECTORY)).unwrap();
        let victim = host.path().join("unrelated");
        fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, file(host.path())).unwrap();
        publish(host.path()).unwrap();
        assert_eq!(fs::read_to_string(victim).unwrap(), "unchanged");
        assert_eq!(fs::read_to_string(file(host.path())).unwrap(), FABRIC);
    }

    #[rstest]
    fn publication_failure_cleans_up_the_temporary_file(host: TempDir) {
        fs::create_dir_all(file(host.path())).unwrap();
        assert!(publish(host.path()).is_err());
        assert_eq!(
            fs::read_dir(host.path().join(DIRECTORY)).unwrap().count(),
            1
        );
        assert!(remove(host.path()).is_err());
    }
}
