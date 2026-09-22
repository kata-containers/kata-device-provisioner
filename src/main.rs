// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

mod device;
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

use crate::device::{discover, DeviceState, Provisioning};
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
    let mut states = discover(sysfs)?;
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

    for (address, err) in &failures {
        eprintln!("{address}: CC mode unreadable: {err}");
    }

    Ok(())
}

fn apply(mode: Mode, roots: &Roots) -> Result<()> {
    let sysfs = roots.sysfs();

    host::check(&sysfs)?;

    let states = in_scope(&sysfs)?;
    if states.is_empty() {
        bail!("no devices in scope: this node was selected for provisioning but has none");
    }

    // Across every device before any is touched: refuse whole, not half.
    verify_capable(&states, mode)?;
    verify_idle(roots, &sysfs, &states)?;

    let passthrough = passthrough_set(&states, mode);

    // Before any reset, so a missing driver is not found out half way through.
    let drivers = resolve_drivers(&sysfs, roots, &passthrough)?;

    mode_stage(&states, mode)?;

    // Before binding, so a crash leaves a node that comes back bound.
    persist(&roots.host_root, &passthrough, &drivers)?;

    for state in &passthrough {
        let driver = &drivers[&state.address].driver;
        match vfio::bind(&sysfs, &state.address, driver)? {
            Bound::Already => println!("{}: already bound to {driver}", state.address),
            Bound::Now => println!("{}: bound to {driver}", state.address),
        }
    }

    release(&sysfs, &states, &passthrough)?;

    verify(&sysfs, mode, &passthrough, &drivers)?;

    println!(
        "node provisioned: mode={mode}, {} devices",
        passthrough.len()
    );
    Ok(())
}

/// The boot config is rewritten from the new set, so a device dropped out of
/// it is handed back now rather than at the next reboot. Only what vfio holds:
/// anything else was never ours to take.
fn release(sysfs: &Sysfs, states: &[DeviceState], passthrough: &[DeviceState]) -> Result<()> {
    for state in states {
        if passthrough.iter().any(|kept| kept.address == state.address) {
            continue;
        }

        // Substring, because a variant driver is named after the one it
        // stands in for: nvgrace_gpu_vfio_pci, mlx5_vfio_pci.
        let Some(driver) = vfio::current_driver(sysfs, &state.address)
            .filter(|driver| driver.replace('-', "_").contains("vfio"))
        else {
            continue;
        };

        vfio::unbind(sysfs, &state.address)
            .with_context(|| format!("{}: release from {driver}", state.address))?;
        println!("{}: released from {driver}", state.address);
    }

    Ok(())
}

/// Only PPCIE gives one guest the whole baseboard; the other modes are
/// single-GPU, where taking the switches off the host would strand them.
fn passthrough_set(states: &[DeviceState], mode: Mode) -> Vec<DeviceState> {
    states
        .iter()
        .filter(|state| {
            mode == Mode::Ppcie || state.provisioning() != Some(Provisioning::NvidiaInBandPpcie)
        })
        .cloned()
        .collect()
}

/// The fallback to `vfio-pci` is right for an ordinary PCIe GPU and wrong for
/// a coherently attached one, which would reach a VM with its memory missing.
fn verify_variant_available(state: &DeviceState, module: &str) -> Result<()> {
    if !cc::is_c2c(state.device_id) || module.replace('-', "_") != "vfio_pci" {
        return Ok(());
    }

    bail!(
        "{} ({}) is coherently attached over NVLink-C2C and needs a vfio driver that can map \
         its memory, but this kernel's module alias table offers only {module}, which cannot. \
         The node needs a kernel whose nvgrace-gpu vfio driver claims this device",
        state.address,
        state.device_name
    )
}

/// modprobe takes the module, `driver_override` the driver, and they differ.
struct VfioDriver {
    module: String,
    driver: String,
}

/// Per device: a Grace GPU needs `nvgrace_gpu_vfio_pci` where the NIC beside
/// it takes plain `vfio-pci`.
fn resolve_drivers(
    sysfs: &Sysfs,
    roots: &Roots,
    states: &[DeviceState],
) -> Result<HashMap<String, VfioDriver>> {
    let alias = modules::modules_alias(&roots.host_root, &roots.proc)?;
    let mut drivers = HashMap::new();

    for state in states {
        let module = vfio::module_for(&alias, state.vendor, state.device_id)
            .with_context(|| format!("{}: find its vfio module", state.address))?;

        verify_variant_available(state, &module)?;

        modules::modprobe(&roots.host_root, &module)
            .with_context(|| format!("{}: load {module}", state.address))?;

        let driver = vfio::driver_for(sysfs, &module)
            .with_context(|| format!("{}: find the driver {module} registers", state.address))?;

        drivers.insert(state.address.clone(), VfioDriver { module, driver });
    }

    Ok(drivers)
}

