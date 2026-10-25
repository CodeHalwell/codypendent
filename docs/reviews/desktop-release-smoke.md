# Desktop release smoke checks

The current release workflow ships desktop bundles for Apple Silicon macOS and
x86-64 Linux. Native transport uses Unix sockets. Windows desktop and Intel
macOS desktop bundles are not currently shipped; neither should be labelled as
a tested native target. CLI archives have their own release matrix.

CI and the release quality gate run native Rust checks on both macOS and Linux.
A passing source/build check does not replace the packaged GUI checks below.
Record the exact build, OS, architecture, and outcome for each platform before
publishing a release.

Use a disposable repository and a separate `CODYPENDENT_DATA_DIR`. Set
`CODYPENDENT_CONFIG_DIR` and `CODYPENDENT_SOCKET` inside it as well. The test
must not depend on an existing user session or an authenticated paid provider.

For a complete local send/stream/reconnect check, run
`python3 apps/desktop/scripts/smoke-provider.py` from the repository root. It
prints a new disposable data directory, configuration directory, socket path and
git checkout. Launch both clients with those three `CODYPENDENT_*` values, select
the printed checkout and the `local-smoke` model, then submit a short objective.
The response explicitly identifies itself as a fixture. Its fixed token counts
are synthetic; do not use this check for model quality or cost claims. Stop the
provider with Ctrl-C after closing the clients and stopping the isolated daemon.

| Flow | Expected result |
| --- | --- |
| First launch without a daemon | Clear connection status; starting an installed daemon succeeds or names the missing installation |
| Choose repository | Invalid paths rejected; a subdirectory resolves to the checkout root; selection survives restart |
| Model list and explicit Test | Opening the list causes no provider requests; Test uses the daemon verdict; an older daemon produces a compatibility explanation |
| ACP onboarding | Selection starts nothing; Connect shows pending state; failed sign-in leaves no profiles; successful handshake exposes advertised models; community consent is enforced |
| Compose and queue | IME Enter commits composition; refused/failed queue requests preserve the draft; accepted requests clear only the submitted text |
| Artifact Save | Cancel creates nothing; saving writes the full verified artifact; success is shown only after completion; denied destinations show an error |
| Approval keyboard flow | Full action/risk evidence readable; Tab reaches decisions; unknown actions have rejection only |
| Session reconnect | Old requests cannot alter the new session; A → B → A search never admits an old result or continuation |
| Extension delivery | Confirmed attachment negotiates; attested document mounts; patches update it; revision gaps request recovery; disposed or hidden surfaces disappear |
| Usage and failures | Missing usage stays absent; partial/unknown coverage is labelled; credentials disappear while adjacent error details survive |
| Native workspace menu | Open Repository, Focus Composer, palette, sidebar and Settings act on the current window; standard Copy/Paste and Services work |
| IDE / Finder / Terminal hand-off | Only installed targets appear; the selected checkout opens, including paths with spaces and shell punctuation |
| Standalone Apple bundle | With a minimal PATH and no globally installed CLI, Start daemon launches the bundled CLI; Remote UI finds the bundled worker and sealed Node tree |

Local evidence for the September review is recorded in
[the review report](2026-09-08-app-review.md). Results from one platform must not
be copied to another platform's row.

## Apple distribution preparation — 4 October 2026

The direct-distribution target is an Apple Silicon `.app` archive with Developer
ID signing and notarization. The workflow first downloads the same-version CLI
archive from its build job, then stages `codypendent`, the worker launcher and
the complete sealed Node runtime inside `Contents/MacOS`. The desktop launcher
prefers this bundled CLI over an older global installation. An explicit
`CODYPENDENT_BINARY` still overrides discovery for development.

