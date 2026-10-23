# Releasing

For maintainers: how a release is built, signed and published.

Pushing a version tag builds and publishes a GitHub Release
([`.github/workflows/release.yml`](../.github/workflows/release.yml)):

```bash
# bump `version` in Cargo.toml first; the tag must match it
git tag v0.2.0
git push origin v0.2.0
```

The release gets a universal macOS `Kuana.app` (`.dmg` and `.zip`), a Windows installer
(`-setup.exe`, per user, no admin rights; made with Inno Setup from
[`scripts/kuana.iss`](../scripts/kuana.iss)) and `.zip`, and a Linux `.tar.gz`, each for x86_64 and
aarch64 (built on GitHub's Arm runners, `windows-11-arm` and `ubuntu-22.04-arm`), plus `latest.json`
(one archive per platform key, the keys `platform()` in `src/updater.rs` asks for), which the workflow signs with
[minisign](https://jedisct1.github.io/minisign/) using the `MINISIGN_SECRET_KEY` repository
secret. Tags with a hyphen (`v0.3.0-beta.1`) become pre-releases, which the updater skips.
Once published, the [smoke test](../.github/workflows/smoke.yml) downloads each platform's build
and drives it through its main areas (`scripts/smoke.autopilot`), Linux under Xvfb with software
Vulkan; its screenshots are the run's artifacts. Run it by hand for any tag from Actions → Smoke
test.
The binaries are built with Cargo.toml's `dist` profile (thin LTO and one codegen unit, as Zed's
releases), which takes longer than `cargo build --release`; to try one locally,
`cargo build --profile dist` puts it in `target/dist/`. Its line tables for backtraces go to a file
beside the binary (`kuana.dSYM`, `kuana.dwp`, `kuana.pdb`), which the releases don't include.
The release notes come from [git-cliff](https://git-cliff.org) ([`cliff.toml`](../cliff.toml)):
every commit and pull request since the previous tag, each change once (a pull request is one
line, however it was merged), with authors @mentioned and first-time contributors listed.

### Homebrew cask

Once a release is published, the workflow's `homebrew` job copies
[`scripts/kuana.rb`](../scripts/kuana.rb) into
[berkayakin/homebrew-tap](https://github.com/berkayakin/homebrew-tap) as `Casks/kuana.rb`, with the
release's version and the SHA-256 of its macOS `.zip`, and pushes it. Change the cask in
`scripts/kuana.rb`, not in the tap. It needs the `HOMEBREW_TAP_TOKEN` repository secret: a
fine-grained token with read and write access to the tap's contents only. Without it, or for a
pre-release or a release without a macOS build, the cask stays as it was. Once releases are
notarized, remove the cask's Gatekeeper `caveats`.

### Signing the macOS app

The workflow signs `Kuana.app` and the `.dmg` with a Developer ID and has Apple notarize them as
soon as these repository secrets exist (Settings → Secrets and variables → Actions). It needs
an [Apple Developer Program](https://developer.apple.com/programs/) membership.

1. **Certificate:** in Xcode → Settings → Accounts → Manage Certificates, add a **Developer ID
   Application** certificate. In Keychain Access → My Certificates, export it (with its private
   key) as a `.p12` with a password.
   - `MACOS_CERTIFICATE`: the file as base64, `base64 -i Certificates.p12 | pbcopy`
   - `MACOS_CERTIFICATE_PASSWORD`: the export password
2. **Notarization key:** in [App Store Connect](https://appstoreconnect.apple.com) → Users and
   Access → Integrations → App Store Connect API, generate a team key with the Developer role and
   download `AuthKey_….p8` (possible only once).
   - `APPLE_API_KEY`: the `.p8` file's text, `pbcopy < AuthKey_….p8`
   - `APPLE_API_KEY_ID`: the key's ID
   - `APPLE_API_ISSUER`: the Issuer ID shown above the keys

With only the certificate, releases are signed but not notarized, which macOS still blocks.
Notarizing adds a few minutes per release; a rejection fails the macOS build with Apple's log
([`scripts/notarize.sh`](../scripts/notarize.sh)).
