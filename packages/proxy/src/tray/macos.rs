//! macOS install path for `parsec tray` — a generated `.app` bundle plus a
//! user-scope LaunchAgent.
//!
//! Why a bundle at all: macOS resolves an app's icon, name, and Dock presence
//! from the bundle it finds by walking UP from the running executable's path.
//! A loose binary belongs to no bundle and gets a generic terminal icon, so
//! the binary is COPIED inside `Contents/MacOS` and the login item passes the
//! subcommand as arguments.

use std::path::PathBuf;

use super::LABEL;

/// Dock / app-switcher source, converted to .icns at install time.
const ICON_PNG_512: &[u8] =
    include_bytes!("../../../../brand/parsecbrandkit/logo/png/parsec-icon-512.png");

pub fn support_dir() -> PathBuf {
    crate::setup::home_dir()
        .join("Library")
        .join("Application Support")
        .join("parsec")
}

pub fn bundle_dir() -> PathBuf {
    support_dir().join("parsec.app")
}

/// The bundle's executable — a COPY of the parsec binary, not a shim and not
/// a symlink.
///
/// This is load-bearing, and the first version got it wrong. macOS finds an
/// app's bundle by walking UP from the running executable's path. A shell
/// shim that `exec`s `~/.parsec/bin/parsec` replaces the process image with a
/// path that has no `.app` ancestor, so the process belongs to no bundle:
/// `Info.plist` is ignored, `CFBundleIconFile` never loads, and the Dock
/// shows a generic terminal icon. The binary must physically live here.
///
/// The subcommand comes from the login item's `ProgramArguments` instead,
/// which is why no shim is needed at all.
fn bundle_exec() -> PathBuf {
    bundle_dir()
        .join("Contents")
        .join("MacOS")
        .join("parsec-tray")
}

fn launch_agent() -> PathBuf {
    crate::setup::home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// The binary the shim execs. The alias, never `current_exe()`: the plugin
/// cache path this process runs from changes on every plugin update, and the
/// login item must keep working across those. `~/.parsec/bin/parsec` is the
/// stable path, and it self-heals to the current build
/// (`setup_opencode::refresh_bin_alias`).
fn target_binary() -> PathBuf {
    crate::setup_opencode::bin_alias_path()
}

// ── generated bundle ────────────────────────────────────────────────────────

/// The bundle carries a real icon and NO `LSUIElement`, so parsec appears in
/// BOTH places: the menu bar (the status item below) and the Dock / app
/// switcher. Setting `LSUIElement` would suppress the Dock half.
///
/// Worth knowing: the Dock entry is presence, not a surface — parsec has no
/// window, so clicking it just activates the app. The menu bar stays the
/// place where things actually happen.
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>parsec</string>
    <key>CFBundleDisplayName</key>
    <string>parsec</string>
    <key>CFBundleIdentifier</key>
    <string>{LABEL}</string>
    <key>CFBundleExecutable</key>
    <string>parsec-tray</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>{}</string>
    <key>CFBundleIconFile</key>
    <string>parsec</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
</dict>
</plist>
"#,
        env!("CARGO_PKG_VERSION")
    )
}

/// Copy the binary into the bundle. A copy, deliberately: a symlink or
/// hardlink here either defeats the bundle association or pins a stale inode
/// across updates.
///
/// Staleness is bounded by design — the tray shells out to
/// `~/.parsec/bin/parsec` for every ACTION (see `macos::App::spawn`), so an
/// old copy still drives the current parsec. Only the menu UI itself ages,
/// and `parsec tray install` re-copies.
fn install_bundle_binary(binary: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    // Temp + rename: a running tray holds its image open, and replacing the
    // file under it must not produce a half-written binary.
    let tmp = dest.with_extension("parsec-tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(binary, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn agent_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>tray</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        bundle_exec().display(),
        log_path().display(),
        log_path().display(),
    )
}

fn log_path() -> PathBuf {
    crate::setup::parsec_home().join("tray.log")
}

pub fn installed() -> bool {
    launch_agent().exists() && bundle_exec().exists()
}

fn uid() -> u32 {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(501)
}

