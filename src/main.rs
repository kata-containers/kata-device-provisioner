// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

mod device;
mod drivers;
mod host;
mod idle;
mod mode;
mod modules;
mod nvidia;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use pcilibs_rs::cc;

use crate::device::{discover, in_scope, DeviceState, Provisioning};
use crate::drivers::{Planned, Target};
use crate::idle::{DEV_VFIO, PROC};
use crate::mode::Mode;
use crate::nvidia::Transition;
use pcilibs_rs::vfio::{self, Bound};
use pcilibs_rs::{Sysfs, SYSFS};

#[derive(Parser)]
#[command(name = "kata-device-provisioner", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Roots {
    #[arg(long, default_value = SYSFS, global = true)]
    sysfs: PathBuf,

    #[arg(long, default_value = PROC, global = true)]
    proc: PathBuf,

    #[arg(long, default_value = DEV_VFIO, global = true)]
    dev_vfio: PathBuf,

    /// The node's filesystem, which a Job sees under a mount point.
    #[arg(long, default_value = "/", global = true)]
    host_root: PathBuf,
}

impl Roots {
    fn sysfs(&self) -> Sysfs {
        Sysfs::new(&self.sysfs)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Report the node's passthrough devices and their provisioning state.
    Status {
        /// Read the live CC mode. Requires root, maps BAR0, wakes suspended
        /// devices.
        #[arg(long)]
        probe: bool,

        /// Also list devices with no device-table row, which are never touched.
        #[arg(long)]
        all: bool,

        #[command(flatten)]
        roots: Roots,
    },

    /// Put the node into `--mode` and leave it able to get back there alone.
    ///
    /// Idempotent: a node already in that state comes out untouched.
    Apply {
        /// off | on | devtools | ppcie
        #[arg(long)]
        mode: Mode,

        #[command(flatten)]
        roots: Roots,
    },

    /// Remove the boot configuration, leaving live devices unchanged.
    Uninstall {
        #[command(flatten)]
        roots: Roots,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Status { probe, all, roots } => status(probe, all, &roots.sysfs()),
        Command::Apply { mode, roots } => apply(mode, &roots),
        Command::Uninstall { roots } => uninstall(&roots),
    }
}

fn status(probe: bool, all: bool, sysfs: &Sysfs) -> Result<()> {
    let node = discover(sysfs)?;
    let mut states = node.devices;
    states.retain(|state| all || state.in_scope());

    if states.is_empty() {
        println!("no devices to provision (try --all to see what the node has)");
        return Ok(());
    }

    let failures = if probe {
        host::check_probeable(sysfs)?;
        nvidia::probe_modes(&mut states)
    } else {
        Default::default()
    };

    for state in &states {
        let provisioning = match state.provisioning() {
            Some(Provisioning::NvidiaInBandCc) => "nvidia-in-band-cc",
            Some(Provisioning::NvidiaInBandPpcie) => "nvidia-in-band-ppcie",
            Some(Provisioning::FabricManagement) => "fabric-management",
            None => "out-of-scope",
        };
        let cc = match (&state.cc_mode, state.cc_chip) {
            (Some(mode), _) => mode.to_string(),
            (None, Some(_)) if probe => "unreadable".to_string(),
            (None, Some(_)) => "capable".to_string(),
            (None, None) => "n/a".to_string(),
        };
        let ppcie = match (&state.ppcie_mode, state.carries_ppcie()) {
            (Some(mode), _) => mode.to_string(),
            (None, true) if probe => "unreadable".to_string(),
            (None, true) => "capable".to_string(),
            (None, false) => "n/a".to_string(),
        };

        println!(
            "{addr}  {vendor:#06x}:{device_id:04x}  class={class:#08x}  {provisioning}  chip={chip}  driver={driver}  iommu_group={group}  numa={numa}  cc={cc}  ppcie={ppcie}  {name}",
            addr = state.address,
            vendor = state.vendor,
            device_id = state.device_id,
            class = state.class,
            chip = state.cc_chip.unwrap_or("-"),
            driver = state.driver.as_deref().unwrap_or("<unbound>"),
            group = state.iommu_group,
            numa = state.numa_node,
            name = state.device_name,
        );

        for (label, value) in state.cc_label_values() {
            println!("    would label {label}={value}");
        }
    }

    if !node.fabric.is_empty() {
        println!("fabric: {}", node.fabric.join(", "));
    }

    for (address, err) in &failures {
        eprintln!("{address}: CC mode unreadable: {err}");
    }

    Ok(())
}

fn apply(mode: Mode, roots: &Roots) -> Result<()> {
    let sysfs = roots.sysfs();

    host::check(&sysfs)?;

    let mut states = in_scope(&sysfs)?.devices;
    if states.is_empty() {
        bail!("no devices in scope: this node was selected for provisioning but has none");
    }

    // Across every device before any is touched: refuse whole, not half.
    verify_capable(&states, mode)?;

    // Re-read in `nvidia::apply`: hardware may change after this safety check.
    let unreadable = nvidia::probe_modes(&mut states);

    // Before any reset, so a missing driver is not found out half way through.
    let planned = drivers::plan(&roots.host_root, &roots.proc, &states, |state| {
        passes_through(state, mode)
    })?;
    drivers::verify_bindable(&sysfs, &planned)?;

    verify_idle(
        roots,
        &sysfs,
        &writable(&sysfs, &planned, mode, &unreadable),
    )?;

    // The first write to the node: `vfio-pci` binds the ids a saved `ids=`
    // option names as it loads.
    let targets = drivers::load(&sysfs, &roots.host_root, &planned)?;

    mode_stage(&states, mode)?;

    // Before binding, so a crash leaves a node that comes back bound.
    persist(&roots.host_root, &targets)?;

    for target in &targets {
        let (address, driver) = (&target.state.address, &target.driver);
        match vfio::bind(&sysfs, address, driver)? {
            Bound::Already => println!("{address}: already bound to {driver}"),
            Bound::Now => println!("{address}: bound to {driver}"),
        }
    }

    release(&sysfs, &planned)?;

    verify(&sysfs, mode, &targets)?;

    println!("node provisioned: mode={mode}, {} devices", targets.len());
    Ok(())
}

/// The boot config is rewritten from the new set, so a device dropped out of
/// it is handed back now rather than at the next reboot. Only what vfio holds:
/// anything else was never ours to take.
fn release(sysfs: &Sysfs, planned: &[Planned]) -> Result<()> {
    for state in planned
        .iter()
        .filter(|device| device.module.is_none())
        .map(|device| device.state)
    {
        let Some(driver) =
            vfio::current_driver(sysfs, &state.address).filter(|driver| drivers::is_vfio(driver))
        else {
            continue;
        };

        vfio::unbind(sysfs, &state.address)
            .with_context(|| format!("{}: release from {driver}", state.address))?;
        println!("{}: released from {driver}", state.address);
    }

    Ok(())
}

/// Only PPCIE gives one guest the whole baseboard. In the other modes each
/// guest gets one GPU and the fabric stays with the host, switch chips and
/// management PFs alike.
fn passes_through(state: &DeviceState, mode: Mode) -> bool {
    mode == Mode::Ppcie || !state.is_fabric()
}

fn persist(host_root: &Path, targets: &[Target]) -> Result<()> {
    let claims: Vec<_> = targets
        .iter()
        .map(|target| modules::Claim {
            address: target.state.address.clone(),
            vendor: target.state.vendor,
            device: target.state.device_id,
            module: target.module.clone(),
            driver: target.driver.clone(),
            scope: match target.state.provisioning() {
                // A management PF shares its ids with the node's data-path NICs.
                Some(Provisioning::FabricManagement) => modules::Scope::Address,
                Some(Provisioning::NvidiaInBandCc | Provisioning::NvidiaInBandPpcie) | None => {
                    modules::Scope::Ids
                }
            },
        })
        .collect();

    let overridden = modules::persist(host_root, &claims)?;
    println!("boot config written for {} devices", claims.len());
    if !overridden.is_empty() {
        println!(
            "{} through a udev rule, which is what carries driver_override: {}",
            overridden.len(),
            overridden.join(", ")
        );
    }

    Ok(())
}

/// Refused whole, before the first GPU is touched: provisioning only the
/// capable ones would label the node ready with hardware on it that the
/// workload must never reach.
fn verify_capable(states: &[DeviceState], mode: Mode) -> Result<()> {
    if !mode.cc_ready() {
        return Ok(());
    }

    let (requirement, capable): (&str, fn(&DeviceState) -> bool) = match mode {
        // Blackwell has per-GPU CC but no PPCIE: refused, not downgraded.
        Mode::Ppcie => (
            "PPCIE support, which is Hopper-only",
            DeviceState::carries_ppcie,
        ),
        _ => ("in-band CC support", DeviceState::cc_capable),
    };

    let incapable: Vec<String> = states
        .iter()
        .filter(|state| {
            state.provisioning() == Some(Provisioning::NvidiaInBandCc)
                || (mode == Mode::Ppcie && state.is_fabric())
        })
        .filter(|state| !capable(state))
        .map(|state| format!("{} ({})", state.address, state.device_name))
        .collect();

    if !incapable.is_empty() {
        bail!(
            "mode {mode} needs {requirement}, which {} of this node's {} devices lack: {}. \
             Provision this node with --mode off, or leave it out of the run's node selection",
            incapable.len(),
            states.len(),
            incapable.join(", ")
        );
    }

    if mode.cc_target() != cc::CcMode::Off {
        let grace_attached: Vec<String> = states
            .iter()
            .filter(|state| state.provisioning() == Some(Provisioning::NvidiaInBandCc))
            .filter(|state| {
                cc::is_c2c(state.device_id)
                    && cc::chip_for(state.device_id).is_some_and(|chip| !chip.c2c_cc_supported)
            })
            .map(|state| format!("{} ({})", state.address, state.device_name))
            .collect();

        if !grace_attached.is_empty() {
            bail!(
                "mode {mode} needs confidential computing from the CPU as well as the GPU, and \
                 the Grace CPU {} of this node's devices are attached to has none: {}. Provision \
                 this node with --mode off, or leave it out of the run's node selection",
                grace_attached.len(),
                grace_attached.join(", ")
            );
        }
    }

    Ok(())
}

fn mode_stage(states: &[DeviceState], mode: Mode) -> Result<()> {
    for state in states {
        if state.provisioning() == Some(Provisioning::NvidiaInBandCc) && !state.cc_capable() {
            println!("{}: no CC support, bind only", state.address);
        }
    }

    for (address, transition) in nvidia::apply(states, mode)? {
        let target = target_of(states, &address, mode);
        match transition {
            Transition::AlreadySet => println!("{address}: already {target}"),
            Transition::Applied => println!("{address}: set to {target} and reset"),
        }
    }

    Ok(())
}

/// Per knob, not per node or per device kind: under `ppcie` a Hopper GPU takes
/// CC off *and* PPCIE on, and naming one of them reports what it did not do.
fn target_of(states: &[DeviceState], address: &str, mode: Mode) -> String {
    let Some(state) = states.iter().find(|state| state.address == address) else {
        return format!("cc {}", mode.cc_target());
    };

    let mut knobs = Vec::new();
    if state.cc_capable() {
        knobs.push(format!("cc {}", mode.cc_target()));
    }
    if state.carries_ppcie() {
        knobs.push(format!("ppcie {}", mode.ppcie_target()));
    }

    knobs.join(", ")
}

fn verify(sysfs: &Sysfs, mode: Mode, targets: &[Target]) -> Result<()> {
    // A stage reports what it asked for; this reports what is true.
    for target in targets {
        vfio::verify_bound(sysfs, &target.state.address, &target.driver)?;
        nvidia::verify_mode(target.state, mode)?;
    }

    Ok(())
}

/// Busy devices block only runs that would change them, so converged nodes can
/// be upgraded while Kata workloads are running. The mode stage writes to
/// every in-scope device, so a pending mode write counts for a device the run
/// leaves on the host too.
fn writable<'a>(
    sysfs: &Sysfs,
    planned: &[Planned<'a>],
    mode: Mode,
    unreadable: &HashMap<String, String>,
) -> Vec<&'a DeviceState> {
    planned
        .iter()
        .filter(|device| {
            let state = device.state;
            let rebinds = match &device.module {
                // Released after the bind stage if vfio holds it.
                None => vfio::current_driver(sysfs, &state.address)
                    .is_some_and(|driver| drivers::is_vfio(&driver)),
                Some(module) => !drivers::bound(sysfs, state, module),
            };

            rebinds
                || unreadable.contains_key(&state.address)
                || !nvidia::writes_for(state.cc_mode, state.ppcie_mode, mode).is_empty()
        })
        .map(|device| device.state)
        .collect()
}

