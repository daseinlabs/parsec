//! The cross-platform half of `parsec tray run` — the status item, its menu,
//! and the guided Claude Desktop setup.
//!
//! Two constraints shape everything here:
//!
//! 1. **Every menu mutation must happen on the main thread.** AppKit requires
//!    it and Win32 message queues are thread-affine, so the rule is the same
//!    on both. The refresh runs inside winit's `new_events` (main thread) on
//!    a `ControlFlow::WaitUntil` tick, and background work never touches a
//!    `MenuItem` — it publishes into a mutex the tick reads.
//! 2. **The tray owns no serving state.** It reads the same files and ports
//!    the CLI reads and shells out to the same commands. A crashed tray costs
//!    an icon, never a request.
//!
//! Platform difference that shapes the window handling below: macOS puts the
//! app in the Dock from its bundle, with no window involved. Windows gives a
//! taskbar button ONLY to a process with a visible top-level window — so the
//! Windows build creates a small status window to earn its taskbar presence,
//! and closing it hides to the tray rather than exiting (the Windows
//! convention for a resident utility).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(not(windows))]
use winit::window::WindowId;
#[cfg(windows)]
use winit::window::{Window, WindowId};

use crate::setup_desktop::{self, ExtensionStatus};

/// The brand mark, embedded so the app is self-contained (brand/BRANDING.md
/// §1: the parallax angle — two sightlines converging on a star, phosphor
/// `#4AF626`). 64px is the source for both the status item and the Windows
/// window icon: platforms scale it down, so starting above the target size
/// keeps it crisp on HiDPI.
const MARK_PNG_64: &[u8] =
    include_bytes!("../../../../brand/parsecbrandkit/logo/png/parsec-favicon-64.png");

/// How often the menu re-reads the world. Cheap: a loopback GET, a PID probe,
/// and a ledger tail.
const TICK: Duration = Duration::from_secs(5);

/// What the guided setup is currently doing. Written by the worker thread,
/// rendered by the main-thread tick — the whole cross-thread contract.
#[derive(Default, Clone)]
struct Guided {
    active: bool,
    line: String,
    /// Set when the run finished (either way), so the tick can stop showing
    /// progress and go back to plain status.
    done: bool,
}

struct Status {
    proxy: String,
    desktop: String,
    savings: String,
    intercepting: bool,
}

fn read_status() -> Status {
    let target = setup_desktop::target_url();
    let port: u16 = target
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);

    let proxy = if port != 0 && crate::hook::port_listening(port) {
        if crate::setup::parsec_owns(port) {
            format!("Proxy: running on :{port}")
        } else {
            format!("Proxy: :{port} held by another process")
        }
    } else {
        "Proxy: not running".to_string()
    };

    let intercepting = setup_desktop::running();
    let desktop = if intercepting {
        match setup_desktop::extension_status() {
            // The silent-failure case is worth its own line: mitmdump is up
            // and capturing nothing.
            ExtensionStatus::AwaitingApproval => {
                "Desktop: NOT capturing — extension unapproved".to_string()
            }
            _ => "Desktop: intercepting".to_string(),
        }
    } else if setup_desktop::load_desktop_state().is_some_and(|s| s.enabled) {
        "Desktop: configured, stopped".to_string()
    } else {
        "Desktop: not set up".to_string()
    };

    Status {
        proxy,
        desktop,
        savings: savings_line(),
        intercepting,
    }
}

/// Lifetime tokens avoided, from the ledger. Measured only — a row with a
/// null probe contributes nothing rather than an estimate (§8.4).
fn savings_line() -> String {
    let path = crate::setup::parsec_home().join("ledger.jsonl");
    let Ok(data) = std::fs::read_to_string(path) else {
        return "Saved: no requests yet".to_string();
    };
    let agg = crate::statusline::aggregate_ledger(&data);
    if agg.rows == 0 {
        return "Saved: no requests yet".to_string();
    }
    let desktop = agg
        .by_tool
        .get("claude-desktop")
        .map(|(rows, _, _, saved, _)| format!("  ({rows} from Desktop, {saved} tok)"))
        .unwrap_or_default();
    format!("Saved: {} tok over {} req{desktop}", agg.saved, agg.rows)
}

/// Decode the embedded brand mark into the RGBA buffer AppKit wants.
///
/// Not a template image: `with_icon_as_template` would flatten the mark to a
/// monochrome silhouette that follows the system appearance, and phosphor
/// green IS the brand (brand/BRANDING.md §2) — a grey parsec mark is not
/// parsec. The cost is that it does not auto-invert in light menu bars, which
/// is the right trade for a mark that reads on both.
fn brand_icon() -> Option<Icon> {
    let (rgba, w, h) = decode_mark()?;
    Icon::from_rgba(rgba, w, h).ok()
}

