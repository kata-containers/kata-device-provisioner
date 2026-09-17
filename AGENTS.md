# Agent guidance for kata-device-provisioner

Read [CLAUDE.md](CLAUDE.md) first. This file adds agent-specific constraints on
top of it.

## Language and style

- **Rust only.** Never suggest Go, Python, or shell scripts for production code.
- One file per concern, flat. `main.rs` owns the CLI and stage order;
  `device.rs` owns vendor-neutral discovery and the `DEVICES` table;
  `nvidia.rs` owns everything NVIDIA. Add a file only when a concern is real,
  not in anticipation — a new vendor gets its own module and one table row.
- No comments that restate the code. Comment the WHY only when it is a hardware
  or security constraint that the code cannot show.

## Safety invariants — never violate these

- Never change CC mode or reset a GPU that could be in use. The idle check is
  not optional and must not be downgraded to a warning.
- Never leave a node half-provisioned. If a device fails, stop and report;
  do not continue with the remaining devices as if nothing happened.
- Never set a mode without the reset that activates it.
- Never bind a device without writing the boot configuration that rebinds it
  at boot. A binding that disappears on reboot is a node that silently stops
  working.
- Never write the boot configuration after binding. A crash between the two
  must leave a node that comes back bound, not one bound once and never again.
- Never resolve a device's vfio module from a device-id list in this tree. Read
  `modules.alias`: the variant drivers match on nothing but `driver_override`,
  so the kernel's table is the only authority on which module claims what.
- Never assume a device is bound because its module is loaded. A variant driver
  is `override_only` and will sit there loaded and idle.
- Never put a module name where a driver name belongs. `driver_override` and
  `drivers/pci:` take the driver, `modprobe` and `kmod load` take the module,
  and neither can be derived from the other by swapping punctuation.
- Never claim success without re-reading the hardware in `verify`.
- Do not add a reconcile loop, a controller-runtime dependency, or a CRD.
  This component runs, converges, and exits.
- Never touch a device with no `DEVICES` row. Out of scope means untouched,
  reported by `status --all` and nothing more.

## Do not

- Shell out to `nvidia-smi`, `gpu-admin-tools`, `lspci`, or `driverctl`.
  Use `pcilibs-rs`. The host's `modprobe`, run under a `chroot`
  into `--host-root`, is the one exception, and it is deliberate: see below.
- Replace that `modprobe` with libkmod or the `kmod` crate. Whether a `.ko.zst`
  loads would then depend on how the *container's* libkmod was built, and the
  image would have to carry libkmod plus its compression libraries. kata-deploy
  reached the same conclusion; `modules.rs` mirrors what it does.
- Add a Kubernetes client, RBAC, or anything that reads a ServiceAccount token.
  The API work is `k8s-job-dispatcher`'s, and this Job runs token-free on nodes
  that also host untrusted workloads.
- Reimplement node selection, pacing, per-node fan-out, or taint admission.
  That is `k8s-job-dispatcher`.
- Re-implement PCI sysfs parsing that `pcilibs-rs` already exposes, or fork its
  device classification.
- Add device *advertisement* of any kind — no kubelet socket, no CDI writing.
  That is `kata-device-plugin`'s job and the boundary is deliberate.
- Add Kata installation logic. That is `kata-deploy`'s job.
- Add `unwrap()` or `expect()` outside tests — propagate with `?` and `anyhow`.
- Add register offsets, PRC knob ids or chip device-id ranges here. That is
  `pcilibs_rs::cc`'s, and it is maintained rather than vendored, so changing it
  there is the expected move. None of it can be rediscovered from the hardware,
  so a new one gets a comment naming the `gpu-admin-tools` file it came from,
  and its MIT SPDX header stays put inside that otherwise Apache-2.0 crate.

## Testing

Hardware access is mockable: keep sysfs roots and device paths as parameters so
tests can point them at a temp directory, the way `pcilibs-rs` and the device
plugin's test suite do. No test may require a GPU or a cluster.

**Use `rstest` as much as possible.**

- Node layouts are `#[fixture]`s, never inline setup repeated across tests.
  Fixtures compose (`hopper_node(sysfs)`), so build the base tree once.
- Anything that varies over a table of hardware — PCI class, device id, driver,
  CC mode, chip generation — is `#[case]`s on one `#[rstest]`, not copy-pasted
  test functions. Name every case (`#[case::h100(...)]`) so a failure names the
  hardware.
- A plain `#[test]` needs a reason: it takes no fixture and has exactly one
  input.
