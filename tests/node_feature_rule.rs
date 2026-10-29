// Copyright (c) 2026 Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! The chart's NodeFeatureRule is generated from the PCI identity catalog in
//! pcilibs-rs, so NFD selects nodes by the same identities the binary
//! classifies with, and a hand-kept device-id list cannot drift from them.
//!
//! `UPDATE_NODE_FEATURE_RULE=1 cargo test --test node_feature_rule` rewrites
//! the template; a plain run fails when the template and the catalog disagree.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use pcilibs_rs::catalog::{Attachment, Catalog, DeviceKind};
use pcilibs_rs::gpu::{Family, CHIPS};
use rstest::rstest;

const TEMPLATE: &str = "deploy/helm/kata-device-provisioner/templates/node-feature-rule.yaml";
const LABELS: &str = "kata.feature.node.kubernetes.io";
const NVIDIA: u16 = 0x10de;
const MELLANOX: u16 = 0x15b3;
/// 3D controller: how every data-centre GPU in the catalog enumerates.
const GPU_CLASS: &str = "0302";
const NVSWITCH_CLASS: &str = "0680";
/// InfiniBand controller: the class of the management ConnectX PFs, the one
/// the device table admits. VPD, not PCI identity, tells a management PF from
/// the data-path NIC beside it, and only the Job on the node reads that.
const NIC_CLASSES: &[&str] = &["0207"];

/// The subsystem ids the catalog gives one device id for one attachment.
/// `None` is the catalog's `*`: the device id alone decides.
type Subsystems = BTreeSet<Option<u16>>;

#[derive(Default)]
struct Identity {
    family: Option<Family>,
    sxm: Subsystems,
    pcie: Subsystems,
    coherent: Subsystems,
}

fn gpus() -> BTreeMap<u16, Identity> {
    let mut gpus: BTreeMap<u16, Identity> = BTreeMap::new();
    for entry in Catalog::builtin().entries() {
        if entry.vendor != NVIDIA || entry.properties.kind != DeviceKind::Gpu {
            continue;
        }
        let identity = gpus.entry(entry.device).or_default();
        let family = CHIPS
            .iter()
            .find(|chip| chip.name == entry.properties.profile)
            .map(|chip| chip.family);
        assert!(
            identity.family.is_none() || identity.family == family,
            "{:04x} is catalogued under two chip families",
            entry.device
        );
        identity.family = family;
        let attachments = match entry.properties.attachment {
            Attachment::Sxm => &mut identity.sxm,
            Attachment::Pcie => &mut identity.pcie,
            Attachment::Coherent => &mut identity.coherent,
            Attachment::Unknown => continue,
        };
        attachments.insert(entry.subsystem_device);
    }
    gpus
}

/// One `matchFeatures` entry: these device ids, narrowed to these subsystem
/// ids where one device id is sold as more than one attachment.
#[derive(Debug, PartialEq, Eq)]
struct Branch {
    devices: Vec<u16>,
    subsystems: Vec<u16>,
}

#[derive(Clone, Copy)]
enum Wanted {
    Sxm,
    Coherent,
    /// The Rubin HGX profile takes coherent parts too: on Rubin, attachment
    /// does not decide whether CC can be enabled.
    SxmOrCoherent,
}

/// Device ids whose every catalogued attachment is wanted match on the device
/// id alone, in one branch. A device id that is also sold with another
/// attachment gets a branch of its own, keyed on subsystem id as well.
fn select(gpus: &BTreeMap<u16, Identity>, family: Option<Family>, wanted: Wanted) -> Vec<Branch> {
    let mut plain = Vec::new();
    let mut narrowed = Vec::new();
    for (&device, identity) in gpus {
        if family.is_some() && identity.family != family {
            continue;
        }
        let (yes, no): (Vec<&Subsystems>, Vec<&Subsystems>) = match wanted {
            Wanted::Sxm => (
                vec![&identity.sxm],
                vec![&identity.pcie, &identity.coherent],
            ),
            Wanted::Coherent => (
                vec![&identity.coherent],
                vec![&identity.sxm, &identity.pcie],
            ),
            Wanted::SxmOrCoherent => (
                vec![&identity.sxm, &identity.coherent],
                vec![&identity.pcie],
            ),
        };
        let wanted_ids: Subsystems = yes.into_iter().flatten().copied().collect();
        if wanted_ids.is_empty() {
            continue;
        }
        if no.into_iter().all(|set| set.is_empty()) {
            plain.push(device);
            continue;
        }
        let subsystems: Vec<u16> = wanted_ids
            .iter()
            .map(|subsystem| {
                subsystem.unwrap_or_else(|| {
                    panic!(
                        "{device:04x}: a wildcard subsystem cannot be told from another attachment"
                    )
                })
            })
            .collect();
        narrowed.push(Branch {
            devices: vec![device],
            subsystems,
        });
    }
    let mut branches = Vec::new();
    if !plain.is_empty() {
        branches.push(Branch {
            devices: plain,
            subsystems: Vec::new(),
        });
    }
    branches.extend(narrowed);
    branches
}

