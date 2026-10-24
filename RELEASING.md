# Releasing ptt-radio

A release is one tag, `v<version>`. Pushing it runs `.github/workflows/release.yml`, which builds every asset and publishes the GitHub Release at <https://github.com/vshuraeff/ptt-radio/releases>. With the portal secret configured, it also uploads the mod to the Factorio mod portal.

## What a release contains

| Asset | Contents |
| --- | --- |
| `ptt-radio_<ver>.zip` | the mod, ready for the mod portal or the `mods` folder |
| `ptt-radio-companion-<ver>-linux-x86_64.tar.gz` | the companion for Linux x86_64, `LICENSE`, `README.md` |
| `ptt-radio-companion-<ver>-windows-x86_64.zip` | the companion for Windows x86_64, `LICENSE`, `README.md` |
| `ptt-radio-companion-<ver>-macos-universal.zip` | the companion for macOS (Apple silicon and Intel in one binary), `LICENSE`, `README.md` |
| `ptt-radio-hub-<ver>-linux-x86_64.tar.gz`, `ptt-radio-hub-<ver>-linux-aarch64.tar.gz` | the static hub, `LICENSE` |
| `install.sh`, `install.ps1` | companion installers, also available through `releases/latest/download/` |
| `SHA256SUMS` | the SHA-256 of every asset above |

The Linux companion is built inside an Ubuntu 22.04 container, so it needs glibc 2.35 or newer and runs on Ubuntu 22.04, Debian 12 and later; the run fails if the binary would need a newer glibc. That container is a deliberate exception to keeping every image at its latest version, made to hold the glibc 2.35 floor that this check enforces.

The README inside each companion archive is written by `tools/archive-readme.sh`, which turns its repository-relative links into links to the tagged files on GitHub, since the archive holds only the binary, `LICENSE` and `README.md`.

The publication job copies both installers from the tagged checkout and includes them in `SHA256SUMS`. Players install with `curl -fsSL https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.sh | sh` on macOS/Linux, or `irm https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.ps1 | iex` in Windows PowerShell. The installers resolve the latest release without a token, verify its platform archive, and stage the binary, license, README and `install.json` before replacing the existing installation. They do not install the mod; players install that through Factorio or its mod zip.

Before release, run `uv run -q tools/test-installers.py` on macOS, Linux and Windows. The fixture server publishes only temporary fake archives; the test installs, updates, rejects a bad checksum, rejects an unsafe archive layout and rejects a missing download without running any binary. It verifies that previous files and `mod-list.json` survive failures. Windows uses PowerShell 5.1 or `pwsh`; run PSScriptAnalyzer there when available. Also run `sh -n install.sh`, `shellcheck install.sh` and `actionlint`. `PTT_RADIO_INSTALL_TEST_RELEASE_URL` is a test-only release-root override for local fixtures (`latest` redirect to `tag/<tag>`, plus `download/<tag>/`); leave it unset for real installations.

The same run also pushes the hub image `ghcr.io/vshuraeff/ptt-radio-hub` with the tags `<ver>`, `<major>.<minor>` and `latest`. The release notes are the section of `mod/ptt-radio/changelog.txt` for that version.

## Cutting a release

1. Pick the version. The mod and the companion share one version, so it goes in two places:
   - `mod/ptt-radio/info.json`, the `"version"` field;
   - `companion/Cargo.toml`, the `version` of `[package]`. Then run `cargo update --workspace` in `companion/` so `Cargo.lock` follows; CI builds with `--locked` and fails on a stale lock file.
2. Add the version's entry at the top of `mod/ptt-radio/changelog.txt`. It uses the Factorio changelog format, and the release fails when the section is missing:

   ```text
   ---------------------------------------------------------------------------------------------------
   Version: 0.4.1
   Date: 2026-11-01
     Bugfixes:
       - Fixed ...
   ```

3. Check the versions agree:

   ```sh
   tools/check-version.sh
   ```

   Optionally build the mod zip locally to look at it: `tools/package-mod.sh 0.4.1 dist` writes `dist/ptt-radio_0.4.1.zip`.
