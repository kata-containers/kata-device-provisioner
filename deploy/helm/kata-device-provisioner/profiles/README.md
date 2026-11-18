# Profiles

Ready-made values for the hardware layouts this component is built for. They
differ in two things that matter — which nodes they select, and which CC mode
they ask for — and are otherwise the chart's defaults.

Tools such as KRAB can read the generated [`index.json`](index.json) instead of
parsing this page. It lists each deployable profile's GPU family, example model
names, CC mode, and values file. Model names are examples, not a guarantee of
end-to-end Kata support. `MIXED-FLEET` is an example, not a standalone profile.

The preset values files are written by hand. The generator combines their CC
modes and settings with the names and display details in
[`catalog.json`](catalog.json) to write `index.json`. After changing either
input, regenerate and commit the index:

```sh
cargo run --manifest-path tools/profile-index/Cargo.toml
```

CI does the same on every pull request and fails if the index is out of date.

| Profile | Nodes | Mode |
| --- | --- | --- |
| [`HGX-Hx00`](HGX-Hx00.values.yaml) | HGX Hx00 (H100, H200, H800, H20) | `off` |
| [`HGX-Hx00-PPCIE`](HGX-Hx00-PPCIE.values.yaml) | HGX Hx00 | `ppcie`, whole board |
| [`HGX-Bx00`](HGX-Bx00.values.yaml) | HGX Bx00 (B200, B300) | `off` |
| [`HGX-Bx00-CC`](HGX-Bx00-CC.values.yaml) | HGX Bx00 | `on` — single-GPU and multi-GPU |
| [`HGX-Rx00`](HGX-Rx00.values.yaml) | HGX Rx00 (Rubin SXM, coherent variants included) | `off` |
| [`PCIE-GPU`](PCIE-GPU.values.yaml) | discrete cards, no NVSwitch | `off` |
| [`PCIE-GPU-CC`](PCIE-GPU-CC.values.yaml) | discrete cards, no NVSwitch | `on`, per GPU |
| [`GBx00`](GBx00.values.yaml) | Grace-Blackwell superchips (GB200, GB300) | `off` |
| [`MIXED-FLEET`](MIXED-FLEET.values.yaml) | several of the above, in one release | per profile |

Profiles are per chip generation, not per SKU: `HGX-Hx00` matches any GH100
board (an H100, H200, H800 or H20 baseboard looks the same from PCI config
space), `HGX-Bx00` any GB100/GB110 SXM board (B200 or B300) and `HGX-Rx00` any
Rubin SXM board, so one release covers a fleet mixing them without a profile
per part number. The baseboard is the same regardless of which OEM (Supermicro,
Lenovo, ...) builds it, so nothing here selects on that either, and nothing
selects on CPU type.

What makes a node an HGX board is its fabric. Hx00 shows the NVSwitches as PCI
functions, so those profiles select on `nvidia-nvswitch`. Bx00 and Rx00 reach
the switches through the management ConnectX PFs, which NFD cannot tell from
the data-path NICs beside them, so those profiles select on
`nvidia-managed-fabric-candidate`, a GPU of the family beside an InfiniBand
ConnectX function, and the Job qualifies the PFs by VPD role on the node: an
SXM board whose ConnectX functions turn out to be ordinary NICs is selected and
then refused, never bound. Every HGX profile sets `bindFabric: true`, which
binds the whole fabric for a ServiceVM alongside the GPUs, in CC mode or not;
the mechanism is in the [README](../../../../README.md).

GH200 and GB200/GB300 fall inside the Hopper and Blackwell chip families but
are excluded from the HGX profiles: a Grace superchip is a different machine,
its GPUs coherently attached, and CC needs support from the CPU as well, which
Grace has not got. GB200/GB300 have [`GBx00`](GBx00.values.yaml) instead, which
binds for passthrough and leaves CC off. GH200 has no profile yet. Coherent
Rubin, by contrast, stays in `HGX-Rx00`: on Rubin, attachment does not decide
whether CC can be enabled.

Whether a GPU shares its IOMMU group with the data-path ConnectX PFs behind
the same PCIe switch depends on the kernel; where it does, those PFs must
leave `mlx5_core` before the Job will provision the GPUs.

```sh
helm install kata-device-provisioner deploy/helm/kata-device-provisioner \
  --namespace kata-system --create-namespace \
  -f deploy/helm/kata-device-provisioner/profiles/HGX-Hx00.values.yaml
```