/// Every device id with a coherent variant, by device id alone: the same
/// conservative answer `pcilibs_rs::cc::is_c2c` gives the binary, which has
/// no subsystem id to hand, so the PCIe profiles never select a node it would
/// then refuse.
fn maybe_coherent(gpus: &BTreeMap<u16, Identity>) -> Branch {
    Branch {
        devices: gpus
            .iter()
            .filter(|(_, identity)| !identity.coherent.is_empty())
            .map(|(&device, _)| device)
            .collect(),
        subsystems: Vec::new(),
    }
}

fn hex(ids: &[u16]) -> String {
    ids.iter()
        .map(|id| format!("\"{id:04x}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quoted(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `matchExpressions` on one PCI function. NFD evaluates every expression
/// against the same instance, so device and subsystem id are a pair here.
fn pci_match(
    out: &mut String,
    indent: usize,
    vendor: u16,
    classes: &[&str],
    branch: Option<&Branch>,
) {
    let pad = " ".repeat(indent);
    writeln!(out, "{pad}- feature: pci.device").unwrap();
    writeln!(out, "{pad}  matchExpressions:").unwrap();
    writeln!(
        out,
        "{pad}    vendor: {{ op: In, value: [\"{vendor:04x}\"] }}"
    )
    .unwrap();
    writeln!(
        out,
        "{pad}    class: {{ op: In, value: [{}] }}",
        quoted(classes)
    )
    .unwrap();
    if let Some(branch) = branch {
        writeln!(
            out,
            "{pad}    device: {{ op: In, value: [{}] }}",
            hex(&branch.devices)
        )
        .unwrap();
        if !branch.subsystems.is_empty() {
            writeln!(
                out,
                "{pad}    subsystem_device: {{ op: In, value: [{}] }}",
                hex(&branch.subsystems)
            )
            .unwrap();
        }
    }
}

fn rule_header(out: &mut String, name: &str, comment: &str) {
    for line in comment.lines() {
        writeln!(out, "    # {line}").unwrap();
    }
    writeln!(out, "    - name: \"nvidia.{name}\"").unwrap();
    writeln!(out, "      labels:").unwrap();
    writeln!(out, "        \"{LABELS}/nvidia-{name}\": \"true\"").unwrap();
}

/// A single branch is a plain `matchFeatures`; several are a `matchAny`, of
/// which NFD requires one to hold.
fn gpu_rule(out: &mut String, name: &str, comment: &str, branches: &[Branch]) {
    assert!(!branches.is_empty(), "nvidia.{name} selects nothing");
    rule_header(out, name, comment);
    match branches {
        [branch] => {
            writeln!(out, "      matchFeatures:").unwrap();
            pci_match(out, 8, NVIDIA, &[GPU_CLASS], Some(branch));
        }
        _ => {
            writeln!(out, "      matchAny:").unwrap();
            gpu_branches(out, branches);
        }
    }
}

fn gpu_branches(out: &mut String, branches: &[Branch]) {
    for branch in branches {
        writeln!(out, "        - matchFeatures:").unwrap();
        pci_match(out, 12, NVIDIA, &[GPU_CLASS], Some(branch));
    }
}

fn render() -> String {
    let gpus = gpus();
    let hopper = select(&gpus, Some(Family::Hopper), Wanted::Sxm);
    let blackwell = select(&gpus, Some(Family::Blackwell), Wanted::Sxm);
    let rubin = select(&gpus, Some(Family::Rubin), Wanted::SxmOrCoherent);
    let coherent = [maybe_coherent(&gpus)];
    let grace_blackwell = select(&gpus, Some(Family::Blackwell), Wanted::Coherent);

    let mut out = String::from(
        "{{- /*
Generated from the PCI identity catalog in pcilibs-rs by
`UPDATE_NODE_FEATURE_RULE=1 cargo test --test node_feature_rule`. Do not edit
by hand: `cargo test` fails when this file and the catalog disagree.

The labels select candidates for a profile's Job. They prove PCI identity and
nothing else: only the provisioner's VPD discovery on the node can tell a
ConnectX management PF from the NIC beside it, and a candidate label never
authorizes a binding.

GPU identities are the catalog's, keyed on device id and, where one device id
is sold as more than one attachment, on subsystem device id as well. That is
the same catalog the binary classifies devices with, so a part the catalog
does not know is a node that is not selected, never one that is mislabelled.

Cluster-scoped with no node-specific content, so the release owns it and
`helm uninstall` takes it away.
*/ -}}
{{- if include \"kata-device-provisioner.nodeFeatureRuleEnabled\" . | trim }}
apiVersion: nfd.k8s-sigs.io/v1alpha1
kind: NodeFeatureRule
metadata:
  name: {{ .Chart.Name }}
  labels:
{{- include \"kata-device-provisioner.labels\" . | nindent 4 }}
spec:
  rules:
",
    );

    rule_header(
        &mut out,
        "gpu",
        "Any NVIDIA 3D controller: the PCIe profiles' base match.",
    );
    writeln!(out, "      matchFeatures:").unwrap();
    pci_match(&mut out, 8, NVIDIA, &[GPU_CLASS], None);

    rule_header(
        &mut out,
        "nvswitch",
        "A host-visible NVSwitch, which is what makes an Hx00 baseboard.\n\
         NFD does not label class 0680 by default, so this rule does.",
    );
    writeln!(out, "      matchFeatures:").unwrap();
    pci_match(&mut out, 8, NVIDIA, &[NVSWITCH_CLASS], None);

    gpu_rule(
        &mut out,
        "hopper",
        "SXM Hopper parts: the GPU half of an HGX Hx00 baseboard. Coherent GH200\n\
         and PCIe cards are not HGX and are left out.",
        &hopper,
    );
    gpu_rule(
        &mut out,
        "blackwell",
        "SXM Blackwell parts: the GPU half of an HGX Bx00 baseboard. A device id\n\
         sold both as SXM and as a coherent or PCIe part is narrowed by subsystem.",
        &blackwell,
    );
    gpu_rule(
        &mut out,
        "rubin",
        "SXM and coherent Rubin parts. On Rubin, attachment does not decide\n\
         whether CC can be enabled, so the Rx00 profile takes both.",
        &rubin,
    );
    gpu_rule(
        &mut out,
        "c2c",
        "Every device id the catalog knows a coherent variant of, by device id\n\
         alone, subsystem notwithstanding: the same conservative answer\n\
         `pcilibs_rs::cc::is_c2c` gives the binary, so the PCIe profiles never\n\
         select a node whose GPU the binary would then refuse plain vfio-pci for.",
        &coherent,
    );
    gpu_rule(
        &mut out,
        "grace-blackwell",
        "Coherent Blackwell: GB200 and GB300 superchips, which GBx00 selects.",
        &grace_blackwell,
    );

    rule_header(
        &mut out,
        "managed-fabric-candidate",
        "An SXM Blackwell or Rubin GPU beside an InfiniBand ConnectX function.\n\
         On those baseboards the NVSwitches are not on PCI and the fabric is\n\
         reached through ConnectX management PFs, which NFD cannot tell from\n\
         a NIC: the Job qualifies them by VPD and refuses the node otherwise.",
    );
    writeln!(out, "      matchFeatures:").unwrap();
    pci_match(&mut out, 8, MELLANOX, NIC_CLASSES, None);
    writeln!(out, "      matchAny:").unwrap();
    gpu_branches(&mut out, &blackwell);
    gpu_branches(&mut out, &rubin);

    out.push_str("{{- end }}\n");
    out
}

#[test]
fn template_matches_the_catalog() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(TEMPLATE);
    let expected = render();
    if std::env::var_os("UPDATE_NODE_FEATURE_RULE").is_some() {
        fs::write(&path, &expected).unwrap();
    }
    let actual = fs::read_to_string(&path).unwrap();
    assert!(
        actual == expected,
        "{TEMPLATE} disagrees with the pcilibs-rs catalog: run \
         UPDATE_NODE_FEATURE_RULE=1 cargo test --test node_feature_rule"
    );
}

fn branch_for(branches: &[Branch], device: u16) -> Option<&Branch> {
    branches
        .iter()
        .find(|branch| branch.devices.contains(&device))
}

/// The ids a hand-kept list got wrong or could not express at all.
#[rstest]
#[case::hgx_b300_is_sxm_by_subsystem(Family::Blackwell, Wanted::Sxm, 0x31c2, Some(vec![0x20e5, 0x20e6]))]
#[case::gb300_is_coherent_by_subsystem(Family::Blackwell, Wanted::Coherent, 0x31c2, Some(vec![0x21f1]))]
#[case::b200_sxm_needs_no_subsystem(Family::Blackwell, Wanted::Sxm, 0x2901, Some(vec![]))]
#[case::gb200_is_coherent_only(Family::Blackwell, Wanted::Coherent, 0x2941, Some(vec![]))]
#[case::gb300_variant_31a1_is_coherent(Family::Blackwell, Wanted::Coherent, 0x31a1, Some(vec![]))]
#[case::pcie_b200_is_not_hgx(Family::Blackwell, Wanted::Sxm, 0x2924, None)]
#[case::gb200_is_not_hgx(Family::Blackwell, Wanted::Sxm, 0x2941, None)]
#[case::h100_sxm(Family::Hopper, Wanted::Sxm, 0x2330, Some(vec![]))]
#[case::h100_pcie_is_not_hgx(Family::Hopper, Wanted::Sxm, 0x2331, None)]
#[case::gh200_is_not_hgx(Family::Hopper, Wanted::Sxm, 0x2342, None)]
#[case::coherent_rubin_stays_in_rx00(Family::Rubin, Wanted::SxmOrCoherent, 0x3041, Some(vec![]))]
fn selection_follows_the_catalog(
    #[case] family: Family,
    #[case] wanted: Wanted,
    #[case] device: u16,
    #[case] subsystems: Option<Vec<u16>>,
) {
    let branches = select(&gpus(), Some(family), wanted);
    let branch = branch_for(&branches, device);
    assert_eq!(branch.map(|branch| branch.subsystems.clone()), subsystems);
}

#[rstest]
#[case::gh200(0x2342)]
#[case::gb200(0x2941)]
#[case::gb300(0x31c2)]
#[case::gb300_variant(0x31a1)]
#[case::rubin(0x3041)]
fn every_coherent_part_is_c2c(#[case] device: u16) {
    assert!(
        maybe_coherent(&gpus()).devices.contains(&device),
        "{device:04x}"
    );
}

/// `is_c2c` cannot see a subsystem id, so neither does the label it mirrors:
/// a PCIe or SXM part sharing its device id with a coherent one is excluded
/// from the PCIe profiles rather than selected and refused.
#[rstest]
#[case::b200a(0x29bc)]
#[case::hgx_b300(0x31c2)]
fn c2c_stays_device_only_like_is_c2c(#[case] device: u16) {
    let c2c = maybe_coherent(&gpus());
    assert!(c2c.devices.contains(&device));
    assert!(c2c.subsystems.is_empty());
}

#[rstest]
fn a_family_label_never_takes_another_family_s_parts() {
    let gpus = gpus();
    for (family, other) in [
        (Family::Hopper, Family::Blackwell),
        (Family::Blackwell, Family::Rubin),
        (Family::Rubin, Family::Hopper),
    ] {
        for branch in select(&gpus, Some(family), Wanted::SxmOrCoherent) {
            for device in branch.devices {
                assert_ne!(gpus[&device].family, Some(other), "{device:04x}");
            }
        }
    }
}