4. Commit, push `master`, and wait for CI to pass.
5. Tag the commit and push the tag:

   ```sh
   git tag -a v0.4.1 -m "ptt-radio 0.4.1"
   git push origin v0.4.1
   ```

6. Watch the `release` run in the Actions tab. Compilation runs in parallel with `tools/check-version.sh v0.4.1`; the final publication waits for that check and every asset. A mismatched tag cannot publish a release. The hub workflow also checks the version before building or pushing its image.

If neither GitHub nor the mod portal has published the version, fix the cause, delete the tag locally and on GitHub (`git push origin :refs/tags/v0.4.1`), and tag again. Once either service has published it, preserve the version and tag: retry only the failed job where safe, or fix forward with a new version. Do not rerun a successful portal upload. Once GitHub has published, companions may already have updated to it (see Self-update).

## Factorio mod portal

The first publication of `ptt-radio` is manual and requires the owner's explicit instruction. The workflow only adds releases to an existing mod; it never calls `init_publish`. Before the mod exists, an upload fails with the portal's `UnknownMod` error.

After that first publication, set the repository Actions secret `FACTORIO_MOD_PORTAL_KEY` to a Factorio API key with the `ModPortal: Upload Mods` usage, created at <https://factorio.com/profile>. Adding the secret enables portal uploads on future `v*` tag pushes. With the secret absent or empty, the portal job succeeds with a skip notice. Branch pushes and pull requests do not trigger it.

