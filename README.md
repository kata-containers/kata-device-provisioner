# kata-device-provisioner

Puts a node's passthrough devices into the state a Kata GPU workload expects —
confidential computing mode set, VFIO-bound, node labelled — and then exits.

It is the mutating half of the pair whose read-only half is
[kata-device-plugin](https://github.com/kata-containers/kata-device-plugin):
the provisioner *configures* the hardware, the plugin *declares* it. Together
they replace the NVIDIA GPU Operator operands (`nvidia-cc-manager`,
`nvidia-vfio-manager`, `nvidia-sandbox-device-plugin`) for Kata nodes.

## Why it exists

Today a Kata GPU node needs the GPU Operator for three things: set CC mode,
bind GPUs to `vfio-pci`, advertise them to the kubelet. The last one is
kata-device-plugin. This component covers the first two, in the same shape
kata-deploy uses to install Kata: a short-lived per-node Job, no always-on
privileged DaemonSet.

## What it does

```text
host-check -> discover -> verify idle -> resolve vfio drivers
           -> set CC mode -> reset -> write boot config -> bind -> verify
```

Each stage is idempotent and the whole run converges: a node already in the
desired state is a no-op.

The two changes it makes have different lifetimes, and the design turns on it.
CC mode lives in the GPU's EEPROM and survives reboots and power cycles. A VFIO
binding is kernel runtime state and does not survive anything. So the Job's real
product is persistent node configuration — it writes the node's own boot
configuration, and binds immediately as well so the node is usable without
waiting for a reboot:

```text
/etc/modules-load.d/kata-device-provisioner.conf   vfio, vfio_iommu_type1, vfio-pci
/etc/modprobe.d/kata-device-provisioner.conf       options vfio-pci ids=10de:2330,10de:22a3
/etc/udev/rules.d/71-kata-device-provisioner.rules for anything needing driver_override
```

That is the whole mechanism. No service runs at boot and nothing is ordered
against the kubelet: `vfio-pci` is handed the ids as it loads, which is before
any other driver has looked at those devices.

Which module a device needs is read from the running kernel's `modules.alias`
rather than decided here, so a Grace GPU gets `nvgrace_gpu_vfio_pci` and the
GPU beside it gets plain `vfio-pci` without either being named in this tree.
The variant drivers match on nothing but `driver_override`, which no modprobe
file can write, so those devices are carried by the udev rule instead.

The work is driverless. CC mode is set in-band over BAR0 and the FSP mailbox
via [`pcilibs_rs::cc`](https://github.com/kata-containers/pcilibs-rs), so the NVIDIA
kernel driver never has to be present, and PCI/VFIO truth is read from sysfs
via [`pcilibs-rs`](https://github.com/kata-containers/pcilibs-rs) — the same
crate the device plugin uses to classify what it advertises.

Which devices are in scope is one compile-time table keyed on PCI identity, so
supporting another accelerator is one row. There is no vendor abstraction on
purpose: TDISP devices (AMD SEV-TIO, Intel TDX Connect) are attested and locked
to a VM at bind time by the platform, so they need no node-level mode at all —
see [ARCHITECTURE.md](ARCHITECTURE.md).

## What it does not do

- It is not a controller. There is no reconcile loop, no watch, no CRD.
  A mode change is a `helm upgrade`, not a label edit picked up by a daemon.
- It does not run in the workload cluster's steady state. Privileged
  containers exist only while a node is being provisioned.
- It does not talk to Kubernetes. No client, no RBAC, no ServiceAccount token —
  node selection, pacing and labelling belong to
  [`k8s-job-dispatcher`](https://github.com/kata-containers/k8s-job-dispatcher),
  which does that API work from a single unprivileged pod.
- It does not advertise devices to the kubelet. That is kata-device-plugin.
- It does not install Kata. That is kata-deploy.

## Running it

The binary acts on the node it runs on.

```sh
cargo build --release

kata-device-provisioner status           # devices this node would provision
kata-device-provisioner status --all     # every passthrough-capable device
kata-device-provisioner status --probe   # read live CC mode (root, maps BAR0)

kata-device-provisioner apply --mode on  # provision this node
kata-device-provisioner uninstall        # remove the boot config
```

`uninstall` leaves the live bindings and CC mode alone. It only stops the node
from restoring the bindings after its next reboot.

Every kernel path is a flag (`--sysfs`, `--proc`, `--dev-vfio`,
`--host-root`), so the whole thing can be exercised against a directory tree
instead of a node.

The node needs an IOMMU enabled on the kernel command line and `kmod`
installed; `vfio-pci` is loaded for you on the first run, using the host's own
`modprobe`.

## Deploying it

Cluster-wide fan-out is one privileged, token-free Job per node via
[`k8s-job-dispatcher`](https://github.com/kata-containers/k8s-job-dispatcher):

```sh
helm install kata-device-provisioner deploy/helm/kata-device-provisioner \
  --namespace kata-system --create-namespace \
  --set ccMode=on \
  --set 'job.nodes={gpu-node-1}'
```

`job.nodes` names nodes outright, which is the quick way to try one machine.
Drop it and set `nodeSelector` to provision a fleet — an empty one selects every
node in the cluster, so select deliberately.
[`profiles/`](deploy/helm/kata-device-provisioner/profiles/README.md) has ready
-made values for HGX Hx00 and HGX Bx00 boards and for discrete PCIe cards, and
shows how to read a node before choosing between them.

A node can ask for a different mode with the label the GPU Operator already
uses, for the modes the release enables:

```sh
helm upgrade ... --set 'ccModeOverrides={ppcie}'
kubectl label node gpu-node-2 nvidia.com/cc.mode=ppcie
```

Each enabled mode gets a dispatcher run of its own, and `ccMode` covers the
rest. The label is read when a rollout starts, so editing it takes effect on the
next `helm upgrade` — nothing watches it.

`helm uninstall` removes the boot configuration and releases the devices; it
leaves CC mode alone, because reverting it costs a GPU reset per device.

The per-node Jobs are privileged and share the host's PID namespace, so on a
cluster that enforces Pod Security the namespace needs to allow it:

```sh
kubectl label namespace kata-system pod-security.kubernetes.io/enforce=privileged
```

See [`values.yaml`](deploy/helm/kata-device-provisioner/values.yaml) for the
rest.

## Status

Early, but the pipeline runs end to end. Every stage is implemented and tested
against a mocked sysfs, and the command-line path has been validated on real
hardware. All four modes including PPCIE are implemented, NVSwitches and all.

On an HGX H100 board: discovery, `cc=on` across all eight GPUs, and `ppcie` over
the GPUs and all four NVSwitches, each verified by re-reading the hardware, with
the mode and the binding surviving a cold reboot.

On a GH200: multi-domain PCI, refusal to raise a mode the Grace CPU cannot back, BAR0
on a coherent GPU, and `nvgrace_gpu_vfio_pci` resolved from the alias table and
re-bound across a cold boot by the generated udev rule — see
[ARCHITECTURE.md](ARCHITECTURE.md#variant-drivers-need-a-udev-rule-instead).

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — boundary, pipeline, decisions
- [CLAUDE.md](CLAUDE.md) / [AGENTS.md](AGENTS.md) — contributor and agent guidance
