#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod engine;
mod keywords;
mod snippets;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

/// Increments every time the global hotkey is pressed.
static HOTKEY_HITS: AtomicU64 = AtomicU64::new(0);

/// Increments on every toast, so a timer from an older message cannot hide a
/// newer one that replaced it.
static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// True while the toast window is showing the dictation pill instead of a
/// message. The two share one window, so only one of them owns it at a time.
static PILL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Logical size of the toast window. Kept in step with tauri.conf.json.
const TOAST_WIDTH: f64 = 380.0;
const TOAST_HEIGHT: f64 = 72.0;

/// True while the setup screen is only testing the hotkey (so we don't record).
static TESTING: AtomicBool = AtomicBool::new(false);

/// True while the setup screen is capturing a new shortcut. The listener must
/// stay stopped so the current shortcut reaches the window instead of toggling
/// a recording, and so the accessibility watchdog does not re-arm it.
static HOTKEY_PAUSED: AtomicBool = AtomicBool::new(false);

/// Appends a line to debug.log so problems can be seen without a debugger.
fn debug_log(message: &str) {
    let path = data_dir().join("debug.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Also write to a fixed path so the log is readable before the app has
    // created its data folder.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/dictate-debug.log")
    {
        use std::io::Write;
        let _ = writeln!(file, "{message}");
    }
    // Keep the log small: start over once it gets big.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 200_000 {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(file, "[{now:.3}] {message}");
    }
}

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent, Wry};

use engine::{HotkeyCapture, HotkeyService, HotkeySignal, Recording};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

// The toast window becomes this panel on macOS. `is_floating_panel` is what
// keeps it above full-screen content; `can_become_key_window: false` is what
// stops the pill from stealing keystrokes out of the app you are dictating into.
#[cfg(target_os = "macos")]
tauri_nspanel::tauri_panel! {
    panel!(ToastPanel {
        config: {
            can_become_key_window: false,
            is_floating_panel: true
        }
    })
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    #[serde(default = "default_hotkey")]
    hotkey: String,
    #[serde(default)]
    onboarded: bool,
    /// Names and terms the transcript should be corrected to.
    #[serde(default)]
    keywords: Vec<String>,
    /// Spoken phrases replaced with text that was never said.
    #[serde(default)]
    snippets: Vec<snippets::Snippet>,
    /// Start with the system. A menu-bar dictation app that has to be launched
    /// by hand after every restart does not do its job, so this defaults on.
    /// There is deliberately no UI for it: macOS shows its own notification and
    /// Login Items is one System Settings click away.
    #[serde(default = "default_true")]
    launch_at_login: bool,
}

fn default_true() -> bool {
    true
}

/// Right Option on macOS, right Control elsewhere: it is a bare modifier that
/// exists on every keyboard, and Alt is AltGr on many layouts.
fn default_hotkey() -> String {
    if cfg!(target_os = "macos") {
        "right_option".into()
    } else {
        "right_ctrl".into()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey: default_hotkey(),
            onboarded: false,
            keywords: Vec::new(),
            snippets: Vec::new(),
            launch_at_login: true,
        }
    }
}

struct AppState {
    dir: PathBuf,
    auth_file: PathBuf,
    config: Mutex<Config>,
    history: Mutex<Vec<Value>>,
    recording: Mutex<Option<Recording>>,
    busy: Mutex<bool>,
    hotkey_service: Mutex<Option<HotkeyService>>,
    tap_sender: Mutex<Option<mpsc::Sender<HotkeySignal>>>,
    hotkey_error: Mutex<Option<String>>,
    capture: Mutex<Option<HotkeyCapture>>,
}

fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"))
        .unwrap_or_else(|_| PathBuf::from("."));
    #[cfg(target_os = "windows")]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("dev.dictate")
}

fn load_config(dir: &PathBuf) -> Config {
    std::fs::read_to_string(dir.join("config.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_config(dir: &PathBuf, config: &Config) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(raw) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(dir.join("config.json"), raw);
    }
}

fn load_history(dir: &PathBuf) -> Vec<Value> {
    std::fs::read_to_string(dir.join("history.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<Value>>(&raw).ok())
        .unwrap_or_default()
}

fn save_history(dir: &PathBuf, items: &[Value]) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(raw) = serde_json::to_string_pretty(items) {
        let _ = std::fs::write(dir.join("history.json"), raw);
    }
}

// --------------------------------------------------------------------------- //
// tray
// --------------------------------------------------------------------------- //

fn preview(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > limit {
        format!("{}…", flat.chars().take(limit).collect::<String>())
    } else {
        flat
    }
}

/// How wide one History row's text may be, in points, padding included.
///
/// macOS opens a submenu to the right only when it fits between the menu and the
/// screen edge, and otherwise flips it to the left. Counting characters cannot
/// keep that width steady — the system font is proportional, so thirty narrow
/// letters are far narrower than thirty wide ones and the submenu crosses the
/// threshold in both directions. Measuring in points does keep it steady, and
/// padding to the full width makes every row identical.
///
/// The menu adds its own insets either side of the title — around 45 points in
/// total at the default size — so the title is kept under the ~200pt of room to
/// the right of the tray menu, leaving the submenu itself just inside it.
const HISTORY_ROW_WIDTH: f64 = 160.0;

/// Width of `text` drawn in the system menu font, in points.
#[cfg(target_os = "macos")]
fn text_width(text: &str) -> f64 {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSFont, NSFontAttributeName, NSStringDrawing};
    use objc2_foundation::{NSDictionary, NSString};

    let string = NSString::from_str(text);
    unsafe {
        let font = NSFont::menuFontOfSize(0.0);
        let font: &AnyObject = &*(Retained::as_ptr(&font) as *const AnyObject);
        let attributes = NSDictionary::from_slices(&[NSFontAttributeName], &[font]);
        string.sizeWithAttributes(Some(&attributes)).width
    }
}

/// Width of `text` in the system menu font. Only macOS can measure for real;
/// this build has no tray submenu to keep narrow, so an estimate will do.
#[cfg(not(target_os = "macos"))]
fn text_width(text: &str) -> f64 {
    text.chars().count() as f64 * 7.0
}

/// One line of a transcript, trimmed and padded to exactly `max_width` points.
/// Padding uses no-break spaces so AppKit cannot collapse them the way it would
/// trailing plain spaces, and so the text never wraps.
fn history_row(text: &str, max_width: f64) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut row = if text_width(&flat) <= max_width {
        flat
    } else {
        // Longest prefix that still fits once the ellipsis is added. Width grows
        // with every character, so binary search finds it in a handful of
        // measurements even for a very long transcript.
        let chars: Vec<char> = flat.chars().collect();
        let (mut fits, mut too_long) = (0usize, chars.len());
        while fits < too_long {
            let mid = (fits + too_long).div_ceil(2);
            let mut candidate: String = chars[..mid].iter().collect();
            candidate.push('…');
            if text_width(candidate.trim_end()) <= max_width {
                fits = mid;
            } else {
                too_long = mid - 1;
            }
        }
        let mut trimmed: String = chars[..fits].iter().collect();
        trimmed.truncate(trimmed.trim_end().len());
        format!("{trimmed}…")
    };
    while text_width(&row) < max_width {
        row.push('\u{00a0}');
    }
    row
}

