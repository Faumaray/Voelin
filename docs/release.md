# Releasing

How a release is versioned, built, signed and checked. The app id
`io.github.faumaray.Voelin` ([packaging/README.md](../packaging/README.md))
cannot change after the first release on Flathub or Google Play.

CI resource settings and the Linux Gitea runner setup are documented in
[ci.md](ci.md). Release packaging and draft publication run on GitHub and
Gitea ([git.faumaray.ru](https://git.faumaray.ru)); runner and credential
requirements differ.

## Gitea releases

`.gitea/workflows/release.yml` builds Linux tarballs and `.deb` files, Windows
zip and NSIS installer (MinGW cross-build), Android debug APK (arm64-v8a and
x86_64), a Flatpak bundle, and the gateway container image. It reuses the
Dockerfiles and `scripts/package.sh`, and builds packages only: it runs none
of the checks (formatting, Clippy, tests, TeamSpeak smoke tests); run CI by
hand for those before accepting a release.

- **Manual dispatch:** after all builds pass, download the
  `voelin-packages` artifact from the run. Manual runs never publish a draft,
  even when dispatched against a tag.
- **Push a `v*` tag:** the same artifact is uploaded, then packages and a
  combined `SHA256SUMS` are attached to a **draft** Gitea release. Review,
  sign checksums and publish manually, just as on GitHub.

Set the repository secret `RELEASE_TOKEN` to a Gitea personal access
token with `write:repository`, limited to this repository where supported
(Gitea refuses secret names that start with `GITEA_` or `GITHUB_`; the
workflow hands it to the script as `GITEA_RELEASE_TOKEN`).
The token's account must be able to write releases. The publishing script
uses the runner's `GITHUB_SERVER_URL` and `GITHUB_REPOSITORY`; it requires
HTTPS and never uses GitHub's release API. On this instance the server URL
is `https://git.faumaray.ru`. No registry credentials are needed: the gateway
image is attached as `tsgw-image.tar.gz`, not pushed to an OCI registry.

Retries update only a matching **draft**, replacing same-named assets.
Published releases are not changed; a failed upload leaves a partial draft
for retry. Do not publish or modify a draft while the workflow runs. Gitea
1.25 ignores workflow concurrency and environment approvals; use the
dedicated, serial trusted runner described in [ci.md](ci.md#gitea-release-runner).

For an optional signed Android release APK, set the same four signing secrets
listed below. They are written outside the Docker build context with private
permissions, passed as BuildKit secrets, and removed on success or failure.
Signed builds invalidate the signing stage's cache on every run; changing a
key or password cannot return a cached APK signed with the previous key.
Without the keystore secret, only the installable debug APK is produced; a
partially configured keystore fails the job rather than producing an
unsigned release. Windows and Flatpak packages are not code-signed by this
workflow.

## Versioning

- One version for the apps: `version` of `crates/voelin-ui` (desktop; the
  installer and the About page read it), `crates/voelin-android` and the
  Android app's `versionName`. Semantic versioning; before 1.0 a minor bump
  may break settings or the gateway protocol, and the changelog says so.
- Android `versionCode` must grow with every upload:
  `major * 1000000 + minor * 1000 + patch` (0.2.3 is 2003).
- The gateway `tsgw` (`crates/voelin-gateway`) is versioned on its own, since
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
of `packaging/linux/io.github.faumaray.Voelin.metainfo.xml` (shown by
software centers and Flathub). Release notes also state the TeamSpeak server
versions the release was tested against (TS3 and TS6 from
`dev/docker-compose.yml`).

## Third-party notices

`THIRD_PARTY_NOTICES.md` lists the licenses of everything compiled into the
apps. It is shown in the About page (`voelin_platform::notices::text()`) and
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
reputation with the publisher's certificate. Sign `voelin.exe` first,
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
    /dlib Azure.CodeSigning.Dlib.dll /dmdf metadata.json target\release\voelin.exe
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
- For our own releases (GitHub): a tarball of `voelin` with the desktop
  file, metainfo, icon and notices, and optionally a Flatpak bundle
  (`flatpak build-bundle --gpg-sign=<key> ...`). Publish `SHA256SUMS` and
  sign it (`gpg --detach-sign` or minisign) with a key whose fingerprint is
  in the README.

### Android

APKs and app bundles are signed with a keystore kept outside the
repository; losing it means existing installs can no longer be updated.

- Create it once:
  `keytool -genkeypair -v -keystore voelin-release.jks -alias voelin -keyalg RSA -keysize 4096 -validity 10000`,
  and back it up (two offline copies, password in a password manager).
- The Gradle build reads the keystore path, alias and passwords from Gradle
  properties or environment variables (`~/.gradle/gradle.properties`, CI
  secrets), never from files in the repository: `voelin.signing.storeFile`,
  `voelin.signing.storePassword`, `voelin.signing.keyAlias`,
  `voelin.signing.keyPassword`, or `VOELIN_SIGNING_STORE_FILE`,
  `VOELIN_SIGNING_STORE_PASSWORD`, `VOELIN_SIGNING_KEY_ALIAS`,
  `VOELIN_SIGNING_KEY_PASSWORD` ([android.md](android.md)). Without them it
  builds unsigned or debug-signed artifacts only.
- CI: the `android` job signs a release APK when the repository has the
  secrets `ANDROID_KEYSTORE_BASE64` (`base64 -w0 voelin-release.jks`),
  `VOELIN_SIGNING_STORE_PASSWORD`, `VOELIN_SIGNING_KEY_ALIAS` and
  `VOELIN_SIGNING_KEY_PASSWORD`; the keystore is decoded to `$RUNNER_TEMP` and
  removed afterwards. Check with `apksigner verify --print-certs app-release.apk`.
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
6. Versions bumped (`crates/voelin-ui`, `crates/voelin-android`, Android
   `versionName` / `versionCode`, `voelin-gateway` if it changed),
   `Cargo.lock` updated, changelog and metainfo `<release>` written.

Release:

7. Tag `v<version>` (signed) on the release commit and push it.
8. The tag's release workflow (`release.yml`, [building.md](building.md))
   builds every package (it runs no checks: run CI by hand first): the app's
   Linux tarball and `.deb`, Flatpak bundle, Windows zip and installer and
   Android APKs (signed when the secrets are set), and the gateway's Linux
   tarball, `.deb` and image; its `release` job attaches them with a
   `SHA256SUMS` to a draft GitHub release. Sign the
   Windows executable and installer, and an AAB for Google Play, by hand
   until CI has the certificates.
9. Verify: signatures (`signtool verify`, `apksigner verify`), the installer
   and the Flatpak on a clean machine (matrix rows IN1-IN3), the version in
   the About page.
10. Sign `SHA256SUMS`; write the changelog section, known issues and the
    tested server versions into the draft release; publish it.
11. Update the Flathub manifest (tag, commit, `cargo-sources.json`), Google
    Play / F-Droid.

After:

12. Add an empty `Unreleased` section to the changelog.
