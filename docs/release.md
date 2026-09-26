# Releasing

How a release is versioned, built, signed and checked. The product name and
app id are still placeholders (see [packaging/README.md](../packaging/README.md));
rename them before the first public release, since the app id cannot change
afterwards on Flathub or Google Play.

## Versioning

- One version for the apps: `version` of `crates/tsc-ui` (desktop; the
  installer and the About page read it), `crates/tsc-android` and the
  Android app's `versionName`. Semantic versioning; before 1.0 a minor bump
  may break settings or the gateway protocol, and the changelog says so.
- Android `versionCode` must grow with every upload:
  `major * 1000000 + minor * 1000 + patch` (0.2.3 is 2003).
- The gateway `tsgw` (`crates/tsc-gateway`) is versioned on its own, since
  server admins update it separately. Its wire protocol is versioned in the
  WebSocket subprotocol (`tsgw.v1+json`); a breaking change needs `v2` and a
  period in which the gateway serves both.
- Library crates are not published (`publish = false`) and keep their
  versions.
- Tags: `v<version>` for the apps (`v0.2.0`), `tsgw-v<version>` for the
  gateway. Tags are annotated and signed (`git tag -s`).

## Changelog

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/). Every
change a user or server admin notices adds a line under `Unreleased`
(Added / Changed / Fixed / Removed / Security). At release the section gets
the version and date, and its highlights also go into the `<releases>` entry
of `packaging/linux/io.github.faumaray.TsClient.metainfo.xml` (shown by
software centers and Flathub). Release notes also state the TeamSpeak server
versions the release was tested against (TS3 and TS6 from
`dev/docker-compose.yml`).

## Third-party notices

`THIRD_PARTY_NOTICES.md` lists the licenses of everything compiled into the
apps. It is shown in the About page (`tsc_platform::notices::text()`) and
shipped in every package. It is generated, never edited:

```sh
cargo install cargo-about --locked --version 0.9.2 --features cli
scripts/notices.sh           # regenerate
scripts/notices.sh --check   # what CI runs
```

- `about.toml`: accepted licenses (the same list as `deny.toml`), the
  targets we ship (Linux, Windows, Android), no build or dev dependencies,
  and clarifications for crates whose license files cargo-about does not
  find (Slint, the bundled libopus, AWS-LC, OpenH264). Clarifications pin
  file checksums; after a dependency update that changes such a file the
  script fails and the new text has to be checked and its checksum updated.
- `about.hbs`: the Markdown template, with the notices that do not come from
  crates: the Slint attribution, OpenH264 (downloaded at runtime, not
  shipped), the vendored tsclientlib and libvpx.
- The output does not depend on the host, so CI (`notices` job in
  `.github/workflows/ci.yml`) compares the committed file exactly and fails
  when `Cargo.lock` changed without a regeneration. The cargo-about version
  is pinned in the script and the job: a newer version may format
  differently, so update both together.

A new license has to be allowed in `deny.toml` (cargo-deny fails first) and
in `about.toml`, and must be compatible with distributing the apps under
MIT OR Apache-2.0 (no GPL/AGPL/LGPL code linked in; LGPL only as a
dynamically loaded system library, after review).

## Signing

Signing keys and certificates never go into the repository. In CI they live
in the secrets of a GitHub environment `release` that requires approval,
are written to `$RUNNER_TEMP` in the job and deleted at its end.

### Windows (Authenticode)

Unsigned executables trigger SmartScreen warnings; signed ones build
reputation with the publisher's certificate. Sign `tsc-desktop.exe` first,
then build the installer with `/DSIGN=...`, which signs the installer and the
uninstaller it writes (see [packaging/windows/README.md](../packaging/windows/README.md)).
Always timestamp (`/tr ... /td SHA256`), so signatures stay valid after the
certificate expires.

Options for the certificate:

