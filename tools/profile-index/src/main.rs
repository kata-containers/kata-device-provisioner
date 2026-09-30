use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    families: Vec<FamilySource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FamilySource {
    id: String,
    display_name: String,
    model_examples: Vec<String>,
    profiles: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Values {
    profiles: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    schema_version: u32,
    families: Vec<Family>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Family {
    id: String,
    display_name: String,
    model_examples: Vec<String>,
    profiles: Vec<Profile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    name: String,
    cc_mode: String,
    values_file: String,
    values_sha256: String,
}

fn main() -> Result<()> {
    let check = match env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => bail!("usage: generate-profile-index [--check]"),
    };
    let profiles = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("deploy/helm/kata-device-provisioner/profiles");
    generate(&profiles, check)
}

fn generate(profiles: &Path, check: bool) -> Result<()> {
    let catalog: Catalog = serde_json::from_str(
        &fs::read_to_string(profiles.join("catalog.json")).context("read catalog.json")?,
    )
    .context("parse catalog.json")?;
    if catalog.schema_version != 1 {
        bail!("unsupported catalog schema");
    }

    let mut family_ids = HashSet::new();
    let mut profile_names = HashSet::new();
    let mut families = Vec::new();
    for family in catalog.families {
        if family.id.is_empty() || !family_ids.insert(family.id.clone()) {
            bail!("empty or duplicate family id: {}", family.id);
        }
        if family.display_name.is_empty() || family.profiles.is_empty() {
            bail!("family {} needs a display name and profiles", family.id);
        }
        let mut entries = Vec::new();
        for name in family.profiles {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
                || !profile_names.insert(name.clone())
            {
                bail!("invalid or duplicate profile name: {name}");
            }
            let values_file = format!("profiles/{name}.values.yaml");
            let content = fs::read_to_string(profiles.join(format!("{name}.values.yaml")))
                .with_context(|| format!("read {values_file}"))?;
            let values: Values =
                serde_yaml::from_str(&content).with_context(|| format!("parse {values_file}"))?;
            if values.profiles.len() != 1 {
                bail!("{values_file} must contain one profile");
            }
            let profile = values
                .profiles
                .get(&name)
                .with_context(|| format!("{values_file} must contain profiles.{name}"))?;
            let enabled = profile.get("enabled").and_then(serde_json::Value::as_bool);
            let cc_mode = profile.get("ccMode").and_then(serde_json::Value::as_str);
            if enabled != Some(true)
                || !cc_mode.is_some_and(|mode| ["off", "on", "devtools", "ppcie"].contains(&mode))
            {
                bail!("{values_file} has a disabled profile or invalid ccMode");
            }
            let digest = Sha256::digest(serde_json::to_vec(profile)?);
            entries.push(Profile {
                name,
                cc_mode: cc_mode.context("missing ccMode")?.to_owned(),
                values_file,
                values_sha256: format!("{digest:x}"),
            });
        }
        families.push(Family {
            id: family.id,
            display_name: family.display_name,
            model_examples: family.model_examples,
            profiles: entries,
        });
    }
    for entry in fs::read_dir(profiles).context("list profile values")? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(name) = name.strip_suffix(".values.yaml") {
            if name != "MIXED-FLEET" && !profile_names.contains(name) {
                bail!("unlisted deployable profile: {name}");
            }
        }
    }
    let output = format!(
        "{}\n",
        serde_json::to_string_pretty(&Index {
            schema_version: 1,
            families,
        })?
    );
    let path = profiles.join("index.json");
    if check {
        if fs::read_to_string(&path).context("read generated index.json")? != output {
            bail!("profiles/index.json is stale; run cargo run --manifest-path tools/profile-index/Cargo.toml");
        }
    } else {
        fs::write(&path, output).context("write generated index.json")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::{fixture, rstest};
    use tempfile::TempDir;

    const CATALOG: &str = r#"{"schemaVersion":1,"families":[{"id":"hopper","displayName":"Hopper","modelExamples":["H100"],"profiles":["H100"]}]}"#;
    const VALUES: &str =
        "profiles:\n  H100:\n    enabled: true\n    ccMode: on\n    job:\n      parallelism: 2\n";

    #[fixture]
    fn node() -> TempDir {
        let node = TempDir::new().unwrap();
        fs::write(node.path().join("catalog.json"), CATALOG).unwrap();
        fs::write(node.path().join("H100.values.yaml"), VALUES).unwrap();
        node
    }

    #[rstest]
    #[case::malformed_catalog("{", VALUES, None, "parse catalog.json")]
    #[case::duplicate_family(r#"{"schemaVersion":1,"families":[{"id":"hopper","displayName":"Hopper","modelExamples":[],"profiles":["H100"]},{"id":"hopper","displayName":"Other","modelExamples":[],"profiles":["H200"]}]}"#, VALUES, None, "duplicate family id")]
    #[case::duplicate_profile(r#"{"schemaVersion":1,"families":[{"id":"hopper","displayName":"Hopper","modelExamples":[],"profiles":["H100","H100"]}]}"#, VALUES, None, "duplicate profile name")]
    #[case::unlisted_profile(
        CATALOG,
        VALUES,
        Some("H200.values.yaml"),
        "unlisted deployable profile"
    )]
    #[case::extra_helm_values(
        CATALOG,
        "profiles:\n  H100:\n    enabled: true\n    ccMode: on\njob:\n  parallelism: 3\n",
        None,
        "unknown field `job`"
    )]
    #[case::wrong_profile(
        CATALOG,
        "profiles:\n  H200:\n    enabled: true\n    ccMode: on\n",
        None,
        "must contain profiles.H100"
    )]
    #[case::disabled_profile(
        CATALOG,
        "profiles:\n  H100:\n    enabled: false\n    ccMode: on\n",
        None,
        "disabled profile or invalid ccMode"
    )]
    fn rejects_invalid_inputs(
        node: TempDir,
        #[case] catalog: &str,
        #[case] values: &str,
        #[case] extra_file: Option<&str>,
        #[case] expected: &str,
    ) {
        fs::write(node.path().join("catalog.json"), catalog).unwrap();
        fs::write(node.path().join("H100.values.yaml"), values).unwrap();
        if let Some(file) = extra_file {
            fs::write(node.path().join(file), VALUES).unwrap();
        }

        let error = generate(node.path(), false).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }

    #[rstest]
    fn checks_stale_output_and_setting_changes(node: TempDir) {
        generate(node.path(), false).unwrap();
        let first = fs::read_to_string(node.path().join("index.json")).unwrap();
        generate(node.path(), true).unwrap();

        fs::write(
            node.path().join("H100.values.yaml"),
            VALUES.replace("parallelism: 2", "parallelism: 3"),
        )
        .unwrap();
        let error = generate(node.path(), true).unwrap_err();
        assert!(format!("{error:#}").contains("stale"));

        generate(node.path(), false).unwrap();
        let updated = fs::read_to_string(node.path().join("index.json")).unwrap();
        assert_ne!(first, updated);
        generate(node.path(), true).unwrap();
    }
}
