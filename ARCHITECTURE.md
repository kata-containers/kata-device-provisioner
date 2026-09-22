# Architecture

## Scope

`kata-device-provisioner` owns the *mutating* node-level GPU work that the
NVIDIA GPU Operator does today on Kata nodes: setting the confidential
computing mode of the GPUs and binding them to `vfio-pci`, in a way that
survives a reboot. It runs as a one-shot, node-pinned Job and exits.

Everything it touches is host state that must be settled *before* a Kata VM
can be started with a GPU. Nothing it does is per-pod.

## The components

| Component | Lifetime | Privilege | Owns |
| --- | --- | --- | --- |
| `kata-device-provisioner` | one-shot Job per node | privileged, host PCI, **no API access** | CC mode, VFIO binding, the node's boot configuration |
| `k8s-job-dispatcher` | one-shot pod per rollout | unprivileged, API only | node selection, pacing, labels, taints |
| `kata-deploy` (job mode) | one-shot Job per node | privileged, host filesystem | Kata artifacts, CRI config, runtime labels |
| `kata-device-plugin` | DaemonSet | unprivileged, `/dev/vfio` read-only | advertising bound devices to the kubelet |

Two splits are at work. The first is the one the device plugin's architecture
draws: *declaring* what exists is cheap and safe and can live next to workloads;
*reconfiguring* the infrastructure is neither, so it is confined to a short-lived
Job that is gone by the time workloads run.

The second is along privilege: the component that touches the hardware holds no
Kubernetes credentials, and the component holding Kubernetes credentials never
touches a host.

These are separate charts. Composition (one `helm install` that brings up
a GPU-capable Kata node) belongs in an umbrella chart or a values profile, not
in a subchart dependency of any one of them: GPU firmware transitions must not
be coupled to Kata runtime upgrades, and non-GPU users must not inherit GPU
machinery.

## Pipeline

One per-node Job running `apply` once:

```text
host-check -> discover -> verify-idle -> resolve-drivers
           -> cc-mode -> reset -> persist -> bind -> verify
```

The stages are functions in one process, not ordered `initContainers`. Splitting
them would make each container rediscover the node and would put a container
boundary in the middle of the set-and-reset pair, which has to be atomic.

| Stage | What it does |
| --- | --- |
| `host-check` | Refuse a node that cannot do passthrough at all: no IOMMU groups in sysfs, or an NVIDIA host driver holding a GPU. Neither is fixable from a pod without a reboot, so they fail early and loudly. It says nothing about vfio drivers, since which one a device needs is `resolve-drivers`' answer |
| `discover` | Enumerate devices from sysfs, classify against the `DEVICES` table (`pcilibs-rs`), read current driver and CC mode |
| `verify-idle` | Refuse to touch a device anything holds open, found by scanning `/proc/*/fd` for the device's `/dev/vfio` group node or cdev. Every device is checked before any device is touched, so a busy node is refused whole rather than half-transitioned |
| `resolve-drivers` | Per device, read its vfio module out of the running kernel's `modules.alias`, load it, and resolve the driver name it registers. Before anything is mutated: a node whose kernel has no driver for one of its GPUs must be refused before a GPU is reset, not after |
| `cc-mode` | Persist the desired CC mode in the GPU's EEPROM (`pcilibs_rs::cc`) |
| `reset` | Function-level reset; the new mode only takes effect after it |
| `persist` | Write the node's `modules-load.d` and `modprobe.d` configuration, plus a `udev` rule for anything that binds only through `driver_override`, so the binding survives a reboot |
| `bind` | `driver_override` + probe, so the node is usable now without waiting for one |
| `verify` | Re-read the mode and the binding from the hardware; the run fails loudly if it disagrees |

The label that follows a successful run is written by the dispatcher, not by
this component — see below.

Uninstall removes only the boot configuration. Live bindings and CC mode are
deliberately left alone — see the decisions below.

### Ordering

CC mode is set before binding. The reset that activates the mode must not race
a VFIO consumer, and a GPU that has just been reset should be handed to
`vfio-pci` in a known-good state.

Because `pcilibs_rs::cc` is driverless (BAR0 + FSP mailbox, no `nvidia.ko`), an
already-VFIO-bound idle GPU does not need to be unbound to change its mode —
it is runtime-suspended in D3hot, and the tool wakes it via `power/control` and
restores the policy afterwards. This is a real simplification over the GPU
Operator's `cc-manager`, which unbinds, flips, resets and rebinds. Re-running
the provisioner on an already-provisioned node therefore does not have to
disturb the binding.

