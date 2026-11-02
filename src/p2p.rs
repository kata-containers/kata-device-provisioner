// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! The PCIe peer-to-peer path GPUDirect RDMA takes between a passthrough
//! GPU and the ConnectX beside it (HGX Shared NVSwitch guide 19.0, sections
//! 2.6 and 2.7): Direct Translated P2P on the switch ports between them, so
//! a request the NIC has translated through ATS crosses the switch instead
//! of climbing to the root complex, and ATS itself, a setting in the NIC's
//! firmware.
//!
//! Neither is needed for passthrough to work. The kernel sets the ACS bits
//! that isolate IOMMU groups on its own, and a NIC without ATS still reaches
//! a GPU through the root complex. This is the fast path, and a profile asks
//! for it.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pcilibs_rs::pcie::{self, Config, ACS_DIRECT_TRANSLATED};
use pcilibs_rs::{attr_hex, Sysfs};

use crate::device::DeviceState;
use crate::modules;

/// Mellanox, whose ConnectX firmware carries the ATS setting.
const MELLANOX: u32 = 0x15b3;
/// Network controller, the base class.
const CLASS_NETWORK: u32 = 0x02;

/// MFT's mlxconfig and mstflint's mstconfig take the same arguments; either
/// reaches the firmware through the function's configuration space, so
/// neither needs a driver on it.
const CONFIG_TOOL_CANDIDATES: &[&str] = &[
    "/usr/bin/mlxconfig",
    "/usr/bin/mstconfig",
    "/usr/sbin/mlxconfig",
    "/usr/sbin/mstconfig",
];
const SETPCI_CANDIDATES: &[&str] = &[
    "/usr/bin/setpci",
    "/usr/sbin/setpci",
    "/sbin/setpci",
    "/bin/setpci",
];

/// What the firmware setting is called in both tools.
const ATS_SETTING: &str = "ATS_ENABLED";

/// A downstream port of a switch on the path between a passthrough device
/// and the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub address: String,
    pub vendor: u16,
    pub device: u16,
    /// A port without the capability routes every P2P request through the
    /// root complex, and nothing here can change that.
    pub direct_translated: bool,
}

/// What a run sets up: the ports, and the ConnectX functions below them
/// that the GPUs exchange RDMA traffic with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub ports: Vec<Port>,
    pub nics: Vec<String>,
}

/// The ports of every switch a passthrough device sits behind, and the
/// ConnectX functions under them. Reading ACS needs root.
pub fn plan(sysfs: &Sysfs, passthrough: &[DeviceState]) -> Result<Plan> {
    let mut addresses = BTreeSet::new();
    for state in passthrough {
        addresses
            .extend(pcie::switch_ports(sysfs, &state.address).with_context(|| {
                format!("{}: find the switch ports on its path", state.address)
            })?);
    }

    let mut ports = Vec::new();
    let mut nics = BTreeSet::new();
    for address in addresses {
        let dir = sysfs.device(&address).expect("a switch port address");
        let acs = Config::open(sysfs, &address)?
            .acs()?
            .with_context(|| format!("{address}: ACS capability disappeared"))?;
        ports.push(Port {
            vendor: attr_hex(&dir, "vendor")? as u16,
            device: attr_hex(&dir, "device")? as u16,
            direct_translated: acs.supports(ACS_DIRECT_TRANSLATED),
            address: address.clone(),
        });

        for endpoint in pcie::endpoints(sysfs, &address)? {
            if is_connectx(sysfs, &endpoint) {
                nics.insert(endpoint);
            }
        }
    }

    Ok(Plan {
        ports,
        nics: nics.into_iter().collect(),
    })
}

fn is_connectx(sysfs: &Sysfs, address: &str) -> bool {
    let Some(dir) = sysfs.device(address) else {
        return false;
    };
    attr_hex(&dir, "vendor").is_ok_and(|vendor| vendor == MELLANOX)
        && attr_hex(&dir, "class").is_ok_and(|class| class >> 16 == CLASS_NETWORK)
}

