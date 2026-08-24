# Releasing

Releases are tag driven. Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`, which builds every platform through the regular build workflow, packages the artifacts into install ready archives, and creates a draft GitHub release. Nothing goes public until the draft is published by hand, which matches the policy that tagged releases are fully tested.

## Steps

1. Bump the version in `Cargo.toml` (the `version = "X.Y.Z"` under `[workspace.package]`, the one source of truth: `scripts/package.sh` names the archives from it and the Windows installer reads it). Run a build so `Cargo.lock` picks up the new version, and commit both. The release workflow fails early if the tag and this version disagree.

2. Commit the bump:

   ```
   git commit -am "chore(obs-irl-source): bump version to X.Y.Z"
   ```

3. Tag and push:

   ```
   git tag vX.Y.Z
   git push origin master vX.Y.Z
   ```

4. Wait for the Release workflow to finish. It creates a draft release containing:

   * `obs-irl-source-X.Y.Z-linux-x64.tar.gz` (extract into `~/.config/obs-studio/plugins/`)
   * `obs-irl-source-X.Y.Z-windows-x64.zip` (extract into the OBS install folder)
   * `obs-irl-source-X.Y.Z-windows-x64-setup.exe` (same payload as the zip, installed by `installer/obs-irl-source.iss`)
   * `obs-irl-source-X.Y.Z-macos-arm64.zip` (extract into `~/Library/Application Support/obs-studio/plugins/`)
   * `sha256sums.txt`

   The three archives are staged by `scripts/package.sh`, so a maintainer can reproduce any of them locally from a `cargo build --release`.

   The release body starts with install instructions (from `.github/release-notes-header.md`) followed by a changelog built from the commits since the previous tag.

5. Download and test every artifact on a real OBS install before publishing. At minimum: the plugin loads, a source connects, audio and video play.

6. Read over the changelog, edit anything that reads badly, then publish the draft.

## The changelog

`scripts/changelog.sh` groups every non-merge commit between the previous `v*` tag and the one being released, using the conventional commit type in the subject: `feat` becomes Features, `fix` becomes Bug fixes, then `perf`, `refactor`, `docs`, and `build`/`ci`. Anything with a `!` or a `BREAKING CHANGE:` body goes to the top under Breaking changes, along with the text after the marker. A subject that is not a conventional commit still shows up, under Other changes.

Preview the notes before tagging:

```
scripts/changelog.sh HEAD
```

Commit subjects are the changelog, so they are worth writing carefully. Give a commit a scope (`fix(video): ...`) when it makes the entry clearer; the scope is printed in bold ahead of the subject, and a redundant `obs-irl-source` scope is dropped.

This replaced GitHub's `generate-notes`, which only sees pull requests and so left out everything pushed straight to `master`.

## Fixing a bad tag

If the version check fails or an artifact is broken, delete the draft release in the GitHub UI, fix the problem, then move the tag:

```
git tag -f vX.Y.Z
git push -f origin vX.Y.Z
```

## Supported OBS lines

One archive per platform covers every supported OBS line. The plugin bundles its own media stack and binds to libobs through hand-written FFI, so nothing links a specific libobs at all; libobs gates a plugin on `(major, minor) <= host`, so declaring the oldest supported line produces a binary that also loads on every newer one.

`OBS_VERSION` at the top of `.github/workflows/build.yml` documents that oldest line, and `api_version` in the `declare_module!` call (`crates/irl-source/src/lib.rs`) is what actually reports it to libobs. Raise both only to drop support for older OBS releases, never to chase a newer one. See the CI section in `CLAUDE.md`.

## Not automated (yet)

* macOS signing and notarization. The zip ships unsigned, and the release notes tell users to clear the quarantine attribute.
* Windows code signing. The installer is built but not signed, so SmartScreen warns on first run until the download builds reputation.