/// Decode the embedded brand mark to raw RGBA.
fn decode_mark() -> Option<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(MARK_PNG_64));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    // Both icon types demand exactly 4 bytes per pixel; the brand PNGs are
    // RGBA, but decode defensively rather than panic on a swapped asset.
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    Some((rgba, info.width, info.height))
}

/// The same brand PNG as the tray icon, decoded into winit's icon type.
/// `tray_icon::Icon` and `winit::window::Icon` are different types over the
/// same RGBA bytes, so the decode is shared and only the wrapper differs.
#[cfg(windows)]
fn window_icon() -> Option<winit::window::Icon> {
    let (rgba, w, h) = decode_mark()?;
    winit::window::Icon::from_rgba(rgba, w, h).ok()
}

struct App {
    tray: Option<TrayIcon>,
    /// Windows only: the taskbar button is tied to a visible top-level
    /// window, so one exists purely to give parsec taskbar presence. macOS
    /// gets its Dock entry from the bundle and needs nothing here.
    #[cfg(windows)]
    window: Option<Window>,
    proxy: MenuItem,
    desktop: MenuItem,
    savings: MenuItem,
    restart: MenuItem,
    setup_desktop_item: MenuItem,
    stop_desktop: MenuItem,
    quit: MenuItem,
    next: Instant,
    guided: Arc<Mutex<Guided>>,
    busy: Arc<AtomicBool>,
}

impl App {
    fn new() -> Self {
        App {
            tray: None,
            #[cfg(windows)]
            window: None,
            proxy: MenuItem::new("Proxy: …", false, None),
            desktop: MenuItem::new("Desktop: …", false, None),
            savings: MenuItem::new("Saved: …", false, None),
            restart: MenuItem::new("Restart proxy", true, None),
            setup_desktop_item: MenuItem::new("Set up Claude Desktop…", true, None),
            stop_desktop: MenuItem::new("Stop intercepting", true, None),
            quit: MenuItem::new("Quit parsec", true, None),
            next: Instant::now(),
            guided: Arc::new(Mutex::new(Guided::default())),
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    fn build_tray(&mut self) {
        let menu = Menu::new();
        let _ = menu.append(&self.proxy);
        let _ = menu.append(&self.desktop);
        let _ = menu.append(&self.savings);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&self.restart);
        let _ = menu.append(&self.setup_desktop_item);
        let _ = menu.append(&self.stop_desktop);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&self.quit);
        let mut b = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("parsec");
        // The brand mark if it decodes, the ⌁ glyph if it does not — a status
        // item with neither is invisible, which is worse than a fallback.
        match brand_icon() {
            Some(icon) => b = b.with_icon(icon),
            None => b = b.with_title(crate::brand::MARK.to_string()),
        }
        self.tray = b.build().ok();
    }

    /// Windows only. The window is small and informational; it exists so the
    /// taskbar has something to attach a button to. Its icon is set at
    /// runtime, which covers the taskbar and alt-tab — the icon Explorer
    /// shows for parsec.exe itself would need an embedded resource, which is
    /// a build-step change and not done here.
    #[cfg(windows)]
    fn build_window(&mut self, el: &ActiveEventLoop) {
        let mut attrs = Window::default_attributes()
            .with_title("parsec")
            .with_inner_size(winit::dpi::LogicalSize::new(360.0, 200.0))
            .with_resizable(false);
        if let Some(icon) = window_icon() {
            attrs = attrs.with_window_icon(Some(icon));
        }
        self.window = el.create_window(attrs).ok();
    }

    /// Main thread only.
    fn refresh(&mut self) {
        let g = self.guided.lock().map(|g| g.clone()).unwrap_or_default();
        if g.active && !g.done {
            self.proxy.set_text("Setting up Claude Desktop…");
            self.desktop.set_text(format!("  {}", g.line));
            self.savings.set_text("  (this menu updates as it goes)");
            self.setup_desktop_item.set_enabled(false);
            return;
        }
        if g.done {
            if let Ok(mut g) = self.guided.lock() {
                *g = Guided::default();
            }
            self.setup_desktop_item.set_enabled(true);
        }
        let s = read_status();
        self.proxy.set_text(&s.proxy);
        self.desktop.set_text(&s.desktop);
        self.savings.set_text(&s.savings);
        self.stop_desktop.set_enabled(s.intercepting);
        self.setup_desktop_item.set_text(if s.intercepting {
            "Re-run Claude Desktop setup…"
        } else {
            "Set up Claude Desktop…"
        });
    }

