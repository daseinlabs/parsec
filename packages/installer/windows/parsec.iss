; parsec — Windows installer (Inno Setup 6.5+; windows-latest runners ship 6.7).
;
;   ISCC /DVersion=0.2.8 /DBinDir=C:\path\with\parsec.exe parsec.iss
;
; Per-user install, no administrator for the install itself. The binary and
; its app-local VC++ CRT DLLs land at %USERPROFILE%\.parsec\bin — a path
; contract, not a choice: bin_alias_path(), the codex/opencode/pi shims, the
; interceptor addon, the tray's Run entry and install.ps1 all hardcode it.
;
; [Files] land in {app}\staging; SwapIn (ssPostInstall) renames them into
; place. A Windows process keeps its mapped image alive under any NAME but
; blocks overwrite and delete, so an upgrade over a live proxy / tray / MCP
; server can only ever be a rename — and the old files leave their names
; only once every new file is on disk beside them. Any failure renames the
; old set back: the bin dir is never left without a parsec.exe (v0.2.11 was —
; rename, failed copy, Inno rollback — and the still-running supervisor,
; whose current_exe() is the load-time path, could not spawn a worker again).
;
; Every post-copy action runs from [Code] (CurStepChanged), not [Run]: [Run]
; discards exit codes, cannot set an environment variable for one child, and
; cannot position the ONE administrator prompt (Claude Desktop: WinDivert
; driver + root CA + scheduled task) after the user-scope steps so a declined
; prompt costs nothing else. The Approvals page before Install lists exactly
; what will ask. The one [Run] entry is the exception that fits its limits:
; the Finished-page "Sign in" checkbox launches `parsec login` fire-and-forget
; (no exit code to read, no env to set) — it opens the browser and hands this
; machine its dashboard key, so nobody has to open a terminal after install.
;
; Brand: brand/parsecbrandkit/BRANDING.md — the wizard is themed dark
; (WizardStyle=modern dark, chrome on the void #0A0E0C) and the side/header
; images carry the parallax mark, the wordmark and the tagline in JetBrains
; Mono (rendered into the PNGs by packages/installer/assets/gen.sh: the font
; cannot be used for wizard chrome, which is drawn before anything installs).

#ifndef Version
  #error Pass /DVersion=x.y.z
#endif
#ifndef BinDir
  #error Pass /DBinDir=<dir containing parsec.exe + the four CRT DLLs>
#endif
; VersionInfoVersion must be numeric: strip a -rc.1 suffix.
#define NumVersion Copy(Version, 1, Pos("-", Version + "-") - 1)

[Setup]
; Minted once; never change it — it is the upgrade identity.
AppId={{8D3B6A0E-6C4F-4C1B-9E1A-7F2B5C3D9A41}
AppName=parsec
AppVersion={#Version}
VersionInfoVersion={#NumVersion}
AppPublisher=Dasein Labs
AppPublisherURL=https://getparsec.ai
AppSupportURL=https://github.com/daseinlabs/parsec
AppUpdatesURL=https://github.com/daseinlabs/parsec/releases
DefaultDirName={code:ParsecBinDir}
DisableDirPage=yes
UsePreviousAppDir=no
DisableProgramGroupPage=yes
DisableWelcomePage=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
ChangesEnvironment=yes
CloseApplications=no
RestartApplications=no
Uninstallable=yes
UninstallDisplayName=parsec
UninstallDisplayIcon={uninstallexe}
CreateUninstallRegKey=yes
OutputDir=..\..\..\target\installer
OutputBaseFilename=parsec-{#Version}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes
SetupIconFile=assets\parsec.ico
WizardStyle=modern dark
WizardSizePercent=120
WizardImageFile=assets\wizard-side-202x386.png,assets\wizard-side-336x643.png,assets\wizard-side-534x1022.png
WizardSmallImageFile=assets\wizard-small-58.png,assets\wizard-small-97.png,assets\wizard-small-159.png
; $BBGGRR — the void.
WizardImageBackColor=$0C0E0A
WizardSmallImageBackColor=$0C0E0A
#ifdef Sign
; build.ps1 passes /Sparsecsign="signtool …"; signs Setup AND the uninstaller.
SignTool=parsecsign
SignedUninstaller=yes
SignToolRetryCount=3
#endif

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"

[Messages]
WelcomeLabel1=parsec
WelcomeLabel2=2× the context. ½ the cost.%n%nInstalls parsec {#Version} to %USERPROFILE%\.parsec\bin and wires it into the Claude clients on this machine.%n%nNo administrator rights for the install itself. Claude Desktop interception is the one step that asks.
FinishedHeadingLabel=Done.
FinishedLabel=parsec is on your PATH in new terminals.%n%nOpen a new Claude Code session — routing is read at session start.%n%nLeave "Sign in to parsec" ticked to link your dashboard now: it opens https://app.getparsec.ai and hands this machine its key. Later, it is the Sign in item in the parsec tray menu, or:  parsec login

[Tasks]
Name: "claude";     Description: "Claude Code — install the parsec plugin (routing, status line, hooks, skills)"; Flags: unchecked
Name: "codex";      Description: "Codex CLI — route through the local proxy"; Flags: unchecked
; Radio children (exclusive), NOT a plain child checkbox: Inno checks a parent
; with coCheckWithChildren, so a checkbox child gets force-ticked with the
; parent, and a parent whose only child is unticked cannot stay ticked at all
; (TNewCheckListBox.CalcState) — ticking "Codex" used to tick "--byok" and
; unticking "--byok" untick "Codex". Both radios start unchecked: a checked
; child ticks its parent at list-population time (AddItem → CheckItem →
; UpdateParentStates), which would tick "Codex" on every machine. Ticking
; the parent (UI, WizardSelectTasks, /TASKS=codex, UsePreviousTasks) selects
; the first radio — the subscription — unless "byok" is named explicitly.
Name: "codex\sub";  Description: "Sign in with the ChatGPT subscription (default)"; Flags: exclusive unchecked
Name: "codex\byok"; Description: "Use an OPENAI_API_KEY provider (--byok)"; Flags: exclusive unchecked
Name: "opencode";   Description: "opencode — install the parsec plugin shim"; Flags: unchecked
Name: "pi";         Description: "pi — route its anthropic provider through the local proxy"; Flags: unchecked
Name: "desktop";    Description: "Claude Desktop — intercept Cowork / Agent traffic (installs mitmproxy, trusts a root CA, asks for administrator ONCE)"; Flags: unchecked; Check: not NativeArm64
Name: "tray";       Description: "Show parsec in the notification area and start it at sign-in"

[Files]
; Staged, not installed to their real names — see SwapIn. Uninstall of the
; real names is [UninstallDelete]'s job; Inno only knows the staged paths.
Source: "{#BinDir}\parsec.exe";         DestDir: "{app}\staging"; Flags: ignoreversion
Source: "{#BinDir}\msvcp140.dll";       DestDir: "{app}\staging"; Flags: ignoreversion
Source: "{#BinDir}\msvcp140_1.dll";     DestDir: "{app}\staging"; Flags: ignoreversion
Source: "{#BinDir}\vcruntime140.dll";   DestDir: "{app}\staging"; Flags: ignoreversion
Source: "{#BinDir}\vcruntime140_1.dll"; DestDir: "{app}\staging"; Flags: ignoreversion
; Installed, not dontcopy: the uninstaller's StopParsec needs it too, and
; ExtractTemporaryFile is not callable from uninstall code (Inno raises).
; Setup still ExtractTemporaryFile()s it in ssPostInstall, before the swap.
Source: "scripts\stop-parsec.ps1";      DestDir: "{app}\staging"; Flags: ignoreversion
; Setup-only: every post-install child runs through it (see Run).
Source: "scripts\run-step.ps1";        Flags: dontcopy

[Run]
; Finished-page checkbox (postinstall). Skipped when a key is already stored
; (an upgrade) and in silent installs. The console window it opens shows the
; sign-in URL for the case where no browser opens; it closes itself.
Filename: "{app}\parsec.exe"; Parameters: "login --timeout 600"; Description: "Sign in to parsec (opens your browser, links this machine to your dashboard)"; Flags: postinstall nowait skipifsilent; Check: NotSignedIn

[UninstallDelete]
; Keep in step with PAYLOAD in [Code].
Type: files; Name: "{app}\parsec.exe"
Type: files; Name: "{app}\msvcp140.dll"
Type: files; Name: "{app}\msvcp140_1.dll"
Type: files; Name: "{app}\vcruntime140.dll"
Type: files; Name: "{app}\vcruntime140_1.dll"
Type: files; Name: "{app}\stop-parsec.ps1"
; Displaced files a process still had mapped at upgrade time. Setup sweeps
; them itself (SweepAside, best-effort); the uninstaller gets what is free.
Type: files; Name: "{app}\*.old"
Type: files; Name: "{app}\*.old.*"
Type: dirifempty; Name: "{app}\staging"
Type: dirifempty; Name: "{app}"

[Code]
const
  ERROR_CANCELLED = 1223;
  RUN_KEY = 'Software\Microsoft\Windows\CurrentVersion\Run';
  MARKETPLACE_URL = 'https://github.com/daseinlabs/parsec';
  // Everything [Files] stages; must match it and [UninstallDelete].
  PAYLOAD = 'parsec.exe;msvcp140.dll;msvcp140_1.dll;vcruntime140.dll;vcruntime140_1.dll;stop-parsec.ps1';

var
  ApprovalsPage: TOutputMsgMemoWizardPage;
  Warnings: TStringList;        // per-step problems, shown once on the Finished page
  TrayWasRunning: Boolean;      // StopParsec stopped a tray: bring it back
  PurgeData: Boolean;           // uninstall: also delete ~\.parsec

// Not in Inno's stdlib; children of Setup inherit the process environment.
function SetEnvironmentVariable(lpName, lpValue: String): Boolean;
  external 'SetEnvironmentVariableW@kernel32.dll stdcall';

// ---- paths -------------------------------------------------------------------

// Mirrors setup.rs home_dir(): HOME first (Git Bash users), then USERPROFILE.
function ParsecHome(): String;
begin
  Result := GetEnv('HOME');
  if Result = '' then Result := GetEnv('USERPROFILE');
  Result := AddBackslash(Result) + '.parsec';
end;

function ParsecBinDir(Param: String): String;
begin
  Result := ParsecHome() + '\bin';
end;

function ParsecExe(): String;
begin
  Result := ExpandConstant('{app}\parsec.exe');
end;

// [Run] Check for the Finished-page sign-in: only offered while no dashboard
// key is stored (credentials.rs writes ~\.parsec\credentials.json).
function NotSignedIn(): Boolean;
begin
  Result := not FileExists(ParsecHome() + '\credentials.json');
end;

// ---- architecture --------------------------------------------------------------

// The native architecture, not the emulated one: x64 emulation rewrites the
// PROCESSOR_ARCHITECTURE env var to AMD64, so read the machine-wide registry
// value — the same probe gate_windows_arm64() in setup_desktop.rs uses.
function NativeArm64(): Boolean;
var
  arch: String;
begin
  Result := IsArm64;
  if (not Result) and RegQueryStringValue(HKLM,
      'SYSTEM\CurrentControlSet\Control\Session Manager\Environment',
      'PROCESSOR_ARCHITECTURE', arch) then
    Result := CompareText(arch, 'ARM64') = 0;
end;

// ---- detection (ported from install.ps1) ---------------------------------------

function OnPath(exe: String): Boolean;
begin
  Result := FileSearch(exe, GetEnv('PATH')) <> '';
end;

// The claude CLI as a full path, '' if none. Setup's PATH is whatever the
// shell that launched it had — a Claude Code installed minutes ago is not
// on it yet, and HasClaudeCode ticks the task on ~\.claude alone — so the
// two install locations are probed directly: npm's global shim and the
// native installer's ~\.local\bin. A full path also lets run-step.ps1 start
// the .cmd itself (CreateProcess runs .cmd/.bat through cmd.exe), with no
// `cmd /C` and no nested quoting.
function ClaudeCli(): String;
var
  c: String;
begin
  Result := FileSearch('claude.exe', GetEnv('PATH'));
  if Result = '' then Result := FileSearch('claude.cmd', GetEnv('PATH'));
  if Result <> '' then exit;
  c := ExpandConstant('{userappdata}\npm\claude.cmd');
  if FileExists(c) then begin Result := c; exit; end;
  c := AddBackslash(GetEnv('USERPROFILE')) + '.local\bin\claude.exe';
  if FileExists(c) then Result := c;
end;

function HasClaudeCode(): Boolean;
begin
  Result := (ClaudeCli() <> '')
    or DirExists(AddBackslash(GetEnv('USERPROFILE')) + '.claude');
end;

function HasCodex(): Boolean;
var
  h: String;
begin
  h := GetEnv('CODEX_HOME');
  if h = '' then h := AddBackslash(GetEnv('USERPROFILE')) + '.codex';
  Result := OnPath('codex.cmd') or OnPath('codex.exe') or DirExists(h);
end;

function HasOpencode(): Boolean;
var
  x: String;
begin
  x := GetEnv('XDG_CONFIG_HOME');
  if x = '' then x := AddBackslash(GetEnv('USERPROFILE')) + '.config';
  Result := OnPath('opencode.exe') or OnPath('opencode.cmd')
    or DirExists(x + '\opencode') or DirExists(ExpandConstant('{userappdata}\opencode'));
end;

// pi keeps models.json and extensions\ under its agent dir; PI_CODING_AGENT_DIR
// is pi's own override for that dir (setup_pi.rs honours the same variable).
function HasPi(): Boolean;
var
  d: String;
begin
  d := GetEnv('PI_CODING_AGENT_DIR');
  if d = '' then d := AddBackslash(GetEnv('USERPROFILE')) + '.pi\agent';
  Result := OnPath('pi.cmd') or OnPath('pi.exe') or DirExists(d);
end;

// Presence only: mitmproxy matches the process by NAME, so a path we cannot
// find never stops interception — which is why the task can be ticked anyway.
function HasClaudeDesktop(): Boolean;
var
  fr: TFindRec;
  la, v: String;
begin
  la := ExpandConstant('{localappdata}');
  Result := FileExists(la + '\AnthropicClaude\Claude.exe')
    or FileExists(la + '\Programs\Claude\Claude.exe')
    or FileExists(la + '\Programs\claude-desktop\Claude.exe')
    or FileExists(ExpandConstant('{commonpf}\Claude\Claude.exe'))
    or DirExists(la + '\Packages\Claude_pzs8sxrjxfjjc');
  if (not Result) and FindFirst(la + '\AnthropicClaude\app-*', fr) then begin
    try
      repeat
        if FileExists(la + '\AnthropicClaude\' + fr.Name + '\claude.exe') then Result := True;
      until Result or (not FindNext(fr));
    finally
      FindClose(fr);
    end;
  end;
  if not Result then
    Result := RegQueryStringValue(HKCU, 'SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\Claude.exe', '', v)
      or RegQueryStringValue(HKLM, 'SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\Claude.exe', '', v);
end;

function FreshInstall(): Boolean;
begin
  Result := not RegKeyExists(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\' +
    ExpandConstant('{#SetupSetting("AppId")}') + '_is1');
end;

// ---- wizard --------------------------------------------------------------------

procedure InitializeWizard;
var
  sel: String;
begin
  Warnings := TStringList.Create;
  TrayWasRunning := False;
  if FreshInstall() then begin
    // Upgrades keep the previous task selection (UsePreviousTasks).
    sel := 'tray';
    if HasClaudeCode() then sel := sel + ',claude';
    if HasCodex() then sel := sel + ',codex';
    if HasOpencode() then sel := sel + ',opencode';
    if HasPi() then sel := sel + ',pi';
    if HasClaudeDesktop() and (not NativeArm64()) then sel := sel + ',desktop';
    WizardSelectTasks(sel);
  end;
  ApprovalsPage := CreateOutputMsgMemoPage(wpSelectTasks, 'Approvals',
    'What this install will ask you for', 'Nothing runs until you click Install.', '');
  ApprovalsPage.RichEditViewer.Font.Name := 'Consolas';
end;

procedure CurPageChanged(CurPageID: Integer);
var
  s, app: String;
begin
  if CurPageID <> ApprovalsPage.ID then exit;
  app := ExpandConstant('{app}');
  s := 'WITHOUT ADMINISTRATOR (your account only)' + #13#10 +
       '  parsec.exe    -> ' + app + #13#10 +
       '  user PATH     += ' + app + #13#10 +
       '  proxy         restarted on the routed port' + #13#10;
  if WizardIsTaskSelected('claude') then
    s := s + '  claude        plugin marketplace add + plugin install parsec@parsec-marketplace' + #13#10 +
             '                ANTHROPIC_BASE_URL + status line in ~\.claude\settings.json' + #13#10;
  if WizardIsTaskSelected('codex') then begin
    s := s + '  codex         [model_providers.parsec] in ~\.codex\config.toml' + #13#10;
    if WizardIsTaskSelected('codex\byok') then
      s := s + '                OPENAI_API_KEY provider (--byok)' + #13#10
    else
      s := s + '                ChatGPT subscription sign-in' + #13#10;
  end;
  if WizardIsTaskSelected('opencode') then
    s := s + '  opencode      plugin shim in opencode''s config directory' + #13#10;
  if WizardIsTaskSelected('pi') then
    s := s + '  pi            baseUrl on the anthropic provider in ~\.pi\agent\models.json + extension' + #13#10;
  if WizardIsTaskSelected('tray') then
    s := s + '  tray          HKCU Run\ParsecTray (wscript, no console window)' + #13#10;
  s := s + #13#10;
  if WizardIsTaskSelected('desktop') then
    s := s + 'ONE ADMINISTRATOR PROMPT (Claude Desktop)' + #13#10 +
             '  WinDivert driver   mitmproxy intercepts claude.exe only' + #13#10 +
             '  root CA            certutil -addstore root <mitmproxy CA>' + #13#10 +
             '  scheduled task     ParsecInterceptor, at sign-in, highest privileges' + #13#10 +
             '  (mitmproxy installs via winget first; its own installer may prompt too)' + #13#10 +
             '  decline = everything else still installs; the re-run command is shown.' + #13#10
  else
    s := s + 'NO ADMINISTRATOR PROMPTS.' + #13#10;
  if NativeArm64() then
    s := s + #13#10 + 'ARM64: Claude Desktop interception is hidden — WinDivert has no ARM64 kernel driver.' + #13#10;
  ApprovalsPage.RichEditViewer.Text := s;
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
  if IsAdmin then
    SuppressibleMsgBox('Setup was started as administrator. parsec installs per-user and asks for ' +
      'administrator itself only where needed — prefer running it normally.',
      mbInformation, MB_OK, IDOK);
end;

// ---- helpers -------------------------------------------------------------------

procedure Explode(var parts: TArrayOfString; s, sep: String);
var
  i, n: Integer;
begin
  n := 0;
  SetArrayLength(parts, 0);
  while True do begin
    i := Pos(sep, s);
    SetArrayLength(parts, n + 1);
    if i = 0 then begin
      parts[n] := s;
      exit;
    end;
    parts[n] := Copy(s, 1, i - 1);
    s := Copy(s, i + Length(sep), MaxInt);
    n := n + 1;
  end;
end;

// Inno has no LoadStringFromFile-or-empty; keep the call sites readable.
// Pascal Script resolves identifiers top-down, so this sits above its callers.
function LoadStringFromFileOrEmpty(path: String): String;
var
  s: AnsiString;
begin
  Result := '';
  if LoadStringFromFile(path, s) then Result := String(s);
end;

// Where every post-install child's output lands — the diagnosis when a user
// reports a wizard that never finished.
function StepLog(): String;
begin
  Result := ParsecHome() + '\installer.log';
end;

// Run one step through run-step.ps1: stdin at EOF, a deadline, output to
// StepLog. Exec alone gives a hidden child a console nobody can see, so a
// child that asks a question (a CLI's first-run prompt, a "continue?") waits
// forever and the wizard sits on its last page — Setup keeps pumping
// messages, so nothing looks crashed, it just never ends. That is what
// "stuck on the final step" reports were. Records a warning on failure.
function Run(cmd, params, what: String; timeoutSec: Integer; var code: Integer): Boolean;
var
  runner: String;
begin
  Log('step: ' + what + ' -> ' + cmd + ' ' + params);
  runner := ExpandConstant('{tmp}\run-step.ps1');
  if not FileExists(runner) then ExtractTemporaryFile('run-step.ps1');
  Result := Exec('powershell.exe', '-NoProfile -ExecutionPolicy Bypass -File "' + runner +
    '" -Exe "' + cmd + '" -Arguments "' + params + '" -TimeoutSec ' + IntToStr(timeoutSec) +
    ' -Log "' + StepLog() + '"', ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, code) and (code = 0);
  if Result then exit;
  if code = 124 then
    Warnings.Add(what + ' did not finish within ' + IntToStr(timeoutSec) + 's and was stopped: ' + cmd + ' ' + params)
  else
    Warnings.Add(what + ' failed (exit ' + IntToStr(code) + '): ' + cmd + ' ' + params);
end;

procedure StopParsec(outFile: String);
var
  code: Integer;
  script: String;
begin
  if IsUninstaller then
    // usUninstall runs before [Files] are removed, so the installed copy is
    // there; ExtractTemporaryFile is unavailable in the uninstaller.
    script := ExpandConstant('{app}\stop-parsec.ps1')
  else begin
    // Setup runs this before the swap: pull it from the setup payload.
    ExtractTemporaryFile('stop-parsec.ps1');
    script := ExpandConstant('{tmp}\stop-parsec.ps1');
  end;
  if not FileExists(script) then exit;
  Exec('powershell.exe', '-NoProfile -ExecutionPolicy Bypass -File "' +
    script + '" -ParsecHome "' + ParsecHome() + '" -Out "' + outFile + '"',
    '', SW_HIDE, ewWaitUntilTerminated, code);
end;

// ---- the swap ------------------------------------------------------------------

// Leftovers of Displace: files a process still had mapped at an earlier
// upgrade. Best-effort — a mapped one cannot be deleted; the next upgrade or
// the uninstaller gets it. Never raises, unlike [InstallDelete].
procedure SweepAside(dir: String);
var
  fr: TFindRec;
begin
  if not FindFirst(dir + '\*.old*', fr) then exit;
  try
    repeat
      if (fr.Attributes and FILE_ATTRIBUTE_DIRECTORY) = 0 then
        DeleteFile(dir + '\' + fr.Name);
    until not FindNext(fr);
  finally
    FindClose(fr);
  end;
end;

// Move a file out of its name by RENAME — never delete: a rename works on a
// mapped image and a free file alike, and it can be undone. An earlier
// upgrade's .old may itself still be mapped, so try further names.
function Displace(path: String; var aside: String): Boolean;
var
  i: Integer;
begin
  Result := False;
  for i := 0 to 20 do begin
    if i = 0 then aside := path + '.old' else aside := path + '.old.' + IntToStr(i);
    if FileExists(aside) and (not DeleteFile(aside)) then continue;
    if RenameFile(path, aside) then begin
      Result := True;
      exit;
    end;
  end;
  aside := '';
end;

// Rename the staged payload into place, all files or none. Per file: the old
// one steps aside (Displace), the new one takes the name. Any failure undoes
// every rename made so far, so the previous install stays complete and
// runnable and Setup's own [Files] rollback (which only knows {app}\staging)
// never has anything to take away from the real names. Between a file's two
// renames its name is empty for microseconds; nothing exec-loads parsec in
// that window because the proxy and tray were stopped just before (an MCP
// server keeps running on its displaced image). Returns False on failure
// with the reason in `why`.
function SwapIn(var why: String): Boolean;
var
  app, stage, src, dst: String;
  names, moved: TArrayOfString;   // moved[i]: where the old file went; '' = nothing was there
  i, j, n: Integer;
begin
  Result := False;
  app := ExpandConstant('{app}');
  stage := app + '\staging';
  Explode(names, PAYLOAD, ';');
  n := GetArrayLength(names);
  SetArrayLength(moved, n);
  for i := 0 to n - 1 do moved[i] := '';
  i := 0;
  while i < n do begin
    src := stage + '\' + names[i];
    dst := app + '\' + names[i];
    if not FileExists(src) then begin
      why := 'staged file missing: ' + src;
      break;
    end;
    if FileExists(dst) and (not Displace(dst, moved[i])) then begin
      why := names[i] + ' could not be moved out of the way';
      break;
    end;
    if not RenameFile(src, dst) then begin
      why := 'could not place ' + names[i] + ' (error ' + IntToStr(DLLGetLastError) + ')';
      break;
    end;
    Log('swapped in ' + names[i]);
    i := i + 1;
  end;
  if i = n then begin
    Result := True;
    // Committed. The displaced set is garbage now: delete what is free, the
    // mapped rest waits for SweepAside. Empty staging dir goes too.
    for j := 0 to n - 1 do
      if moved[j] <> '' then DeleteFile(moved[j]);
    RemoveDir(stage);
    exit;
  end;
  // Undo, newest first. File i never got its new copy (its rename failed);
  // files < i did and give it back to staging (delete if even that fails —
  // nothing has mapped it). Then every displaced old file returns.
  Log('swap failed at ' + names[i] + ': ' + why + ' — rolling back');
  for j := i downto 0 do begin
    src := stage + '\' + names[j];
    dst := app + '\' + names[j];
    if (j < i) and FileExists(dst) then
      if not RenameFile(dst, src) then DeleteFile(dst);
    if (moved[j] <> '') and (not RenameFile(moved[j], dst)) then
      Log('ROLLBACK FAILED: ' + moved[j] + ' -> ' + dst);
  end;
end;

// ---- PATH ---------------------------------------------------------------------

function PathContains(p, dir: String): Boolean;
begin
  Result := Pos(';' + Lowercase(dir) + ';', ';' + Lowercase(p) + ';') > 0;
end;

// No `setx` (truncates at 1024 chars) and no leading ';' when Path is unset —
// the two traps setup.rs add_dir_to_path documents.
procedure AddToUserPath(dir: String);
var
  p: String;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', p) then p := '';
  if PathContains(p, dir) then exit;
  if p = '' then
    p := dir
  else if Copy(p, Length(p), 1) = ';' then
    p := p + dir
  else
    p := p + ';' + dir;
  RegWriteExpandStringValue(HKCU, 'Environment', 'Path', p);
end;

procedure RemoveFromUserPath(dir: String);
var
  p, o: String;
  parts: TArrayOfString;
  i: Integer;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', p) then exit;
  Explode(parts, p, ';');
  o := '';
  for i := 0 to GetArrayLength(parts) - 1 do
    if (parts[i] <> '') and (CompareText(parts[i], dir) <> 0) then begin
      if o = '' then o := parts[i] else o := o + ';' + parts[i];
    end;
  RegWriteExpandStringValue(HKCU, 'Environment', 'Path', o);
end;

// ---- mitmproxy (user scope, best-effort) ---------------------------------------

function MitmdumpPresent(): Boolean;
var
  up, mp: String;
begin
  // Setup's own PATH is stale after winget; read the stored PATHs instead.
  RegQueryStringValue(HKCU, 'Environment', 'Path', up);
  RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', mp);
  Result := (FileSearch('mitmdump.exe', ExpandConstant(up) + ';' + ExpandConstant(mp)) <> '')
    or FileExists(ExpandConstant('{localappdata}\Microsoft\WinGet\Links\mitmdump.exe'));
end;

procedure EnsureMitmproxy();
var
  code: Integer;
begin
  if MitmdumpPresent() then exit;
  if not OnPath('winget.exe') then begin
    Warnings.Add('winget is not available (LTSC / Server?) — install mitmproxy by hand (pip install mitmproxy), then from an administrator PowerShell: parsec setup desktop --install-ca --autostart');
    exit;
  end;
  Exec('winget.exe', 'install --id mitmproxy.mitmproxy --source winget --silent ' +
    '--accept-package-agreements --accept-source-agreements --disable-interactivity',
    '', SW_SHOW, ewWaitUntilTerminated, code);
  if not MitmdumpPresent() then
    Warnings.Add('mitmproxy did not install (winget exit ' + IntToStr(code) + ') — Claude Desktop skipped. Install it, then from an administrator PowerShell: parsec setup desktop --install-ca --autostart');
end;

// ---- the one elevated step ------------------------------------------------------

procedure SetupDesktopElevated();
var
  code: Integer;
  args, rerun: String;
begin
  args := 'setup desktop --install-ca --autostart';
  rerun := 'parsec ' + args + '   (from an administrator PowerShell)';
  if IsAdmin then begin
    Run(ParsecExe(), args, 'Claude Desktop setup', 300, code);
    exit;
  end;
  // ShellExec with the runas verb = the UAC prompt, in-flow. ERROR_CANCELLED
  // is the user saying no; everything else installed already.
  if ShellExec('runas', ParsecExe(), args, ExpandConstant('{app}'), SW_SHOWNORMAL, ewWaitUntilTerminated, code) then begin
    if code <> 0 then
      Warnings.Add('Claude Desktop setup exited ' + IntToStr(code) + ' — re-run: ' + rerun);
  end else if code = ERROR_CANCELLED then
    Warnings.Add('Administrator approval declined — Claude Desktop was not set up. Everything else is installed. Re-run: ' + rerun)
  else
    Warnings.Add('Could not launch the elevated setup (error ' + IntToStr(code) + ') — re-run: ' + rerun);
end;

// ---- orchestration ------------------------------------------------------------

procedure CurStepChanged(CurStep: TSetupStep);
var
  code: Integer;
  exe, marker, why, claude: String;
begin
  if CurStep <> ssPostInstall then exit;
  exe := ParsecExe();

  // 0. Stop what we will restart (proxy: graceful /shutdown; tray: killed —
  //    both would otherwise linger on the old image next to their
  //    replacements), then rename the staged payload into place. The old
  //    proxy served right up to here: staging is what let the copy happen
  //    while it was still running.
  if FileExists(exe) then begin
    marker := ExpandConstant('{tmp}\stop-parsec.out');
    StopParsec(marker);
    if FileExists(marker) then
      TrayWasRunning := Pos('tray-was-running', LoadStringFromFileOrEmpty(marker)) > 0;
  end;
  SweepAside(ExpandConstant('{app}'));
  if not SwapIn(why) then begin
    // The previous version is intact and gets restarted below; only the
    // registry now claims the new one. Say so loudly, in silent mode too.
    why := 'parsec {#Version} was NOT installed: ' + why + '. The previous version is still in place. ' +
      'Close Claude Code / Codex / opencode / pi sessions that use parsec and run this installer again.';
    Warnings.Add(why);
    SuppressibleMsgBox(why, mbError, MB_OK, IDOK);
  end;
  AddToUserPath(ExpandConstant('{app}'));

  // 1. The fresh binary must own the port. The interceptor bounce is the
  //    elevated step's job (an unelevated bounce can only fail on Windows).
  SetEnvironmentVariable('PARSEC_SKIP_DESKTOP_BOUNCE', '1');
  Run(exe, 'up --restart', 'proxy restart', 60, code);
  SetEnvironmentVariable('PARSEC_SKIP_DESKTOP_BOUNCE', '');

  // 2. User-scope tools, in install.ps1's order.
  if WizardIsTaskSelected('claude') then begin
    claude := ClaudeCli();
    if claude = '' then
      Warnings.Add('the claude CLI was not found — after installing Claude Code, run by hand: ' +
        'claude plugin marketplace add ' + MARKETPLACE_URL + ' && claude plugin install parsec@parsec-marketplace')
    else begin
      // "already added" is fine; plugin install is the arbiter — so the
      // add's warning, if any, is dropped again.
      if not Run(claude, 'plugin marketplace add ' + MARKETPLACE_URL, 'Claude Code marketplace', 120, code) then
        Warnings.Delete(Warnings.Count - 1);
      if not Run(claude, 'plugin install parsec@parsec-marketplace', 'Claude Code plugin', 300, code) then
        Warnings.Add('   by hand: claude plugin marketplace add ' + MARKETPLACE_URL + ' && claude plugin install parsec@parsec-marketplace');
    end;
    // Manual mode: routing env + status line now, not at the first session.
    Run(exe, 'setup', 'Claude Code routing', 60, code);
  end;
  if WizardIsTaskSelected('codex') then begin
    if WizardIsTaskSelected('codex\byok') then
      Run(exe, 'setup codex --byok', 'Codex', 60, code)
    else
      Run(exe, 'setup codex', 'Codex', 60, code);
  end;
  if WizardIsTaskSelected('opencode') then
    Run(exe, 'setup opencode', 'opencode', 60, code);
  if WizardIsTaskSelected('pi') then
    Run(exe, 'setup pi', 'pi', 60, code);

  // 3. The elevated step, LAST among the tools: a declined prompt costs
  //    nothing that came before it.
  if WizardIsTaskSelected('desktop') then begin
    EnsureMitmproxy();
    if MitmdumpPresent() then SetupDesktopElevated();
  end;

  // 4. Tray. `tray install` writes the Run entry and starts it; an upgrade
  //    that stopped a running tray (and did not pick the task) restarts it.
  if WizardIsTaskSelected('tray') then
    Run(exe, 'tray install', 'tray', 30, code)
  else if TrayWasRunning and FileExists(ParsecHome() + '\parsec-tray.vbs') then
    Exec('wscript.exe', '"' + ParsecHome() + '\parsec-tray.vbs"', '', SW_HIDE, ewNoWait, code);

  if Warnings.Count > 0 then
    WizardForm.FinishedLabel.Caption := WizardForm.FinishedLabel.Caption + #13#10#13#10 +
      'Needs attention:' + #13#10 + Warnings.Text + #13#10 + 'Details: ' + StepLog();
end;

// ---- uninstall ----------------------------------------------------------------

function InitializeUninstall(): Boolean;
begin
  Result := True;
  PurgeData := SuppressibleMsgBox('Also delete parsec''s data in ' + ParsecHome() +
    ' (ledger, logs, settings)?', mbConfirmation, MB_YESNO, IDNO) = IDYES;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  code: Integer;
  exe, elev: String;
begin
  exe := ParsecExe();
  if CurUninstallStep = usUninstall then begin
    // ONE administrator prompt, best-effort: stop the elevated interceptor,
    // drop its scheduled task, untrust the CA. Declined ⇒ the commands are shown.
    if FileExists(ParsecHome() + '\interceptor\start-interceptor.cmd') then begin
      elev := '/C ""' + exe + '" desktop stop & schtasks /Delete /TN ParsecInterceptor /F & certutil -delstore root mitmproxy"';
      if not ShellExec('runas', 'cmd.exe', elev, '', SW_SHOWNORMAL, ewWaitUntilTerminated, code) then
        SuppressibleMsgBox('Administrator declined. Run later from an administrator PowerShell:' + #13#10 +
          '  parsec desktop stop' + #13#10 +
          '  schtasks /Delete /TN ParsecInterceptor /F' + #13#10 +
          '  certutil -delstore root mitmproxy', mbInformation, MB_OK, IDOK);
    end;
    Exec(exe, 'disable desktop', '', SW_HIDE, ewWaitUntilTerminated, code);
    Exec(exe, 'disable codex', '', SW_HIDE, ewWaitUntilTerminated, code);
    Exec(exe, 'disable opencode', '', SW_HIDE, ewWaitUntilTerminated, code);
    Exec(exe, 'disable pi', '', SW_HIDE, ewWaitUntilTerminated, code);
    Exec(exe, 'disable claude', '', SW_HIDE, ewWaitUntilTerminated, code);
    Exec(exe, 'tray uninstall', '', SW_HIDE, ewWaitUntilTerminated, code);
    StopParsec(ExpandConstant('{tmp}\stop-parsec.out'));
    if PurgeData then Exec(exe, 'uninstall', '', SW_HIDE, ewWaitUntilTerminated, code);
    // Belt and braces: parsec's own disable/uninstall remove these, but a
    // stale Run value must never outlive the binary it points at.
    RegDeleteValue(HKCU, RUN_KEY, 'ParsecTray');
    RegDeleteValue(HKCU, RUN_KEY, 'ParsecProxy');
    RegDeleteValue(HKCU, RUN_KEY, 'ParsecInterceptor');
    RemoveFromUserPath(ExpandConstant('{app}'));
  end;
  if (CurUninstallStep = usPostUninstall) and PurgeData then
    DelTree(ParsecHome(), True, True, True);
end;
