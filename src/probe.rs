// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! The `probe` subcommand: what a udev rule written by `persist` runs for one
//! device at boot, after the rule has set `driver_override` and loaded the
//! module. It writes the device to `drivers_probe` until the device has a
//! driver.
//!
//! One write at a time across all instances, under a lock: udev handles the
//! add events of a tray's GPUs in parallel, and two Grace GPUs of one socket
//! register the same EGM node, so of two probes that run at the same time the
//! second fails with EEXIST.
//!
//! Repeated, because the write does not report the probe: the kernel returns
//! success for a write whose probe failed, and the driver link is the only
//! result there is.

use std::fs;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use pcilibs_rs::{vfio, Sysfs};

/// Under `--host-root`, which is `/` where udev runs this. `/run` is there
/// before the first event.
pub const LOCK: &str = "run/kata-device-provisioner.lock";

/// The last moment at which a wait for the lock or a probe write starts. A
/// write that has started returns when the kernel's probe has finished, so a
/// run lasts at most this plus one probe. udevd kills a RUN program at its
/// event timeout, 180 s by default; ending well before that makes a device
/// that never binds a failed RUN with a reason in the udev log, not a kill.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// Between a write that did not bind and the next. Nothing in sysfs says when
/// the probe that made it fail has finished, so this is a pause, not a
/// measurement: 30 attempts fit the deadline.
pub const INTERVAL: Duration = Duration::from_secs(1);

/// `File::lock` has no timeout, and another device's probe can hold the lock
/// for seconds, so the lock is polled until the deadline.
const LOCK_POLL: Duration = Duration::from_millis(50);

pub fn run(
    sysfs: &Sysfs,
    address: &str,
    lock: &Path,
    deadline: Duration,
    interval: Duration,
) -> Result<()> {
    let end = Instant::now() + deadline;
    if sysfs.device(address).is_none() {
        bail!("{address} is not a PCI address");
    }
    // A device another driver holds counts as bound: the detach rule takes it
    // from that driver on its bind event, and a probe write would do nothing.
    let bound = || vfio::current_driver(sysfs, address).is_some();
    let lock = fs::File::create(lock).with_context(|| format!("open {}", lock.display()))?;

    while !bound() {
        acquire(&lock, end).with_context(|| format!("{address} has no driver"))?;
        let probed = fs::write(sysfs.drivers_probe(), address);
        lock.unlock().context("release the probe lock")?;
        probed.with_context(|| format!("write {address} to drivers_probe"))?;

        if !bound() {
            sleep(interval);
        }
    }

    Ok(())
}