- **Azure Trusted Signing**: Microsoft-managed certificates, no hardware
  token, billed monthly; needs an Azure subscription and identity
  validation of the publisher (check the current eligibility rules). Sign
  with `signtool` and the Trusted Signing dlib (NuGet package
  `Microsoft.Trusted.Signing.Client`):

  ```powershell
  # metadata.json: {"Endpoint": "https://<region>.codesigning.azure.net",
  #   "CodeSigningAccountName": "<account>", "CertificateProfileName": "<profile>"}
  signtool sign /v /fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256 `
    /dlib Azure.CodeSigning.Dlib.dll /dmdf metadata.json target\release\tsc-desktop.exe
  ```

  In GitHub Actions, `azure/trusted-signing-action` does the same with a
  service principal (client id, tenant id, secret as environment secrets).
- An OV or EV code-signing certificate (on a hardware token or in a cloud
  HSM, as CAs require since 2023) with `signtool /f` or the HSM's CSP.
- SignPath's free plan for open-source projects (signing happens in their
  service from a CI artifact).

Check the result with `signtool verify /pa /v <file>` for the executable,
the installer and `uninstall.exe` after installing.

### Linux

- **Flathub** builds and signs the Flatpak itself (with Flathub's key) from
  a manifest in the `flathub/<app id>` repository. For the first submission
  open a pull request against `flathub/flathub` with the manifest, where the
  source is this repository at the release tag and commit (not `type: dir`)
  plus the generated `cargo-sources.json`. Before submitting: run
  `flatpak run --command=flatpak-builder-lint org.flatpak.Builder manifest`
  and `... repo`, `appstreamcli validate` on the metainfo, add screenshots
  and the release entry to the metainfo. Flathub reviews names and branding:
  the app must not look like an official TeamSpeak product.
- For our own releases (GitHub): a tarball of `tsc-desktop` with the desktop
  file, metainfo, icon and notices, and optionally a Flatpak bundle
  (`flatpak build-bundle --gpg-sign=<key> ...`). Publish `SHA256SUMS` and
  sign it (`gpg --detach-sign` or minisign) with a key whose fingerprint is
  in the README.

### Android

APKs and app bundles are signed with a keystore kept outside the
repository; losing it means existing installs can no longer be updated.

- Create it once:
  `keytool -genkeypair -v -keystore tsc-release.jks -alias tsc -keyalg RSA -keysize 4096 -validity 10000`,
  and back it up (two offline copies, password in a password manager).
- The Gradle build reads the keystore path, alias and passwords from Gradle
  properties or environment variables (`~/.gradle/gradle.properties`, CI
  secrets), never from files in the repository: `tsc.signing.storeFile`,
  `tsc.signing.storePassword`, `tsc.signing.keyAlias`,
  `tsc.signing.keyPassword`, or `TSC_SIGNING_STORE_FILE`,
  `TSC_SIGNING_STORE_PASSWORD`, `TSC_SIGNING_KEY_ALIAS`,
  `TSC_SIGNING_KEY_PASSWORD` ([android.md](android.md)). Without them it
  builds unsigned or debug-signed artifacts only.
- CI: the keystore as a base64 secret, decoded to `$RUNNER_TEMP`, removed
  afterwards. Check with `apksigner verify --print-certs app-release.apk`.
- Google Play: use Play App Signing; our keystore is then the upload key
  (it can be reset through Play support if lost). F-Droid builds and signs
  itself unless reproducible builds are set up.

## Checklist

Before tagging:

1. `main` is green in CI, including the smoke job (TS3 + TS6) and the
   browser interop job.
2. `cargo deny --workspace check` is clean; the ignores in `deny.toml` are
   still needed and still justified.
3. `scripts/notices.sh` has been run after the last dependency change (CI
   `notices` job green); new licenses in the diff were reviewed. The About
   page shows the AboutSlint widget (required by Slint's royalty-free
   license), the OpenH264 attribution and the notices.
4. The [manual test matrix](testing/manual-matrix.md) has been run for this
   version, results committed; failures are fixed or listed as known issues.
5. Translations: `.pot` regenerated and `.po` files merged
   ([i18n.md](i18n.md)); string freeze announced a week before.
6. Versions bumped (`crates/tsc-ui`, `crates/tsc-android`, Android
   `versionName` / `versionCode`, `tsc-gateway` if it changed),
   `Cargo.lock` updated, changelog and metainfo `<release>` written.

Release:

7. Tag `v<version>` (signed) on the release commit and push it.
8. Build the artifacts from the tag: Windows executable and installer
   (signed), Linux tarball, Flatpak bundle, Android APK / AAB (signed); the
   gateway container image if it changed.
9. Verify: signatures (`signtool verify`, `apksigner verify`), the installer
   and the Flatpak on a clean machine (matrix rows IN1-IN3), the version in
   the About page.
10. `SHA256SUMS` (signed), a draft GitHub release with the changelog section,
    known issues and the tested server versions; publish it.
11. Update the Flathub manifest (tag, commit, `cargo-sources.json`), Google
    Play / F-Droid.

After:

12. Add an empty `Unreleased` section to the changelog.
