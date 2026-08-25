# parsec -- one-line installer for Windows (PowerShell 5.1+):
#
#   powershell -c "irm https://raw.githubusercontent.com/daseinlabs/plugins/main/install.ps1 | iex"
#
# Auto-detects the coding agents on this machine (Claude Code, Codex CLI,
# opencode) and activates parsec for each. Claude Code gets the plugin
# (`claude plugin install parsec@parsec-marketplace`); codex/opencode get
# the win-x64 parsec binary downloaded to %USERPROFILE%\.parsec\bin\
# parsec.exe (added to the user PATH) followed by `parsec setup <tool>`.
# The app-local VC++ CRT DLLs are downloaded beside it -- the loader only
# searches next to the exe. Pass -Tray to also install the tray app
# (notification area + taskbar, HKCU Run entry, no admin).
# Claude Desktop is detected and set up too: it is the one surface with no
# endpoint setting, so parsec reaches it by process-scoped TLS interception --
# which means installing mitmproxy (winget) and trusting its CA in the machine
# root store. That last step is machine-wide, so it always goes through a
# visible Windows administrator prompt, never silently. Skip it with
# -NoDesktop, or take the interception without the CA with -NoCa.
# Nothing is written outside ~\.parsec and the tools' own config dirs; no
# admin rights needed -- except the Claude Desktop CA, which is machine-wide
# and prompts for administrator approval. Undo:
# `parsec disable codex|opencode|desktop` (which prints the CA removal
# command), `claude plugin uninstall parsec`.
#
# Explicit selection instead of auto-detect, and Codex API-key mode:
#
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools codex
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools codex -Byok
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools claude,opencode
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools desktop
#   & ([scriptblock]::Create((irm .../install.ps1))) -NoDesktop
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools desktop -NoCa
#
# (or set $env:PARSEC_TOOLS = "codex" / $env:PARSEC_BYOK = "1" /
# $env:PARSEC_NO_DESKTOP = "1" / $env:PARSEC_NO_CA = "1" before the plain
# irm|iex form.)
#
# Source of truth: scripts/install.ps1 in the parsec repo; release.yml
# publishes it next to the binaries it references, so script and binaries
# always ship from the same commit.
param(
    [string[]]$Tools = @(),
    [switch]$Byok,
    # Leave Claude Desktop alone even when it is installed. The interception
    # path is the only one that needs a third-party tool and a root CA, so it
    # gets its own opt-out rather than making people list every other tool.
    [switch]$NoDesktop,
    # Set Desktop up but do NOT trust mitmproxy's CA -- parsec prints the
    # command and Desktop stays unintercepted until it is run. For anyone who
    # wants to read the command before a root cert lands in their trust store.
    [switch]$NoCa,
    # Install the tray app (notification area + taskbar) and register it to
    # start at sign-in. Opt-in: a login item is a persistent, visible addition
    # to someone's machine and should not appear because they installed a CLI.
    [switch]$Tray
)
$ErrorActionPreference = "Stop"
# PS 7.4 defaults $PSNativeCommandUseErrorActionPreference to $true, which turns
# every non-zero exit from a native command (winget, certutil, parsec.exe) into
# a terminating error -- so the $LASTEXITCODE checks below would never run and a
# recoverable failure would abort the install. 5.1 has no such variable and
# ignores this line.
$PSNativeCommandUseErrorActionPreference = $false
# PowerShell 5.1 defaults to TLS 1.0 -- GitHub requires 1.2+.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Base = if ($env:PARSEC_INSTALL_BASE) { $env:PARSEC_INSTALL_BASE } else { "https://raw.githubusercontent.com/daseinlabs/plugins/main" }
$MarketplaceUrl = if ($env:PARSEC_MARKETPLACE_URL) { $env:PARSEC_MARKETPLACE_URL } else { "https://github.com/daseinlabs/plugins" }

# -- arguments (param, or env fallbacks for the plain irm|iex form) -----------
if (-not $Tools -and $env:PARSEC_TOOLS) { $Tools = $env:PARSEC_TOOLS -split "[ ,]+" }
if ($env:PARSEC_BYOK -eq "1") { $Byok = $true }
if ($env:PARSEC_NO_DESKTOP -eq "1") { $NoDesktop = $true }
if ($env:PARSEC_NO_CA -eq "1") { $NoCa = $true }
$Tools = @($Tools | ForEach-Object { if ($_ -eq "claude-code") { "claude" } else { $_ } })
foreach ($t in $Tools) {
    if ($t -notin @("claude", "codex", "opencode", "desktop")) {
        Write-Error "unknown tool: $t (expected: claude, codex, opencode, desktop)"
    }
}