fn acquire(lock: &fs::File, end: Instant) -> Result<()> {
    loop {
        if Instant::now() >= end {
            bail!("the deadline passed");
        }
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) => sleep(LOCK_POLL),
            Err(fs::TryLockError::Error(err)) => return Err(err).context("take the probe lock"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::thread::JoinHandle;
    use tempfile::TempDir;

    const ADDRESS: &str = "0000:65:00.0";
    const LIMIT: Duration = Duration::from_secs(5);

    /// `drivers_probe` is a FIFO the test reads: the test counts the probe
    /// writes, a write blocks while nothing reads, which is what a probe in
    /// flight in the kernel looks like, and the device gets its driver when
    /// the test decides.
    struct Node {
        fake: Fake,
        run: TempDir,
    }

    #[fixture]
    fn node() -> Node {
        let fake = testfs::fake();
        fake.add_driver("vfio-pci");
        fake.add_pci_device(ADDRESS, 0x10de, 0x2330, 0x030200, None);
        fs::remove_file(fake.sysfs.drivers_probe()).unwrap();
        let status = Command::new("mkfifo")
            .arg(fake.sysfs.drivers_probe())
            .status()
            .unwrap();
        assert!(status.success());

        Node {
            fake,
            run: tempfile::tempdir().unwrap(),
        }
    }

    impl Node {
        fn lock(&self) -> PathBuf {
            self.run.path().join("lock")
        }

        fn bind(&self) {
            self.fake.add_device(ADDRESS, Some("vfio-pci"));
        }

        fn probe(&self, deadline: Duration, interval: Duration) -> JoinHandle<Result<()>> {
            let (root, lock) = (self.fake.root().to_path_buf(), self.lock());
            std::thread::spawn(move || run(&Sysfs::new(&root), ADDRESS, &lock, deadline, interval))
        }

        /// Non-blocking, so a probe write completes at once.
        fn reader(&self) -> fs::File {
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(self.fake.sysfs.drivers_probe())
                .unwrap()
        }

        fn lock_is_held(&self) -> bool {
            let lock = fs::File::create(self.lock()).unwrap();
            matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock))
        }
    }

    /// The number of probe writes read since the last call.
    fn probes(reader: &mut fs::File) -> usize {
        let mut written = String::new();
        let mut chunk = [0u8; 256];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => written.push_str(std::str::from_utf8(&chunk[..n]).unwrap()),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("{err}"),
            }
        }
        written.matches(ADDRESS).count()
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < LIMIT, "{what}");
            sleep(Duration::from_millis(5));
        }
    }

    fn finish(probe: JoinHandle<Result<()>>) -> Result<()> {
        until("the probe did not finish", || probe.is_finished());
        probe.join().unwrap()
    }

    #[rstest]
    fn repeats_the_write_until_the_device_has_a_driver(node: Node) {
        let mut reader = node.reader();
        let probe = node.probe(LIMIT, Duration::from_millis(10));

        let mut written = 0;
        until("the probe was not repeated", || {
            written += probes(&mut reader);
            written >= 2
        });
        node.bind();

        finish(probe).unwrap();
    }

    /// udev logs a failed RUN for a device that never binds.
    #[rstest]
    fn fails_at_the_deadline_when_no_driver_appears(node: Node) {
        let mut reader = node.reader();
        let probe = node.probe(Duration::from_millis(300), Duration::from_millis(10));

        let err = finish(probe).unwrap_err();

        assert!(format!("{err:#}").contains("has no driver"), "{err:#}");
        assert!(probes(&mut reader) >= 2);
    }

    /// Nothing reads the FIFO, so the write blocks as a probe in the kernel
    /// does; another instance cannot take the lock until it has returned.
    #[rstest]
    fn holds_the_lock_while_a_probe_is_in_flight(node: Node) {
        let probe = node.probe(LIMIT, Duration::from_millis(10));
        until("the probe never took the lock", || node.lock_is_held());

        node.bind();
        let mut reader = node.reader();

        finish(probe).unwrap();
        assert_eq!(probes(&mut reader), 1);
        assert!(!node.lock_is_held(), "the lock outlived the probe");
    }

    /// Another device's probe holding the lock must not keep this one waiting
    /// into udevd's kill.
    #[rstest]
    fn fails_when_the_lock_is_held_past_the_deadline(node: Node) {
        let mut reader = node.reader();
        let holder = fs::File::create(node.lock()).unwrap();
        holder.lock().unwrap();

        let probe = node.probe(Duration::from_millis(200), Duration::from_millis(10));
        let err = finish(probe).unwrap_err();

        assert!(format!("{err:#}").contains("deadline"), "{err:#}");
        assert_eq!(probes(&mut reader), 0);
    }

    /// The management PF a host driver took first: its detach rule handles
    /// the bind event, and this run must not wait for a driver of its own.
    #[rstest]
    fn leaves_a_device_that_has_a_driver_alone(node: Node) {
        node.fake.add_driver("mlx5_core");
        node.fake.add_device(ADDRESS, Some("mlx5_core"));
        let mut reader = node.reader();

        finish(node.probe(LIMIT, Duration::from_millis(10))).unwrap();

        assert_eq!(probes(&mut reader), 0);
    }

    #[rstest]
    fn refuses_a_name_that_is_not_a_pci_address(node: Node) {
        let sysfs = Sysfs::new(node.fake.root());

        let err = run(&sysfs, "../65", &node.lock(), LIMIT, INTERVAL).unwrap_err();

        assert!(err.to_string().contains("not a PCI address"), "{err}");
    }
}
