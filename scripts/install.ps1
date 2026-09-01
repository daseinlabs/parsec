# parsec -- one-line installer for Windows (PowerShell 5.1+):
#
#   powershell -c "irm https://raw.githubusercontent.com/daseinlabs/plugins/main/install.ps1 | iex"
#
# Auto-detects the coding agents on this machine (Claude Code, Codex CLI,
# opencode) and activates parsec for each. Claude Code gets the plugin
# (`claude plugin install parsec@parsec-marketplace`); codex/opencode get
# the win-x64 parsec binary downloaded to %USERPROFILE%\.parsec\bin\
# parsec.exe (added to the user PATH) followed by `parsec setup <tool>`.
# ARM64 Windows 11 gets the same win-x64 binary (runs under the OS's x64
# emulation); only Claude Desktop interception is excluded there, since
# WinDivert's kernel driver has no ARM64 build.
# The app-local VC++ CRT DLLs are downloaded beside it -- the loader only
# searches next to the exe. Pass -Tray to also install the tray app
# (notification area + taskbar, HKCU Run entry, no admin).
# Claude Desktop is detected and set up too: it is the one surface with no
# endpoint setting, so parsec reaches it by process-scoped TLS interception --
# which means installing mitmproxy (winget) and trusting its CA in the machine
# root store. That last step is machine-wide, so it always goes through a
# visible Windows administrator prompt, never silently. A boot service (HKCU
# Run entry) is installed too, so interception survives reboots -- skip that
# with -NoAutostart. Skip Desktop entirely with -NoDesktop, or take the
# interception without the CA with -NoCa.
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
# $env:PARSEC_NO_DESKTOP = "1" / $env:PARSEC_NO_CA = "1" /
# $env:PARSEC_NO_AUTOSTART = "1" before the plain irm|iex form.)
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
    # Skip the boot service (HKCU Run entry) that keeps Desktop interception
    # alive across reboots. Default is to install it: without it the
    # interceptor dies with the session and Desktop silently goes unrouted
    # (fail open) until someone runs `parsec desktop start`.
    [switch]$NoAutostart,
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
if ($env:PARSEC_NO_AUTOSTART -eq "1") { $NoAutostart = $true }
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