// --------------------------------------------------------------------------- //
// toast
// --------------------------------------------------------------------------- //

/// Shows a short message in a small floating window of our own.
///
/// macOS notifications need permission, register a second app bundle, and drag
/// Dictate to the front — all of which showed up as the Dock icon bouncing and
/// an entry in the app switcher. This window does none of that: it is ours, it
/// carries no permission prompt, and it is not focusable, so the app you are
/// typing in keeps the keyboard while the message is on screen.
fn toast(app: &AppHandle, message: &str) {
    toast_with(app, message, false);
}

/// A toast that reports a failure. The window gives it an amber edge so a
/// failed dictation does not read like "History cleared".
fn toast_error(app: &AppHandle, message: &str) {
    toast_with(app, message, true);
}

fn toast_with(app: &AppHandle, message: &str, error: bool) {
    let Some(window) = app.get_webview_window("toast") else {
        debug_log("toast: window not found");
        return;
    };
    // A message owns the window: the pill would otherwise sit behind it, and an
    // error is what the user needs to see first.
    if PILL_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = app.emit_to("toast", "pill", "idle");
    }
    let seq = TOAST_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.emit_to(
        "toast",
        "toast",
        json!({ "message": message, "error": error }),
    );
    place_toast(app, &window);

    if window.show().is_err() {
        debug_log("toast: could not show the window");
        return;
    }
    debug_log(&format!("toast: {}", preview(message, 60)));

    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2600));
        if TOAST_SEQ.load(Ordering::SeqCst) == seq {
            if let Some(window) = handle.get_webview_window("toast") {
                let _ = window.hide();
            }
        }
    });
}

/// Bottom centre of the screen that holds the menu bar, just above the Dock.
/// The work area already has the menu bar and Dock subtracted.
fn place_toast(app: &AppHandle, window: &tauri::WebviewWindow<Wry>) {
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let scale = monitor.scale_factor();
        let area = monitor.work_area();
        let width = TOAST_WIDTH * scale;
        let height = TOAST_HEIGHT * scale;
        let x = area.position.x as f64 + (area.size.width as f64 - width) / 2.0;
        let y = area.position.y as f64 + area.size.height as f64 - height - 6.0 * scale;
        let _ = window.set_position(tauri::PhysicalPosition::new(
            x.round() as i32,
            y.round() as i32,
        ));
    }
}

/// Turns the toast window into a non-activating NSPanel that floats on every
/// Space, full-screen apps included.
///
/// Collection-behaviour flags alone are not enough: AppKit refuses to draw a
/// plain NSWindow over another app's full-screen Space, whatever flags and
/// level it has. The window's class has to become NSPanel, which is why this
/// goes through tauri-nspanel — the same route Handy's overlay takes.
#[cfg(target_os = "macos")]
fn pin_toast_over_everything(app: &AppHandle) {
    use tauri_nspanel::{
        objc2_app_kit::NSWindowCollectionBehavior, CollectionBehavior, PanelLevel, StyleMask,
        WebviewWindowExt,
    };

    let Some(window) = app.get_webview_window("toast") else {
        debug_log("toast: window not found for panel conversion");
        return;
    };
    let panel = match window.to_panel::<ToastPanel<Wry>>() {
        Ok(panel) => panel,
        Err(error) => {
            debug_log(&format!("toast: panel conversion failed: {error}"));
            return;
        }
    };

    // Borderless and non-activating: the pill never takes focus from the app
    // you are dictating into, even when it appears over a full-screen window.
    let mask = StyleMask::empty().borderless().nonactivating_panel().value();
    if let Err(error) = panel.set_style_mask(mask) {
        debug_log(&format!("toast: style mask rejected: {error:?}"));
    }
    // Order matters: set_floating_panel resets the level to floating (3), so the
    // status level has to go on afterwards to sit above full-screen content.
    panel.set_floating_panel(true);
    panel.set_level(PanelLevel::Status.value());
    panel.set_collection_behavior(
        CollectionBehavior::new()
            .can_join_all_spaces()
            .full_screen_auxiliary()
            .value(),
    );
    // Decoration only: clicks go to whatever is underneath.
    panel.set_ignores_mouse_events(true);
    panel.set_hides_on_deactivate(false);

    // Say what AppKit ended up with, so a silent no-op shows up in debug.log.
    let ns_panel = panel.as_panel();
    let is_panel: bool = unsafe {
        objc2::msg_send![ns_panel, isKindOfClass: objc2::class!(NSPanel)]
    };
    let behavior: NSWindowCollectionBehavior = unsafe { objc2::msg_send![ns_panel, collectionBehavior] };
    let level: isize = unsafe { objc2::msg_send![ns_panel, level] };
    debug_log(&format!(
        "toast: panel={is_panel} flags={:#x} level={level}",
        behavior.0
    ));
}