## Decisions

### A Job, not an operator

CC mode changes are rare, disruptive, and node-wide. A reconcile loop invites
a label edit to reset a GPU under a running workload. Making the transition an
explicit `helm upgrade` keeps the blast radius visible and matches how the
fleet is already provisioned (kata-deploy job mode). No CRD, no watch, no
always-on privileged pod.

### It runs in the workload cluster

Wherever the GPUs are is where this runs, and on a Kata GPU fleet that is the
workload cluster. An admin-cluster-only design would be cleaner on paper and
would not describe any cluster anyone has.

That means accepting a privileged, host-PCI pod on a node that also runs
untrusted workloads, so the exposure is cut down instead of wished away:

- it exists only while a node is being provisioned, and nothing is left running
  afterwards;
- it carries **no ServiceAccount token**, because all API work is the
  dispatcher's (below), so compromising it yields host PCI access on a node the
  attacker is already on, not cluster credentials;
- workloads are kept off the node until provisioning succeeded, via the
  start-up taint the dispatcher lifts only after the Job passed.

**Known inconsistency:** kata-device-plugin's `ARCHITECTURE.md` says VFIO
binding happens in an admin cluster and never in the workload cluster. That was
written before this component existed and the two documents now disagree. This
one is the current position; the plugin's boundary section needs updating to
match.

### Dispatch is not ours