# Where is Claude Desktop? One hardcoded path is not enough: it is a Squirrel
# app, so the launcher at the root of AnthropicClaude\ is a stub beside
# versioned app-<ver>\ directories, and machine-wide installs land somewhere
# else entirely. Returns a path, or $null when nothing matched.
#
# Presence is all this is used for. mitmproxy's local mode matches the process
# by NAME (`--mode local:Claude.exe`), so a path we failed to find never stops
# interception from working -- which is why -Tools desktop overrides a miss.
function Find-ClaudeDesktop {
    # 1. Running right now: the most authoritative answer available.
    try {
        $proc = Get-Process -Name claude -ErrorAction SilentlyContinue |
            Where-Object { $_.Path } | Select-Object -First 1
        if ($proc) { return $proc.Path }
    }
    catch {}

    # 2. Known install roots, per-user first (that is how Desktop ships).
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "AnthropicClaude\Claude.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\Claude\Claude.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\claude-desktop\Claude.exe"),
        (Join-Path $env:ProgramFiles "Claude\Claude.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Claude\Claude.exe")
    ) | Where-Object { $_ }
    foreach ($c in $candidates) { if (Test-Path $c) { return $c } }

    # 3. Squirrel's versioned payload: app-1.2.3\claude.exe, newest first.
    $sq = Join-Path $env:LOCALAPPDATA "AnthropicClaude"
    if (Test-Path $sq) {
        $hit = Get-ChildItem $sq -Directory -Filter "app-*" -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "claude.exe" } |
            Where-Object { Test-Path $_ } | Select-Object -First 1
        if ($hit) { return $hit }
    }

    # 4. App Paths -- what ShellExecute uses to resolve a bare "claude.exe".
    foreach ($k in @("HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\Claude.exe",
                     "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\Claude.exe")) {
        try {
            $v = (Get-ItemProperty -Path $k -ErrorAction Stop).'(default)'
            if ($v -and (Test-Path $v)) { return $v }
        }
        catch {}
    }

    # 5. Uninstall entries: InstallLocation, or DisplayIcon (which is the exe
    #    itself, sometimes with a ",0" icon index glued on).
    $roots = @("HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
               "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
               "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall")
    try {
        $entries = Get-ChildItem $roots -ErrorAction SilentlyContinue |
            ForEach-Object { Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue } |
            Where-Object { $_.DisplayName -like "*Claude*" -and $_.DisplayName -notlike "*Claude Code*" }
        foreach ($e in $entries) {
            if ($e.InstallLocation) {
                $exe = Join-Path $e.InstallLocation "Claude.exe"
                if (Test-Path $exe) { return $exe }
            }
            if ($e.DisplayIcon) {
                $exe = ($e.DisplayIcon -split ",")[0].Trim('"')
                if ($exe -and (Test-Path $exe)) { return $exe }
            }
        }
    }
    catch {}

    # 6. Start Menu shortcut, resolved through the shell.
    try {
        $lnk = Get-ChildItem (Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs") `
            -Filter "Claude.lnk" -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($lnk) {
            $target = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk.FullName).TargetPath
            if ($target -and (Test-Path $target)) { return $target }
        }
    }
    catch {}

    # 7. MSIX package -- the current Desktop installer registers under
    #    Program Files\WindowsApps. That root denies enumeration, but the
    #    package's own folder grants Users read, so the resolved path works.
    #    No Uninstall entry, no .lnk: none of the probes above can see it.
    try {
        $pkg = Get-AppxPackage -Name Claude -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($pkg -and $pkg.InstallLocation) {
            foreach ($rel in @("app\claude.exe", "Claude.exe")) {
                $exe = Join-Path $pkg.InstallLocation $rel
                if (Test-Path $exe) { return $exe }
            }
            # Layout moved? The registered package is presence enough (that
            # is all this function is used for -- see the header comment).
            return $pkg.InstallLocation
        }
    }
    catch {}

    return $null
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
    }
    catch { return } # nothing listening, or not ours -- nothing to stop
    try {
        Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$port/shutdown" -TimeoutSec 2 | Out-Null
        Write-Host "stopped the running parsec proxy on port $port (old binary)"
    }
    catch {
        # The supervisor answers /shutdown and THEN exits, so a throw here can
        # still mean a shutdown already in flight. Fall through to the poll
        # rather than assume it either worked or did not.
        Write-Host "shutdown request to port $port errored - checking whether it exits anyway"
    }
    # Poll instead of sleeping a fixed 800ms. The swap below races a process
    # that has ACKed the shutdown but not yet released its image, and a fixed
    # sleep is either wasted time or -- on a loaded machine -- not enough,
    # which surfaces as the "something still holds it" error at Move-Item.
    for ($i = 0; $i -lt 40; $i++) {
        try { Invoke-RestMethod -Uri "http://127.0.0.1:$port/health" -TimeoutSec 1 | Out-Null }
        catch { return } # connection refused = really gone
        Start-Sleep -Milliseconds 100
    }
    Write-Warning "the proxy on port $port acknowledged shutdown but is still listening"
}

# The tray runs `parsec.exe tray run` -- the SAME image the swap replaces, and
# Windows locks a running exe. Nothing else stops it: `parsec tray uninstall`
# deliberately leaves a running icon alone. Without this, an upgrade on any
# machine that ever ran -Tray (its HKCU Run entry starts it at every sign-in)
# fails at Move-Item with a sharing violation. Returns whether it stopped one,
# so the caller can bring it back on the NEW binary.
function Stop-ParsecTray {
    $stopped = $false
    try { $procs = Get-CimInstance Win32_Process -Filter "Name = 'parsec.exe'" -ErrorAction Stop }
    catch { return $false }
    foreach ($p in $procs) {
        # Match the command line, never just the path: a proxy, an MCP server
        # and the tray are all parsec.exe, and only the tray is ours to kill
        # here (the proxy got a graceful /shutdown above).
        if (-not ($p.CommandLine -and $p.CommandLine -match '\btray\s+run\b')) { continue }
        Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        $stopped = $true
    }
    if ($stopped) {
        Write-Host "stopped the running parsec tray (it holds parsec.exe open)"
        Start-Sleep -Milliseconds 300
    }
    return $stopped
}