    /// Run a blocking command off the main thread so the menu stays live.
    fn spawn(&self, args: &'static [&'static str]) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return; // one action at a time — double-clicks are common
        }
        let busy = self.busy.clone();
        let exe = crate::setup_opencode::bin_alias_path();
        std::thread::spawn(move || {
            let _ = std::process::Command::new(exe).args(args).status();
            busy.store(false, Ordering::SeqCst);
        });
    }

    /// The reason the tray exists: present each gate, then WAIT on it instead
    /// of exiting and making the user re-run the command.
    fn spawn_guided_setup(&self) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let guided = self.guided.clone();
        let busy = self.busy.clone();
        let say = move |g: &Arc<Mutex<Guided>>, line: &str| {
            if let Ok(mut s) = g.lock() {
                s.active = true;
                s.line = line.to_string();
            }
        };
        std::thread::spawn(move || {
            say(&guided, "checking mitmproxy…");
            if setup_desktop::mitmdump_path().is_none() {
                say(&guided, "mitmproxy not installed — see the terminal");
                std::thread::sleep(Duration::from_secs(6));
                finish(&guided, &busy);
                return;
            }
            // The gate that makes this worth building: mitmdump starts clean
            // without approval and captures nothing, so we open the pane and
            // poll until macOS says it is enabled.
            if setup_desktop::extension_status() != ExtensionStatus::Ready {
                setup_desktop::open_extension_settings();
                say(
                    &guided,
                    "waiting — enable “Mitmproxy Redirector” in System Settings",
                );
                let deadline = Instant::now() + Duration::from_secs(300);
                while Instant::now() < deadline {
                    if setup_desktop::extension_status() == ExtensionStatus::Ready {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                if setup_desktop::extension_status() != ExtensionStatus::Ready {
                    say(&guided, "timed out waiting for approval — try again");
                    std::thread::sleep(Duration::from_secs(6));
                    finish(&guided, &busy);
                    return;
                }
            }
            say(&guided, "starting the interceptor…");
            let exe = crate::setup_opencode::bin_alias_path();
            let out = std::process::Command::new(exe)
                .args(["setup", "desktop"])
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    say(&guided, "done — quit and reopen Claude Desktop (⌘Q)")
                }
                _ => say(
                    &guided,
                    "setup failed — run `parsec setup desktop` to see why",
                ),
            }
            std::thread::sleep(Duration::from_secs(8));
            finish(&guided, &busy);
        });
    }
}

fn finish(guided: &Arc<Mutex<Guided>>, busy: &Arc<AtomicBool>) {
    if let Ok(mut g) = guided.lock() {
        g.done = true;
    }
    busy.store(false, Ordering::SeqCst);
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _: &ActiveEventLoop) {}

    #[cfg(not(windows))]
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    /// Closing the status window hides it to the tray rather than quitting —
    /// the Windows convention for a resident utility. Quit stays explicit,
    /// via the tray menu.
    #[cfg(windows)]
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            if let Some(w) = &self.window {
                w.set_visible(false);
            }
        }
    }

    fn new_events(&mut self, el: &ActiveEventLoop, _: StartCause) {
        // The status item can only be created once the event loop is up.
        if self.tray.is_none() {
            self.build_tray();
            #[cfg(windows)]
            self.build_window(el);
        }
        if Instant::now() >= self.next {
            self.refresh();
            self.next = Instant::now() + TICK;
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next));

        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.quit.id() {
                el.exit();
            } else if ev.id == self.restart.id() {
                self.spawn(&["up", "--restart"]);
            } else if ev.id == self.stop_desktop.id() {
                self.spawn(&["desktop", "stop"]);
            } else if ev.id == self.setup_desktop_item.id() {
                self.spawn_guided_setup();
            }
            // Reflect the click immediately rather than at the next tick.
            self.next = Instant::now() + Duration::from_millis(300);
        }
    }
}

pub fn run_app() -> anyhow::Result<()> {
    // parsec.exe is console-subsystem on purpose (every other subcommand
    // writes to stdout). The tray is the one role that must not own a
    // console, so it drops the one the loader gave it.
    #[cfg(windows)]
    crate::tray::windows::detach_console();

    let el = EventLoop::new()?;
    let mut app = App::new();
    el.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_mark_is_a_real_png_that_decodes() {
        // include_bytes! of a renamed brand file would compile to something
        // useless; assert the magic AND that it actually decodes to RGBA.
        assert!(MARK_PNG_64.starts_with(b"\x89PNG\r\n\x1a\n"));
        let (rgba, w, h) = decode_mark().expect("brand mark must decode");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(w >= 32 && h >= 32, "too small to scale down cleanly");
    }
}
