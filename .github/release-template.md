## Install

| Platform | Installer | One-line script | Raw binary |
| --- | --- | --- | --- |
| **macOS** (Apple silicon) | [`parsec-{{VERSION}}-macos-arm64.pkg`]({{DL}}/parsec-{{VERSION}}-macos-arm64.pkg) | `curl -fsSL https://raw.githubusercontent.com/{{REPO}}/main/scripts/install.sh \| bash` | [`parsec-darwin-arm64`]({{DL}}/parsec-darwin-arm64) |
| **Windows** (x64) | [`parsec-{{VERSION}}-windows-x64-setup.exe`]({{DL}}/parsec-{{VERSION}}-windows-x64-setup.exe) | `irm https://raw.githubusercontent.com/{{REPO}}/main/scripts/install.ps1 \| iex` | [`parsec-win-x64.exe`]({{DL}}/parsec-win-x64.exe) + VC++ runtime DLLs |
| **Linux** (x64) | — | `curl -fsSL https://raw.githubusercontent.com/{{REPO}}/main/scripts/install.sh \| bash` | [`parsec-linux-x64`]({{DL}}/parsec-linux-x64) |

The installers sign you in, install the `parsec` binary, and install the
Claude Code plugin. Intel Macs are not built.

**Claude Code plugin only** — the plugin fetches the matching binary from this
release on first use:

```sh
claude plugin marketplace add https://github.com/{{REPO}}
claude plugin install parsec@parsec-marketplace
```

[`parsec-plugin.zip`]({{DL}}/parsec-plugin.zip) is the same plugin with all
three binaries bundled, for offline installs.

**Checksums**: [`manifest.json`]({{DL}}/manifest.json) lists the sha256 of
every asset; the install scripts and the plugin shim verify against it.