function Start-ParsecTray {
    param([string]$Exe)
    # Re-launch through the same .vbs the Run key uses, so a restarted tray is
    # identical to a sign-in one (hidden console window and all).
    $launcher = Join-Path $env:USERPROFILE ".parsec\parsec-tray.vbs"
    if (Test-Path $launcher) {
        Start-Process wscript.exe -ArgumentList "`"$launcher`"" -WindowStyle Hidden
        Write-Host "restarted the parsec tray on the new binary"
        return
    }
    & $Exe tray install | Out-Null # older install with no launcher -- rebuild it
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
    # Claude Desktop. -NoDesktop opts out, because this is the one tool whose
    # setup installs mitmproxy and trusts a root CA. A miss here is not final:
    # -Tools desktop forces it, since interception keys off the process name.
    $desktopExe = Find-ClaudeDesktop
    if ($desktopExe -and -not $NoDesktop) { $Tools += "desktop"; $desktopAuto = $true }
    if (-not $Tools) {
        Write-Error "no supported Claude client found (looked for: claude, codex, opencode, Claude Desktop). Install one first, or pick explicitly: -Tools codex"
    }
    Write-Host "detected: $($Tools -join ' ')"
    if ($Tools -contains "desktop") {
        Write-Host "  Claude Desktop at $desktopExe"
        Write-Host "  desktop needs mitmproxy + a trusted root CA (you will get an administrator prompt); skip it with -NoDesktop"
    }
    elseif (-not $NoDesktop) {
        Write-Host "  no Claude Desktop found - if it IS installed, force it with: -Tools desktop"
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

# $env:PROCESSOR_ARCHITECTURE cannot be trusted here: inside an x64-emulated
# shell on ARM64 hardware the loader rewrites it to AMD64, which made this
# script install desktop tooling on machines where WinDivert can never load.
# The machine-wide registry value keeps the real architecture.
$nativeArch = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment").PROCESSOR_ARCHITECTURE
$isX64 = $nativeArch -eq "AMD64"
# Windows 11 on ARM emulates x64 transparently, so the win-x64 binary runs
# fine there -- the Claude Code plugin's dispatcher has always done exactly
# that on these machines. Windows 10 on ARM only emulates x86 (build 22000 is
# the Windows 11 floor), so it stays unsupported.
$isArm64Emu = ($nativeArch -eq "ARM64") -and
    ([Environment]::OSVersion.Version.Build -ge 22000)

# Claude Desktop interception is the one thing x64 emulation cannot carry:
# WinDivert is a KERNEL driver, x64 drivers cannot load on an ARM64 kernel,
# and neither WinDivert nor mitmproxy's redirector ships ARM64. When desktop
# was AUTO-detected, drop it here rather than let it abort an install that
# can still set everything else up. Asked for explicitly, error -- with the
# real reason, since "unsupported architecture" would be wrong now that the
# binary itself runs.
if (-not $isX64 -and ("desktop" -in $Tools)) {
    if ($desktopAuto) {
        Write-Warning "skipping Claude Desktop: interception needs WinDivert's kernel driver, which has no ARM64 build (x64 emulation does not extend to kernel drivers)"
        $Tools = @($Tools | Where-Object { $_ -ne "desktop" })
    }
    else {
        Write-Error "Claude Desktop interception cannot work on $nativeArch (WinDivert's kernel driver has no ARM64 build) - re-run without 'desktop'"
    }
}

# -- resolve the newest published version --------------------------------------
# The plugins TREE only advances on stable (v0.X.0) tags, but someone
# explicitly running the installer is asking for the newest build -- so
# resolve latest.json (the pointer release.yml maintains) and pull binaries
# from that tag's GitHub release assets, verified against the sha256 map in
# the same file. Patch binaries exist ONLY as release assets; the tree would
# silently serve the previous stable. Resolution failure falls back to the
# stable tree, and a custom PARSEC_INSTALL_BASE (test installs point at a
# tree, not at github releases) skips resolution entirely.
$ReleaseTag = $null
$ReleaseAssets = $null
if (-not $env:PARSEC_INSTALL_BASE) {
    try {
        $latest = Invoke-RestMethod -Uri "$Base/latest.json" -UseBasicParsing
        $chan = if ($latest.patch) { $latest.patch } else { $latest.stable }
        if ($chan -and $chan.tag) {
            $ReleaseTag = $chan.tag
            $ReleaseAssets = $chan.assets
            Write-Host "newest published version: $($chan.version) ($ReleaseTag)"
        }
    }
    catch {
        Write-Host "(could not resolve latest.json - falling back to the stable tree)"
    }
}

# Download one published file: from the resolved release's assets
# (sha256-verified) when resolution succeeded, else from the stable tree.
function Get-ParsecAsset([string]$ReleaseName, [string]$TreePath, [string]$OutFile) {
    if ($ReleaseTag) {
        Invoke-WebRequest -Uri "https://github.com/daseinlabs/plugins/releases/download/$ReleaseTag/$ReleaseName" `
            -OutFile $OutFile -UseBasicParsing
        $expected = if ($ReleaseAssets) { $ReleaseAssets.$ReleaseName } else { $null }
        if ($expected) {
            $actual = (Get-FileHash -Algorithm SHA256 $OutFile).Hash.ToLowerInvariant()
            if ($actual -ne $expected.ToLowerInvariant()) {
                throw "sha256 mismatch for ${ReleaseName}: expected $expected, got $actual"
            }
        }
    }
    else {
        Invoke-WebRequest -Uri "$Base/$TreePath" -OutFile $OutFile -UseBasicParsing
    }
}

