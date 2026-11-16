# Releasing

## Assets and tags

The update checker relies on everything in this section; change it only together with the checker.

- At most one stable release and one pre-release exist at a time; while there is no stable release, exactly one pre-release. Every commit on `main` that passes `ci` becomes a pre-release, and every other pre-release and its tag are then deleted; a stable release is never deleted there (`.github/workflows/prerelease.yml`). A stable tag makes a release, and every other release and pre-release with its tag is then deleted (`.github/workflows/release.yml`). The rules are `scripts/release.sh`, tested by `scripts/release-test.sh` in `ci`.
- No pre-release is published when a stable tag `v<Cargo.toml version>` already exists (its pre-release would sort below the release by semver, so no install would ever take it) or when the commit itself carries a stable tag. The `prerelease` run says why in a notice and ends green. So bump the version right after a stable release (section 2).
- Tags:
  - `v<version>-pre.<n>`: automatic pre-release. `<version>` is the `Cargo.toml` workspace version at that commit, `<n>` the run number of `prerelease.yml` (grows by one per run). Marked pre-release.
  - `vX.Y.Z`: a release pushed by hand (section "Tag" below), `.github/workflows/release.yml`.
  - `vX.Y.Z-<suffix>` (`-rc.1`, `-beta.2`): a hand-pushed test tag, marked pre-release. It replaces the other pre-releases, as an automatic one does.
  - `release.yml` fails when the tag is no semver version (numbers without leading zeros: `v0.2.0-rc.01` fails, write `v0.2.0-rc.1`; the updater would print it as `rc.1` and find no asset) or when the tag without its `-<suffix>` differs from the `Cargo.toml` version.