#[cfg(not(target_os = "macos"))]
fn pin_toast_over_everything(_app: &AppHandle) {}

/// Dictate is a menu-bar app: no Dock icon, no entry in the app switcher.
///
/// The bundle says so with `LSUIElement`, but that key only applies to a real
/// app bundle, so `cargo tauri dev` would still show an icon. Setting the
/// activation policy at runtime makes the dev build behave like the shipped one.
/// Showing the setup window still brings it to the front: the window is made key
/// and the app is activated, which accessory apps can do.
#[cfg(target_os = "macos")]
fn hide_dock_icon(app: &AppHandle) {
    match app.set_dock_visibility(false) {
        Ok(()) => debug_log("dock: icon hidden"),
        Err(error) => debug_log(&format!("dock: could not hide the icon ({error})")),
    }
}

#[cfg(not(target_os = "macos"))]
fn hide_dock_icon(_app: &AppHandle) {}

/// Shows the dictation pill in the toast window. `state` is "listening" while
/// the microphone is open and "working" while the transcript is on its way.
fn show_pill(app: &AppHandle, state: &str) {
    let Some(window) = app.get_webview_window("toast") else {
        debug_log("pill: window not found");
        return;
    };
    PILL_ACTIVE.store(true, Ordering::SeqCst);
    // Invalidate any message timer still pending, so it cannot hide the pill.
    TOAST_SEQ.fetch_add(1, Ordering::SeqCst);
    place_toast(app, &window);
    let _ = app.emit_to("toast", "pill", state.to_string());
    if window.show().is_err() {
        debug_log("pill: could not show the window");
    } else {
        debug_log(&format!("pill: shown ({state})"));
    }
}

/// Hides the pill, leaving the window up briefly so its fade-out can play.
fn hide_pill(app: &AppHandle) {
    if !PILL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = app.emit_to("toast", "pill", "idle");
    debug_log("pill: hidden");
    let handle = app.clone();
    // A message or a new dictation may claim the window during the fade.
    let messages = TOAST_SEQ.load(Ordering::SeqCst);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(180));
        if PILL_ACTIVE.load(Ordering::SeqCst) || TOAST_SEQ.load(Ordering::SeqCst) != messages {
            return;
        }
        if let Some(window) = handle.get_webview_window("toast") {
            let _ = window.hide();
        }
    });
}