fn persist(
    host_root: &Path,
    states: &[DeviceState],
    drivers: &HashMap<String, VfioDriver>,
) -> Result<()> {
    let claims: Vec<_> = states
        .iter()
        .map(|state| modules::Claim {
            address: state.address.clone(),
            vendor: state.vendor,
            device: state.device_id,
            module: drivers[&state.address].module.clone(),
            driver: drivers[&state.address].driver.clone(),
        })
        .collect();

    let overridden = modules::persist(host_root, &claims)?;
    println!("boot config written for {} devices", states.len());
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
        .filter(|state| state.provisioning() == Some(Provisioning::NvidiaInBandCc))
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
            .filter(|state| cc::is_c2c(state.device_id))
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

fn verify(
    sysfs: &Sysfs,
    mode: Mode,
    states: &[DeviceState],
    drivers: &HashMap<String, VfioDriver>,
) -> Result<()> {
    // A stage reports what it asked for; this reports what is true.
    for state in states {
        vfio::verify_bound(sysfs, &state.address, &drivers[&state.address].driver)?;
        nvidia::verify_mode(state, mode)?;
    }

    Ok(())
}

fn verify_idle(roots: &Roots, sysfs: &Sysfs, states: &[DeviceState]) -> Result<()> {
    for state in states {
        let holders = idle::holders(&roots.proc, &roots.dev_vfio, sysfs, state);
        if let Some(holder) = holders.first() {
            bail!(
                "{}: in use by pid {} ({}): refusing to reset a device a VM may be running on",
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

fn in_scope(sysfs: &Sysfs) -> Result<Vec<DeviceState>> {
    Ok(discover(sysfs)?
        .into_iter()
        .filter(DeviceState::in_scope)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};

    const H100: (u16, u32) = (0x2330, 0x030200);
    const B200: (u16, u32) = (0x2901, 0x030200);
    const A100: (u16, u32) = (0x20b0, 0x030200);
    const NVSWITCH: (u16, u32) = (0x22a3, 0x068000);
    const GH200: (u16, u32) = (0x2342, 0x030200);
    const GB200: (u16, u32) = (0x2941, 0x030200);

    #[fixture]
    fn sysfs() -> Fake {
        testfs::fake()
    }

    fn node(sysfs: &Fake, devices: &[(u16, u32)]) -> Vec<DeviceState> {
        for (slot, (device, class)) in devices.iter().enumerate() {
            sysfs.add_pci_device(
                &format!("0000:{:02x}:00.0", slot),
                0x10de,
                *device,
                *class,
                None,
            );
        }

        in_scope(&sysfs.sysfs).unwrap()
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

        let passthrough = passthrough_set(&states, mode);

        assert_eq!(states.len(), 12);
        assert_eq!(passthrough.len(), expected);
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

        release(&sysfs.sysfs, &states, &passthrough_set(&states, mode)).unwrap();

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

    /// The kernel names the variant module with underscores and sysfs names
    /// the driver with dashes, and either can reach this.
    #[rstest]
    #[case::grace_hopper(GH200, "nvgrace_gpu_vfio_pci", true)]
    #[case::grace_blackwell(GB200, "nvgrace-gpu-vfio-pci", true)]
    #[case::pcie_gpu_needs_no_variant(H100, "vfio-pci", true)]
    #[case::nvswitch_needs_no_variant(NVSWITCH, "vfio_pci", true)]
    #[case::grace_on_a_kernel_too_old(GH200, "vfio-pci", false)]
    #[case::grace_on_a_kernel_too_old_underscored(GB200, "vfio_pci", false)]
    fn refuses_a_grace_gpu_the_kernel_has_no_driver_for(
        sysfs: Fake,
        #[case] device: (u16, u32),
        #[case] module: &str,
        #[case] expected: bool,
    ) {
        let states = node(&sysfs, &[device]);

        assert_eq!(
            verify_variant_available(&states[0], module).is_ok(),
            expected
        );
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

    /// Turning CC off is still allowed there: it is only a raised mode that
    /// needs the CPU's half.
    #[rstest]
    #[case::grace_hopper_on(GH200, Mode::On, false)]
    #[case::grace_blackwell_on(GB200, Mode::On, false)]
    #[case::grace_blackwell_devtools(GB200, Mode::DevTools, false)]
    #[case::grace_blackwell_off(GB200, Mode::Off, true)]
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
