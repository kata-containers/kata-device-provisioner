// Copyright (c) Kata Containers contributors
//
// SPDX-License-Identifier: Apache-2.0

//! The chart's NodeFeatureRule is generated from the PCI identity catalog in
//! pcilibs-rs, so NFD selects nodes by the same identities the binary
//! classifies with. A hand-kept device-id list diverged from them.
//!
//! `UPDATE_NODE_FEATURE_RULE=1 cargo test node_feature_rule` rewrites
//! the template; a plain run fails when the template and the catalog disagree.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use crate::device::{Provisioning, DEVICES};
use pcilibs_rs::catalog::{Attachment, Catalog, DeviceKind};
use pcilibs_rs::gpu::{Family, CHIPS};
use rstest::rstest;

const TEMPLATE: &str = "deploy/helm/kata-device-provisioner/templates/node-feature-rule.yaml";
const LABELS: &str = "kata.feature.node.kubernetes.io";

/// Vendor and class from `DEVICES`, so the rule selects what the binary
/// provisions. For a management PF that is the InfiniBand class only: VPD
/// tells it from the data-path NIC beside it, and only the Job reads that.
fn identity(provisioning: Provisioning) -> (u16, u16) {
    let row = DEVICES
        .iter()
        .find(|row| row.provisioning == provisioning)
        .expect("every provisioning kind has a DEVICES row");
    (row.vendor, row.class_prefix)
}

/// One catalogued sale of a device id. `None` is the catalog's `*`: the
/// device id alone decides the attachment.
type Variant = (Attachment, Option<u16>);

#[derive(Default)]
struct Identity {
    family: Option<Family>,
    variants: Vec<Variant>,
}