function Test-Cmd([string]$Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

# Pick up PATH changes a just-run installer made -- winget updates the stored
# environment, not this process's copy, so mitmdump would look missing until a
# new shell without this.
function Update-SessionPath {
    $parts = @(
        [Environment]::GetEnvironmentVariable("Path", "Machine"),
        [Environment]::GetEnvironmentVariable("Path", "User")
    ) | Where-Object { $_ }
    $env:Path = $parts -join ";"
}

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    return (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

# Identity-checked stop of a running parsec proxy (never kills a foreign
# process). Needed BEFORE replacing the exe: Windows locks a running binary.
function Stop-ParsecProxy {
    $port = 8082
    $statePath = Join-Path $env:USERPROFILE ".parsec\setup_state.json"
    if (Test-Path $statePath) {
        try {
            $st = Get-Content -Raw $statePath | ConvertFrom-Json
            if ($st.port -gt 0) { $port = $st.port }
        }
        catch {}
    }
    try {
        $h = Invoke-RestMethod -Uri "http://127.0.0.1:$port/health" -TimeoutSec 2
        if ("$($h.service)" -ne "parsec-proxy") { return }
        Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$port/shutdown" -TimeoutSec 2 | Out-Null
        Write-Host "stopped the running parsec proxy on port $port (old binary)"
        Start-Sleep -Milliseconds 800
    }
    catch {}
}

# -- auto-detect --------------------------------------------------------------
if (-not $Tools) {
    # Claude Code needs its CLI present (the plugin installs through it).
    if (Test-Cmd claude) { $Tools += "claude" }
    $codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE ".codex" }
    if ((Test-Cmd codex) -or (Test-Path $codexHome)) { $Tools += "codex" }
    # opencode uses XDG-style paths on every platform.
    $ocCfg = if ($env:XDG_CONFIG_HOME) { Join-Path $env:XDG_CONFIG_HOME "opencode" } else { Join-Path $env:USERPROFILE ".config\opencode" }
    if ((Test-Cmd opencode) -or (Test-Path $ocCfg)) { $Tools += "opencode" }
    # Claude Desktop: same install location setup_desktop.rs probes
    # (claude_desktop_installed()), so detection here and the binary's own
    # report cannot disagree. -NoDesktop opts out, because this is the one
    # tool whose setup installs mitmproxy and trusts a root CA.
    $desktopExe = Join-Path $env:LOCALAPPDATA "AnthropicClaude\Claude.exe"
    if ((Test-Path $desktopExe) -and -not $NoDesktop) { $Tools += "desktop"; $desktopAuto = $true }
    if (-not $Tools) {
        Write-Error "no supported Claude client found (looked for: claude, codex, opencode, Claude Desktop). Install one first, or pick explicitly: -Tools codex"
    }
    Write-Host "detected: $($Tools -join ' ')"
    if ($Tools -contains "desktop") {
        Write-Host "  desktop needs mitmproxy + a trusted root CA (you will get an administrator prompt); skip it with -NoDesktop"
    }
}

if ($Byok -and ("codex" -notin $Tools)) {
    Write-Error "-Byok only applies to codex"
}
if ($NoDesktop -and ("desktop" -in $Tools)) {
    Write-Error "-NoDesktop contradicts -Tools desktop - pick one"
}
if ($NoCa -and ("desktop" -notin $Tools) -and -not $NoDesktop) {
    # Harmless on its own, but it usually means someone expected desktop to be
    # in the list and it is not -- say so instead of silently ignoring it.
    Write-Host "note: -NoCa only affects Claude Desktop setup"
}

# Desktop needs the parsec binary, which is published for win-x64 only. When
# desktop was AUTO-detected, drop it here rather than let the architecture
# check below abort an install that would otherwise have set Claude Code up
# fine. Asked for explicitly, it still errors -- that was a request, not a
# guess.
if ($desktopAuto -and $env:PROCESSOR_ARCHITECTURE -ne "AMD64" -and ("desktop" -in $Tools)) {
    Write-Warning "skipping Claude Desktop: it needs the win-x64 parsec binary, which does not run on $env:PROCESSOR_ARCHITECTURE"
    $Tools = @($Tools | Where-Object { $_ -ne "desktop" })
}

# -- platform binary (codex/opencode/desktop -- the Claude Code plugin ships
#    its own) ------------------------------------------------------------------
$dest = Join-Path $env:USERPROFILE ".parsec\bin\parsec.exe"
$needsBinary = ($Tools -contains "codex") -or ($Tools -contains "opencode") -or ($Tools -contains "desktop") -or $Tray
if ($needsBinary) {
    if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
        Write-Error "unsupported architecture: $env:PROCESSOR_ARCHITECTURE (only win-x64 today; ARM64 Windows: use WSL or the Claude Code plugin)"
    }
    $destDir = Split-Path $dest
    New-Item -ItemType Directory -Force -Path $destDir | Out-Null
    # Download beside the destination, verify it runs, then move into place.
    $tmp = Join-Path $destDir ("parsec-download-{0}.exe" -f ([IO.Path]::GetRandomFileName() -replace "\..*$", ""))
    try {
        Write-Host "downloading parsec (win-x64)..."
        Invoke-WebRequest -Uri "$Base/plugins/parsec/bin/win-x64/parsec.exe" -OutFile $tmp -UseBasicParsing
        # App-local VC++ CRT. parsec.exe imports msvcp140/vcruntime140, which
        # are absent on a clean Windows box; the loader only searches NEXT TO
        # the exe, so these must land in the same directory or the process
        # dies before main() with 0xC0000135 and no stderr. release.yml ships
        # them beside the exe for exactly this reason -- downloading the exe
        # alone reproduced the bug the bundling exists to prevent.
        foreach ($dll in "msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll") {
            try {
                Invoke-WebRequest -Uri "$Base/plugins/parsec/bin/win-x64/$dll" `
                    -OutFile (Join-Path $destDir $dll) -UseBasicParsing
            }
            catch {
                # A release that no longer needs the CRT will not publish them;
                # the --version check below is the real gate either way.
                Write-Host "(no $dll published - continuing)"
            }
        }
        & $tmp --version | Out-Null # refuse to install a binary that cannot run
        if ($LASTEXITCODE -ne 0) { throw "downloaded binary failed --version" }
        # Windows locks a running exe -- stop an old proxy BEFORE the swap.
        Stop-ParsecProxy
        try {
            Move-Item -Force $tmp $dest
        }
        catch {
            Write-Error "could not replace $dest (something still holds it -- close it and re-run): $_"
        }
    }
    finally {
        if (Test-Path $tmp) { Remove-Item -Force $tmp }
    }
    # A proxy that predates this install keeps serving the OLD binary --
    # restart so the fresh one owns the port (identity-checked: a foreign
    # process on the port is never killed). In-flight requests from other
    # sessions see one brief blip and recover on their next request.
    & $dest up --restart
    if ($LASTEXITCODE -ne 0) { Write-Error "proxy restart failed" }
    # Stable PATH entry so the skills/shims' `parsec` fallback resolves
    # (user-scope; no admin). Current session too.
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $destDir) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$destDir", "User")
        Write-Host "added $destDir to your user PATH (takes effect in new terminals)"
    }
    if (($env:Path -split ";") -notcontains $destDir) { $env:Path = "$env:Path;$destDir" }
}

# -- Claude Desktop -----------------------------------------------------------
# Desktop has no endpoint setting (its embedded SDK is pinned to
# api.anthropic.com), so the only route in is process-scoped TLS interception
# via mitmproxy -- see docs/claude-desktop-integration.md.
#
# On Windows that interception runs through WinDivert, whose driver needs
# ADMINISTRATOR rights, and so does trusting the CA. Both live inside
# `parsec setup desktop`, so this asks for elevation ONCE and runs the whole
# provision under it. Doing it any other way loses a race: setup_desktop.rs
# spawns mitmdump detached and requires it alive 1.5s later, which a human
# approving a UAC dialog cannot beat.
function Install-ParsecDesktop([string]$Exe, [bool]$TrustCa) {
    if (-not (Test-Cmd mitmdump)) {
        if (Test-Cmd winget) {
            Write-Host "mitmproxy not found - installing it (winget)..."
            # --disable-interactivity: this script is run through irm|iex and
            # must never block on an agreement prompt. A failure here is not
            # fatal; the missing-mitmdump message below is the real gate.
            try {
                winget install --id mitmproxy.mitmproxy --source winget --silent `
                    --accept-package-agreements --accept-source-agreements --disable-interactivity
            }
            catch { Write-Host "(winget install failed - continuing)" }
            Update-SessionPath
        }
    }
    if (-not (Test-Cmd mitmdump)) {
        Write-Warning ("mitmproxy is not installed, so Claude Desktop cannot be intercepted.`n" +
            "  install: winget install mitmproxy.mitmproxy   (or: pip install mitmproxy)`n" +
            "  then:    parsec setup desktop")
        return
    }
    Write-Host ("mitmproxy: {0}" -f (Get-Command mitmdump).Source)

    $setupArgs = @("setup", "desktop")
    if ($TrustCa) { $setupArgs += "--install-ca" }

    if (Test-Admin) {
        try { & $Exe @setupArgs }
        catch {
            Write-Warning "parsec setup desktop failed to run: $($_.Exception.Message)"
            return
        }
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "parsec setup desktop did not complete - fix what it reported above and re-run: parsec setup desktop"
            return
        }
    }
    else {
        Write-Host "Claude Desktop interception needs administrator (WinDivert driver$(if ($TrustCa) { ' + CA trust' })) - approving the prompt runs the whole setup..."
        try {
            $p = Start-Process -FilePath $Exe -ArgumentList $setupArgs -Verb RunAs -Wait -PassThru
        }
        catch {
            # Declined UAC. Everything else this script did still stands; only
            # Desktop is left unprovisioned, and the command is one line.
            Write-Warning ("administrator approval declined - Claude Desktop was not set up.`n" +
                "  from an elevated PowerShell, run:  parsec $($setupArgs -join ' ')")
            return
        }
        if ($p.ExitCode -ne 0) {
            Write-Warning ("parsec setup desktop exited $($p.ExitCode) - re-run it from an elevated PowerShell to see why:`n" +
                "  parsec $($setupArgs -join ' ')")
            return
        }
        # The elevated console is gone with its output, so report from here.
        & $Exe desktop status
    }
    if (-not $TrustCa) {
        Write-Host "CA not trusted (-NoCa) - Desktop stays unintercepted until you run the printed certutil command."
    }
}

