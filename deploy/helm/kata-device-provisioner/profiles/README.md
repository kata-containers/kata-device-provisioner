# Profiles

Ready-made values for the hardware layouts this component is built for. They
differ in two things that matter — which nodes they select, and which CC mode
they ask for — and are otherwise the chart's defaults.

| Profile | Nodes | Mode |
| --- | --- | --- |
| [`HGX-Hx00`](HGX-Hx00.values.yaml) | HGX Hx00 (H100, H200, H800, H20) | `off` |
| [`HGX-Hx00-PPCIE`](HGX-Hx00-PPCIE.values.yaml) | HGX Hx00 | `ppcie`, whole board |
| [`HGX-Bx00`](HGX-Bx00.values.yaml) | HGX Bx00 (B200, B300) | `off` |
| [`HGX-Bx00-CC`](HGX-Bx00-CC.values.yaml) | HGX Bx00 | `on` — single-GPU and multi-GPU |

Profiles are per chip generation, not per SKU: `HGX-Hx00` matches any GH100
board (an H100, H200, H800 or H20 baseboard looks the same from PCI config
space) and `HGX-Bx00` matches any GB100/GB110 board (B200 or B300), so one
release covers a fleet mixing them without a profile per part number. The
baseboard is the same regardless of which OEM (Supermicro, Lenovo, ...)
builds it, so nothing here selects on that either.

GH200 and GB200/GB300 fall inside those same chip families but are explicitly
excluded from both — their CC mode is owned by system firmware, so a
`ppcie`/`cc` run would only reach that refusal after selecting the node.
They need a profile of their own, once one exists.

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
  --image=ghcr.io/kata-containers/kata-device-provisioner:0.1.0 \
  -- /kata-device-provisioner status --all --sysfs=/host/sys
```

`kubectl debug node/` puts the node's filesystem at `/host`, which is why the
sysfs flag is pointed there. Drop `--all` to see only the devices in scope.

A line per device:

```text
0000:0a:00.0  0x10de:2330  class=0x030200  nvidia-in-band-cc  chip=GH100  driver=<unbound>   iommu_group=14  numa=0  cc=capable  GH100 [H100 SXM5 80GB]
0000:06:00.0  0x10de:22a3  class=0x068000  bind-only          chip=-      driver=<unbound>   iommu_group=9   numa=0  cc=n/a      GH100 [NVSwitch]
0000:01:00.0  0x8086:1521  class=0x020000  out-of-scope       chip=-      driver=igb         iommu_group=3   numa=0  cc=n/a      I350 Gigabit Network Connection
```

Four fields decide everything:

- **`chip`** — the GPU generation `pcilibs_rs::cc` recognises, and the answer to "can
  this GPU do confidential computing". `GH100` is Hopper, `GB1xx` Blackwell.
  A `-` on a line that says `nvidia-in-band-cc` is a GPU too old for CC.
- **`nvidia-in-band-cc` / `bind-only` / `out-of-scope`** — what the device table
  says this component may do. `bind-only` is an NVSwitch under per-GPU CC: passed
  through, but it has no CC mode of its own. Under PPCIE the same device is a
  participant. `out-of-scope` devices are listed and never touched.
- **device id and NVSwitches** — a GH100 id (`2330`, ...) plus an NVSwitch is
  HGX Hx00; a GB100/GB110 id (`2901`, ...) plus an NVSwitch is HGX Bx00;
  NVIDIA GPUs and no NVSwitch is a node with add-in cards. A GH200 or
  GB200/GB300 id is neither: those chips sit inside the same families but are
  deliberately excluded — see below.
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
- **Anything not NVIDIA.** An AMD GPU or a passthrough-capable NIC has no row in
  the device table, so it is reported by `status --all` and never touched. It
  does not make the node mixed.

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

`nvidia-gpu` and `nvidia-nvswitch` are vendor and PCI class. `nvidia-hopper` and
`nvidia-blackwell` are chip generations, matched on the device-id ranges
`pcilibs_rs::cc`'s `CHIPS` table already tracks (GH100 for Hopper, GB100 and
GB110 for Blackwell) rather than one id per SKU, so a fleet mixing H100 and
H200, or B200 and B300, is one release. A generation this tree does not know
about yet means a node is not selected; the provisioner still reads the
hardware and refuses a mode the board cannot take.

Both rules carve the C2C ids back out of their range (`pcilibs_rs::cc::
C2C_DEVIDS`): GH200 out of Hopper, GB200 and GB300 out of Blackwell. Their CC
mode is owned by system firmware and cannot be raised in-band, so leaving them
in would mean a `ppcie`/`cc` run selects the node only to have the binary
refuse it. Excluding them here means that refusal never happens — `--mode off`
still finds and binds a GH200 or GB200/GB300 node, just not through these two
labels.