The macOS deployment floor is 11.0, matching the bundled Node 22.23.3 runtime's
[documented minimum](https://github.com/nodejs/node/blob/v22.23.3/BUILDING.md#official-binary-platforms-and-toolchains).
This is a packaging floor, not evidence of testing every macOS version.

On macOS, UI workers retain the hard CPU limit, Seatbelt sandbox, V8 heap ceiling
and mandatory process-group RSS watchdog. Memory enforcement is sampled rather
than a kernel RSS cap. Darwin's `RLIMIT_DATA` checks the entire virtual map,
including reserved address space, so applying a 128 MiB resident-memory budget
there rejected the launcher before Node could start. The launcher leaves inherited
memory limits intact and uses `RLIMIT_AS` on Linux. See [Apple's VM limit implementation](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/vm/vm_map.c).

For a local unsigned build, run from the repository root:

```sh
cd apps/desktop
npm run tauri:build -- --bundles app --no-sign
cd ../..

# Input: an extracted, matching Apple CLI release archive.
python3 apps/desktop/scripts/apple-release.py stage \
  apps/desktop/src-tauri/target/release/bundle/macos/Codypendent.app \
  /path/to/codypendent-aarch64-apple-darwin
python3 apps/desktop/scripts/apple-release.py verify \
  apps/desktop/src-tauri/target/release/bundle/macos/Codypendent.app
```

Staging verifies CLI version, executable architectures and the source Node seal.
It refuses to overwrite an existing runtime or alter a Developer ID signed app.
The signer signs Node with its JIT entitlement, regenerates the seal over those
signed bytes, signs the worker and CLI, then signs the enclosing app. It submits
the complete archive using `notarytool`, staples the ticket, and requires actual
`codesign`, `stapler validate` and `spctl` success. The CI summary reports these
checks, rather than inferring signing from the presence of a secret.

Signing and notarization are a separate authorised release step. With an already
configured Developer ID identity and notarytool keychain profile:

```sh
APPLE_SIGNING_IDENTITY='Developer ID Application: …' \
APPLE_NOTARY_PROFILE='codypendent-notary' \
bash apps/desktop/scripts/sign-apple-release.sh \
  apps/desktop/src-tauri/target/release/bundle/macos/Codypendent.app
```

In CI the existing `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, `APPLE_ID` and `APPLE_PASSWORD`
secret slots are consumed only by the signing step. The certificate is imported
into a temporary keychain which is deleted on exit. An absent certificate leaves
an explicitly labelled unsigned preview only when signing is optional; version-tag
pushes and signing-required manual builds fail without it. No secrets or identities were created
or changed during preparation.

The packaging script's five focused tests and shell syntax check run locally.
Signed distribution remains unverified until the above signer succeeds and the
downloaded archive launches through Gatekeeper on a clean Mac. Record that result
and the minimum-supported macOS result before calling the release ready.

**Mac App Store is a separate distribution project.** The current daemon launches
coding tools, shares a Unix socket across clients and accesses user repositories.
It has not been adapted or tested under the App Sandbox. Store preparation still
needs the sandbox/file-access and shared-container design, provisioning profile,
App Store signing identities, privacy disclosures, listing/screenshots and
App Review. Enabling an entitlement alone does not establish that those workflows
work. See [Tauri's App Store requirements](https://v2.tauri.app/distribute/app-store/),
[Tauri macOS signing](https://v2.tauri.app/distribute/sign/macos/) and
[Apple's distribution packaging guidance](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution).

## Install, update and DMG release verification

Final release artifacts include a `SHA256SUMS` manifest produced **after** signing
and stapling. `install.sh`, `codypendent update`, and `codypendent install` verify
the matching published digest before extraction or installation. Older releases
without this manifest are refused. The manifest detects corrupted or mismatched
downloads; authenticated GitHub access and Developer ID signatures establish the
release source.

The macOS CLI installer validates the bundled runtime, checks Developer ID signed
apps with Gatekeeper, and stages the entire replacement beside the destination.
The previous app remains recoverable until the new copy has passed verification.
A running desktop or bundled daemon must exit before replacement. The installer
preserves quarantine; it does not bypass Gatekeeper. A CLI inside `Codypendent.app`
refuses standalone self-update because changing one signed bundle member would
invalidate the app signature. Use `codypendent install desktop <tag>` to replace
the complete desktop bundle.

From the repository root, build with
`CARGO_TARGET_DIR="$PWD/target" npm --prefix apps/desktop run tauri:build -- --bundles app --no-sign`,
stage the matching **release** CLI and worker with `apple-release.py stage`, then:

```sh
bash apps/desktop/scripts/package-apple-release.sh \
  path/to/Codypendent.app path/to/codypendent-desktop-aarch64-apple-darwin.dmg
python3 apps/desktop/scripts/apple-release.py verify-dmg \
  path/to/codypendent-desktop-aarch64-apple-darwin.dmg
```

The DMG includes the complete app and an Applications shortcut. Verification mounts
it read-only and checks the contained runtime seal. This proves packaging, not
notarization. Test a copy installed from that mounted image, including replacing
an older copy, with Vite stopped and without `CODYPENDENT_BINARY` or system Node.
Preserve user configuration/session data across the replacement. Record a separate
clean-Mac Gatekeeper result; a fresh data directory on a development Mac is not a
clean-machine test.

## Apple account setup still required

No Developer ID certificate or notarization credentials were available for the
2026-10-04 local candidate. The available unrelated development identity is not
used. Complete these steps when an Apple Developer account exists:

1. Enroll in the Apple Developer Program, register the app identifier
   `com.codypendent.desktop`, and create a **Developer ID Application** certificate
   with its private key in the signing Mac's Keychain. This is the direct-download
   distribution certificate; Mac App Store distribution is a separate process.
2. Confirm it appears in `security find-identity -v -p codesigning`. Store Apple
   notarization credentials in a named profile using
   `xcrun notarytool store-credentials codypendent-notary`; let the command prompt
   rather than putting a password into shell history.
3. With the staged optimized app, run:

   ```sh
   export APPLE_SIGNING_IDENTITY='Developer ID Application: YOUR REGISTERED NAME (TEAMID)'
   export APPLE_NOTARY_PROFILE='codypendent-notary'
   bash apps/desktop/scripts/sign-apple-release.sh \
     path/to/Codypendent.app path/to/codypendent-desktop-aarch64-apple-darwin.dmg
   ```

   The script signs helpers and app, notarizes/staples the app, creates the DMG,
   then signs/notarizes/staples the DMG and assesses both with Gatekeeper. It
   verifies hardened runtime and secure timestamps for executable signatures.
4. For CI, configure repository secrets `APPLE_CERTIFICATE` (base64 exported
   certificate plus private key), `APPLE_CERTIFICATE_PASSWORD`,
   `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific password),
   and `APPLE_TEAM_ID`. No credentials are written into this repository.
5. Run the release workflow manually with `require_apple_signing=true`. Manual
   dispatch builds and uploads verified artifacts and checksums; it cannot publish
   a GitHub release, including when dispatched against a tag. Signing required
   but unconfigured is a failure, never an unsigned success. Publishing remains
   the separately gated push-to-main/version-tag path.
6. Download the signed DMG through a browser on a different clean Mac, install
   through Finder, launch with Gatekeeper enabled, and verify repository choice,
   provider setup, a real coding run, IDE handoff, quit/reopen and replacement
   update. Also test offline first launch to confirm stapled tickets are present.

No automatic-update feed or Tauri updater key is configured. The supported update
path here is an explicitly selected, checksum-verified release or replacing the
signed app from its DMG. A future automatic updater must use a real hosted endpoint
and its own configured update-signing key.

References: [Apple packaging](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution),
[Apple notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution),
[Tauri updater signatures](https://v2.tauri.app/plugin/updater/).

### Reproducible performance and installation checks

Use the optimized, fully staged app for performance measurements:

```sh
python3 apps/desktop/scripts/measure-apple-startup.py target/release/bundle/macos/Codypendent.app --samples 3 --idle-seconds 30 --output /tmp/cody-startup-host.json
python3 apps/desktop/scripts/measure-apple-startup.py target/release/bundle/macos/Codypendent.app --samples 3 --idle-seconds 30 --clean-home --output /tmp/cody-startup-empty-home.json
python3 apps/desktop/scripts/measure-workspace.py target/release/bundle/macos/Codypendent.app --files 5000 --sessions 1000 --events 25000 --output /tmp/cody-workspace-performance.json
python3 apps/desktop/scripts/smoke-apple-install.py target/release/codypendent target/release/bundle/macos/Codypendent.app --output /tmp/cody-install-smoke.json
```

Startup records the real first daemon Pong, full Node integrity-check duration,
idle CPU/RSS and host load. These are daemon measurements; record the desktop and
WebKit processes separately during native UI testing. Empty HOME and fresh data
isolation do not substitute for a clean Mac or Gatekeeper distribution test.

The workspace harness creates real sessions and one read-only run through a
loopback synthetic model, then adds valid synthetic notes while its own daemon
is stopped. Its result includes the retained fixture path, so the native app can
open the same 5,000-file checkout and 1,000-session library. It measures real IPC
responses for session listing, search, history pages and file search; it does not
measure model quality or claim real-provider token usage. The synthetic provider
stops before the timed queries. All fixture processes and paths are isolated.

The installer harness uses the actual compiled CLI and complete app, replacing
only GitHub download with an offline fixture. It verifies first installation,
checksum and incomplete-app refusals, preservation of the previous app on both
failures, successful replacement, quarantine preservation, external data
preservation and staging cleanup in a disposable Applications directory.

Tagged publishing also requires the standalone **arm64 and x86_64 macOS CLI
payloads** to pass their own signing gate before tarball creation. The shared
signing helper imports the CI identity once per job; `sign-apple-runtime.sh` signs
Node, reseals its final bytes, signs CLI/worker/optional daemon, submits the ZIP
payload, requires an `Accepted` response, and checks each executable with
`codesign --check-notarization -R=notarized`. Missing credentials or any failed
signature/notarization check blocks the build and therefore publication. Manual
unsigned candidates and rolling prereleases are explicitly marked as previews.

Apple does not support stapling a ticket to a bare executable or ZIP. The CLI
archive may require an online first Gatekeeper check; the signed and stapled
Desktop DMG is the offline installation path. The shell installer and CLI updater
no longer explicitly clear quarantine. See [Apple's notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).
This session exercises Apple Silicon only. The x86_64 CLI workflow is prepared
for cross-compilation and signature verification, but has not been run or tested
on an Intel Mac; no Intel desktop app is built by the current matrix.