- Assets, `<version>` being the tag without its leading `v` (`0.2.0`, `0.2.0-pre.57`):
  - `OxiYapper-<version>-windows-x64.exe`: the whole app in one file. Windows x64, Vulkan build; with no Vulkan driver it runs on the CPU. Needs a CPU with AVX2. Needs the Microsoft Visual C++ runtime (x64) installed; on a clean Windows use the zip.
  - `OxiYapper-<version>-windows-x64.zip`: the portable build, one folder `OxiYapper-<version>-windows-x64/` with `OxiYapper.exe`, an empty `portable` marker (data in `data\` beside the exe, `docs/ARCHITECTURE.md` section 7), `README.txt`, `LICENSE-MIT`, `LICENSE-APACHE`, and the MSVC runtime DLLs next to the exe (`msvcp140.dll`, `vcruntime140.dll`, `vcruntime140_1.dll`: app-local deployment, allowed by Microsoft's redistribution terms), so it starts on a clean Windows with nothing installed.
  - `SHA256SUMS`: `sha256sum` lines (`<hex>  <file name>`) for both files above.
  - A build provenance attestation for each of the three.
- How the checker finds the newest build (`crates/oxi-core/src/update.rs`, `src/daemon/update.rs`): `GET https://api.github.com/repos/makarasty/OxiYapper/releases?per_page=10` (pre-releases included; `/releases/latest` skips pre-releases, so do not use it), drafts and tags that are no `vX.Y.Z[-<pre>]` left out, the highest version by semver (`pre.10` after `pre.9`, a release after its pre-releases), its asset `OxiYapper-<version>-windows-x64.exe`, checked against its line in `SHA256SUMS` of the same release.
- The version a build reports (`--version`, the updater, About) is the tag without `v`: `build-windows.yml` passes it to `cargo build` as `OXI_VERSION`. A local build reports the `Cargo.toml` version, which is newer than its own pre-releases, so it never updates itself.

Linux and macOS are not shipped for now; `ci.yml` still builds them.

## 1. Prepare a release

1. Start from an up-to-date `main` with `ci-ok` green on its last commit.
2. Run the manual QA checklist (`docs/ARCHITECTURE.md` section 13) on Windows and keep the results in the release PR.
3. Bump the version in the root `Cargo.toml` (`[workspace.package] version = "X.Y.Z"`; every crate takes it from there), then run `cargo check` so `Cargo.lock` follows.
4. In `CHANGELOG.md`: rename `## [Unreleased]` content into a new `## [X.Y.Z] - YYYY-MM-DD` section with today's date, leave an empty `## [Unreleased]` above it, and update the compare links at the bottom.
5. Commit (`chore: release vX.Y.Z`), open a PR, merge it when `ci-ok` is green. The merge makes a pre-release `vX.Y.Z-pre.<n>` by itself, unless the tag of section 2 is already there.

## 2. Tag

```
git switch main
git pull
git tag -a vX.Y.Z -m "OxiYapper vX.Y.Z"
git push origin vX.Y.Z
```

Tag the merge commit of the release PR. `release.yml` publishes the release, then deletes every other release and pre-release with its tag. The order with the merge commit's own pre-release does not matter: published first, the release deletes it; still building, it sees the tag and is not published.

Then bump the version on `main` at once (step 3 of section 1 with the next patch, `X.Y.Z+1`, no tag). Until that merge no pre-release is published: each would be `vX.Y.Z-pre.<n>`, older than `vX.Y.Z` by semver, so no install would take it.

## 3. What the workflows do

- `build-windows.yml` (called by both): `cargo build --locked --release --features vulkan` with `GGML_NATIVE=OFF` and `OXI_VERSION=<version>` on windows-latest, `OxiYapper --version` as a smoke test that must print `OxiYapper <version>`, a check that `vulkan-1.dll` is a delay-load import (not a static one: the exe must start without a Vulkan driver), then the `.zip` (the `*140*.dll` files the exe imports, found with `dumpbin`, are copied from the runner's `VC\Redist\MSVC\<version>\x64\Microsoft.VC*.CRT` folder found with `vswhere`; the job fails if one is missing) and the `.exe`, and a last step that unzips the zip into a fresh folder and runs `OxiYapper.exe --version` from there.
- `release.yml` on a `v*` tag push: the version check (`scripts/release.sh tag-version`), the build, then `publish` writes `SHA256SUMS`, attests every file, creates the release with `gh release create` (notes generated by GitHub from the merged PRs; paste the `CHANGELOG.md` section on top by hand) and deletes what it replaces (`scripts/release.sh prune`): for a stable tag every other release and pre-release, for a test tag every other pre-release, each with its tag.
- `prerelease.yml` on a successful `ci` run of a push to `main` in this repository (not a fork, not a PR): checks the tags (`scripts/release.sh pre-skip`; a stable tag of this version or on this commit stops it with a notice), builds that run's commit, checks the tags again, creates the pre-release at that commit, then deletes every other pre-release with its tag. Runs queue (`concurrency: prerelease`), so two never delete at once.

## 4. Check the release

1. On the release page: the `.exe`, the `.zip` and `SHA256SUMS`.
2. Download the exe and check it:

   ```
   sha256sum -c SHA256SUMS --ignore-missing
   gh attestation verify OxiYapper-X.Y.Z-windows-x64.exe --repo makarasty/OxiYapper
   ```

   On Windows without `sha256sum`: `certutil -hashfile <file> SHA256` and compare with the line in `SHA256SUMS`.
3. On a clean Windows user: run `OxiYapper-X.Y.Z-windows-x64.exe --version` from a terminal, start it, download a model, dictate into Notepad.

## 5. Check the self-update

After a pre-release is out, on Windows with an older build or none:

1. Download the newest `OxiYapper-<version>-windows-x64.zip`, unpack it to a writable folder (not `Program Files`).
2. Start it with the hidden test hook, so it takes itself for an old build:

   ```
   set OXI_UPDATE_FAKE_VERSION=0.0.1
   OxiYapper.exe
   ```

   The first check runs 5 s after the start (a minute without the hook). The hook also lets a debug build install, so never set it on a build in a cargo target folder you want to keep.
3. Within a minute: System > Updates shows "Downloading ..." then the app quits and starts again, the tray says "Updated to <version>", the folder holds the release's exe, `OxiYapper.old.exe` is gone a minute later (the first check of the new daemon deletes it), and the log (`data\logs`) has `update installed, restarting`, `daemon started ... updated=true` and `the updated daemon is ready`. The restarted daemon does not inherit the hook, so it does not update again.
4. Fallback: make the folder read-only (or turn "Install updates automatically" off) and press Check now: the status says "Update available" with a Download button that opens the release page; the exe is unchanged.
5. Rollback: the daemon that installs an update starts a watchdog from `OxiYapper.old.exe` (`--update-watch`). It starts the new exe and waits up to 30 s for it to serve its pipe (the new daemon writes `OxiYapper.update-ready` beside the exe). Without that it kills the new exe, puts the old one back, starts it, and writes the version to `OxiYapper.update-failed`: that version is only offered from then on, never installed again. The log has `the updated daemon did not get ready, putting the old exe back`.

## If something fails

- A build job failed: fix on `main`, delete the tag (`git push origin :refs/tags/vX.Y.Z` and `git tag -d vX.Y.Z`), and tag again. No release is created until the build passes.
- A release went out broken: never replace its files. A daemon that does not start is rolled back by the watchdog (section 5), but a build that starts and misbehaves stays installed. Merge the fix with a version above the broken one (after a stable release the version was bumped, section 2): its pre-release is newer by semver, so installs take it. A broken pre-release is replaced by the next one; a broken stable release stays until the next stable tag replaces it.