# -- platform binary ----------------------------------------------------------
# On x64 this is ALWAYS fetched, not just for the tools that cannot work
# without it. The skills, the status line, and every doc tell people to run
# `parsec ...`; a claude-only install used to leave that command missing until
# a Claude Code session happened to create the alias, which reads as a broken
# install. The Claude Code plugin still ships its own copy -- this is the one
# on PATH.
$dest = Join-Path $env:USERPROFILE ".parsec\bin\parsec.exe"
$binaryRequired = ($Tools -contains "codex") -or ($Tools -contains "opencode") -or ($Tools -contains "desktop") -or $Tray
$needsBinary = $isX64 -or $isArm64Emu
if (-not $needsBinary) {
    if ($binaryRequired) {
        Write-Error "unsupported architecture: $nativeArch (win-x64 only today; ARM64 needs Windows 11 for x64 emulation - or use WSL / the Claude Code plugin)"
    }
    Write-Warning "no parsec binary runs on $nativeArch - installing the Claude Code plugin only (it ships its own binary)"
}
if ($isArm64Emu) {
    Write-Host "ARM64 Windows 11: installing the win-x64 binary - it runs under the OS's built-in x64 emulation."
}
if ($needsBinary) {
    $destDir = Split-Path $dest
    New-Item -ItemType Directory -Force -Path $destDir | Out-Null
    # Download beside the destination, verify it runs, then move into place.
    $tmp = Join-Path $destDir ("parsec-download-{0}.exe" -f ([IO.Path]::GetRandomFileName() -replace "\..*$", ""))
    $stagedDlls = @{}
    $trayWasRunning = $false
    try {
        Write-Host "downloading parsec (win-x64)..."
        Get-ParsecAsset "parsec-win-x64.exe" "plugins/parsec/bin/win-x64/parsec.exe" $tmp
        # App-local VC++ CRT. parsec.exe imports msvcp140/vcruntime140, which
        # are absent on a clean Windows box; the loader only searches NEXT TO
        # the exe, so these must land in the same directory or the process
        # dies before main() with 0xC0000135 and no stderr. release.yml ships
        # them beside the exe for exactly this reason -- downloading the exe
        # alone reproduced the bug the bundling exists to prevent.
        #
        # STAGED rather than written straight into $destDir: on an upgrade the
        # running proxy and tray have these DLLs LOADED and Windows denies the
        # overwrite, so the direct write reported "(no <dll> published)" on
        # every upgrade -- blaming the release for a file lock -- and silently
        # kept the old CRT. Downloading first keeps the proxy up while the
        # bytes come down; the swap happens after everything is stopped.
        foreach ($dll in "msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll") {
            $stage = Join-Path $destDir "$dll.parsec-new"
            try {
                Get-ParsecAsset $dll "plugins/parsec/bin/win-x64/$dll" $stage
                $stagedDlls[$dll] = $stage
            }
            catch {
                # A release that no longer needs the CRT will not publish them;
                # the --version check below is the real gate either way.
                Write-Host "(no $dll published - continuing)"
                if (Test-Path $stage) { Remove-Item -Force $stage }
            }
        }
        # Everything holding the old image goes down together, BEFORE any file
        # in $destDir is replaced: the proxy first (it owns the routed port and
        # gets a graceful /shutdown), then the tray (it owns nothing but the
        # lock, and no command stops it).
        Stop-ParsecProxy
        $trayWasRunning = Stop-ParsecTray
        foreach ($dll in @($stagedDlls.Keys)) {
            try { Move-Item -Force $stagedDlls[$dll] (Join-Path $destDir $dll) }
            catch { Write-Warning "could not replace $dll (still in use) - keeping the existing one" }
        }
        # Runs AFTER the CRT is in place: on a clean box the check itself needs
        # those DLLs, so verifying before the move would fail on the very
        # machines the bundling exists for.
        & $tmp --version | Out-Null # refuse to install a binary that cannot run
        if ($LASTEXITCODE -ne 0) { throw "downloaded binary failed --version" }
        try {
            Move-Item -Force $tmp $dest
        }
        catch {
            Write-Error "could not replace $dest (something still holds it -- close it and re-run): $_"
        }
    }
    finally {
        if (Test-Path $tmp) { Remove-Item -Force $tmp }
        foreach ($stage in $stagedDlls.Values) {
            if (Test-Path $stage) { Remove-Item -Force $stage }
        }
    }
    # A proxy that predates this install keeps serving the OLD binary --
    # restart so the fresh one owns the port (identity-checked: a foreign
    # process on the port is never killed). In-flight requests from other
    # sessions see one brief blip and recover on their next request.
    # `up --restart` also bounces the Claude Desktop interceptor, so an
    # updated proxy is never left behind a mitmdump holding the old addon.
    # Not from HERE though: this shell is unelevated and the interceptor runs
    # elevated for WinDivert, so the stop could only fail. The elevated
    # `parsec setup desktop` below restarts it properly.
    if ($Tools -contains "desktop") { $env:PARSEC_SKIP_DESKTOP_BOUNCE = "1" }
    try {
        & $dest up --restart
        if ($LASTEXITCODE -ne 0) { Write-Error "proxy restart failed" }
    }
    finally {
        Remove-Item Env:\PARSEC_SKIP_DESKTOP_BOUNCE -ErrorAction SilentlyContinue
    }
    # The tray is parsec.exe too: it was stopped only to free the file lock, so
    # bring it back on the NEW binary. Skipped under -Tray, where `tray install`
    # below starts it anyway -- doing both would leave two icons. Never started
    # when it was not already running: a login item is -Tray's to add.
    if ($trayWasRunning -and -not $Tray) { Start-ParsecTray -Exe $dest }
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
function Install-ParsecDesktop([string]$Exe, [bool]$TrustCa, [bool]$Autostart) {
    # Reached with -Tools desktop even when Find-ClaudeDesktop came up empty:
    # the interceptor matches the PROCESS NAME, so an install we could not
    # locate on disk still gets captured once Desktop is running. Say so, so
    # a "not found" note does not read as a failure.
    $found = Find-ClaudeDesktop
    if ($found) { Write-Host "Claude Desktop: $found" }
    else {
        Write-Host "Claude Desktop not found on disk - continuing anyway: mitmproxy hooks the process by name (Claude.exe), not by path."
    }
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
    if ($Autostart) {
        $setupArgs += "--autostart"
        Write-Host "installing the boot service too, so interception survives reboots (skip with -NoAutostart)."
    }

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
            Install-ParsecDesktop -Exe $dest -TrustCa (-not $NoCa) -Autostart (-not $NoAutostart)
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
    if (-not (Test-Cmd parsec)) {
        # PATH was updated for this process and for future ones; a shell that
        # predates the install still will not see it.
        Write-Host ("note: 'parsec' resolves in NEW terminals - in this one, run it as " + $dest)
    }
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
    if ($NoAutostart) {
        Write-Host "         -NoAutostart: interception stops at reboot - bring it back with: parsec desktop start"
    }
}
Write-Host "undo: parsec disable codex|opencode|desktop - parsec tray uninstall - claude plugin uninstall parsec"

# -- final pointer: the one step left is adding an API key ---------------------
$keyCmd = if ($needsBinary) { "parsec key set <key>" } else { "/parsec:key in a Claude Code session" }
Write-Host ""
Write-Host ("-> Go to https://app.getparsec.ai - grab your API key, then add it: " + $keyCmd) -ForegroundColor Green