Fanning out one Job per node — selection, pacing, taint admission, per-node
coverage, a single exit code — is
[`k8s-job-dispatcher`](https://github.com/kata-containers/k8s-job-dispatcher),
already used by kata-deploy's job mode and published at
`ghcr.io/kata-containers/k8s-job-dispatcher`. This chart supplies a Job template
and selection flags; it does not reimplement any of that.

The consequence that matters most here is the privilege split. The dispatcher
does the node-scoped API work — claiming the node, waiting for `Ready`, writing
the label, lifting start-up taints — from a single unprivileged pod. So **the
per-node Job needs no ServiceAccount token at all**, and this binary needs no
Kubernetes client, no RBAC, and no API access. A privileged pod with host PCI
access sitting on a node that also runs untrusted workloads is exactly where a
mounted token is worth deleting rather than guarding.

### Labels are output, not input

The provisioner reports what it achieved; nothing watches a label to decide what
to do. Desired state comes from chart values (a cluster default plus per-node
overrides resolved at dispatch time), so a node's configuration is reproducible
from the release, not from accumulated `kubectl label` history.

Because the desired mode comes from the release, the label *value* is known
before the Job runs, and the Job's exit code is the proof that the hardware
reached it. That is exactly the dispatcher's `--node-label-key` /
`--node-label` contract: it writes `nvidia.com/cc.mode.state=<desired>` only
once the Job succeeded as a whole and the node came back `Ready`. A node is
therefore never labelled from inside a pipeline that might still fail after
writing the label.

The label *names* stay compatible with what the GPU Operator publishes
(`nvidia.com/cc.mode.state`, `nvidia.com/cc.ready.state`), because kata-deploy's
RuntimeClass `nodeSelector`s and the existing NVIDIA GPU documentation already
key off them.

### `nvidia.com/cc.mode` is an input — read by selection, not by a watcher

`kubectl label node <node> nvidia.com/cc.mode=on` is the workflow operators
already know from the GPU Operator, and it is worth keeping. It does not need a
controller, because a node label is already how the dispatcher chooses nodes.

The chart renders one dispatcher run per mode. Each selects the nodes carrying
that mode and hands its per-node Jobs the matching `--mode`, with the cluster
default covering nodes that carry no label:

| Selector | Job argument | Label written on success |
| --- | --- | --- |
| `nvidia.com/cc.mode=on` | `--mode=on` | `nvidia.com/cc.mode.state=on` |
| `nvidia.com/cc.mode=ppcie` | `--mode=ppcie` | `nvidia.com/cc.mode.state=ppcie` |
| `nvidia.com/cc.mode=off` | `--mode=off` | `nvidia.com/cc.mode.state=off` |

So the label is an input to *selection*, resolved once when a rollout starts,
and never read by anything long-lived. "Labels are output" still holds for the
`.state` labels, which remain a report of what the hardware actually did.

Editing the label does not by itself do anything — a `helm upgrade` or the
scheduled run is what acts on it. That pairs well with `--skip-satisfied-nodes`:
a node relabelled from `on` to `off` no longer carries the `off` run's finished
value, so it is not skipped and gets transitioned, while nodes already in their
requested mode are passed over. Convergence falls out of the selection rather
than needing a reconcile loop.

### Two mutations, two lifetimes

The component makes two changes to a node and they do not survive the same
things, which is the single most important fact about its design:

| Change | Where it lives | Survives reboot |
| --- | --- | --- |
| CC mode | the GPU's own EEPROM, activated by a function-level reset | **yes**, and power cycles too |
| VFIO binding | kernel runtime state | **no**, gone on every boot |

CC mode is genuinely one-shot: set it, reset the GPU, and it stays until
something sets it again. NVIDIA's own deployment guidance is explicit that the
setting persists and has to be reverted deliberately.

VFIO binding is not. Nothing in a `driver_override` write outlives the kernel
that received it, so a node that reboots comes back with its GPUs unbound, no
`/dev/vfio/devices/vfio*`, and a device plugin correctly advertising zero
capacity. A one-shot Job cannot fix that by running harder: the fix has to be
config the node applies to itself at boot.

So the Job's product is **persistent node configuration**, not a transient
action. `persist` lays down the node's boot configuration; `bind`
additionally does it now so the node is usable without waiting for a reboot.
Both work from the same discovered device set, so what happens at boot cannot
drift from what happened at provisioning time.

This is the same shape as kata-deploy, which also installs artifacts and CRI
configuration that outlive the Job that wrote them. "Nothing keeps running on
the node" was always about *processes*, never about *state*.

#### Why not a per-boot Job

A node that reboots keeps its label, so the dispatcher's `--skip-satisfied-nodes`
will pass it over on the next run, and a periodic `CronJob` rollout will not
repair it. That is correct behaviour for a flag that means "already configured"
— and it is precisely why boot-time binding has to be self-sufficient rather
than something the cluster comes back to fix. The scheduled dispatcher run is
for nodes that *joined*, not for nodes that *rebooted*.

#### What boot-time binding must not depend on

The NVIDIA host driver must not be installed on a provisioned node. With no
`nvidia.ko` to claim the GPU first, binding at boot is binding an unbound
device, and the whole class of initramfs ordering workarounds that VFIO
passthrough guides are full of does not apply. `host-check` enforces this rather
than assuming it, and `nouveau` is checked alongside it.

That is what makes the mechanism as small as it is: a couple of configuration
files and no code running at boot at all, decided below.

### Everything sysfs and CC comes from `pcilibs-rs`

`pcilibs-rs` provides PCI enumeration, driver and IOMMU-group reads, VFIO
driver-type matching and the passthrough-capable class predicate — the same
classification the device plugin uses, so provisioner and plugin cannot disagree
about what a device is. Its `cc` module is a port of the CC subset of NVIDIA's
`gpu-admin-tools`, register-for-register, needing no kernel driver, which is
exactly right for a passthrough node where `nvidia.ko` is never loaded.

This component therefore has one hardware dependency, not two.

CC mode used to live in a crate of its own here, on the reasoning that a
vendor-specific firmware protocol has no business in a PCI enumeration library.
What that reasoning missed is that most of the port is not vendor-specific:
mapping BAR0, resetting a function through sysfs and forcing a device out of
runtime suspend are plain PCI operations, and keeping them in a separate crate
meant maintaining a second PCI device type that `pcilibs-rs` would eventually
need anyway. They are now `pcilibs_rs::PciDev`, usable by the device plugin, and
only the NVIDIA registers and knob ids sit above them in `pcilibs_rs::cc`.

The `cc` module is behind a Cargo feature, off by default, so a consumer that
only enumerates devices neither compiles it nor takes on its dependencies. It is
MIT under NVIDIA's copyright inside an otherwise Apache-2.0 crate, which is why
the per-file SPDX headers and the provenance notes on each register matter: they
are what keeps the boundary legible after the directory stops being a separate
crate.

Binding lives there too, as `pcilibs_rs::vfio`: `driver_override`,
`drivers_probe`, the alias-table lookup that names a device's variant module and
the module-name-is-not-driver-name resolution. It was here first, on the
reasoning that refusing a device another driver holds is this component's policy
rather than a library's, but the refusal is one branch in code that is otherwise
pure kernel mechanism, and the device plugin binds devices too. A caller that
wants the device anyway unbinds first.

What is left here is the part that is genuinely this component's: which mode a
node should be in, which devices are in scope, and writing the boot
configuration that gets the node back there by itself.

### Modules are loaded with the host's modprobe, not libkmod

A node that has never run this may not have its vfio driver registered, and
telling the operator to load it themselves makes a first run fail for a reason
the Job could have fixed. So `resolve-drivers` loads it — by finding the host's
`modprobe` under `--host-root` and `chroot`ing into that root before exec'ing
it, which is what kata-deploy's `install_stage_load_kernel_modules` does.

Loading it *there*, per device, rather than loading `vfio-pci` up front, is the
difference between a node that ends up with what its own hardware needs and one
that is also handed a driver it can never use. A GH200 has no use for `vfio-pci`
and must not be told to load it.

libkmod, or the `kmod` crate that binds it, looks like the tidier answer and is
not. Module compression is the problem: whether a `.ko.zst` loads depends on how
the *container's* libkmod was compiled, so an image without zstd support fails
on any recent distribution, while the host's own modprobe always matches its own
kernel. It would also mean carrying libkmod, libzstd and liblzma in an image
that is otherwise static.

The cost is a `chroot`, so the Job needs the host root mounted and
`CAP_SYS_CHROOT`, and the node needs kmod installed — the same requirements
kata-deploy already places on it. `persist` still writes
`/etc/modules-load.d/kata-device-provisioner.conf`, so later boots do not depend
on any of this.

### Other vendors are a table row, not an abstraction

Most of the pipeline is vendor-neutral already: `host-check`, `discover`,
`verify-idle`, `persist`, `bind` and `verify` are PCI sysfs and VFIO, and
`pcilibs-rs` does not care that `0x10de` is NVIDIA. Only `cc-mode` and the label
names are vendor-specific.

So the extension point is one compile-time table keyed on PCI identity — the
same shape as kata-device-plugin's `RESOURCES` — and supporting another
accelerator is one row:

```rust
pub const DEVICES: &[DeviceRow] = &[
    // vendor, class prefix,  provisioning,               labels
    //  0x10de,      0x0302,  NvidiaInBandCc,             nvidia.com/cc.*
    //  0x10de,      0x0680,  NvidiaInBandPpcie,          -
];
```

Devices with no row are out of scope: reported by `status --all`, never
touched. The rows are deliberately the identities the device plugin matches on,
so provisioner and plugin cannot disagree about what a device is.

There is no `trait CcProvider`. A trait extracted from the single NVIDIA
implementation would encode a transitional design as the interface, because the
industry is not converging on what NVIDIA does today:

- NVIDIA's CC mode is a pre-TDISP arrangement. H100 shipped before TDISP
  existed, so the mode is persisted in the GPU's own firmware and activated by
  a reset — inherently node-level, ahead of any VM.
- AMD [SEV-TIO](https://www.amd.com/content/dam/amd/en/documents/developer/sev-tio-whitepaper.pdf)
  and Intel TDX Connect both implement PCI-SIG TDISP, where the device is
  attested and locked to one VM at TDI bind time by the platform's TSM (the AMD
  ASP, the TDX module) through the kernel's `PCI/TSM` subsystem. There is no
  node-level mode for this component to set at all, so such a device needs no
  `Provisioning` variant: a row with none is bound and left alone.

The honest vendor axis is therefore *what node-level provisioning does this
device need*, which is a per-row fact, not a per-vendor interface. If a second
in-band implementation ever appears, extract the trait from two real callers
then.

For the same reason, the CC fields on the discovered device state are
NVIDIA-shaped and populated only through the `NvidiaInBandCc` arm. Generalising
them now would mean guessing at the shape of an implementation that, on current
evidence, will not exist.

### Grace superchips are a verify-only platform

On C2C parts (GH200, GB200) confidential computing takes more than the GPU: the
CPU has to support it too, and Grace does not. `pcilibs_rs::cc` refuses to
enable CC on those ids, matching `gpu-admin-tools`, so this component never
*sets* a mode there — there is no mode on that node to set. The provisioner
still discovers, verifies, binds and labels, and refuses the node outright if
the release asks for anything but `off`.

This matters because GB200 NVL72 is a primary target of the device plugin.
The provisioner must be honest that its CC stage is a no-op there rather than
silently appearing to succeed.

### All GPUs on a node share one mode

NVIDIA does not support a per-GPU mix, and a partially-transitioned node is a
scheduling hazard. The unit of provisioning is the node: either every GPU
reaches the requested mode, or the node is labelled failed and excluded.

### PPCIE is in scope

Protected PCIe is how Hopper does multi-GPU confidential passthrough: the
NVSwitches are claimed exclusively by a single CVM, which makes it the mode any
HGX H100 node running multi-GPU CC workloads needs. Blackwell uses NVLink
encryption and keeps the switches outside the TCB, so it wants plain `on` — but
Hopper fleets are real and excluding them would mean the GPU Operator stays
installed for exactly one mode.

PPCIE is mutually exclusive with per-GPU CC mode — enabling CC clears the PPCIE
knob — so the node's mode is one enum, not two flags:

```text
off | on | devtools | ppcie
```

That is *our* type, not `cc::CcMode`, which has no PPCIE variant. It is a
node-level mode covering GPUs and switches together, which is why the NVSwitch
row is `NvidiaInBandPpcie` rather than bind-only: in PPCIE the switch is a
device whose mode is set, not merely bound.

Implementing it took two additions to `pcilibs_rs::cc`, both now done there:

1. **Switch support.** `CHIPS` held GPU device-id ranges only, so `Gpu::open`
   refused an NVSwitch outright. Switches are identified by `NV_PMC_BOOT_0` and
   have their own boot-complete register, but reach the FSP over the same EMEM
   channel Hopper uses, so the existing transport was reusable.
2. **A PPCIE mode API.** `KNOB_PPCIE` (45) was only ever *cleared* when enabling
   CC. Setting and querying it are public, on both GPUs and switches.

Because the two knobs are mutually exclusive in firmware, a transition is
planned as an ordered set of writes per device and applied board-wide: every
participating device is opened before anything is written, only devices whose
mode actually changes are reset, and all of them are verified afterwards. A
board part-way between the two modes is not a state the node can be left in.

Labelling follows the same logic. A node in PPCIE has per-GPU CC *off*, so
reporting the CC mode alone would label a protected board as not ready; the
PPCIE state takes precedence when it is on.

### Binding persists through the kernel's own mechanisms, with nothing running at boot

The durable half of a run is a couple of configuration files:

```text
# /etc/modules-load.d/kata-device-provisioner.conf
vfio
vfio_iommu_type1
vfio-pci

# /etc/modprobe.d/kata-device-provisioner.conf
options vfio-pci ids=10de:2330,10de:22a3
```

`ids=` is `vfio-pci`'s own parameter for the `new_id` mechanism: the driver
registers a dynamic PCI match for each id as the module loads, and the devices
are its own from that moment. So the binding is re-established by the module
load itself, before userspace has started, with no service, no unit ordering, no
binary installed on the host and nothing for an operator to debug when it works.

This is only sound because the NVIDIA host driver is absent, which `host-check`
enforces: `ids=` claims a device that nothing else wanted, rather than racing a
driver that already has it. On a node with `nvidia.ko` installed this mechanism
would lose the race, and so would every other mechanism short of the kernel
command line.

It also means the node needs no help from the cluster to come back, which is the
invariant the whole persistence question exists to serve. A rebooted node keeps
its label, is skipped by `--skip-satisfied-nodes`, and is never revisited — see
[Why not a per-boot Job](#why-not-a-per-boot-job) — so anything that needed a
scheduled run to repair it would be broken by design.

#### Variant drivers need a udev rule instead

Grace GPUs need `nvgrace_gpu_vfio_pci` rather than `vfio-pci`, and it has no
`ids=` to give. That is not an omission. Every variant vfio driver declares its
`id_table` with `PCI_DRIVER_OVERRIDE_DEVICE_VFIO`, which sets `override_only` on
each entry, so the driver never matches through normal PCI probing at all. It
binds only when the device's `driver_override` already names it, which is why it
has no module parameters: there would be nothing for them to do.

The aliases show it directly — `vfio_pci:` where an ordinarily-matching driver
would have `pci:`:

```text
mlx5-vfio-pci   vfio_pci:v000015B3d0000101Esv*sd*bc*sc*i*
vfio-pci        vfio_pci:v*d*sv*sd*bc*sc*i*
```

`vfio-pci`'s catch-all is the same shape because it is the fallback for any
override; `ids=` is what gives it *ordinary* matches in addition.

So `driver_override` is runtime state that no modprobe file can carry, and those
devices get a udev rule instead — kernel-side, no service, and the standard
mechanism for exactly this:

```text
ACTION=="add", SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{device}=="0x2342",
  ATTR{driver_override}="nvgrace_gpu_vfio_pci",
  RUN{builtin}+="kmod load nvgrace_gpu_vfio_pci",
  RUN+="/bin/sh -c 'echo %k > /sys/bus/pci/drivers_probe'"
```

Every clause is load-bearing. Writing `driver_override` neither loads the named
driver nor triggers a probe, so the rule has to do both itself, and `kmod load`
alone does not suffice: if the module is already resident when the event
arrives, loading it attaches nothing, which leaves the explicit `drivers_probe`
doing the real work. The match is on the device ids rather than the address, so
two identical cards share one rule and a card that moves slot still gets
claimed.

`driver_override` is matched against the *driver* name while `kmod load` takes
the *module* name; the rule is generated from both, resolved separately.

This was confirmed across a cold boot on a GH200: the GPU came back bound to
`nvgrace_gpu_vfio_pci` with the override in place, from the rule alone.

#### Which module a device needs is the kernel's answer, not ours

`modules.alias` is read at run time and the module name comes out of it. A list
of Grace device ids in this tree would be a second, worse copy of a table the
kernel already ships and extends every release, and it would be consulted on
exactly the nodes where being wrong is least recoverable.

The module name is then resolved to a driver name through
`/sys/module/<module>/drivers/pci:<driver>`, because the two need not agree:
`modprobe` wants `vfio_pci` where `driver_override` wants `vfio-pci`. They do
agree for `nvgrace_gpu_vfio_pci`, which is why the punctuation cannot be guessed
at in either direction.

A device with no alias falls back to `vfio-pci`, which is right for every
ordinary PCIe GPU, so the table's silence cannot be an error in itself. It would
be quietly wrong for a coherently attached GPU, which would be handed to a VM
with its memory missing, so those are refused instead — the one place a device
id list in this tree is unavoidable, since the whole point is to catch the alias
table not knowing.

`pcilibs_rs::cc::is_c2c` carries that list, from NVIDIA's own `has_c2c`, and it
is deliberately wider than the kernel driver's device table: upstream claims
five ids where NVIDIA marks eight as coherent, so a part can be coherently
attached before any released kernel will bind it. The refusal therefore does not
claim the kernel is too old — it may simply not claim that SKU — and says only
that no driver on this node can map the GPU's memory.

NVIDIA keys `has_c2c` on (device, subsystem device) pairs, and `is_c2c` keeps
only the device half, which over-matches two ids: `0x29bc` and `0x31c2` each
have non-coherent subsystem variants. The consequence is a refusal on parts that
would have been fine, chosen over silently mis-binding a coherent one.

The alias table cannot tell "no variant driver exists" from "this kernel has not
heard of it", but `pcilibs_rs::cc` can, because it already carries the C2C device ids to
decide where no CC mode can be raised at all — the same set, since C2C is
what makes the variant driver necessary. So a coherently attached GPU that
resolves to plain `vfio-pci` is refused with the kernel named as the problem,
and every other device keeps the fallback.

#### What was rejected

- **A systemd oneshot running this binary at boot**, with a kubelet drop-in that
  `Requires=` it so a node that cannot bind never goes `Ready`. It bought a real
  property — a bind failure became a visible node failure instead of a green
  node advertising GPUs it does not have — at the cost of a binary installed on
  the host, a unit, a drop-in, and detecting "the kubelet unit" across kubeadm,
  k3s, RKE2, k0s and MicroK8s, none of which agree on its name. It also inverted
  the blast radius: a GPU problem became a whole-node outage. Against a
  mechanism with no code running at boot, there is much less left to fail, and
  the residual failure — a node that comes back with unbound GPUs — shows up as
  a device plugin advertising zero devices.
- **`driverctl`**, a host package and a shell utility to write the same two
  sysfs attributes.
- **`vfio-pci.ids=` on the kernel command line**, which binds earlier than any
  of this and needs nothing on the node, but takes a reboot to change and is the
  image builder's decision rather than the cluster's. Nodes built that way are
  already provisioned as far as binding goes; `status` and `verify` still work.

### Runtime state is not reverted on uninstall

Uninstalling removes the configuration that restores VFIO bindings at boot. It
does not unbind live devices or change CC mode: removing the provisioner must
not disrupt a running workload. The next boot returns the devices to normal
driver discovery, and a later provisioning run converges them again.