/// ATS has to be on in every ConnectX's firmware before the path works, and
/// firmware applies it at the next boot. So this runs before a GPU is
/// touched, turns it on where it is off, and refuses the run when the node
/// has to go round once more: a node labelled provisioned would otherwise
/// carry the slow path until someone noticed.
pub fn verify_ats(host_root: &Path, sysfs: &Sysfs, plan: &Plan) -> Result<()> {
    let mut pending = Vec::new();
    for nic in &plan.nics {
        if Config::open(sysfs, nic)?.ats()?.is_some() {
            println!("{nic}: ATS exposed");
            continue;
        }

        if firmware_ats(host_root, nic)? {
            println!("{nic}: ATS enabled in firmware, not active until the node reboots");
        } else {
            enable_firmware_ats(host_root, nic)?;
            println!("{nic}: ATS enabled in firmware");
        }
        pending.push(nic.as_str());
    }

    if !pending.is_empty() {
        bail!(
            "ATS is enabled in the firmware of {} ConnectX function(s) but only takes effect at \
             the next boot: {}. Reboot the node, then run again",
            pending.len(),
            pending.join(", ")
        );
    }

    Ok(())
}

/// The tool reads the function through the node's own sysfs, which the
/// chroot has to contain.
fn config_tool(host_root: &Path, nic: &str) -> Result<String> {
    let tool = modules::find_host_program(
        host_root,
        CONFIG_TOOL_CANDIDATES,
        "install MFT (mlxconfig) or mstflint (mstconfig) on the node, or enable ATS \
         yourself with `mlxconfig -d <address> set ATS_ENABLED=1` and reboot",
    )?;
    let config = host_root
        .join("sys/bus/pci/devices")
        .join(nic)
        .join("config");
    if !config.is_file() {
        bail!(
            "{nic}: {} is not there: the host root under --host-root has to carry the node's \
             /sys for {tool} to reach the function",
            config.display()
        );
    }

    Ok(tool)
}