The `mod-portal` job waits for the version check and mod packaging, then downloads the same `mod` artifact used by the GitHub Release. The version check verifies `mod/ptt-radio/info.json` against the tag. Following the [mod upload API](https://wiki.factorio.com/Mod_upload_API), the job calls `init_upload` with `mod=ptt-radio`, then posts `ptt-radio_<ver>.zip` to the returned HTTPS upload URL. HTTP, transport, JSON and portal API errors fail the job, as does an upload response without `success: true`.

Portal upload runs independently of the companion and hub builds. GitHub Release publication does not depend on `mod-portal`, so a portal failure does not block it. Check both jobs before considering a release delivered to both services; if GitHub publication succeeded, avoid rerunning the entire workflow just to retry a failed portal job.

## Self-update

The release is published as a normal release, neither a draft nor a prerelease, and marked as the latest, because the companion looks for updates through `releases/latest`. Release builds of the companion check that release at startup and, when it is newer than themselves, download their platform's archive, verify it against `SHA256SUMS`, replace their own executable and restart. A failed check or download is reported in one `self-update:` line and the current binary keeps running.

- Players opt out with `--no-self-update` or by setting `PTT_RADIO_NO_SELF_UPDATE=1`.
- Only the binaries this workflow builds update themselves: it compiles them with `PTT_RADIO_RELEASE_BUILD=1`. A local `cargo build` never self-updates.
- The hub does not self-update; it is updated by pulling a newer image or tarball.

A release is therefore picked up by every running companion on its next start. Publishing a broken companion reaches players quickly, so a bad release is fixed by releasing a newer version, never by deleting it: deleting the latest release makes the previous one latest again, but companions never downgrade.

## macOS signing and notarization

Without signing, macOS Gatekeeper blocks the downloaded companion, and users have to clear the quarantine flag themselves (`xattr -d com.apple.quarantine ptt-radio-companion`). With the secrets below set, the release signs the companion with a Developer ID certificate, the hardened runtime and the microphone entitlement in `companion/macos/entitlements.plist`, then notarizes the zip with Apple. A bare command-line binary cannot be stapled, so Gatekeeper checks the notarization online on first launch.

Set the secrets under Settings > Secrets and variables > Actions > Repository secrets:

| Secret | Value |
| --- | --- |
| `MACOS_CERTIFICATE_P12` | the Developer ID Application certificate and its private key, as a base64 `.p12` |
| `MACOS_CERTIFICATE_PASSWORD` | the password of that `.p12` |
| `MACOS_SIGNING_IDENTITY` | the certificate's name, e.g. `Developer ID Application: Jane Doe (AB12CD34EF)` |
| `APPLE_ID` | the Apple ID email of the developer account |
| `APPLE_TEAM_ID` | the ten-character team ID, shown under Membership details at <https://developer.apple.com/account> |
| `APPLE_APP_PASSWORD` | an app-specific password for that Apple ID, used by `notarytool` |

### Exporting the certificate

You need a paid Apple Developer Program membership.

1. Create a Developer ID Application certificate at <https://developer.apple.com/account/resources/certificates>, or in Xcode under Settings > Accounts > Manage Certificates, and install it in your login keychain.
2. In Keychain Access, open My Certificates, expand the certificate so its private key shows, select both, and choose File > Export Items as a `.p12`. Set a strong export password; it becomes `MACOS_CERTIFICATE_PASSWORD`.
3. Encode the file and paste the output into `MACOS_CERTIFICATE_P12`:

   ```sh
   base64 -i DeveloperID.p12 | pbcopy
   ```

4. Find the identity name for `MACOS_SIGNING_IDENTITY`:

   ```sh
   security find-identity -v -p codesigning
   ```

Delete the exported `.p12` once the secret is saved.

### Creating the app-specific password

1. Sign in at <https://account.apple.com> with the Apple ID.
2. Under Sign-In and Security, open App-Specific Passwords and generate one, named for example `ptt-radio notarytool`.
3. Paste it into `APPLE_APP_PASSWORD`. Revoking it there later disables notarization without touching the certificate.

### Without the secrets

When `MACOS_CERTIFICATE_P12` is empty, every signing and notarization step is skipped, the run log shows a notice saying so, and the release ships the macOS zip unsigned. The Linux, Windows, hub and mod assets do not depend on these secrets. When the certificate is set but any of `APPLE_ID`, `APPLE_TEAM_ID` or `APPLE_APP_PASSWORD` is missing, the run fails rather than ship a signed but un-notarized binary.

The certificate only ever lives in a temporary keychain on the runner, which the job deletes at the end whether it passed or failed. The cargo build runs before the secrets reach any step.

## First publication of the wiki

The wiki is kept in `wiki/` and mirrored by `.github/workflows/wiki.yml` into the repository's wiki. GitHub creates the wiki's git repository only after its first page is saved in the web interface; enabling the wiki in the settings is not enough, and until then the wiki workflow fails. Once, before the first sync:

1. Open the repository's Wiki tab and save any page, for example the suggested Home page.
2. Run the `wiki` workflow from the Actions tab (Run workflow), or push a change under `wiki/`. The sync replaces that placeholder page.

## CI

`.github/workflows/ci.yml` runs on pushes to `master` and pull requests, except changes limited to `wiki/`, `docs/` or Markdown. It runs `cargo fmt --check`, clippy with `-D warnings`, library and integration tests, a hub build without the `audio` feature, Lua checks and `tools/check-version.sh`. Library tests also run on macOS and Windows; Linux release-profile library tests have their own parallel job.

CI also mirrors the release companion builds: the Ubuntu 22.04 container and glibc check, Windows MSVC, and both macOS targets in one Cargo invocation. It neither packages nor signs these builds. Matching CI/release jobs share Cargo dependency caches; only master pushes save them, while PRs and tags restore them. Registry and compiled dependency reuse avoids recompiling unchanged dependencies; the workspace crate still rebuilds, including when its release version changes. Signing material and temporary keychains stay outside the caches and reach only signing steps.

Master CI warms separate BuildKit caches for both hub architectures. Hub archive builds update those caches, and the image imports both. The Docker dependency layer depends on manifests and the lockfile, so a source-only change rebuilds the hub crate while retaining its dependencies.

The real-time audio suites (`scenarios`, `aec_gate` and the ignored `policy_playout` tests) need a quiet dedicated machine and run only in `tools/gate.sh`.