fn verify_idle(roots: &Roots, sysfs: &Sysfs, states: &[&DeviceState]) -> Result<()> {
    for state in states {
        let holders = idle::holders(&roots.proc, &roots.dev_vfio, sysfs, state);
        if let Some(holder) = holders.first() {
            bail!(
                "{}: in use by pid {} ({}): refusing to change a device a VM may be running on",
                state.address,
                holder.pid,
                holder.comm
            );
        }
    }

    Ok(())
}

fn uninstall(roots: &Roots) -> Result<()> {
    modules::unpersist(&roots.host_root)?;

    println!("boot configuration removed; live bindings and CC mode left unchanged");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::tests::{
        managed_fabric_node, node, A100, B200, GB200, GH200, H100, NVSWITCH, RUBIN_C2C,
    };
    use crate::drivers::tests::on_vfio_pci;
    use pcilibs_rs::cc::{CcMode, PpcieMode};
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};
    use std::fs;
    use tempfile::TempDir;

    #[fixture]
    fn sysfs() -> Fake {
        let fake = testfs::fake();
        fake.add_module("vfio_pci", "vfio-pci");
        fake
    }

    fn hold(proc_root: &TempDir, dev_vfio: &TempDir, pid: u32, comm: &str, group: &str) {
        let process = proc_root.path().join(pid.to_string());
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::write(process.join("comm"), format!("{comm}\n")).unwrap();

        let node = dev_vfio.path().join(group);
        fs::write(&node, "").unwrap();
        std::os::unix::fs::symlink(&node, process.join("fd").join("3")).unwrap();
    }

    fn roots(sysfs: &Fake, proc_root: &TempDir, dev_vfio: &TempDir, host: &TempDir) -> Roots {
        Roots {
            sysfs: sysfs.root().to_path_buf(),
            proc: proc_root.path().to_path_buf(),
            dev_vfio: dev_vfio.path().to_path_buf(),
            host_root: host.path().to_path_buf(),
        }
    }

    #[rstest]
    #[case::off_leaves_the_switches_on_the_host(Mode::Off, 8)]
    #[case::on_leaves_the_switches_on_the_host(Mode::On, 8)]
    #[case::devtools_leaves_the_switches_on_the_host(Mode::DevTools, 8)]
    #[case::ppcie_claims_the_whole_baseboard(Mode::Ppcie, 12)]
    fn passes_the_switches_through_only_under_ppcie(
        sysfs: Fake,
        #[case] mode: Mode,
        #[case] expected: usize,
    ) {
        let board = [H100; 8]
            .into_iter()
            .chain([NVSWITCH; 4])
            .collect::<Vec<_>>();
        let states = node(&sysfs, &board);

        let passed = states
            .iter()
            .filter(|state| passes_through(state, mode))
            .count();

        assert_eq!(states.len(), 12);
        assert_eq!(passed, expected);
    }

    /// A switch on the host driver is a node never provisioned for PPCIE, so
    /// taking it would be stealing hardware this tool never claimed.
    #[rstest]
    #[case::vfio_holds_it_and_the_mode_does_not_want_it(Mode::Off, Some("vfio-pci"), true)]
    #[case::ppcie_passes_it_through_instead(Mode::Ppcie, Some("vfio-pci"), false)]
    #[case::the_host_driver_holds_it(Mode::Off, Some("nvidia-nvswitch"), false)]
    #[case::nothing_holds_it(Mode::Off, None, false)]
    fn releases_what_vfio_holds_for_a_mode_that_does_not_pass_it_through(
        sysfs: Fake,
        #[case] mode: Mode,
        #[case] bound: Option<&str>,
        #[case] released: bool,
    ) {
        let states = node(&sysfs, &[NVSWITCH]);
        let address = states[0].address.clone();
        if let Some(driver) = bound {
            sysfs.add_device(&address, Some(driver));
        }

        release(
            &sysfs.sysfs,
            &on_vfio_pci(&states, |state| passes_through(state, mode)),
        )
        .unwrap();

        let unbound = sysfs.driver_unbind(bound.unwrap_or("vfio-pci")) == address;
        assert_eq!(unbound, released);
    }

    #[rstest]
    fn uninstall_removes_only_boot_configuration(sysfs: Fake) {
        let address = "0000:65:00.0";
        sysfs.add_driver("vfio-pci");
        sysfs.add_pci_device(address, 0x10de, H100.0, H100.1, Some("vfio-pci"));

        let proc_root = tempfile::tempdir().unwrap();
        let dev_vfio = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        modules::persist(
            host.path(),
            &[modules::Claim {
                scope: modules::Scope::Ids,
                address: address.to_string(),
                vendor: 0x10de,
                device: H100.0,
                module: "vfio_pci".to_string(),
                driver: "vfio-pci".to_string(),
            }],
        )
        .unwrap();

        uninstall(&Roots {
            sysfs: sysfs.root().to_path_buf(),
            proc: proc_root.path().to_path_buf(),
            dev_vfio: dev_vfio.path().to_path_buf(),
            host_root: host.path().to_path_buf(),
        })
        .unwrap();

        assert_eq!(sysfs.driver_unbind("vfio-pci"), "");
        assert!(!host
            .path()
            .join("etc/modprobe.d/kata-device-provisioner.conf")
            .exists());
    }

    /// `apply` itself requires chroot and the host's modprobe.
    #[rstest]
    #[case::converged(Some(CcMode::On), false)]
    #[case::the_mode_has_to_change(Some(CcMode::Off), true)]
    fn a_holder_stops_only_a_run_with_a_write_for_it(
        sysfs: Fake,
        #[case] cc: Option<CcMode>,
        #[case] refused: bool,
    ) {
        let address = "0000:65:00.0";
        sysfs.add_driver("vfio-pci");
        sysfs.add_pci_device(address, 0x10de, H100.0, H100.1, Some("vfio-pci"));
        sysfs.set_iommu_group(address, 14);

        let proc_root = tempfile::tempdir().unwrap();
        let dev_vfio = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        hold(&proc_root, &dev_vfio, 4242, "qemu-system-x86", "14");

        let mut states = in_scope(&sysfs.sysfs).unwrap().devices;
        states[0].cc_mode = cc;

        let writable = writable(
            &sysfs.sysfs,
            &on_vfio_pci(&states, |_| true),
            Mode::On,
            &HashMap::new(),
        );
        let outcome = verify_idle(
            &roots(&sysfs, &proc_root, &dev_vfio, &host),
            &sysfs.sysfs,
            &writable,
        );

        assert_eq!(outcome.is_err(), refused, "{outcome:?}");
    }

    /// A running Kata workload must not block a no-op upgrade.
    #[rstest]
    #[case::nothing_to_do(Some("vfio-pci"), Some(CcMode::On), false)]
    #[case::the_mode_has_to_change(Some("vfio-pci"), Some(CcMode::Off), true)]
    #[case::it_still_has_to_be_bound(None, Some(CcMode::On), true)]
    #[case::another_driver_has_it(Some("nvidia"), Some(CcMode::On), true)]
    fn only_devices_a_run_would_write_to_are_checked(
        sysfs: Fake,
        #[case] bound: Option<&str>,
        #[case] cc: Option<CcMode>,
        #[case] writes: bool,
    ) {
        let mut states = node(&sysfs, &[H100]);
        let address = states[0].address.clone();
        sysfs.add_device(&address, bound);
        states[0].cc_mode = cc;

        let writable = writable(
            &sysfs.sysfs,
            &on_vfio_pci(&states, |_| true),
            Mode::On,
            &HashMap::new(),
        );

        assert_eq!(!writable.is_empty(), writes, "{writable:?}");
    }

    /// An unreadable mode cannot prove that no write is needed.
    #[rstest]
    fn a_mode_that_could_not_be_read_is_treated_as_writable(sysfs: Fake) {
        let mut states = node(&sysfs, &[H100]);
        let address = states[0].address.clone();
        sysfs.add_device(&address, Some("vfio-pci"));
        states[0].cc_mode = Some(CcMode::On);

        let unreadable = HashMap::from([(address.clone(), "BAR0 unmappable".to_string())]);

        let writable = writable(
            &sysfs.sysfs,
            &on_vfio_pci(&states, |_| true),
            Mode::On,
            &unreadable,
        );

        assert_eq!(writable.len(), 1);
    }

    /// Releasing a held VFIO device can block in the kernel indefinitely.
    #[rstest]
    fn a_device_about_to_be_released_is_writable(sysfs: Fake) {
        let states = node(&sysfs, &[NVSWITCH]);
        sysfs.add_device(&states[0].address, Some("vfio-pci"));

        let planned = on_vfio_pci(&states, |state| passes_through(state, Mode::Off));
        assert!(
            planned[0].module.is_none(),
            "off leaves the switch on the host"
        );

        let writable = writable(&sysfs.sysfs, &planned, Mode::Off, &HashMap::new());

        assert_eq!(writable.len(), 1);
    }

    /// `mode_stage` writes to a switch the run leaves on the host, and the
    /// reset must not reach a device a VM holds.
    #[rstest]
    #[case::ppcie_has_to_be_turned_off(Some(PpcieMode::On), false, true)]
    #[case::already_off(Some(PpcieMode::Off), false, false)]
    #[case::mode_unreadable(None, true, true)]
    fn a_pending_mode_write_makes_a_host_left_device_writable(
        sysfs: Fake,
        #[case] ppcie: Option<PpcieMode>,
        #[case] probe_failed: bool,
        #[case] writes: bool,
    ) {
        let mut states = node(&sysfs, &[NVSWITCH]);
        states[0].ppcie_mode = ppcie;
        let unreadable: HashMap<_, _> = probe_failed
            .then(|| (states[0].address.clone(), "BAR0 unmappable".to_string()))
            .into_iter()
            .collect();

        let planned = on_vfio_pci(&states, |state| passes_through(state, Mode::Off));
        let writable = writable(&sysfs.sysfs, &planned, Mode::Off, &unreadable);

        assert_eq!(!writable.is_empty(), writes);
    }

    /// Which knob a Hopper GPU actually writes depends on where the node was,
    /// so naming one of them is wrong half the time.
    #[rstest]
    #[case::hopper_gpu_under_ppcie(H100, Mode::Ppcie, "cc off, ppcie on")]
    #[case::hopper_gpu_under_off(H100, Mode::Off, "cc off, ppcie off")]
    #[case::hopper_gpu_under_on(H100, Mode::On, "cc on, ppcie off")]
    #[case::switch_has_no_cc_mode(NVSWITCH, Mode::Ppcie, "ppcie on")]
    #[case::blackwell_has_no_ppcie(B200, Mode::On, "cc on")]
    fn a_target_names_every_knob_the_device_has(
        sysfs: Fake,
        #[case] device: (u16, u32),
        #[case] mode: Mode,
        #[case] expected: &str,
    ) {
        let states = node(&sysfs, &[device]);

        assert_eq!(target_of(&states, &states[0].address, mode), expected);
    }

    #[rstest]
    #[case::hgx_h100(&[H100, H100, NVSWITCH])]
    #[case::hgx_b200(&[B200, B200, NVSWITCH])]
    #[case::single_pcie_gpu(&[H100])]
    #[case::mixed_cc_generations(&[H100, B200])]
    fn accepts_a_node_whose_gpus_can_all_do_cc(sysfs: Fake, #[case] devices: &[(u16, u32)]) {
        let states = node(&sysfs, devices);

        verify_capable(&states, Mode::On).unwrap();
    }

    #[rstest]
    #[case::on(Mode::On)]
    #[case::devtools(Mode::DevTools)]
    #[case::ppcie(Mode::Ppcie)]
    fn refuses_a_mixed_node_before_touching_the_gpus_that_could(sysfs: Fake, #[case] mode: Mode) {
        let states = node(&sysfs, &[H100, A100]);

        let err = verify_capable(&states, mode).unwrap_err().to_string();

        assert!(err.contains("0000:01:00.0"), "{err}");
        assert!(
            !err.contains("0000:00:00.0"),
            "the H100 is not the problem: {err}"
        );
    }

    #[rstest]
    fn mode_off_provisions_a_mixed_node(sysfs: Fake) {
        let states = node(&sysfs, &[H100, A100]);

        verify_capable(&states, Mode::Off).unwrap();
    }

    #[rstest]
    fn an_nvswitch_is_not_a_gpu_that_failed_to_support_cc(sysfs: Fake) {
        let states = node(&sysfs, &[NVSWITCH]);

        verify_capable(&states, Mode::On).unwrap();
    }

    #[rstest]
    #[case::hgx_hopper(&[H100, H100, NVSWITCH])]
    #[case::single_hopper(&[H100])]
    fn accepts_a_hopper_board_for_ppcie(sysfs: Fake, #[case] devices: &[(u16, u32)]) {
        let states = node(&sysfs, devices);

        verify_capable(&states, Mode::Ppcie).unwrap();
    }

    #[rstest]
    #[case::hgx_blackwell(&[B200, B200, NVSWITCH])]
    #[case::mixed_generations(&[H100, B200])]
    fn refuses_a_blackwell_board_for_ppcie(sysfs: Fake, #[case] devices: &[(u16, u32)]) {
        let states = node(&sysfs, devices);

        let err = verify_capable(&states, Mode::Ppcie)
            .unwrap_err()
            .to_string();

        assert!(err.contains("Hopper-only"), "{err}");
    }

    #[rstest]
    fn a_blackwell_board_still_does_per_gpu_cc(sysfs: Fake) {
        let states = node(&sysfs, &[B200, B200, NVSWITCH]);

        verify_capable(&states, Mode::On).unwrap();
    }

    /// PPCIE takes the switch chips into the board's mode; a fabric reached
    /// through management PFs has no such mode, so the board is refused.
    #[rstest]
    fn managed_fabric_cannot_take_ppcie(managed_fabric_node: Fake) {
        let states = in_scope(&managed_fabric_node.sysfs).unwrap().devices;

        assert!(verify_capable(&states, Mode::Ppcie).is_err());
    }

    /// Turning CC off is still allowed there: it is only a raised mode that
    /// needs the CPU's half.
    #[rstest]
    #[case::grace_hopper_on(GH200, Mode::On, false)]
    #[case::grace_blackwell_on(GB200, Mode::On, false)]
    #[case::grace_blackwell_devtools(GB200, Mode::DevTools, false)]
    #[case::grace_blackwell_off(GB200, Mode::Off, true)]
    #[case::rubin_c2c_can_enable_cc(RUBIN_C2C, Mode::On, true)]
    #[case::rubin_c2c_can_enable_devtools(RUBIN_C2C, Mode::DevTools, true)]
    #[case::pcie_gpu_is_ours_to_set(H100, Mode::On, true)]
    fn refuses_to_raise_cc_where_the_cpu_cannot_back_it(
        sysfs: Fake,
        #[case] device: (u16, u32),
        #[case] mode: Mode,
        #[case] expected: bool,
    ) {
        let states = node(&sysfs, &[device]);

        assert_eq!(verify_capable(&states, mode).is_ok(), expected);
    }
}
