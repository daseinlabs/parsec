# parsec native installers

Double-clickable, branded installers for macOS and Windows. Both are thin
orchestration around the CLI that already exists (`parsec setup …`,
`parsec setup desktop`, `parsec tray install`, `parsec disable …`,
`parsec uninstall`); what they add is the OS packaging and — the point —
**every permission step inside the one install flow**:

| step | macOS `.pkg` | Windows `setup.exe` |
|---|---|---|
| install the binary, PATH | Installer's one auth dialog (root postinstall) | per-user, no prompt |
| trust mitmproxy's CA (Claude Desktop) | root postinstall, `security add-trusted-cert` — no `sudo` prompt | inside the one UAC prompt |
| WinDivert driver / scheduled task | n/a | inside the same UAC prompt |
| Network Extension approval | registered + System Settings opened by `parsec setup desktop --prepare`; the menu-bar app waits for the toggle and finishes | n/a |
| login items | LaunchAgents, as the user | HKCU Run + scheduled task |

```
packages/installer/
├── assets/          gen.sh (ImageMagick) regenerates every raster from brand/parsecbrandkit; JetBrains Mono vendored (OFL)
├── macos/           Distribution.xml · resources/ (panes, backgrounds) · scripts/<component>/postinstall · payload/uninstall.sh · build.sh
└── windows/         parsec.iss · build.ps1 · scripts/stop-parsec.ps1 · assets/ (wizard images, .ico)
```

## Build

macOS (on a Mac; also what CI runs):

```
cargo build --release -p parsec-proxy
packages/installer/macos/build.sh --binary target/release/parsec --out /tmp/parsec-pkg
open /tmp/parsec-pkg/parsec-*-macos-arm64.pkg           # GUI pass
sudo installer -pkg /tmp/parsec-pkg/parsec-*.pkg -target / -dumplog -verboseR   # CLI pass; scripts log to /var/log/install.log
```

Windows (Inno Setup 6.5+; `winget install JRSoftware.InnoSetup`):

```
cargo build --release -p parsec-proxy
# copy msvcp140{,_1}.dll and vcruntime140{,_1}.dll beside target\release\parsec.exe (see release.yml "Bundle VC++ runtime DLLs")
packages\installer\windows\build.ps1 -Version 0.2.8 -BinDir target\release
```

CI: the `installers` job in `.github/workflows/release.yml` builds both from
the `build` job's artifacts and `pack` adds them to the release assets, so
`manifest.json` carries their sha256.

## Signing

Both builds sign in CI when the repository secrets below exist (Settings →
Secrets and variables → Actions) and build unsigned with a notice when they
don't, so a fork or a missing secret degrades to a working installer rather
than a red job.

### macOS

Two certificates, one App Store Connect API key. Notarization checks the
Mach-O inside the payload, not just the pkg wrapper, so the Application
certificate is not optional: with only the Installer certificate set the job
deliberately builds unsigned and warns.

| secret | what it is | used for |
|---|---|---|
| `APPLE_DEVELOPER_ID_APPLICATION_P12` (base64) + `APPLE_DEVELOPER_ID_APPLICATION_P12_PASSWORD` | "Developer ID Application: Dasein Labs (TEAMID)" cert + private key | `codesign --options runtime --timestamp` on the `parsec` binary and `--deep` on the shipped `parsec.app` |
| `APPLE_DEVELOPER_ID_INSTALLER_P12` (base64) + `APPLE_DEVELOPER_ID_INSTALLER_P12_PASSWORD` | "Developer ID Installer: Dasein Labs (TEAMID)" cert + private key | `productsign` on the .pkg |
| `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_ISSUER_ID`, `APPLE_NOTARY_KEY_P8` | App Store Connect API key (Team key, Developer role), its issuer UUID, and the `.p8` contents | `notarytool submit --wait` + `stapler` |

Getting them, once, on a Mac signed in to the Apple Developer account:

1. **Certificates.** Keychain Access → Certificate Assistant → Request a
   Certificate From a Certificate Authority (save to disk). At
   developer.apple.com/account/resources/certificates create one
   *Developer ID Application* and one *Developer ID Installer* certificate
   from that CSR, download both `.cer` files and double-click them so they
   pair with the private key in the login keychain. Then export each
   identity (the cert *and* its key, so pick the cert row that expands to a
   key) as `.p12` with a password, and base64 it:
   `base64 -i DeveloperIDApplication.p12 | pbcopy`.
2. **Notary key.** App Store Connect → Users and Access → Integrations →
   App Store Connect API → Team Keys → Generate (role: Developer). Note the
   Key ID and Issuer ID shown on that page; download the `.p8` once (Apple
   never shows it again) and paste its full text into `APPLE_NOTARY_KEY_P8`.
3. Sanity check locally before trusting CI:
   `DEVELOPER_ID_APPLICATION='Developer ID Application: …' DEVELOPER_ID_INSTALLER='Developer ID Installer: …' APPLE_NOTARY_KEY_ID=… APPLE_NOTARY_ISSUER_ID=… APPLE_NOTARY_KEY_P8_PATH=~/AuthKey.p8 packages/installer/macos/build.sh --binary target/release/parsec --out /tmp/parsec-pkg --sign --notarize`
   — `spctl -a -vv -t install` at the end must print `accepted`.

Developer ID certificates last five years; the CI job reads them fresh from
the secrets on every run, so rotation is just re-uploading the two `.p12`
values.

