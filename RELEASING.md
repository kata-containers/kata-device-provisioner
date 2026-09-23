# Releasing kata-device-provisioner

A release is one click. The version comes from what is committed on `main`,
and [`release.yml`](.github/workflows/release.yml) builds, publishes and tags
it. Nobody pushes a tag by hand.

## Versions

The version is [SemVer](https://semver.org), and only these forms are
accepted:

| Version          | Tag              | Kind       |
|------------------|------------------|------------|
| `X.Y.Z`          | `vX.Y.Z`         | release    |
| `X.Y.Z-alpha.N`  | `vX.Y.Z-alpha.N` | prerelease |
| `X.Y.Z-beta.N`   | `vX.Y.Z-beta.N`  | prerelease |
| `X.Y.Z-rc.N`     | `vX.Y.Z-rc.N`    | prerelease |

`N` starts at `0`, and leading zeros are refused. The workflow rejects any
other version, including a `+build` suffix, before it builds anything.

The version is read from `Cargo.toml`, and it has to appear in three places.
The workflow fails if any of them disagree:

- `version` in `Cargo.toml`, with `Cargo.lock` matching it
- `version` in `deploy/helm/kata-device-provisioner/Chart.yaml`
- `appVersion` in the same file

The chart and the binary share one version. The chart deploys the image
matching its `appVersion` unless `image.tag` is set, so a chart always pulls
the image it was released with.

## What a release publishes

| Artifact | Where | Release | Prerelease |
|---|---|---|---|
| Multi-arch image (amd64, arm64) | `ghcr.io/kata-containers/kata-device-provisioner` | `:X.Y.Z`, `:X.Y`, `:latest` | `:X.Y.Z-rc.N` only |
| Helm chart | `oci://ghcr.io/kata-containers/kata-device-provisioner-charts/kata-device-provisioner` | `X.Y.Z` | `X.Y.Z-rc.N` |
| Git tag and GitHub release | this repository | marked latest | marked prerelease |

A prerelease never moves `:latest` or `:X.Y`, and it is never the release that
"latest" resolves to on GitHub.

Helm keeps prereleases out of the way too, though the workflow doesn't have
to do anything for that. When `helm install` or `helm upgrade` is given no
`--version`, Helm installs the highest chart version that matches the
constraint `*`. Under SemVer rules, a version with a `-` suffix only matches a
constraint that names a prerelease itself, so `*` never matches `0.2.0-rc.0`.
A user who doesn't ask for a version gets the newest final release, even when
a newer release candidate exists. To install a prerelease, do one of the
following:

- Pin it: `--version 0.2.0-rc.0`.
- Pass `--devel`, which makes Helm use the constraint `>0.0.0-0` instead, and
  that matches prereleases as well.

The GitHub release attaches the chart tarball and a `SHA256SUMS` file. Its
notes are generated from merged pull requests. For a final release they cover
everything since the previous final release, so the notes for `v0.2.0`
include what went into its alphas, betas and release candidates.

Every image digest (the index and each architecture in it) and the chart
tarball get a build-provenance attestation.

## Cutting a release

```mermaid
flowchart TD
    bump["Open a version bump PR against main<br/>Cargo.toml, Cargo.lock, Chart.yaml"]
    merge["CI passes, PR merged"]
    click["Actions → Release → Run workflow<br/>branch: main"]
    preflight{"Preflight<br/>version format, versions agree,<br/>not released yet, tests, helm lint"}
    fix["Fix it in a new PR"]
    build["Build amd64 and arm64 natively<br/>push by digest, smoke test, attest"]
    publish["Tag the multi-arch image<br/>push the chart to GHCR"]
    release["Create tag vX.Y.Z[-alpha|beta|rc.N]<br/>and the GitHub release"]
    kind{"Prerelease?"}
    next(["Next alpha, beta or rc,<br/>or X.Y.Z when ready"])
    done(["Released"])

    bump --> merge --> click --> preflight
    preflight -- "refused, nothing published" --> fix --> bump
    preflight -- "ok" --> build --> publish --> release --> kind
    kind -- "yes" --> next --> bump
    kind -- "no" --> done
```

The example below cuts `v0.2.0-rc.0`. The steps are the same for every
version.

1. **Open a version bump pull request** against `main`:

   ```sh
   git switch -c topic/release-0.2.0-rc.0 origin/main
   # Cargo.toml:   version = "0.2.0-rc.0"
   # Chart.yaml:   version: "0.2.0-rc.0"
   #               appVersion: "0.2.0-rc.0"
   cargo check   # updates Cargo.lock to match
   ```

   While you're in there, check the other pins that a release is a good time
   to refresh: `job.dispatcherImage.tag` in `values.yaml`, the
   `node-feature-discovery` dependency in `Chart.yaml` (then run
   `helm dependency update` so `Chart.lock` follows), and any version shown in
   the docs (`rg '0\.1\.0'`).

2. **Merge it** once CI passes.

3. **Click the button.** Go to **Actions → Release**, click **Run workflow**,
   leave the branch on `main`, and click **Run workflow** again. The workflow
   refuses any other branch.

4. **Watch the run.** It goes through these stages in order:

   1. It reads the version, checks it against the chart and against existing
      tags, runs the tests, and lints the chart.
   2. It builds each architecture natively, pushes it by digest, checks that
      the image starts and reports the right `--version`, and attests it.
   3. It tags the multi-arch image and pushes the chart.
   4. It creates the `v0.2.0-rc.0` tag on the commit it built, and publishes
      the GitHub release. This comes last, so no tag or release exists unless
      everything before it succeeded.

5. **Check the result:**

   ```sh
   gh release view v0.2.0-rc.0
   helm show chart oci://ghcr.io/kata-containers/kata-device-provisioner-charts/kata-device-provisioner \
     --version 0.2.0-rc.0
   gh attestation verify oci://ghcr.io/kata-containers/kata-device-provisioner:0.2.0-rc.0 \
     --repo kata-containers/kata-device-provisioner
   ```

A typical path to a final release is `0.2.0-alpha.0` → `0.2.0-beta.0` →
`0.2.0-rc.0` → `0.2.0-rc.1` → `0.2.0`. Each of those needs its own version
bump pull request, because the workflow releases whatever version is
committed. Skip any stage you don't need.

## Fixes

`main` is the only branch. There are no maintenance branches and no
backports: a fix ships in the next release cut from `main`. Make that release
a patch release (bump `Z`) if `main` has gained only fixes since the last one,
and a minor release (bump `Y`) otherwise.

## When a release fails

- **A job failed for a transient reason** (a registry hiccup or a runner
  problem): open the run and click **Re-run failed jobs**. Every publishing
  step can be repeated. The image and the chart are pushed again under the
  same version, and the run builds the same commit it built the first time.
- **Preflight refused the run** (bad version format, versions that disagree,
  a version that is already released, or a branch other than `main`):
  nothing was published and no tag was created. Fix it in a pull request and
  click **Run workflow** again.
- **Something is wrong with a release that was published**: don't move or
  reuse its tag. Somebody may already have pulled it. Fix the problem and cut
  the next version (`-rc.1`, or the next patch).