/// Unregister, quietly. `launchctl bootout` writes "Boot-out failed: 3: No
/// such process" to stderr whenever nothing is registered — which is the
/// NORMAL case on a first install, so it must not look like an error.
fn bootout() {
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

pub fn install() -> anyhow::Result<()> {
    let binary = target_binary();
    if !binary.exists() {
        anyhow::bail!(
            "no parsec binary at {} — run `parsec setup` first so the stable alias exists",
            binary.display()
        );
    }
    let exec = bundle_exec();
    std::fs::create_dir_all(exec.parent().unwrap())?;
    std::fs::write(
        bundle_dir().join("Contents").join("Info.plist"),
        info_plist(),
    )?;
    write_dock_icon();
    install_bundle_binary(&binary, &exec)?;
    let plist = launch_agent();
    std::fs::create_dir_all(plist.parent().unwrap())?;
    bootout(); // replace any previous registration
    std::fs::write(&plist, agent_plist())?;
    let ok = std::process::Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{}", uid())])
        .arg(&plist)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let started = if ok {
        "started — the mark is in your menu bar and the Dock"
    } else {
        "registered — it starts at your next login"
    };
    register_bundle();
    println!(
        "{}",
        crate::brand::panel(
            "menu bar",
            &[
                started,
                "~bundle: ~/Library/Application Support/parsec/parsec.app",
                "~login item: ~/Library/LaunchAgents/rocks.dasein.parsec.tray.plist",
                "~listed in System Settings → General → Login Items & Extensions",
                "~remove anytime: parsec tray uninstall",
            ],
        )
    );
    if !ok {
        println!(
            "{}",
            crate::brand::muted(&format!("start it now: {} tray run", binary.display()))
        );
    }
    Ok(())
}

/// Put the brand mark in the Dock. `sips` ships with macOS, so this needs no
/// dependency and no committed .icns — the 512px brand PNG stays the source
/// of truth and the bundle asset is derived from it.
///
/// Best-effort by design: a missing Dock icon is cosmetic, and failing the
/// whole install over it would be the wrong trade.
fn write_dock_icon() {
    let res = bundle_dir().join("Contents").join("Resources");
    if std::fs::create_dir_all(&res).is_err() {
        return;
    }
    let set = res.join("parsec.iconset");
    let _ = std::fs::remove_dir_all(&set);
    if std::fs::create_dir_all(&set).is_err() {
        return;
    }
    let src = res.join("parsec-icon.png");
    if std::fs::write(&src, ICON_PNG_512).is_err() {
        return;
    }
    // A real iconset, not a single-resolution convert: `sips -s format icns`
    // on one 512px source produces an icns macOS scales badly at Dock and
    // Finder sizes. iconutil wants these exact filenames.
    for (px, name) in [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
    ] {
        let _ = std::process::Command::new("sips")
            .args(["-z", &px.to_string(), &px.to_string()])
            .arg(&src)
            .arg("--out")
            .arg(set.join(name))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = std::process::Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(&set)
        .arg("-o")
        .arg(res.join("parsec.icns"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&set);
    let _ = std::fs::remove_file(&src);
}

/// Tell LaunchServices the bundle exists, so the Dock resolves its icon and
/// name instead of falling back to a generic one. Finder and the Dock cache
/// bundle metadata aggressively, and this bundle lives outside /Applications
/// where nothing would scan it on its own.
///
/// Best-effort: the app runs fine unregistered, it just looks wrong.
fn register_bundle() {
    let _ = std::process::Command::new("touch")
        .arg(bundle_dir())
        .status();
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = std::process::Command::new(LSREGISTER)
        .arg("-f")
        .arg(bundle_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

pub fn uninstall() -> anyhow::Result<()> {
    let had = installed();
    bootout();
    let _ = std::fs::remove_file(launch_agent());
    let _ = std::fs::remove_dir_all(bundle_dir());
    // Only our own subdir; never the whole Application Support tree.
    let _ = std::fs::remove_dir(support_dir());
    println!(
        "{}",
        if had {
            "menu-bar app removed (the icon disappears when the running one exits)"
        } else {
            "menu-bar app was not installed"
        }
    );
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    println!("MENU-BAR APP");
    println!(
        "  installed:   {}",
        if installed() {
            "yes"
        } else {
            "no — install with: parsec tray install"
        }
    );
    println!("  bundle:      {}", bundle_dir().display());
    println!("  login item:  {}", launch_agent().display());
    println!("  runs:        {} tray run", target_binary().display());
    println!("  log:         {}", log_path().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_dock_icon_is_a_real_png() {
        assert!(ICON_PNG_512.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(ICON_PNG_512.len() > 100);
    }

    #[test]
    fn bundle_shows_in_both_the_menu_bar_and_the_dock() {
        let p = info_plist();
        // No LSUIElement: that key would suppress the Dock / app-switcher
        // entry, and parsec is meant to appear in BOTH places.
        assert!(!p.contains("LSUIElement"));
        // …which only reads as deliberate if there is an icon to show.
        assert!(p.contains("<key>CFBundleIconFile</key>"));
        assert!(p.contains(LABEL));
    }

    #[test]
    fn the_bundle_executable_is_the_binary_itself() {
        // The bug this pins: macOS finds an app's bundle by walking UP from
        // the running executable. A shim that exec'd ~/.parsec/bin/parsec put
        // the process outside any .app, so Info.plist and the icon were
        // ignored and the Dock showed a terminal icon. The binary must be
        // physically inside Contents/MacOS, and the subcommand must therefore
        // come from the login item's arguments.
        let plist = agent_plist();
        assert!(plist.contains(&bundle_exec().display().to_string()));
        assert!(plist.contains("<string>tray</string>"));
        assert!(plist.contains("<string>run</string>"));
        assert!(bundle_exec().starts_with(bundle_dir()));
        assert_eq!(bundle_exec().file_name().unwrap(), "parsec-tray");
        // …and CFBundleExecutable must name that same file.
        assert!(info_plist().contains("<string>parsec-tray</string>"));
    }

    #[test]
    fn login_item_points_at_the_bundle_and_starts_at_login() {
        let p = agent_plist();
        assert!(p.contains(&format!("<string>{LABEL}</string>")));
        assert!(p.contains(&bundle_exec().display().to_string()));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        // Interactive + no KeepAlive: a menu-bar app the user can quit, not a
        // daemon that resurrects itself behind their back.
        assert!(p.contains("<string>Interactive</string>"));
        assert!(p.contains("<key>KeepAlive</key>\n    <false/>"));
    }
}