### Windows

Azure Artifact Signing (Microsoft's managed code-signing service, formerly
Trusted Signing). There is no certificate file: the key lives in Microsoft's
HSM, certificates rotate every few days, and signatures are timestamped so
they outlive them. The CI job authenticates with a GitHub OIDC token, so the
only "secrets" are identifiers.

| secret | value |
|---|---|
| `AZURE_TENANT_ID` | the Entra tenant that owns the signing account |
| `AZURE_CLIENT_ID` | app registration `parsec-github-signing` (has the *Artifact Signing Certificate Profile Signer* role on the account) |
| `AZURE_SUBSCRIPTION_ID` | subscription holding resource group `parsec-signing` |

Endpoint (`https://eus.codesigning.azure.net/`), account
(`daseinlabs-signing`) and certificate profile (`parsec-public-trust`) are
not secret and live in `release.yml`. The app trusts GitHub through a
federated credential whose subject is `repo:daseinlabs@290107210/parsec@1301707912:environment:release` (GitHub
sends owner and repo ids in the subject for this repo, `use_immutable_subject`),
which is why the `installers` job declares `environment: release`.

`build.ps1` signs parsec.exe with `signtool … /dlib Azure.CodeSigning.Dlib.dll
/dmdf metadata.json` (the dlib comes from the `Microsoft.ArtifactSigning.Client`
NuGet package, fetched by the workflow) and hands the same command to Inno's
`SignTool` directive for Setup and the uninstaller, then verifies both
signatures. Locally, `az login` as a user with the Certificate Profile Signer
role and pass `-SigningEndpoint/-SigningAccount/-SigningProfile/-DlibPath`.

The identity validation behind the profile is an *individual* one (the
certificate subject is the developer's name, city and state) and expires
2027-09-12; renew it in the portal before then or signing stops.

### Until then

A browser-downloaded unsigned .pkg is quarantined (macOS 15+: System
Settings → Privacy & Security → Open Anyway, or `xattr -d
com.apple.quarantine`), and SmartScreen shows "More info → Run anyway" for an
unsigned .exe. `curl`-downloaded files carry no quarantine flag. A signed
setup.exe still gets the SmartScreen prompt until the publisher has built up
download reputation; that is per signer identity and only time fixes it.

## What the user-facing flow looks like

**macOS.** Welcome (mark, tagline, what will be asked) → Read Me (per-choice
table) → License → choices (auto-ticked from what is installed: Claude Code,
Codex, opencode, pi, Claude Desktop, menu-bar app) → one password dialog →
scripts → Conclusion. If Claude Desktop was chosen and the Network Extension
is not yet approved, System Settings opens during the install and the
menu-bar mark waits for the toggle, then starts interception. Everything that
still needs a hand is in `~/.parsec/install-summary.txt`.

**Windows.** Welcome → tasks (auto-ticked the same way; `desktop` hidden on
ARM64) → **Approvals** page listing exactly what runs without administrator
and the one UAC prompt if Claude Desktop is ticked → install → the elevated
`parsec setup desktop --install-ca --autostart` (WinDivert, CA, scheduled
task) → Finished, with a "Needs attention" block for anything that failed or
was declined. Upgrades are all-or-nothing: `[Files]` land in
`bin\staging` while the old proxy keeps serving, then the proxy and tray
are stopped and each file is renamed into place (the old one steps aside as
`<name>.old` — Windows allows renaming a mapped image, never overwriting it,
so an MCP server in an open Claude Code session keeps running on its
displaced copy). Any failure renames the old set back, so the bin dir never
loses its `parsec.exe`. The `.old` files are swept on the next upgrade and
at uninstall. Every post-install child (`up --restart`, the `claude plugin`
commands, `setup …`, `tray install`) runs through `scripts/run-step.ps1`:
stdin at EOF so nothing can sit on a hidden prompt, a per-step deadline
(the Finished page names a step that hit it), and stdout/stderr appended to
`%USERPROFILE%\.parsec\installer.log` — ask for that file when someone
reports a wizard that never finished. The Codex task is a
checkbox with two exclusive radio children (ChatGPT subscription, default,
or `--byok`), because a plain child checkbox in Inno is force-ticked with its
parent.

## Regenerating the artwork

```
packages/installer/assets/gen.sh     # needs ImageMagick 7 (brew install imagemagick)
```

Outputs are committed. The light-pane mark is the flat PNG recolored to
`#2FA317` (the one recolor BRANDING.md allows); everything else is the brand
kit's own PNGs composed onto the void.

## Manual QA checklist

macOS: fresh install on a second local account (console-user resolution,
`launchctl asuser`, CA as root); the Desktop path on a clean VM
(NotInstalled → prompt → AwaitingApproval → tray finishes); a Safari
download for the real Gatekeeper dialog; `sudo /usr/local/parsec/uninstall.sh`.

Windows (x64 and an ARM64 Windows 11 box): auto-detected tasks; Approvals
page; winget mitmproxy → one UAC → `parsec desktop status` shows interceptor
running, CA trusted, scheduled-task autostart; sign out/in →
`schtasks /Query /TN ParsecInterceptor /V` Last Result 0 and mitmdump
elevated; declined UAC → everything else installed + re-run command shown;
upgrade over a running proxy/tray/open Claude Code session; ARM64 hides
`desktop`; uninstall with and without purge; the dark wizard at 100/150/200 %.