fn gpus() -> BTreeMap<u16, Identity> {
    let (nvidia, _) = identity(Provisioning::NvidiaInBandCc);
    let mut gpus: BTreeMap<u16, Identity> = BTreeMap::new();
    for entry in Catalog::builtin().entries() {
        if entry.vendor != nvidia
            || entry.properties.kind != DeviceKind::Gpu
            || entry.properties.attachment == Attachment::Unknown
        {
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
        identity
            .variants
            .push((entry.properties.attachment, entry.subsystem_device));
    }
    gpus
}

/// Empty `subsystems` means the device ids alone decide the match.
#[derive(Debug, PartialEq, Eq)]
struct Branch {
    devices: Vec<u16>,
    subsystems: Vec<u16>,
}

/// A device id sold only in wanted attachments matches on the id alone, in
/// one branch with the others like it. NFD cannot tell the attachments of a
/// device id sold in an unwanted one too without the subsystem id, so such an
/// id gets a branch of its own.
fn select(gpus: &BTreeMap<u16, Identity>, family: Family, wanted: &[Attachment]) -> Vec<Branch> {
    let mut plain = Vec::new();
    let mut narrowed = Vec::new();
    for (&device, identity) in gpus {
        if identity.family != Some(family) {
            continue;
        }
        let (yes, no): (Vec<Variant>, Vec<Variant>) = identity
            .variants
            .iter()
            .partition(|(attachment, _)| wanted.contains(attachment));
        if yes.is_empty() {
            continue;
        }
        if no.is_empty() {
            plain.push(device);
            continue;
        }
        let subsystems: BTreeSet<u16> = yes
            .iter()
            .map(|(_, subsystem)| {
                subsystem.unwrap_or_else(|| {
                    panic!(
                        "{device:04x}: a wildcard subsystem cannot be told from another attachment"
                    )
                })
            })
            .collect();
        narrowed.push(Branch {
            devices: vec![device],
            subsystems: subsystems.into_iter().collect(),
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

/// By device id alone, as `pcilibs_rs::cc::is_c2c` answers for the binary,
/// which reads no subsystem id. A narrower label would let a PCIe profile
/// select a node whose GPU the binary then refuses plain vfio-pci for.
fn maybe_coherent(gpus: &BTreeMap<u16, Identity>) -> Branch {
    Branch {
        devices: gpus
            .iter()
            .filter(|(_, identity)| {
                identity
                    .variants
                    .iter()
                    .any(|(attachment, _)| *attachment == Attachment::Coherent)
            })
            .map(|(&device, _)| device)
            .collect(),
        subsystems: Vec::new(),
    }
}

const SXM: &[Attachment] = &[Attachment::Sxm];
const COHERENT: &[Attachment] = &[Attachment::Coherent];
/// On Rubin, attachment does not decide whether CC can be enabled, so the
/// Rx00 profile takes coherent parts too.
const SXM_OR_COHERENT: &[Attachment] = &[Attachment::Sxm, Attachment::Coherent];

fn hex(ids: &[u16]) -> String {
    ids.iter()
        .map(|id| format!("\"{id:04x}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// NFD evaluates every expression of one `matchExpressions` against the same
/// PCI function, so device and subsystem id match as a pair.
fn pci_match(out: &mut String, indent: usize, provisioning: Provisioning, branch: Option<&Branch>) {
    let (vendor, class) = identity(provisioning);
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
        "{pad}    class: {{ op: In, value: [\"{class:04x}\"] }}"
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

/// NFD requires every `matchFeatures` entry to hold but only one `matchAny`
/// entry, so alternative branches go under `matchAny`.
fn gpu_rule(out: &mut String, name: &str, comment: &str, branches: &[Branch]) {
    assert!(!branches.is_empty(), "nvidia.{name} selects nothing");
    rule_header(out, name, comment);
    match branches {
        [branch] => {
            writeln!(out, "      matchFeatures:").unwrap();
            pci_match(out, 8, Provisioning::NvidiaInBandCc, Some(branch));
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
        pci_match(out, 12, Provisioning::NvidiaInBandCc, Some(branch));
    }
}

fn render() -> String {
    let gpus = gpus();
    let hopper = select(&gpus, Family::Hopper, SXM);
    let blackwell = select(&gpus, Family::Blackwell, SXM);
    let rubin = select(&gpus, Family::Rubin, SXM_OR_COHERENT);
    let coherent = [maybe_coherent(&gpus)];
    let grace_blackwell = select(&gpus, Family::Blackwell, COHERENT);

    let mut out = String::from(
        "{{- /*
Generated from the PCI identity catalog in pcilibs-rs by
`UPDATE_NODE_FEATURE_RULE=1 cargo test node_feature_rule`. Do not edit
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
    pci_match(&mut out, 8, Provisioning::NvidiaInBandCc, None);

    rule_header(
        &mut out,
        "nvswitch",
        "A host-visible NVSwitch, which is what makes an Hx00 baseboard.\n\
         NFD does not label class 0680 by default, so this rule does.",
    );
    writeln!(out, "      matchFeatures:").unwrap();
    pci_match(&mut out, 8, Provisioning::NvidiaInBandPpcie, None);

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
    pci_match(&mut out, 8, Provisioning::FabricManagement, None);
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
         UPDATE_NODE_FEATURE_RULE=1 cargo test node_feature_rule"
    );
}

fn branch_for(branches: &[Branch], device: u16) -> Option<&Branch> {
    branches
        .iter()
        .find(|branch| branch.devices.contains(&device))
}

/// Device ids sold in more than one attachment, which a list of device ids
/// alone cannot express.
#[rstest]
#[case::hgx_b300_is_sxm_by_subsystem(Family::Blackwell, SXM, 0x31c2, Some(vec![0x20e5, 0x20e6]))]
#[case::gb300_is_coherent_by_subsystem(Family::Blackwell, COHERENT, 0x31c2, Some(vec![0x21f1]))]
#[case::b200_sxm_needs_no_subsystem(Family::Blackwell, SXM, 0x2901, Some(vec![]))]
#[case::gb200_is_coherent_only(Family::Blackwell, COHERENT, 0x2941, Some(vec![]))]
#[case::gb300_variant_31a1_is_coherent(Family::Blackwell, COHERENT, 0x31a1, Some(vec![]))]
#[case::pcie_b200_is_not_hgx(Family::Blackwell, SXM, 0x2924, None)]
#[case::gb200_is_not_hgx(Family::Blackwell, SXM, 0x2941, None)]
#[case::h100_sxm(Family::Hopper, SXM, 0x2330, Some(vec![]))]
#[case::h100_pcie_is_not_hgx(Family::Hopper, SXM, 0x2331, None)]
#[case::gh200_is_not_hgx(Family::Hopper, SXM, 0x2342, None)]
#[case::coherent_rubin_stays_in_rx00(Family::Rubin, SXM_OR_COHERENT, 0x3041, Some(vec![]))]
fn selection_follows_the_catalog(
    #[case] family: Family,
    #[case] wanted: &[Attachment],
    #[case] device: u16,
    #[case] subsystems: Option<Vec<u16>>,
) {
    let branches = select(&gpus(), family, wanted);
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

/// `is_c2c` reads no subsystem id, so the label it mirrors has none either:
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
        for branch in select(&gpus, family, SXM_OR_COHERENT) {
            for device in branch.devices {
                assert_ne!(gpus[&device].family, Some(other), "{device:04x}");
            }
        }
    }
}
