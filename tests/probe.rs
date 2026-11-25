// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! The command line the udev rules run, against the built binary. `persist`
//! writes `RUN+="<copy> probe %k"`; the unit tests of `probe::run` start
//! behind the argument parsing, and this is the only test of what is in front
//! of it.

use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use pcilibs_rs::testfs::{self, Fake};
use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_kata-device-provisioner");
const ADDRESS: &str = "0000:65:00.0";
const LIMIT: Duration = Duration::from_secs(5);

/// One unbound GPU whose `drivers_probe` is a FIFO the test reads, and a host
/// root with the `/run` the lock goes under.
fn node() -> (Fake, TempDir) {
    let fake = testfs::fake();
    fake.add_driver("vfio-pci");
    fake.add_pci_device(ADDRESS, 0x10de, 0x2330, 0x030200, None);
    fs::remove_file(fake.sysfs.drivers_probe()).unwrap();
    let status = Command::new("mkfifo")
        .arg(fake.sysfs.drivers_probe())
        .status()
        .unwrap();
    assert!(status.success());

    let host = tempfile::tempdir().unwrap();
    fs::create_dir(host.path().join("run")).unwrap();
    (fake, host)
}

fn probe(fake: &Fake, host: &Path, address: &str) -> Child {
    Command::new(BINARY)
        .args(["probe", address, "--sysfs"])
        .arg(fake.root())
        .arg("--host-root")
        .arg(host)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < LIMIT, "{what}");
        sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_rule_command_line_binds_the_device() {
    let (fake, host) = node();
    // Opened before the probe, non-blocking, so the probe write completes.
    let mut reader = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fake.sysfs.drivers_probe())
        .unwrap();
    let mut probe = probe(&fake, host.path(), ADDRESS);

    let mut written = Vec::new();
    until("no probe write arrived", || {
        let mut chunk = [0u8; 256];
        if let Ok(n) = reader.read(&mut chunk) {
            written.extend_from_slice(&chunk[..n]);
        }
        written == ADDRESS.as_bytes()
    });
    fake.add_device(ADDRESS, Some("vfio-pci"));

    until("the probe did not exit", || {
        probe.try_wait().unwrap().is_some()
    });
    let output = probe.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(host
        .path()
        .join("run/kata-device-provisioner.lock")
        .exists());
}

#[test]
fn a_name_that_is_not_a_pci_address_fails_at_once() {
    let (fake, host) = node();

    let output = probe(&fake, host.path(), "../65")
        .wait_with_output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a PCI address"), "{stderr}");
}
