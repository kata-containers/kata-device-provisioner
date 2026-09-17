# kata-device-provisioner

Rust. No Go. No exceptions.

## What this is

A one-shot, node-pinned Job that puts a node's passthrough GPUs into the state
Kata GPU workloads need — CC mode set, bound to `vfio-pci`, and still bound
after a reboot — and then exits. It is the mutating counterpart of
`kata-device-plugin`, which is read-only and only declares what already exists.

It has no Kubernetes client. Node selection, pacing and labelling are
[`k8s-job-dispatcher`](https://github.com/kata-containers/k8s-job-dispatcher)'s
job, which means the per-node Job carries no ServiceAccount token.

## Project layout

```text
src/
  main.rs    CLI (status / apply / uninstall) and stage order
  device.rs  vendor-neutral discovery + the DEVICES table (PCI identity ->
             provisioning kind + label scheme) -> one DeviceState per device
  host.rs    host-check: can this node do passthrough at all
  idle.rs    who has the device open, via /proc and /dev/vfio
  modules.rs loading a module with the host's modprobe, and the boot
             configuration that makes the node redo the binding at boot
  mode.rs    the node-level Mode enum (off | on | devtools | ppcie)
  nvidia.rs  NVIDIA in-band CC mode, reached only via Provisioning::NvidiaInBandCc
```

Supporting another accelerator is one row in `DEVICES`. Do not add a
`CcProvider` trait or a vendor plugin registry: the `Provisioning` enum is the
registry, and TDISP devices (SEV-TIO, TDX Connect) need no node-level mode at
all, so a trait shaped like NVIDIA's would be the wrong abstraction
(see ARCHITECTURE.md). Keep vendor specifics out of `device.rs`.

## Dependencies that carry the design

- [`pcilibs-rs`](https://github.com/kata-containers/pcilibs-rs) — the only
  hardware dependency, and all PCI/VFIO sysfs truth. Do not hand-roll sysfs
  reads that the crate already does, and do not fork its device classification:
  the provisioner and the device plugin must agree on what a device is.
- `pcilibs_rs::cc`, behind that crate's `cc` feature — CC and PPCIE mode query,
  set and reset, in-band over BAR0 and the FSP mailbox; no `nvidia.ko`. Never
  shell out to `nvidia-smi` or the Python `gpu-admin-tools`. Hardware changes
  belong there rather than here, and that code is MIT under NVIDIA's copyright
  inside an Apache-2.0 crate: keep the SPDX headers and say in a comment which
  `gpu-admin-tools` file each new register or knob id came from.
- The host's `modprobe` — the one thing this shells out to, to load `vfio-pci`
  on a node that has never run this. libkmod would tie the image to a build
  that understands the host's module compression; the host's own modprobe
  cannot get that wrong. `modules.rs` does what kata-deploy's
  `install_stage_load_kernel_modules` does, including the `chroot`.

## Principles

- **KISS** — no config file, no plugin registry, no trait objects. A stage is a
  function. The desired state arrives as CLI arguments templated into the Job.
- **Idempotent and converging** — every stage checks before it acts. Re-running
  on a provisioned node is a no-op that refreshes labels. Never reset a GPU that
  is already in the requested mode.
- **Fail loudly, never half-way** — a node is provisioned or it is labelled
  failed. Do not leave a node with some GPUs transitioned and some not, and do
  not paper over a hardware disagreement in `verify`.
- **Never touch a busy GPU** — mode changes reset the device. If a GPU may be in
  use by a VM, refuse.
- **A reboot must not need the cluster** — CC mode persists in the GPU's EEPROM,
  but a VFIO binding is kernel runtime state and is gone on every boot. Anything
  that binds must also write the boot configuration that makes the node redo
  it by itself, from the same device set, so the two cannot drift.
- **Persistence is the kernel's, not a service's** — `modules-load.d` brings the
  vfio stack up and `modprobe.d` hands `vfio-pci` its `ids=` as it loads, early
  enough that no other driver has looked at the devices. A device needing a
  variant driver gets a `udev` rule instead, since `driver_override` is the only
  way in. Nothing is ordered against the kubelet, because nothing needs to be:
  the devices are already vfio's before anything asks for them.
- **The kernel decides which module** — read it out of `modules.alias`, never
  from a device-id list here. A variant driver (`nvgrace_gpu_vfio_pci`) declares
  `override_only` aliases, so the alias table is the only thing that knows, and
  a list in this tree would go stale as the kernel adds ids.
- **Labels are output** — the provisioner reports what it achieved. Desired
  state comes from the release, not from accumulated `kubectl label` edits.

## Platform facts that constrain the code

- The node mode is one enum — `off | on | devtools | ppcie` — not
  `cc::CcMode`, which has no PPCIE variant. PPCIE is mutually exclusive with
  per-GPU CC in firmware and covers switches as well as GPUs, which is why the
  NVSwitch row is `NvidiaInBandPpcie` and why a transition is planned per device
  and applied board-wide.
- A board in PPCIE has per-GPU CC off, so it must not be labelled from the CC
  mode alone: that would report a protected node as not ready.
- On C2C parts (GH200, GB200) CC is owned by system firmware and cannot be set
  in-band. The CC stage verifies and reports there; it does not pretend to set.
  The same device ids also need `nvgrace_gpu_vfio_pci` rather than `vfio-pci`,
  which is what `cc::is_c2c` is used for on the binding side. That driver's
  module and driver names happen to be identical, unlike `vfio_pci`/`vfio-pci`,
  so neither form can be derived from the other.
- A VFIO-bound idle GPU is runtime-suspended in D3hot. `PciDev` wakes it via
  `power/control` and restores the policy, so changing the mode does *not*
  require unbinding first.
- The mode only becomes active after a reset. Setting without resetting is a
  bug, not an optimisation.

## Building and testing

```sh
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
```

Tests use `rstest`: node layouts are fixtures, hardware variation is cases.
sysfs is mocked with temp directories, so no test needs a GPU or a cluster.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — boundary, pipeline, decisions
- [AGENTS.md](AGENTS.md) — agent-specific constraints