To try one named machine before selecting nodes by label, add
`--set 'job.nodes={gpu-node-1}'` on top of any profile above. It overrides
`nodeSelector` outright and needs no NodeFeatureRule, and the per-node Job it
creates tolerates every taint, so nothing about the node's admission state can
quietly turn the run into a no-op.

A cluster holding more than one kind of GPU node takes one release with a
profile enabled per hardware class, each its own run with its own selection,
mode and pacing. [`MIXED-FLEET`](MIXED-FLEET.values.yaml) is that file with
every block commented out: uncomment the classes the fleet has, at most one
mode per class. Their selections cannot collide — the HGX profiles require a
fabric, direct switches or a managed-fabric candidate, the discrete-card ones
refuse both, and both exclude Grace superchips by device id — so the runs never
contend for a node.

## The modes, and NVIDIA's

The four modes and the labels around them are deliberately the GPU Operator's,
because kata-deploy's RuntimeClass selectors and every NVIDIA runbook already
key off them. NVIDIA's reference is
[Managing the Confidential Computing Mode](https://docs.nvidia.com/datacenter/cloud-native/confidential-containers/latest/configure-cc-mode.html).

| Mode | Hardware |
| --- | --- |
| `on` | Hopper and Blackwell. Single-GPU CC, and multi-GPU on Blackwell |
| `off` | Anything. Bound for passthrough, no CC |
| `devtools` | Hopper and Blackwell. CC with the debug paths left open — not for production |
| `ppcie` | Hopper only, multi-GPU. Whole baseboard, GPUs and NVSwitches together |

`nvidia.com/cc.mode` is the input, as it is for cc-manager, and
`nvidia.com/cc.mode.state` the applied result. Two differences are worth
knowing, because a runbook written against the GPU Operator will expect
otherwise:

- **`nvidia.com/cc.ready.state` mirrors the mode, not the per-device value the
  binary itself computes.** It is `false` for `off` and `true` for every other
  mode — correct, since a run that reached a mode other than `off` reset every
  device into it or failed the whole node, so readiness is a function of the
  mode alone by the time a node is labelled at all.
- **There is no `failed` state.** The dispatcher labels a node only once its Job
  succeeded, so a failed node keeps whatever it had before and the run itself
  fails. The Job's log is the error, not a label.

There is also no long-lived manager watching the label: a run converges the node
and exits. Where cc-manager tells you to make sure no workload is running before
you change a mode, this refuses the node itself if anything holds a GPU open.

## Look at the node first

The provisioner reads hardware; it does not need a driver, a cluster, or the
GPU Operator to do it. `status` reports what a run would act on and changes
nothing, so run it before deciding which profile you need:

```sh
kubectl debug node/gpu-node-1 -it --quiet \
  --image=ghcr.io/kata-containers/kata-device-provisioner:0.1.0-alpha.3 \
  -- /kata-device-provisioner status --all --sysfs=/host/sys
```

`kubectl debug node/` puts the node's filesystem at `/host`, which is why the
sysfs flag is pointed there. Drop `--all` to see only the devices in scope.

A line per device:

```text
0000:0a:00.0  0x10de:2330  class=0x030200  nvidia-in-band-cc  chip=GH100  driver=<unbound>   iommu_group=14  numa=0  cc=capable  GH100 [H100 SXM5 80GB]
0000:06:00.0  0x10de:22a3  class=0x068000  nvidia-in-band-ppcie chip=-      driver=<unbound>   iommu_group=9   numa=0  cc=n/a      GH100 [NVSwitch]
0000:01:00.0  0x8086:1521  class=0x020000  out-of-scope       chip=-      driver=igb         iommu_group=3   numa=0  cc=n/a      I350 Gigabit Network Connection
fabric: 0000:06:00.0
```

The last line is the NVSwitch fabric pcilibs-rs names on the node: the switch
chips on an Hx00 board, the management ConnectX PFs on a Bx00 or Rx00 board,
whose VPD role tells them from the data-path NICs that share their identity.

Four fields decide everything:

- **`chip`** — the GPU generation `pcilibs_rs::cc` recognises, and the answer to "can
  this GPU do confidential computing". `GH100` is Hopper, `GB1xx` Blackwell.
  A `-` on a line that says `nvidia-in-band-cc` is a GPU too old for CC.
- **Provisioning role** — what the device table says this component may do.
  `nvidia-in-band-cc` is a GPU, `nvidia-in-band-ppcie` a switch chip, which
  has no CC mode of its own and under PPCIE takes the board's, and
  `fabric-management` a ConnectX PF the scan qualified as the fabric's. An
  InfiniBand function the scan did not name is `out-of-scope`, like every
  other device without a row, and is never touched.
- **Fabric interface** — switch chips on PCI make an Hx00 board; management
  ConnectX PFs make a Bx00 or Rx00 board. GPU identity refines the family; an
  ordinary NIC does not establish an NVLink fabric.
- **`iommu_group`** — every device you intend to pass through needs one. If
  these are missing the IOMMU is off, and `apply` will refuse the node.

Add `--probe` to read each GPU's current CC mode from its firmware instead of
reporting `capable`. That maps BAR0 and needs a privileged pod, so it is worth
doing once you are past "which profile", not before.

## Heterogeneous nodes

**A node whose GPUs cannot all do CC is refused, as a whole, before anything is
touched.** Asking for `on`, `devtools` or `ppcie` on a node holding an H100 and
an A100 fails at the capability check, which runs across every device before the
first firmware write:

```text
mode on needs in-band CC support, which 1 of this node's 3 devices lack:
0000:65:00.0 (GA100 [A100 SXM4 80GB]). Provision this node with --mode off, or
leave it out of the run's node selection
```

Refusing is the point. Provisioning only the capable GPUs would reset them and
label the node `nvidia.com/cc.mode.state=on`, and a confidential workload
scheduled on that label could then land on the GPU that cannot do CC. Refusing
before mutating is what keeps a failed run from leaving the node in a state
neither you nor the cluster can describe.

Three things that are *not* heterogeneity in this sense:

- **Different CC-capable generations together.** An H100 and a B200 on one node
  are both settable, so the node provisions. Mode is per GPU.
- **NVSwitches.** They have no per-GPU CC mode; they are bound, and under PPCIE
  they take the board's mode.
- **Anything not NVIDIA.** An AMD GPU or an ordinary NIC has no row in the
  device table, so it is reported by `status --all` and never touched. A
  management ConnectX PF has one, but it is bind-only and needs no CC
  capability. Neither makes the node mixed.

For a fleet that really does have older GPUs on some nodes, give those nodes
`ccMode: off` — they still get bound to `vfio-pci` and are still usable for
non-confidential passthrough. A node labelled `nvidia.com/cc.mode=off` and a
release with that mode in `ccModeOverrides` is the same idea, per node.

## Why these labels and not `nvidia.com/gpu.*`

The `nvidia.com/gpu.product` and `gpu.count` labels come from GPU Feature
Discovery, which reads them through the NVIDIA driver. A node this component has
provisioned deliberately does not run that driver — the GPUs are bound to
`vfio-pci` — so those labels are absent exactly when you would want to select on
them, and a second `helm upgrade` would find nothing.

The `kata.feature.node.kubernetes.io/*` labels these profiles use come from PCI
config space via node-feature-discovery, so they survive binding. The chart
ships the NodeFeatureRule that produces them (`nodeFeatureRule.enabled`); NFD
itself has to be in the cluster already, which kata-deploy can arrange.

`nvidia-gpu` and `nvidia-nvswitch` match vendor and PCI class. The GPU-family
labels are generated from the PCI identity catalog in pcilibs-rs, the same one
the binary classifies devices with, keyed on device id and, where one id is
sold as several attachments, subsystem device id: `31c2` with an SXM subsystem
is HGX B300 and `nvidia-blackwell`, `31c2` with a coherent subsystem is GB300
and `nvidia-grace-blackwell`. The Hopper and Blackwell HGX labels take SXM
parts only; `nvidia-rubin` takes coherent variants too, because attachment does
not determine whether Rubin can enable CC. A part the catalog does not know is
a node that is not selected, never one that is mislabelled. After a pcilibs-rs
bump, regenerate the template with `UPDATE_NODE_FEATURE_RULE=1 cargo test
node_feature_rule`; `cargo test` fails while it is out of date.

`nvidia-managed-fabric-candidate` is an SXM Blackwell or Rubin GPU beside an
InfiniBand ConnectX function: a selection hint, never proof of a management
role, which only the Job's VPD read on the node can establish. `nvidia-c2c`
excludes coherent variants, Rubin's included, from the PCIe profiles; it is
the one label kept to device ids alone, mirroring the binary's `is_c2c`, so a
PCIe profile never selects a node whose GPU the binary would then refuse plain
`vfio-pci` for. `nvidia-grace-blackwell` is what `GBx00` selects. GH200 has no
label, because nothing selects on it yet.