fn build_menu(app: &AppHandle, history: &[Value]) -> tauri::Result<Menu<Wry>> {
    // Recent transcriptions live in a History submenu. The previews are kept
    // short on purpose: macOS draws the submenu on whichever side has room, and
    // a short list stays narrow enough to open to the right of a tray menu that
    // sits at the screen edge.
    let mut entries: Vec<MenuItem<Wry>> = Vec::new();
    for (index, item) in history.iter().take(5).enumerate() {
        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
        entries.push(MenuItem::with_id(
            app,
            format!("h:{index}"),
            history_row(text, HISTORY_ROW_WIDTH),
            true,
            None::<&str>,
        )?);
    }
    if entries.is_empty() {
        entries.push(MenuItem::with_id(
            app,
            "h:none",
            "Nothing yet",
            false,
            None::<&str>,
        )?);
    }
    let clear = MenuItem::with_id(
        app,
        "h:clear",
        "Clear History",
        !history.is_empty(),
        None::<&str>,
    )?;
    let history_separator = PredefinedMenuItem::separator(app)?;

    let refs: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = entries
        .iter()
        .map(|item| item as &dyn tauri::menu::IsMenuItem<Wry>)
        .chain([
            &history_separator as &dyn tauri::menu::IsMenuItem<Wry>,
            &clear as &dyn tauri::menu::IsMenuItem<Wry>,
        ])
        .collect();

    let history_menu = Submenu::with_items(app, "History", true, &refs)?;
    let keywords_item = MenuItem::with_id(app, "keywords", "Keywords", true, None::<&str>)?;
    let snippets_item = MenuItem::with_id(app, "snippets", "Snippets", true, None::<&str>)?;
    // Lands on the account screen rather than the welcome tour: sign out and the
    // permission states are the only things this needs to reach after setup.
    let settings_item = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Dictate", true, None::<&str>)?;

    Menu::with_items(
        app,
        &[
            &history_menu,
            &PredefinedMenuItem::separator(app)?,
            &keywords_item,
            &snippets_item,
            &settings_item,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
}

fn refresh_tray(app: &AppHandle) {
    let state = app.state::<AppState>();
    let history = state.history.lock().unwrap().clone();
    if let Ok(menu) = build_menu(app, &history) {
        if let Some(tray) = app.tray_by_id("tray") {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn show_window(app: &AppHandle, view: &str) {
    // The setup screen pauses the listener itself while it captures keys.
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
    let _ = app.emit("goto", view);
}

// --------------------------------------------------------------------------- //
// hotkey lifecycle
// --------------------------------------------------------------------------- //

fn ensure_hotkey(app: &AppHandle) {
    if HOTKEY_PAUSED.load(Ordering::SeqCst) {
        return;
    }
    let state = app.state::<AppState>();
    let mut guard = state.hotkey_service.lock().unwrap();
    if let Some(service) = guard.as_ref() {
        // The listener is installed for the whole run; only the registration
        // can be missing, because pausing the setup screen removes it. Nothing
        // is torn down and rebuilt here: on Windows a rebuilt keyboard hook
        // stopped receiving keys after the setup screen had used it.
        if service.is_registered() {
            return;
        }
        let hotkey = state.config.lock().unwrap().hotkey.clone();
        match service.set_hotkey(&hotkey) {
            Ok(()) => {
                debug_log(&format!("hotkey: armed ({hotkey})"));
                *state.hotkey_error.lock().unwrap() = None;
            }
            Err(message) => {
                debug_log(&format!("hotkey: FAILED to arm ({hotkey}): {message}"));
                *state.hotkey_error.lock().unwrap() = Some(message);
            }
        }
        return;
    }
    // Re-check after taking the lock: a pause may have landed while we waited.
    if HOTKEY_PAUSED.load(Ordering::SeqCst) {
        return;
    }
    let hotkey = state.config.lock().unwrap().hotkey.clone();
    let Some(sender) = state.tap_sender.lock().unwrap().clone() else {
        return;
    };
    match HotkeyService::start(&hotkey, sender) {
        Ok(service) => {
            debug_log(&format!("hotkey: armed ({hotkey})"));
            *guard = Some(service);
            *state.hotkey_error.lock().unwrap() = None;
        }
        Err(message) => {
            eprintln!("hotkey: {message}");
            debug_log(&format!("hotkey: FAILED to arm ({hotkey}): {message}"));
            *state.hotkey_error.lock().unwrap() = Some(message);
        }
    }
}

fn stop_hotkey(app: &AppHandle) {
    debug_log("hotkey: stopped");
    let state = app.state::<AppState>();
    let service = state.hotkey_service.lock().unwrap().take();
    if let Some(service) = service {
        service.stop();
    }
}

fn restart_hotkey(app: &AppHandle) {
    HOTKEY_PAUSED.store(false, Ordering::SeqCst);
    ensure_hotkey(app);
}

/// Escape is registered only while recording, so outside a dictation it is an
/// ordinary key for every other app.
fn set_cancel_armed(app: &AppHandle, armed: bool) {
    let state = app.state::<AppState>();
    let guard = state.hotkey_service.lock().unwrap();
    let Some(service) = guard.as_ref() else {
        return;
    };
    if armed {
        match service.arm_cancel() {
            Ok(()) => debug_log("hotkey: Escape armed to cancel the recording"),
            Err(message) => debug_log(&format!("hotkey: {message}")),
        }
    } else {
        service.disarm_cancel();
        debug_log("hotkey: Escape disarmed (recording finished)");
    }
}

/// What a refused token means for the user, in the one place both paths agree.
fn expired_message() -> String {
    "Your ChatGPT session expired. Sign in again from the menu bar.".to_string()
}

/// Drops the recording in progress without transcribing or pasting anything.
fn cancel_recording(app: &AppHandle) {
    let state = app.state::<AppState>();
    let existing = state.recording.lock().unwrap().take();
    let Some(recording) = existing else {
        return;
    };
    set_cancel_armed(app, false);
    hide_pill(app);
    debug_log("recording: cancelled with Escape");
    recording.cancel();
    let _ = app.emit("recording-cancelled", json!({}));
}

// --------------------------------------------------------------------------- //
// recording pipeline
// --------------------------------------------------------------------------- //

fn handle_toggle(app: &AppHandle) {
    HOTKEY_HITS.fetch_add(1, Ordering::SeqCst);
    debug_log("hotkey: pressed");
    let state = app.state::<AppState>();
    let _ = app.emit("hotkey-fired", json!({}));

    // While the setup screen is confirming the shortcut we only test it.
    if TESTING.load(Ordering::SeqCst) {
        debug_log("hotkey: ignored (confirmation test in progress)");
        return;
    }

    if *state.busy.lock().unwrap() {
        return;
    }

    let existing = state.recording.lock().unwrap().take();
    let Some(recording) = existing else {
        let meter = app.clone();
        // One line per dictation is enough to tell a quiet room from a mic that
        // is delivering nothing at all.
        let heard = std::sync::Arc::new(AtomicBool::new(false));
        match Recording::start(move |level| {
            let _ = meter.emit_to("toast", "recording-level", level);
            if level > 0.01 && !heard.swap(true, Ordering::SeqCst) {
                debug_log(&format!("pill: first level {level:.3}"));
            }
        }) {
            Ok(recording) => {
                debug_log("recording: started");
                *state.recording.lock().unwrap() = Some(recording);
                set_cancel_armed(app, true);
                show_pill(app, "listening");
            }
            Err(message) => {
                debug_log(&format!("recording: FAILED to start: {message}"));
                toast_error(app, &message);
                let _ = app.emit("engine-event", json!({"event": "error", "message": message}));
            }
        }
        return;
    };

    // Past this point the microphone is closed, so Escape should pass through
    // to other apps again. The pill stays up, now showing that the transcript
    // is being fetched.
    set_cancel_armed(app, false);
    show_pill(app, "working");
    *state.busy.lock().unwrap() = true;
    let worker = app.clone();
    std::thread::spawn(move || {
        let state = worker.state::<AppState>();
        let result = (|| -> Result<(), String> {
            let finished = recording.finish()?;
            // True when the app's own time limit ended the recording rather than
            // the user letting go, which the transcript alone cannot show.
            let hit_limit = finished.hit_limit;
            let (path, seconds, peak) = (finished.path, finished.seconds, finished.peak);
            debug_log(&format!(
                "recording: stopped after {seconds:.1}s, peak={peak:.4}"
            ));
            if peak < 0.002 {
                let _ = std::fs::remove_file(&path);
                let message = "No sound was captured. Allow Dictate in System Settings > Privacy & Security > Microphone, and make sure your microphone is not muted.".to_string();
                debug_log(&format!("recording: {message}"));
                toast_error(&worker, &message);
                return Ok(());
            }
            if seconds < 0.3 {
                let _ = std::fs::remove_file(&path);
                debug_log("recording: too short, skipped");
                return Ok(());
            }
            let transcription = (|| -> Result<String, String> {
                let mut credentials = auth::valid(&state.auth_file)?;
                match engine::transcribe(&path, &credentials) {
                    Ok(text) => Ok(text),
                    // The account was refused rather than the audio. The stored
                    // expiry can be wrong (a clock change, or a session revoked
                    // elsewhere), so refresh once before giving up.
                    Err(error) if error.is_auth() => {
                        if credentials.refresh_token.is_empty() {
                            return Err(expired_message());
                        }
                        credentials = auth::refresh(&state.auth_file, &credentials)?;
                        match engine::transcribe(&path, &credentials) {
                            Ok(text) => Ok(text),
                            Err(error) if error.is_auth() => Err(expired_message()),
                            Err(error) => Err(error.message),
                        }
                    }
                    Err(error) => Err(error.message),
                }
            })();
            let _ = std::fs::remove_file(&path);
            let transcript = transcription?;
            if transcript.trim().is_empty() {
                debug_log("transcribe: empty transcript");
                return Ok(());
            }
            debug_log(&format!("transcribe: ok ({})", &transcript.chars().take(60).collect::<String>()));

            // Fix names and terms locally, before the text is saved or pasted.
            let custom_words = state.config.lock().unwrap().keywords.clone();
            let corrected = keywords::apply(&transcript, &custom_words);
            if corrected != transcript {
                debug_log(&format!(
                    "keywords: corrected ({})",
                    &corrected.chars().take(60).collect::<String>()
                ));
            }
            // Then expand snippets: spoken phrase in, something longer out.
            let document_snippets = state.config.lock().unwrap().snippets.clone();
            let expanded = snippets::apply(&corrected, &document_snippets);
            if expanded != corrected {
                debug_log(&format!(
                    "snippets: expanded ({})",
                    &expanded.chars().take(60).collect::<String>()
                ));
            }
            let transcript = expanded;

            let entry = json!({
                "text": transcript,
                "created_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            });
            {
                let mut history = state.history.lock().unwrap();
                history.insert(0, entry);
                history.truncate(10);
                save_history(&state.dir, &history);
            }
            // Keep what the user had on the clipboard, paste the transcript,
            // then put the old contents back once the frontmost app has read it.
            let clipboard = engine::backup_clipboard();
            let copied = engine::copy_to_clipboard(&transcript);
            if !copied {
                debug_log("clipboard: could not copy the transcript");
            }
            refresh_tray(&worker);
            if engine::paste_at_cursor() {
                debug_log("paste: sent to the frontmost app");
            } else {
                debug_log("paste: the keystroke could not be sent");
            }
            if let Some(backup) = clipboard {
                std::thread::sleep(std::time::Duration::from_millis(600));
                // Skip the restore if the user copied something new meanwhile.
                if engine::clipboard_holds(&transcript) {
                    engine::restore_clipboard(backup);
                    debug_log("clipboard: previous contents restored");
                }
            }
            // Said last, once the text is in, so it cannot be mistaken for a
            // failure or cover the transcript itself.
            if hit_limit {
                debug_log("recording: stopped at the 5 minute limit");
                toast(&worker, "Recording stopped at the 5-minute limit.");
            }
            Ok(())
        })();

        if let Err(message) = result {
            eprintln!("dictation: {message}");
            debug_log(&format!("dictation failed: {message}"));
            toast_error(&worker, &message);
        }
        // Covers every way out of the worker: pasted, empty, too short, failed.
        hide_pill(&worker);
        *state.busy.lock().unwrap() = false;
    });
}

// --------------------------------------------------------------------------- //
// commands
// --------------------------------------------------------------------------- //

#[tauri::command]
fn debug_log_command(message: String) {
    debug_log(&format!("ui: {message}"));
}

#[tauri::command]
fn set_testing(value: bool) {
    TESTING.store(value, Ordering::SeqCst);
    debug_log(&format!("testing flag: {value}"));
}

/// Toggles a recording from inside the app's own window.
///
/// The Windows keyboard hook does not deliver keys to the window that has
/// focus, so a focused app would never start a dictation. The page sees those
/// keys instead and asks for the toggle through here.
#[tauri::command]
fn toggle_dictation(app: AppHandle) {
    handle_toggle(&app);
}

/// Cancels a recording from inside the app's own window. Escape is a second
/// hook registration, so it is deaf in the same state the shortcut is.
#[tauri::command]
fn cancel_dictation(app: AppHandle) {
    cancel_recording(&app);
}

#[tauri::command]
fn pause_hotkey(app: AppHandle) {
    debug_log("hotkey: paused for the setup screen");
    HOTKEY_PAUSED.store(true, Ordering::SeqCst);
    // Unregister the shortcut but keep the listener: the setup screen needs
    // the keys to reach the window, and rebuilding the listener afterwards is
    // what left it deaf.
    let state = app.state::<AppState>();
    let guard = state.hotkey_service.lock().unwrap();
    if let Some(service) = guard.as_ref() {
        service.pause();
    }
}

#[tauri::command]
fn resume_hotkey(app: AppHandle) {
    debug_log("hotkey: resumed");
    HOTKEY_PAUSED.store(false, Ordering::SeqCst);
    ensure_hotkey(&app);
}

#[tauri::command]
fn hotkey_hits() -> u64 {
    HOTKEY_HITS.load(Ordering::SeqCst)
}

#[tauri::command]
fn bootstrap(app: AppHandle) -> Value {
    let state = app.state::<AppState>();
    let config = state.config.lock().unwrap().clone();
    let credentials = auth::load(&state.auth_file);
    json!({
        "signed_in": credentials.is_some(),
        "email": credentials.as_ref().and_then(|c| c.email()),
        "hotkey": config.hotkey,
        "hotkey_label": engine::pretty(&config.hotkey),
        "onboarded": config.onboarded,
        "engine_running": state.hotkey_service.lock().unwrap().is_some(),
        "accessibility": engine::accessibility_trusted(),
        "platform": engine::PLATFORM,
        "hotkey_error": state.hotkey_error.lock().unwrap().clone(),
    })
}

#[tauri::command]
fn sign_out(app: AppHandle) -> Value {
    let state = app.state::<AppState>();
    // A sign-in can still be waiting for its callback. Ending it here is what
    // stops an approval in an already-open tab from writing the tokens back
    // after the user has signed out, and it frees the callback port at once.
    auth::cancel_attempt();
    let credentials = auth::load(&state.auth_file);
    let _ = std::fs::remove_file(&state.auth_file);
    // Off the main thread, and best effort: the local tokens are already gone.
    std::thread::spawn(move || auth::revoke(credentials.as_ref()));
    debug_log("sign-out: removed the stored tokens");
    json!({ "signed_in": false, "email": null })
}

#[tauri::command]
fn sign_in(app: AppHandle) {
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let result = auth::login(&state.auth_file);
        // The attempt is over however it ended, so there is no longer a tab that
        // could complete it.
        auth::clear_pending_url();
        match result {
            Ok(_) => {
                HOTKEY_PAUSED.store(false, Ordering::SeqCst);
                ensure_hotkey(&app);
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
                let payload = json!({"ok": true, "email": auth::load(&state.auth_file).and_then(|c| c.email())});
                let _ = app.emit("sign-in-finished", payload);
            }
            Err(message) => {
                // A sign-out cancels the attempt, which is not a failure the UI
                // should report as one.
                let cancelled = message == auth::CANCELLED_MESSAGE;
                let _ = app.emit(
                    "sign-in-finished",
                    json!({ "ok": false, "message": message, "cancelled": cancelled }),
                );
            }
        }
    });
}

#[tauri::command]
fn get_keywords(app: AppHandle) -> Vec<String> {
    app.state::<AppState>().config.lock().unwrap().keywords.clone()
}

#[tauri::command]
fn set_keywords(app: AppHandle, keywords: Vec<String>) -> Vec<String> {
    let state = app.state::<AppState>();
    let mut cleaned: Vec<String> = Vec::new();
    for word in keywords {
        let word = word.trim();
        if word.is_empty() || word.chars().count() > 80 {
            continue;
        }
        if cleaned
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(word))
        {
            continue;
        }
        cleaned.push(word.to_string());
        if cleaned.len() >= 200 {
            break;
        }
    }
    {
        let mut config = state.config.lock().unwrap();
        config.keywords = cleaned.clone();
        save_config(&state.dir, &config);
    }
    debug_log(&format!("keywords: saved {} entries", cleaned.len()));
    cleaned
}

#[tauri::command]
fn get_snippets(app: AppHandle) -> Vec<snippets::Snippet> {
    app.state::<AppState>().config.lock().unwrap().snippets.clone()
}

#[tauri::command]
fn set_snippets(app: AppHandle, snippets: Vec<snippets::Snippet>) -> Vec<snippets::Snippet> {
    let state = app.state::<AppState>();
    let cleaned = snippets::clean(snippets);
    {
        let mut config = state.config.lock().unwrap();
        config.snippets = cleaned.clone();
        save_config(&state.dir, &config);
    }
    debug_log(&format!("snippets: saved {} entries", cleaned.len()));
    cleaned
}

#[tauri::command]
fn auth_status(app: AppHandle) -> Value {
    let state = app.state::<AppState>();
    let credentials = auth::load(&state.auth_file);
    json!({
        "signed_in": credentials.is_some(),
        "email": credentials.as_ref().and_then(|c| c.email()),
        "pending": auth::pending_url().is_some(),
    })
}

/// Opens the browser tab for a sign-in that is still waiting.
///
/// The tab can be closed at any point and the app cannot see that, so it holds
/// the authorize URL for the length of the attempt and reopens it on request.
/// The URL carries the same state and PKCE challenge, so approving in the new
/// tab completes the sign-in that is already running.
#[tauri::command]
fn reopen_sign_in() -> Value {
    match auth::pending_url() {
        Some(url) => match open::that(&url) {
            Ok(()) => {
                debug_log("sign-in: reopened the browser");
                json!({ "ok": true })
            }
            Err(error) => {
                debug_log(&format!("sign-in: could not reopen the browser: {error}"));
                json!({ "ok": false })
            }
        },
        None => {
            debug_log("sign-in: no attempt is waiting for a callback");
            json!({ "ok": false })
        }
    }
}#[tauri::command]
fn request_mic_access(_app: AppHandle) -> Value {
    // Deliberately no tccutil reset here. TCC caches a decision for the life of
    // the process, so a reset while running cannot produce a prompt: the request
    // below answers false at once. Worse, the reset deletes the row from the
    // Microphone pane, which leaves the user with nothing to switch on and no
    // way to recover short of relaunching. A denied app is re-enabled in System
    // Settings, so the UI sends people there instead.

    let (tx, rx) = mpsc::channel::<bool>();
    // AVFoundation answers on its own queue and the system prompt is presented
    // by tccd, so the request does not need the main thread; wait here instead
    // of freezing the window.
    engine::request_mic_access(tx);
    let granted = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .unwrap_or(false);
    debug_log(&format!("mic access request answered: {granted}"));
    json!({ "granted": granted, "permission": engine::mic_permission() })
}

#[tauri::command]
fn check_permissions(app: AppHandle, prompt: bool) -> Value {
    if prompt && !engine::accessibility_trusted() {
        let _ = app.run_on_main_thread(|| {
            engine::prompt_accessibility();
        });
    }
    let accessibility = engine::accessibility_trusted();
    let mic_permission = engine::mic_permission();
    if accessibility {
        ensure_hotkey(&app);
    }
    json!({
        "accessibility": accessibility,
        "mic": mic_permission == "granted",
        "mic_permission": mic_permission,
        "platform": engine::PLATFORM,
    })
}

#[tauri::command]
fn set_hotkey(app: AppHandle, hotkey: String) -> Result<Value, String> {
    let state = app.state::<AppState>();
    engine::parse_hotkey(&hotkey)?;
    {
        let mut config = state.config.lock().unwrap();
        config.hotkey = hotkey.clone();
        save_config(&state.dir, &config);
    }
    HOTKEY_PAUSED.store(false, Ordering::SeqCst);
    // Swap the registration on the listener that is already running instead of
    // restarting it, so the shortcut the setup screen is about to ask the user
    // to press is the same hook that just captured the key they chose.
    let swapped = {
        let service = state.hotkey_service.lock().unwrap();
        match service.as_ref() {
            Some(service) => {
                service.set_hotkey(&hotkey)?;
                true
            }
            None => false,
        }
    };
    if swapped {
        debug_log(&format!("hotkey: armed ({hotkey})"));
        *state.hotkey_error.lock().unwrap() = None;
    } else {
        ensure_hotkey(&app);
    }
    let running = app
        .state::<AppState>()
        .hotkey_service
        .lock()
        .unwrap()
        .is_some();
    Ok(json!({
        "hotkey": hotkey,
        "hotkey_label": engine::pretty(&hotkey),
        "engine_running": running,
    }))
}

/// Starts reading bare modifier presses for the setup screen's shortcut box.
#[tauri::command]
fn start_capture(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut guard = state.capture.lock().unwrap();
    if guard.is_some() {
        return Ok(());
    }
    let (tx, rx) = mpsc::channel::<String>();
    let capture = HotkeyCapture::start(tx)?;
    let handle = app.clone();
    std::thread::spawn(move || {
        while let Ok(id) = rx.recv() {
            let _ = handle.emit("capture-key", id);
        }
    });
    *guard = Some(capture);
    Ok(())
}

#[tauri::command]
fn stop_capture(app: AppHandle) {
    let state = app.state::<AppState>();
    let capture = state.capture.lock().unwrap().take();
    if let Some(capture) = capture {
        capture.stop();
    }
}

/// Records that setup is finished, without touching the window.
///
/// The Ready screen calls this as soon as it appears, so closing the window any
/// other way still remembers that setup is done. Only the Done button hides it.
#[tauri::command]
fn mark_onboarded(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut config = state.config.lock().unwrap();
        if !config.onboarded {
            config.onboarded = true;
            save_config(&state.dir, &config);
        }
    }
    // Not conditional on the write above: walking the setup screens again from
    // the tray pauses the hotkey, and every exit from them has to undo that.
    HOTKEY_PAUSED.store(false, Ordering::SeqCst);
    ensure_hotkey(&app);
    refresh_tray(&app);
    Ok(())
}

#[tauri::command]
fn complete_onboarding(app: AppHandle) -> Result<(), String> {
    mark_onboarded(app.clone())?;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    Ok(())
}

#[tauri::command]
fn open_privacy(kind: String) {
    #[cfg(target_os = "macos")]
    {
        let pane = if kind == "mic" {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        } else {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        };
        let _ = Command::new("open").arg(pane).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        // Windows keeps the microphone switch in Settings; there is no
        // per-app accessibility grant to open.
        if kind == "mic" {
            let mut command = Command::new("cmd");
            command.args(["/C", "start", "", "ms-settings:privacy-microphone"]);
            // cmd is a console program, so without this the window flashes.
            engine::hide_console(&mut command);
            let _ = command.spawn();
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = kind;
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    stop_hotkey(&app);
    app.exit(0);
}

// --------------------------------------------------------------------------- //
// app
// --------------------------------------------------------------------------- //

fn main() {
    let dir = data_dir();
    let config = load_config(&dir);
    let onboarded = config.onboarded;
    let history = load_history(&dir);
    let (tap_sender, tap_receiver) = mpsc::channel::<HotkeySignal>();

    // The panel manager has to exist before the toast window is converted.
    #[cfg(target_os = "macos")]
    let builder = tauri::Builder::default().plugin(tauri_nspanel::init());
    #[cfg(not(target_os = "macos"))]
    let builder = tauri::Builder::default();

    let builder = builder
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState {
            auth_file: dir.join("auth.json"),
            dir,
            config: Mutex::new(config),
            history: Mutex::new(history),
            recording: Mutex::new(None),
            busy: Mutex::new(false),
            hotkey_service: Mutex::new(None),
            tap_sender: Mutex::new(Some(tap_sender)),
            hotkey_error: Mutex::new(None),
            capture: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            get_keywords,
            set_keywords,
            get_snippets,
            set_snippets,
            sign_in,
            sign_out,
            reopen_sign_in,
            auth_status,
            check_permissions,
            request_mic_access,
            set_hotkey,
            start_capture,
            stop_capture,
            hotkey_hits,
            debug_log_command,
            set_testing,
            pause_hotkey,
            resume_hotkey,
            toggle_dictation,
            cancel_dictation,
            mark_onboarded,
            complete_onboarding,
            open_privacy,
            quit_app
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            hide_dock_icon(&handle);

            let worker = handle.clone();
            std::thread::spawn(move || {
                while let Ok(signal) = tap_receiver.recv() {
                    match signal {
                        HotkeySignal::Toggle => handle_toggle(&worker),
                        HotkeySignal::Cancel => cancel_recording(&worker),
                    }
                }
            });

            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
            let menu = build_menu(&handle, &[])?;
            TrayIconBuilder::with_id("tray")
                .icon(icon)
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "quit" => {
                        stop_hotkey(app);
                        app.exit(0);
                    }
                    "settings" => show_window(app, "signin"),
                    "keywords" => show_window(app, "keywords"),
                    "snippets" => show_window(app, "snippets"),
                    "h:clear" => {
                        let state = app.state::<AppState>();
                        state.history.lock().unwrap().clear();
                        save_history(&state.dir, &[]);
                        debug_log("history: cleared");
                        // Replacing the tray menu from inside its own click
                        // handler is unreliable on macOS; let the menu finish
                        // closing first.
                        let handle = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            refresh_tray(&handle);
                        });
                        toast(app, "History cleared");
                    }
                    id if id.starts_with("h:") => {
                        let index: usize = id[2..].parse().unwrap_or(usize::MAX);
                        let state = app.state::<AppState>();
                        let text = state
                            .history
                            .lock()
                            .unwrap()
                            .get(index)
                            .and_then(|item| item.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if !text.is_empty() {
                            if engine::copy_to_clipboard(&text) {
                                debug_log(&format!(
                                    "history: copied ({})",
                                    &text.chars().take(40).collect::<String>()
                                ));
                                toast(app, &format!("Copied: {}", preview(&text, 60)));
                            } else {
                                debug_log("history: the clipboard could not be written");
                                toast(app, "Could not copy that to the clipboard.");
                            }
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            // Re-registers the login item at the path of the binary that is
            // actually running. Moving between the dev build and the bundled
            // app would otherwise leave startup pointing at the old one.
            if handle.state::<AppState>().config.lock().unwrap().launch_at_login {
                match handle.autolaunch().enable() {
                    Ok(()) => debug_log("autostart: enabled"),
                    Err(error) => debug_log(&format!("autostart: could not enable ({error})")),
                }
            }

            ensure_hotkey(&handle);
            refresh_tray(&handle);

            // The pill follows the user across Spaces and shows above
            // full-screen apps.
            pin_toast_over_everything(&handle);

            debug_log(&format!(
                "startup microphone permission: {}",
                engine::mic_permission()
            ));

            // If Accessibility is granted later, start listening without a relaunch.
            let watchdog = handle.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if engine::accessibility_trusted() {
                    ensure_hotkey(&watchdog);
                }
            });

            if onboarded {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            } else if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                let app = window.app_handle();
                // Closing the window always ends any shortcut capture, so the
                // paused listener has to come back and any pending test has to
                // stop swallowing recordings.
                TESTING.store(false, Ordering::SeqCst);
                restart_hotkey(app);
            }
        });

    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building Dictate");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            stop_hotkey(app_handle);
        }
    });
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn a_config_saved_before_snippets_existed_still_loads() {
        // Everyone upgrading has a config.json with no `snippets` key. If this
        // failed to parse, load_config would silently reset their keywords.
        let raw = r#"{"hotkey":"right_option","onboarded":true,"keywords":["Ghostty"]}"#;
        let config: Config = serde_json::from_str(raw).expect("old config must still parse");
        assert_eq!(config.keywords, vec!["Ghostty".to_string()]);
        assert!(config.snippets.is_empty());
    }

    #[test]
    fn a_config_from_before_launch_at_login_started_out_enabled() {
        // Anyone upgrading has a config.json without this key. Starting at login
        // is the point of the app, so a missing key has to mean "on", not off.
        let raw = r#"{"hotkey":"right_option","onboarded":true,"keywords":[],"snippets":[]}"#;
        let config: Config = serde_json::from_str(raw).expect("old config must still parse");
        assert!(config.launch_at_login);
    }

    #[test]
    fn snippets_round_trip_through_the_config_file() {
        let config = Config {
            hotkey: "right_option".into(),
            onboarded: true,
            keywords: vec![],
            snippets: vec![snippets::Snippet {
                trigger: "my email".into(),
                replacement: "someone@example.com".into(),
            }],
            launch_at_login: true,
        };
        let raw = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.snippets, config.snippets);
    }

    #[test]
    fn history_rows_are_one_fixed_width() {
        // Rows are measured, not counted, so wide letters and narrow ones have
        // to land on the same width to the point. Padding overshoots by at most
        // one no-break space.
        let cases = [
            "hi",
            "Now try thirty.",
            "WWW MMM iii lll",
            "Okay, I'm gonna do a really long dictation about windows and pixels",
        ];
        for text in cases {
            let row = history_row(text, HISTORY_ROW_WIDTH);
            let width = text_width(&row);
            assert!(
                width >= HISTORY_ROW_WIDTH && width <= HISTORY_ROW_WIDTH + 5.0,
                "{text:?} measured {width}"
            );
        }
    }
}