# -- per-tool setup -----------------------------------------------------------
foreach ($t in $Tools) {
    Write-Host ""
    Write-Host ("-- setting up {0} --" -f $t)
    switch ($t) {
        "claude" {
            # The plugin route: same binary plus the status line, hooks, and
            # skills. marketplace add is idempotent-ish -- tolerate "already
            # added" and let install be the arbiter.
            try { claude plugin marketplace add $MarketplaceUrl 2>$null | Out-Null } catch { Write-Host "(marketplace already added - continuing)" }
            claude plugin install parsec@parsec-marketplace
            if ($LASTEXITCODE -eq 0) {
                Write-Host "Claude Code plugin installed - get a key at https://app.getparsec.ai and run /parsec:key in a session."
            }
            else {
                Write-Warning "plugin install failed - do it manually:`n  claude plugin marketplace add $MarketplaceUrl`n  claude plugin install parsec@parsec-marketplace"
            }
        }
        "codex" {
            if ($Byok) { & $dest setup codex --byok } else { & $dest setup codex }
        }
        "opencode" {
            & $dest setup opencode
        }
        "desktop" {
            Install-ParsecDesktop -Exe $dest -TrustCa (-not $NoCa)
        }
    }
}

# -- tray app (opt-in) --------------------------------------------------------
if ($Tray) {
    Write-Host ""
    Write-Host "-- setting up the tray app --"
    # `tray install` copies nothing on Windows: it writes a hidden-window
    # launcher and a HKCU Run entry pointing at the alias below. No admin.
    & $dest tray install
    if ($LASTEXITCODE -ne 0) { Write-Warning 'tray install failed - run: parsec tray install' }
}

Write-Host ""
if ($needsBinary) {
    Write-Host ("installed {0} at {1}" -f (& $dest --version), $dest)
}
if (-not $Tray) {
    Write-Host "tray app (notification area + taskbar, starts at sign-in): parsec tray install"
}
if ($Tools -contains "claude") { Write-Host "claude: restart Claude Code (or start a new session) - setup runs automatically." }
if ($Tools -contains "codex") { Write-Host "codex: start (or restart) codex - every session routes through parsec; type `$ and pick parsec-savings." }
if ($Tools -contains "opencode") { Write-Host "opencode: restart opencode to activate (Anthropic API-key providers only); /parsec-savings shows the ledger." }
if ($Tools -contains "desktop") {
    Write-Host "desktop: quit Claude Desktop COMPLETELY (tray icon -> Quit, not just the window) and reopen it - mitmproxy hooks the process at launch."
    Write-Host "         only Cowork / Agent mode is routed; the normal chat sidebar is not. Check with: parsec desktop status"
    Write-Host "         the interceptor runs elevated (WinDivert), so stopping it needs an admin shell: parsec desktop stop"
}
Write-Host "undo: parsec disable codex|opencode|desktop - parsec tray uninstall - claude plugin uninstall parsec"
