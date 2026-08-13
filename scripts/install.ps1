# parsec -- one-line installer for Windows (PowerShell 5.1+):
#
#   powershell -c "irm https://raw.githubusercontent.com/daseinlabs/plugins/main/install.ps1 | iex"
#
# Auto-detects the coding agents on this machine (Claude Code, Codex CLI,
# opencode) and activates parsec for each. Claude Code gets the plugin
# (`claude plugin install parsec@parsec-marketplace`); codex/opencode get
# the win-x64 parsec binary downloaded to %USERPROFILE%\.parsec\bin\
# parsec.exe (added to the user PATH) followed by `parsec setup <tool>`.
# Nothing is written outside ~\.parsec and the tools' own config dirs; no
# admin rights needed. Undo: `parsec disable codex|opencode`,
# `claude plugin uninstall parsec`.
#
# Explicit selection instead of auto-detect, and Codex API-key mode:
#
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools codex
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools codex -Byok
#   & ([scriptblock]::Create((irm .../install.ps1))) -Tools claude,opencode
#
# (or set $env:PARSEC_TOOLS = "codex" / $env:PARSEC_BYOK = "1" before the
# plain irm|iex form.)
#
# Source of truth: scripts/install.ps1 in the parsec repo; release.yml
# publishes it next to the binaries it references, so script and binaries
# always ship from the same commit.
param(
    [string[]]$Tools = @(),
    [switch]$Byok
)
$ErrorActionPreference = "Stop"
# PowerShell 5.1 defaults to TLS 1.0 -- GitHub requires 1.2+.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Base = if ($env:PARSEC_INSTALL_BASE) { $env:PARSEC_INSTALL_BASE } else { "https://raw.githubusercontent.com/daseinlabs/plugins/main" }
$MarketplaceUrl = if ($env:PARSEC_MARKETPLACE_URL) { $env:PARSEC_MARKETPLACE_URL } else { "https://github.com/daseinlabs/plugins" }

# -- arguments (param, or env fallbacks for the plain irm|iex form) -----------
if (-not $Tools -and $env:PARSEC_TOOLS) { $Tools = $env:PARSEC_TOOLS -split "[ ,]+" }
if ($env:PARSEC_BYOK -eq "1") { $Byok = $true }
$Tools = @($Tools | ForEach-Object { if ($_ -eq "claude-code") { "claude" } else { $_ } })
foreach ($t in $Tools) {
    if ($t -notin @("claude", "codex", "opencode")) {
        Write-Error "unknown tool: $t (expected: claude, codex, opencode)"
    }
}

function Test-Cmd([string]$Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
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
    if (-not $Tools) {
        Write-Error "no supported coding agent found (looked for: claude, codex, opencode). Install one first, or pick explicitly: -Tools codex"
    }
    Write-Host "detected: $($Tools -join ' ')"
}

if ($Byok -and ("codex" -notin $Tools)) {
    Write-Error "-Byok only applies to codex"
}

# -- platform binary (codex/opencode only -- the Claude Code plugin ships its
#    own) --------------------------------------------------------------------
$dest = Join-Path $env:USERPROFILE ".parsec\bin\parsec.exe"
if (($Tools -contains "codex") -or ($Tools -contains "opencode")) {
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
    }
}

Write-Host ""
if (($Tools -contains "codex") -or ($Tools -contains "opencode")) {
    Write-Host ("installed {0} at {1}" -f (& $dest --version), $dest)
}
if ($Tools -contains "claude") { Write-Host "claude: restart Claude Code (or start a new session) - setup runs automatically." }
if ($Tools -contains "codex") { Write-Host "codex: start (or restart) codex - every session routes through parsec; type `$ and pick parsec-savings." }
if ($Tools -contains "opencode") { Write-Host "opencode: restart opencode to activate (Anthropic API-key providers only); /parsec-savings shows the ledger." }
Write-Host "undo: parsec disable codex|opencode - claude plugin uninstall parsec"