/// The setting as the firmware will boot with it.
fn firmware_ats(host_root: &Path, nic: &str) -> Result<bool> {
    let tool = config_tool(host_root, nic)?;
    let output = modules::run_host(host_root, &tool, &["-d", nic, "query", ATS_SETTING])?;
    if !output.status.success() {
        bail!(
            "{nic}: {tool} query failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_ats_query(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("{nic}: {tool} reports no {ATS_SETTING} setting"))
}

/// `-y` answers the tool's own confirmation prompt; the decision was the
/// profile's.
fn enable_firmware_ats(host_root: &Path, nic: &str) -> Result<()> {
    let tool = config_tool(host_root, nic)?;
    let setting = format!("{ATS_SETTING}=1");
    let output = modules::run_host(host_root, &tool, &["-d", nic, "-y", "set", &setting])?;
    if output.status.success() {
        return Ok(());
    }

    bail!(
        "{nic}: {tool} set {setting} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// The query prints one line per setting, the value last as `False(0)` or
/// `True(1)`; `-e` would add the default and current columns, and the
/// next-boot value is the one that matters here.
fn parse_ats_query(stdout: &str) -> Option<bool> {
    stdout
        .lines()
        .map(str::split_whitespace)
        .find_map(|mut fields| (fields.next() == Some(ATS_SETTING)).then(|| fields.last())?)
        .and_then(|value| {
            let (_, digit) = value.split_once('(')?;
            digit.strip_suffix(')')?.parse::<u8>().ok()
        })
        .map(|value| value != 0)
}

/// On top of what the kernel enabled, which stays: the write is read back
/// first and only the one bit is added.
pub fn apply_acs(sysfs: &Sysfs, plan: &Plan) -> Result<()> {
    for port in &plan.ports {
        if !port.direct_translated {
            println!(
                "{}: no Direct Translated P2P capability, requests climb to the root complex",
                port.address
            );
            continue;
        }

        let config = Config::open(sysfs, &port.address)?;
        let acs = config
            .acs()?
            .with_context(|| format!("{}: ACS capability disappeared", port.address))?;
        if acs.enabled(ACS_DIRECT_TRANSLATED) {
            println!("{}: Direct Translated P2P already on", port.address);
            continue;
        }

        config.set_acs_ctrl(&acs, acs.ctrl | ACS_DIRECT_TRANSLATED)?;
        println!("{}: Direct Translated P2P on", port.address);
    }

    Ok(())
}

/// The kernel enables its own ACS bits at enumeration and a udev `add` event
/// follows, so a rule on the port re-adds this one bit on top through the
/// node's `setpci`, whose mask form leaves the rest alone. Keyed on the
/// address as well as the ids: the rule describes this node's switch, not a
/// card type.
pub fn rules(host_root: &Path, plan: &Plan) -> Result<Vec<String>> {
    let ports: Vec<&Port> = plan
        .ports
        .iter()
        .filter(|port| port.direct_translated)
        .collect();
    if ports.is_empty() {
        return Ok(Vec::new());
    }

    let setpci =
        modules::find_host_program(host_root, SETPCI_CANDIDATES, "install pciutils on the node")?;

    Ok(ports
        .iter()
        .map(|port| {
            format!(
                "ACTION==\"add\", SUBSYSTEM==\"pci\", KERNEL==\"{}\", \
                 ATTR{{vendor}}==\"0x{:04x}\", ATTR{{device}}==\"0x{:04x}\", \
                 RUN+=\"{setpci} -s %k ECAP_ACS+0x6.w=0x{bit:02x}:0x{bit:02x}\"\n",
                port.address,
                port.vendor,
                port.device,
                bit = ACS_DIRECT_TRANSLATED,
            )
        })
        .collect())
}

/// What is true, re-read from the hardware.
pub fn verify(sysfs: &Sysfs, plan: &Plan) -> Result<()> {
    for port in &plan.ports {
        if !port.direct_translated {
            continue;
        }
        let acs = Config::open(sysfs, &port.address)?
            .acs()?
            .with_context(|| format!("{}: ACS capability disappeared", port.address))?;
        if !acs.enabled(ACS_DIRECT_TRANSLATED) {
            bail!(
                "{}: Direct Translated P2P did not take (ACS control reads {:#06x})",
                port.address,
                acs.ctrl
            );
        }
    }

    for nic in &plan.nics {
        if Config::open(sysfs, nic)?.ats()?.is_none() {
            bail!("{nic}: ATS is not exposed");
        }
    }

    Ok(())
}

/// One line per port and per NIC, for `status`.
pub fn report(sysfs: &Sysfs, states: &[DeviceState]) -> Result<Vec<String>> {
    let plan = plan(sysfs, states)?;
    let mut lines = Vec::new();

    for port in &plan.ports {
        let direct = match port.direct_translated {
            false => "unsupported".to_string(),
            true => {
                let acs = Config::open(sysfs, &port.address)?
                    .acs()?
                    .with_context(|| format!("{}: ACS capability disappeared", port.address))?;
                match acs.enabled(ACS_DIRECT_TRANSLATED) {
                    true => "on".to_string(),
                    false => "off".to_string(),
                }
            }
        };
        lines.push(format!(
            "p2p port {}  {:#06x}:{:04x}  direct-translated={direct}",
            port.address, port.vendor, port.device
        ));
    }

    for nic in &plan.nics {
        let ats = match Config::open(sysfs, nic)?.ats()? {
            Some(ats) if ats.enabled() => "enabled",
            Some(_) => "exposed",
            None => "absent",
        };
        lines.push(format!("p2p nic  {nic}  ats={ats}"));
    }

    Ok(lines)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::in_scope;
    use pcilibs_rs::pcie::{EXT_CAP_ACS, EXT_CAP_ATS};
    use pcilibs_rs::testfs::{self, Fake};
    use rstest::{fixture, rstest};
    use std::fs;
    use tempfile::TempDir;

    const BRIDGE: (u16, u16, u32) = (0x15b3, 0x197c, 0x060400);
    const RUBIN: (u16, u16, u32) = (0x10de, 0x303f, 0x030200);
    const CONNECTX9: (u16, u16, u32) = (0x15b3, 0x1025, 0x020000);
    const BLUEFIELD_DMA: (u16, u16, u32) = (0x15b3, 0xc2d5, 0x080100);

    /// Every isolation bit and Direct Translated P2P.
    const PORT_CAP: u16 = 0x7f;
    /// What the kernel enables at enumeration.
    const KERNEL_CTRL: u16 = 0x1d;

    fn acs_body(cap: u16, ctrl: u16) -> Vec<u8> {
        let mut body = cap.to_le_bytes().to_vec();
        body.extend(ctrl.to_le_bytes());
        body
    }

    fn add(fake: &Fake, parent: Option<&str>, address: &str, identity: (u16, u16, u32)) {
        let (vendor, device, class) = identity;
        match parent {
            Some(parent) => fake.add_pci_device_under(parent, address, vendor, device, class),
            None => fake.add_pci_device(address, vendor, device, class, None),
        }
    }

    /// One GPU's corner of a DGX: a root port, a switch whose downstream
    /// ports carry the NIC and a second switch, and the GPU below that.
    #[fixture]
    pub(crate) fn switch() -> Fake {
        let fake = testfs::fake();
        add(&fake, None, "0000:14:02.0", BRIDGE);
        add(&fake, Some("0000:14:02.0"), "0000:15:00.0", BRIDGE);
        add(&fake, Some("0000:15:00.0"), "0000:16:00.0", BRIDGE);
        add(&fake, Some("0000:15:00.0"), "0000:16:01.0", BRIDGE);
        add(&fake, Some("0000:16:00.0"), "0000:17:00.0", CONNECTX9);
        add(&fake, Some("0000:16:01.0"), "0000:18:00.0", BRIDGE);
        add(&fake, Some("0000:18:00.0"), "0000:19:00.0", BRIDGE);
        add(&fake, Some("0000:19:00.0"), "0000:1a:00.0", RUBIN);
        for port in ["0000:14:02.0", "0000:15:00.0", "0000:18:00.0"] {
            fake.set_ext_capabilities(port, &[]);
        }
        for port in ["0000:16:00.0", "0000:16:01.0", "0000:19:00.0"] {
            fake.set_ext_capabilities(port, &[(EXT_CAP_ACS, &acs_body(PORT_CAP, KERNEL_CTRL))]);
        }
        fake.set_ext_capabilities("0000:17:00.0", &[]);
        fake
    }

    pub(crate) fn gpu(switch: &Fake) -> Vec<DeviceState> {
        in_scope(&switch.sysfs).unwrap()
    }

    fn expose_ats(switch: &Fake, nic: &str) {
        switch.set_ext_capabilities(nic, &[(EXT_CAP_ATS, &[0, 0, 0, 0x80])]);
    }

    fn ctrl(switch: &Fake, port: &str) -> u16 {
        Config::open(&switch.sysfs, port)
            .unwrap()
            .acs()
            .unwrap()
            .unwrap()
            .ctrl
    }

    #[fixture]
    fn host() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn add_file(root: &TempDir, path: &str) {
        let path = root.path().join(path.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
    }

    #[rstest]
    fn the_plan_is_the_switch_ports_around_the_gpu_and_the_nic_below_them(switch: Fake) {
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        assert_eq!(
            plan.ports
                .iter()
                .map(|port| port.address.as_str())
                .collect::<Vec<_>>(),
            ["0000:16:00.0", "0000:16:01.0", "0000:19:00.0"]
        );
        assert!(plan.ports.iter().all(|port| port.direct_translated));
        assert_eq!(plan.ports[0].vendor, 0x15b3);
        assert_eq!(plan.ports[0].device, 0x197c);
        assert_eq!(plan.nics, ["0000:17:00.0"]);
    }

    /// A BlueField's DMA function is Mellanox but no NIC, and an Intel NIC
    /// has no ConnectX firmware setting to flip.
    #[rstest]
    fn only_connectx_functions_are_nics(switch: Fake) {
        add(&switch, Some("0000:16:00.0"), "0000:17:00.2", BLUEFIELD_DMA);
        add(
            &switch,
            Some("0000:16:01.0"),
            "0000:18:00.1",
            (0x8086, 0x1521, 0x020000),
        );

        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        assert_eq!(plan.nics, ["0000:17:00.0"]);
    }

    #[rstest]
    fn a_gpu_on_a_root_port_has_nothing_to_set(switch: Fake) {
        let fake = testfs::fake();
        fake.add_pci_device("0000:65:00.0", 0x10de, 0x2330, 0x030200, None);
        drop(switch);

        let plan = plan(&fake.sysfs, &gpu(&fake)).unwrap();

        assert_eq!(plan, Plan::default());
        assert!(rules(Path::new("/nowhere"), &plan).unwrap().is_empty());
        verify(&fake.sysfs, &plan).unwrap();
    }

    #[rstest]
    fn apply_adds_direct_translated_on_top_of_the_kernels_bits(switch: Fake) {
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        apply_acs(&switch.sysfs, &plan).unwrap();

        for port in ["0000:16:00.0", "0000:16:01.0", "0000:19:00.0"] {
            assert_eq!(
                ctrl(&switch, port),
                KERNEL_CTRL | ACS_DIRECT_TRANSLATED,
                "{port}"
            );
        }
        // Idempotent: a second pass finds it on and writes nothing.
        apply_acs(&switch.sysfs, &plan).unwrap();
        assert_eq!(
            ctrl(&switch, "0000:16:00.0"),
            KERNEL_CTRL | ACS_DIRECT_TRANSLATED
        );
    }

    /// A port that cannot do it is reported and left alone, not refused:
    /// the request still reaches the GPU, through the root complex.
    #[rstest]
    fn a_port_without_the_capability_is_skipped(switch: Fake) {
        switch.set_ext_capabilities(
            "0000:19:00.0",
            &[(EXT_CAP_ACS, &acs_body(0x1d, KERNEL_CTRL))],
        );
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();
        assert!(!plan.ports[2].direct_translated);

        apply_acs(&switch.sysfs, &plan).unwrap();
        expose_ats(&switch, "0000:17:00.0");
        verify(&switch.sysfs, &plan).unwrap();

        assert_eq!(ctrl(&switch, "0000:19:00.0"), KERNEL_CTRL);
    }

    #[rstest]
    fn verify_reads_the_bit_back(switch: Fake) {
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();
        expose_ats(&switch, "0000:17:00.0");

        let err = verify(&switch.sysfs, &plan).unwrap_err().to_string();
        assert!(err.contains("0000:16:00.0"), "{err}");
        assert!(err.contains("did not take"), "{err}");

        apply_acs(&switch.sysfs, &plan).unwrap();
        verify(&switch.sysfs, &plan).unwrap();
    }

    #[rstest]
    fn verify_needs_every_nic_to_expose_ats(switch: Fake) {
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();
        apply_acs(&switch.sysfs, &plan).unwrap();

        let err = verify(&switch.sysfs, &plan).unwrap_err().to_string();

        assert!(err.contains("0000:17:00.0"), "{err}");
        assert!(err.contains("ATS"), "{err}");
    }

    #[rstest]
    fn a_nic_that_exposes_ats_passes_the_preflight_without_a_tool(switch: Fake, host: TempDir) {
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();
        expose_ats(&switch, "0000:17:00.0");

        verify_ats(host.path(), &switch.sysfs, &plan).unwrap();
    }

    #[rstest]
    fn a_nic_without_ats_needs_the_firmware_tool(switch: Fake, host: TempDir) {
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        let err = verify_ats(host.path(), &switch.sysfs, &plan)
            .unwrap_err()
            .to_string();

        assert!(err.contains("mlxconfig"), "{err}");
        assert!(err.contains("mstconfig"), "{err}");
        assert!(err.contains("ATS_ENABLED=1"), "{err}");
    }

    /// The tool reads the function through the chroot's /sys.
    #[rstest]
    fn the_tool_needs_the_nodes_sysfs_under_the_host_root(switch: Fake, host: TempDir) {
        add_file(&host, "/usr/bin/mlxconfig");
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        let err = verify_ats(host.path(), &switch.sysfs, &plan)
            .unwrap_err()
            .to_string();

        assert!(err.contains("/sys"), "{err}");
        assert!(err.contains("0000:17:00.0"), "{err}");
    }

    #[rstest]
    #[case::off(
        "Device #1:\n\nConfigurations:          Next Boot\n        ATS_ENABLED      False(0)\n",
        Some(false)
    )]
    #[case::on("        ATS_ENABLED                                     True(1)                          \n", Some(true))]
    #[case::other_settings_only(
        "        NUM_OF_VFS                                      0\n",
        None
    )]
    #[case::garbled("ATS_ENABLED maybe\n", None)]
    fn the_query_is_read_from_the_settings_last_column(
        #[case] stdout: &str,
        #[case] expected: Option<bool>,
    ) {
        assert_eq!(parse_ats_query(stdout), expected);
    }

    /// Pinned verbatim: the mask form is what keeps the kernel's bits.
    #[rstest]
    fn the_rule_sets_the_one_bit_through_the_hosts_setpci(switch: Fake, host: TempDir) {
        add_file(&host, "/usr/bin/setpci");
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        let rules = rules(host.path(), &plan).unwrap();

        assert_eq!(rules.len(), 3);
        assert_eq!(
            rules[0],
            "ACTION==\"add\", SUBSYSTEM==\"pci\", KERNEL==\"0000:16:00.0\", \
             ATTR{vendor}==\"0x15b3\", ATTR{device}==\"0x197c\", \
             RUN+=\"/usr/bin/setpci -s %k ECAP_ACS+0x6.w=0x40:0x40\"\n"
        );
    }

    #[rstest]
    fn a_port_without_the_capability_gets_no_rule(switch: Fake, host: TempDir) {
        add_file(&host, "/sbin/setpci");
        switch.set_ext_capabilities(
            "0000:19:00.0",
            &[(EXT_CAP_ACS, &acs_body(0x1d, KERNEL_CTRL))],
        );
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        let rules = rules(host.path(), &plan).unwrap();

        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|rule| rule.contains("/sbin/setpci")));
        assert!(!rules.iter().any(|rule| rule.contains("0000:19:00.0")));
    }

    #[rstest]
    fn the_rule_needs_setpci_on_the_node(switch: Fake, host: TempDir) {
        let plan = plan(&switch.sysfs, &gpu(&switch)).unwrap();

        let err = rules(host.path(), &plan).unwrap_err().to_string();

        assert!(err.contains("pciutils"), "{err}");
    }

    #[rstest]
    fn status_reports_a_line_per_port_and_nic(switch: Fake) {
        switch.set_ext_capabilities(
            "0000:19:00.0",
            &[(EXT_CAP_ACS, &acs_body(0x1d, KERNEL_CTRL))],
        );
        let states = gpu(&switch);
        let before = report(&switch.sysfs, &states).unwrap();
        assert_eq!(
            before,
            [
                "p2p port 0000:16:00.0  0x15b3:197c  direct-translated=off",
                "p2p port 0000:16:01.0  0x15b3:197c  direct-translated=off",
                "p2p port 0000:19:00.0  0x15b3:197c  direct-translated=unsupported",
                "p2p nic  0000:17:00.0  ats=absent",
            ]
        );

        apply_acs(&switch.sysfs, &plan(&switch.sysfs, &states).unwrap()).unwrap();
        expose_ats(&switch, "0000:17:00.0");
        let after = report(&switch.sysfs, &states).unwrap();
        assert_eq!(
            after[0],
            "p2p port 0000:16:00.0  0x15b3:197c  direct-translated=on"
        );
        assert_eq!(after[3], "p2p nic  0000:17:00.0  ats=enabled");

        switch.set_ext_capabilities("0000:17:00.0", &[(EXT_CAP_ATS, &[0, 0, 0, 0])]);
        assert_eq!(
            report(&switch.sysfs, &states).unwrap()[3],
            "p2p nic  0000:17:00.0  ats=exposed"
        );
    }
}
