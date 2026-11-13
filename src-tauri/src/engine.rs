//! Recording, transcription, pasting and the global hotkey — native Rust, no helpers.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings as EnigoSettings};
use handy_keys::{Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener, Modifiers};
use handy_recorder::{list_input_devices, AudioChunk, InputDevice, Recorder, RecorderConfig, Sink};

use crate::auth::Credentials;

const TRANSCRIBE_URL: &str = "https://chatgpt.com/backend-api/transcribe";

/// This route is Codex Desktop's dictation backend, so the request announces
/// the same caller that uses it. A borrowed identity that does not match the
/// traffic it claims to be is what the edge quietly rejects.
const TRANSCRIBE_ORIGINATOR: &str = "Codex Desktop";
/// Only the version's shape is copied. Dictate never reads or requires a local
/// Codex install, so this is not detected from disk.
const TRANSCRIBE_AGENT_VERSION: &str = "26.707.72221";

fn transcribe_user_agent() -> String {
    let os = match std::env::consts::OS {
        "macos" => "Mac OS",
        "windows" => "Windows",
        _ => "Linux",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => std::env::consts::ARCH,
    };
    format!("{TRANSCRIBE_ORIGINATOR}/{TRANSCRIBE_AGENT_VERSION} ({os}; {arch})")
}

/// Keeps a spawned console program from opening a console window.
///
/// The implementation lives in `console` because the probe binary includes
/// modules of its own and cannot reach into this one. This re-export is kept so
/// every existing `engine::hide_console` call site keeps working.
pub use crate::console::hide_console;

/// What the global keyboard listener reports to the app.
pub enum HotkeySignal {
    /// The dictation shortcut was pressed.
    Toggle,
    /// Escape was pressed while a recording was in progress.
    Cancel,
}

// --------------------------------------------------------------------------- //
// microphone permission (macOS TCC, asked for through AVFoundation)
// --------------------------------------------------------------------------- //

/// The operating system this build runs on, as the UI spells it.
pub const PLATFORM: &str = std::env::consts::OS;

/// The app the user is dictating into, and the site when that app is a browser.
#[derive(Clone, Debug)]
pub struct FrontApp {
    /// Bundle identifier on macOS, program name on Windows.
    pub id: String,
    pub name: String,
    /// A short name for the page, e.g. `discord`. Only set for a browser, and
    /// only ever derived from the window title: no URL, no address bar, no page
    /// content. It is what lets a rule say "Discord, whichever browser".
    pub site: Option<String>,
}

/// Browsers, by the pieces their bundle ids and program names contain.
const BROWSERS: &[&str] = &[
    "safari", "chrome", "chromium", "firefox", "edgemac", "msedge", "brave", "arc", "vivaldi",
    "opera", "orion", "zen",
];

fn is_browser(id: &str) -> bool {
    let id = id.to_lowercase();
    BROWSERS.iter().any(|browser| id.contains(browser))
}

/// The site a window title names.
///
/// Browsers put the page in the title and their own name at the end, so
/// "#general | Discord - Mozilla Firefox" is the Discord page. A domain, when
/// the title carries one, is the strongest identity available; otherwise the
/// page's own name is used. This is the only place a window title is read, and
/// only a short token survives it: the title itself is neither stored nor sent.
fn site_from_title(title: &str) -> Option<String> {
    let mut parts: Vec<&str> = title.split(" - ").map(str::trim).collect();
    while parts.len() > 1 && is_browser(parts.last()?.trim()) {
        parts.pop();
    }
    if let Some(domain) = parts.iter().filter_map(|part| find_domain(part)).next() {
        return Some(domain);
    }
    // Without a domain, the page's own name is the single-word piece of the
    // title nearest the end: "Inbox (3) - someone@gmail.com - Gmail" is Gmail,
    // and "GitHub - nub/dictate: app" is GitHub. A "#channel | Site" title
    // names the site after the pipe, as Slack's does.
    for part in parts.iter().rev() {
        let candidate = part.rsplit('|').next().unwrap_or(part).trim();
        if candidate.is_empty()
            || candidate.contains(|c: char| c.is_whitespace() || c == '/' || c == ':')
        {
            continue;
        }
        let token = token_of(candidate);
        if !token.is_empty() {
            return Some(token);
        }
    }
    let first = parts.first().copied().unwrap_or(title);
    let token = token_of(first.trim());
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn token_of(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// The first domain-shaped word in `text`, lowercased and without a path.
fn find_domain(text: &str) -> Option<String> {
    for word in text.split(|c: char| c.is_whitespace() || c == '/' || c == '(' || c == ')') {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '-');
        // An email address is not a site, and one in a title is usually the
        // account's own address. Nothing that looks like one leaves the app.
        if word.contains('@') {
            continue;
        }
        let lower = word.to_lowercase();
        let mut labels = lower.split('.');
        let Some(first) = labels.next() else { continue };
        if first.len() < 2 {
            continue;
        }
        let rest: Vec<&str> = labels.collect();
        let Some(tld) = rest.last() else { continue };
        if rest.iter().any(|label| {
            label.len() < 2 || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        }) {
            continue;
        }
        if tld.chars().all(|c| c.is_ascii_alphabetic()) && (2..=24).contains(&tld.len()) {
            return Some(lower);
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub fn front_app() -> Option<FrontApp> {
    use objc2_app_kit::NSWorkspace;
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    let id = app.bundleIdentifier()?.to_string();
    let name = app
        .localizedName()
        .map(|name| name.to_string())
        .unwrap_or_else(|| id.clone());
    // Dictate's own window is never a dictation target.
    if id == "dev.dictate.mic" {
        return None;
    }
    let site = if is_browser(&id) {
        window_title(app.processIdentifier()).and_then(|title| site_from_title(&title))
    } else {
        None
    };
    Some(FrontApp { id, name, site })
}

/// The window's text, or an empty string when it has none.
#[cfg(target_os = "windows")]
fn window_text(window: windows_sys::Win32::Foundation::HWND) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut buffer = [0u16; 512];
    let length = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

/// The program behind a window, as (id, name).
///
/// The program name is the stable identity here, as the bundle id is on macOS:
/// a display name would follow the language of the install.
#[cfg(target_os = "windows")]
fn program_of_window(window: windows_sys::Win32::Foundation::HWND) -> Option<(String, String)> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if pid == 0 {
        return None;
    }
    let mut program = String::new();
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if !process.is_null() {
        let mut path = [0u16; 1024];
        let mut size = path.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut size) } != 0 {
            program = String::from_utf16_lossy(&path[..size as usize]);
        }
        unsafe { CloseHandle(process) };
    }
    let file = Path::new(&program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())?;
    let name = file.strip_suffix(".exe").unwrap_or(&file).to_string();
    if name.is_empty() {
        return None;
    }
    Some((name.to_lowercase(), name))
}

#[cfg(target_os = "windows")]
pub fn front_app() -> Option<FrontApp> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let title = window_text(window);
    let (id, name) = program_of_window(window)?;
    // Dictate's own window is never a dictation target.
    if id == "dictate" {
        return None;
    }
    let site = if is_browser(&id) {
        site_from_title(&title)
    } else {
        None
    };
    Some(FrontApp { id, name, site })
}

#[cfg(target_os = "windows")]
pub fn running_apps() -> Vec<FrontApp> {
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, IsWindowVisible};

    // EnumWindows hands back a callback, not a collection, so the results are
    // collected in a static. Only one call runs at a time: the page asks once.
    static FOUND: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    // The callback's result is the C BOOL the Windows headers use, so 1 keeps
    // enumeration going and 0 would stop it.
    unsafe extern "system" fn visit(window: HWND, _param: LPARAM) -> i32 {
        if IsWindowVisible(window) == 0 || window_text(window).is_empty() {
            return 1;
        }
        if let Some(found) = program_of_window(window) {
            if found.0 != "dictate" {
                if let Ok(mut list) = FOUND.lock() {
                    list.push(found);
                }
            }
        }
        1
    }

    if let Ok(mut list) = FOUND.lock() {
        list.clear();
    }
    unsafe { EnumWindows(Some(visit), 0) };
    let mut apps: Vec<FrontApp> = FOUND
        .lock()
        .map(|list| {
            list.iter()
                .map(|(id, name)| FrontApp {
                    id: id.clone(),
                    name: name.clone(),
                    site: None,
                })
                .collect()
        })
        .unwrap_or_default();
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps.dedup_by(|a, b| a.id == b.id);
    apps
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn front_app() -> Option<FrontApp> {
    None
}

/// The apps with a user interface that are running now.
///
/// The page offers this list because "the app I am in" is ambiguous: by the time
/// someone goes looking for the button, Dictate's own window is what the system
/// says is in front. Picking from what is running is both accurate and faster
/// than typing a bundle identifier.
#[cfg(target_os = "macos")]
pub fn running_apps() -> Vec<FrontApp> {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
    let workspace = NSWorkspace::sharedWorkspace();
    let mut apps: Vec<FrontApp> = Vec::new();
    for app in workspace.runningApplications().iter() {
        // Background agents have no window to dictate into.
        if app.isTerminated() || app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let Some(id) = app.bundleIdentifier().map(|id| id.to_string()) else {
            continue;
        };
        if id == "dev.dictate.mic" {
            continue;
        }
        let name = app
            .localizedName()
            .map(|name| name.to_string())
            .unwrap_or_else(|| id.clone());
        apps.push(FrontApp {
            id,
            name,
            site: None,
        });
    }
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps.dedup_by(|a, b| a.id == b.id);
    apps
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn running_apps() -> Vec<FrontApp> {
    Vec::new()
}

/// Reading the focused window's title on macOS.
///
/// The accessibility API is used because it asks for no new permission: the
/// grant pasting already needs is the same one. Without it every call fails
/// quietly and the app falls back to the app-only rule.
#[cfg(target_os = "macos")]
mod ax {
    use std::ffi::CString;

    type CFTypeRef = *const std::ffi::c_void;

    /// kCFStringEncodingUTF8.
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            allocator: CFTypeRef,
            bytes: *const i8,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFStringGetLength(string: CFTypeRef) -> isize;
        fn CFStringGetCString(
            string: CFTypeRef,
            buffer: *mut i8,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: CFTypeRef);
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
        fn AXUIElementCopyAttributeValue(
            element: CFTypeRef,
            attribute: CFTypeRef,
            value: *mut CFTypeRef,
        ) -> i32;
    }

    /// The title of the focused window of `pid`, or nothing when the app has no
    /// window, has no title, or the accessibility grant is missing.
    pub fn focused_title(pid: i32) -> Option<String> {
        unsafe {
            let application = AXUIElementCreateApplication(pid);
            if application.is_null() {
                return None;
            }
            let window = attribute(application, "AXFocusedWindow");
            CFRelease(application);
            let window = window?;
            let title = attribute(window, "AXTitle");
            CFRelease(window);
            let title = title?;
            let text = text_of(title);
            CFRelease(title);
            text
        }
    }

    unsafe fn attribute(element: CFTypeRef, name: &str) -> Option<CFTypeRef> {
        let name = CString::new(name).ok()?;
        let key = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), UTF8);
        if key.is_null() {
            return None;
        }
        let mut value: CFTypeRef = std::ptr::null();
        let status = AXUIElementCopyAttributeValue(element, key, &mut value);
        CFRelease(key);
        if status == 0 && !value.is_null() {
            Some(value)
        } else {
            None
        }
    }

    unsafe fn text_of(value: CFTypeRef) -> Option<String> {
        let length = CFStringGetLength(value);
        if length <= 0 {
            return None;
        }
        let mut buffer = vec![0i8; length as usize * 4 + 1];
        let size = buffer.len() as isize;
        if CFStringGetCString(value, buffer.as_mut_ptr(), size, UTF8) == 0 {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned())
    }
}

#[cfg(target_os = "macos")]
fn window_title(pid: i32) -> Option<String> {
    ax::focused_title(pid)
}

#[cfg(test)]
mod site_tests {
    use super::site_from_title;

    /// The browser's own name is the tail of the title, and the page is what is
    /// left: that is the whole job here.
    #[test]
    fn a_page_is_named_from_the_title_without_the_browser() {
        let cases = [
            ("#general | Discord - Mozilla Firefox", Some("discord")),
            // A "#channel | Site" title names the site last, as Slack's does.
            ("general | Slack - Google Chrome", Some("slack")),
            ("Discord | Slack - Google Chrome", Some("slack")),
            ("GitHub - nub/dictate: app - Google Chrome", Some("github")),
            // Gmail's title carries the account's address. It must never become
            // the site, or the domain, that gets sent with a dictation.
            ("Inbox (3) - someone@gmail.com - Gmail", Some("gmail")),
            ("", None),
        ];
        for (title, expected) in cases {
            let got = site_from_title(title);
            assert_eq!(got.as_deref(), expected, "title: {title:?}");
        }
    }

    #[test]
    fn version_numbers_are_not_mistaken_for_domains() {
        // "1.5" and "v2.0" look like hostnames to a loose matcher, and would be
        // sent as the site.
        assert_eq!(site_from_title("Release notes 1.5 - Google Chrome").as_deref(), Some("releasenotes15"));
    }
}

/// Where to send the user when a grant is missing. Only macOS has a per-app
/// list to visit; Windows and Linux just have a microphone switch.
pub const ACCESSIBILITY_HINT: &str = if cfg!(target_os = "macos") {
    " Allow Dictate in System Settings › Privacy & Security › Accessibility."
} else {
    ""
};

const MIC_HINT: &str = if cfg!(target_os = "macos") {
    " Allow Dictate in System Settings › Privacy & Security › Microphone."
} else {
    " Check the microphone in your system privacy settings."
};

/// Where microphone access is turned back on, as a sentence of its own, so the
/// messages that report a blocked microphone name a pane that exists on the
/// platform they are shown on.
pub const MIC_SETTINGS_HINT: &str = if cfg!(target_os = "macos") {
    "Allow Dictate in System Settings > Privacy & Security > Microphone"
} else if cfg!(target_os = "windows") {
    "Turn on microphone access in Settings > Privacy & security > Microphone"
} else {
    "Turn on microphone access in your system's privacy settings"
};

/// "granted", "denied", "not_determined" or "unknown".
pub fn mic_permission() -> &'static str {
    use handy_recorder::Permission;
    match handy_recorder::permission_status() {
        Permission::Granted => "granted",
        Permission::Denied => "denied",
        Permission::NotDetermined => "not_determined",
        _ => "unknown",
    }
}

/// Asks macOS for microphone access. This is what makes the system show the
/// "would like to access the microphone" prompt; opening a stream alone does not.
#[cfg(target_os = "macos")]
pub fn request_mic_access(result: mpsc::Sender<bool>) {
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        let _ = result.send(false);
        return;
    };
    let block = block2::StackBlock::new(move |granted: objc2::runtime::Bool| {
        let _ = result.send(granted.as_bool());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &block);
    }
}

/// Windows and Linux have no per-app microphone prompt, so there is nothing to
/// ask for: report the status the recorder already sees.
#[cfg(not(target_os = "macos"))]
pub fn request_mic_access(result: mpsc::Sender<bool>) {
    let _ = result.send(mic_permission() != "denied");
}

// --------------------------------------------------------------------------- //
// audio capture (handy-recorder for the microphone, WAV via hound)
// --------------------------------------------------------------------------- //

/// How often the input level is reported to the UI while recording. Fast enough
/// for the meter to track speech, slow enough not to flood the webview.
const LEVEL_INTERVAL: Duration = Duration::from_millis(40);

/// Longest a single recording may run before the app stops it on its own. The
/// upload is one WAV of 16 kHz mono audio, so this also bounds the file size.
const MAX_RECORDING: Duration = Duration::from_secs(300);

/// A recording that has finished.
pub struct Finished {
    pub path: PathBuf,
    pub seconds: f64,
    pub peak: f32,
    /// True when `MAX_RECORDING` ended the recording instead of the user, so the
    /// app can tell them why it stopped rather than just cutting them off.
    pub hit_limit: bool,
}

/// A recording in progress. The microphone is only open while this exists.
pub struct Recording {
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<Result<Finished, String>>,
    /// The device the audio is coming from, for the log.
    device: String,
}

impl Recording {
    /// `on_level` receives the loudest sample of each interval while the
    /// microphone is open, from 0 to 1. It runs on the audio delivery thread, so
    /// it must return quickly; the app uses it to drive the recording meter.
    pub fn start(on_level: impl FnMut(f32) + Send + 'static) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<String, String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<Result<Finished, String>>();

        std::thread::spawn(move || {
            let result = capture(ready_tx, stop_rx, on_level);
            let _ = done_tx.send(result);
        });

        match ready_rx.recv_timeout(Duration::from_secs(6)) {
            Ok(Ok(device)) => Ok(Recording {
                stop: stop_tx,
                done: done_rx,
                device,
            }),
            Ok(Err(message)) => Err(message),
            Err(_) => Err(format!("The microphone did not start.{MIC_HINT}")),
        }
    }

    /// What was opened, e.g. "MacBook Air Microphone".
    pub fn device(&self) -> &str {
        &self.device
    }

    /// Stops capture and hands back the finished WAV, its length and loudest peak.
    pub fn finish(self) -> Result<Finished, String> {
        let _ = self.stop.send(());
        self.done
            .recv_timeout(Duration::from_secs(8))
            .map_err(|_| "Could not finish the recording.".to_string())?
    }

    /// Stops capture and throws the audio away without transcribing it.
    pub fn cancel(self) {
        let _ = self.stop.send(());
        if let Ok(Ok(finished)) = self.done.recv_timeout(Duration::from_secs(8)) {
            let _ = std::fs::remove_file(finished.path);
        }
    }
}

/// Collects the recording like `CollectingSink` does, and reports a running
/// level on the side. `CollectingSink` only hands its samples over at stop, so
/// a meter built on it could not move until the recording was already done.
struct LevelSink<F> {
    samples: Vec<f32>,
    on_level: F,
    last_emit: Instant,
    /// Loudest sample since the last report, reset on each one.
    peak: f32,
}

impl<F: FnMut(f32)> LevelSink<F> {
    fn new(on_level: F) -> Self {
        LevelSink {
            samples: Vec::new(),
            on_level,
            last_emit: Instant::now(),
            peak: 0.0,
        }
    }

    fn into_samples(self) -> Vec<f32> {
        self.samples
    }
}

impl<F: FnMut(f32) + Send + 'static> Sink for LevelSink<F> {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        let channels = chunk.channels.max(1) as usize;
        // The final chunk of a recording is zero padded; only real audio counts.
        let frames = chunk.valid_frames.min(chunk.samples.len() / channels);
        let real = &chunk.samples[..frames * channels];
        self.samples.extend_from_slice(real);

        // Peak rather than average: a muted or dead microphone has to read as a
        // flat meter, not a quiet one.
        self.peak = real.iter().fold(self.peak, |acc, v| acc.max(v.abs()));
        if self.last_emit.elapsed() >= LEVEL_INTERVAL {
            let level = std::mem::take(&mut self.peak);
            (self.on_level)(level);
            self.last_emit = Instant::now();
        }
    }
}

fn capture(
    ready: mpsc::Sender<Result<String, String>>,
    stop: mpsc::Receiver<()>,
    on_level: impl FnMut(f32) + Send + 'static,
) -> Result<Finished, String> {
    let mut config = RecorderConfig::speech();
    config.device = preferred_device();
    let recorder = Recorder::open_with_failure_handler(config, |error| {
        eprintln!("audio: recorder failed: {error}");
    })
    .map_err(|error| mic_error_message(&error))?;

    let info = recorder.info().clone();
    let sink = LevelSink::new(on_level);
    recorder
        .start(sink)
        .map_err(|_| "The microphone could not start recording.".to_string())?;
    let _ = ready.send(Ok(info.device.name.clone()));

    // Wait for the hotkey to toggle off. If nothing arrives within
    // MAX_RECORDING the app stops the recording itself, and the caller needs to
    // know that happened so it can say so.
    let hit_limit = stop.recv_timeout(MAX_RECORDING).is_err();

    let stopped = recorder
        .stop()
        .map_err(|error| format!("The recording stopped early: {error}"))?;
    if !stopped.is_complete() {
        eprintln!("audio: incomplete recording: {:?}", stopped.end_reason);
    }
    let samples = stopped.sink.into_samples();

    let sample_rate = info.format.sample_rate.max(1);
    let channels = info.format.channels.max(1) as usize;
    let frames = samples.len() / channels;
    let seconds = frames as f64 / sample_rate as f64;
    let peak = samples.iter().fold(0.0f32, |acc, value| acc.max(value.abs()));

    let path = std::env::temp_dir().join(format!("dictate-{}.wav", nanos()));
    let wav_spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, wav_spec)
        .map_err(|e| format!("Could not write the recording: {e}"))?;
    for frame in samples.chunks(channels) {
        let mut sum = 0.0f32;
        for value in frame {
            sum += *value;
        }
        let value = (sum / channels as f32).clamp(-1.0, 1.0);
        writer
            .write_sample((value * i16::MAX as f32) as i16)
            .map_err(|e| format!("Could not write the recording: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("Could not write the recording: {e}"))?;

    Ok(Finished {
        path,
        seconds,
        peak,
        hit_limit,
    })
}

/// True for the microphone of a Bluetooth headset, which macOS names after the
/// headset and lists under its Bluetooth address (`80-95-3A-F2-3B-D2:input`) —
/// the same UID shape handy-recorder uses to find a headset's output device.
fn is_bluetooth(device: &InputDevice) -> bool {
    looks_bluetooth(&device.id, &device.name)
}

fn looks_bluetooth(uid: &str, name: &str) -> bool {
    if let Some((address, side)) = uid.rsplit_once(':') {
        let looks_like_address = address.len() == 17
            && address.split('-').count() == 6
            && address
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-');
        if looks_like_address && matches!(side, "input" | "output") {
            return true;
        }
    }
    let name = name.to_lowercase();
    ["airpods", "beats", "bluetooth"]
        .iter()
        .any(|word| name.contains(word))
}

/// True for the microphone built into the machine. CoreAudio gives it the same
/// UID on every Mac; the names cover anything that reports it differently, and
/// hardware names are not localized.
fn is_built_in(device: &InputDevice) -> bool {
    looks_built_in(&device.id, &device.name)
}

fn looks_built_in(uid: &str, name: &str) -> bool {
    if uid == "BuiltInMicrophoneDevice" {
        return true;
    }
    let name = name.to_lowercase();
    [
        "built-in",
        "builtin",
        "internal",
        "macbook",
        "imac",
        "mac mini",
        "mac studio",
        "mac pro",
    ]
    .iter()
    .any(|word| name.contains(word))
}

/// The device to record from, or `None` to take the system default.
///
/// A Bluetooth headset's microphone costs the first seconds of every dictation:
/// the link only starts streaming once macOS has moved the headset out of its
/// music profile, which takes seconds, so whatever was said in the meantime is
/// simply not captured — and the audio that does arrive is narrowband. The
/// machine's own microphone is always awake and always the better capture, so
/// it is used whenever the system default has become a headset. Any other
/// input the user picked is left alone, and a Mac without a built-in
/// microphone keeps the default rather than recording nothing.
fn preferred_device() -> Option<String> {
    let devices = list_input_devices().ok()?;
    let default = devices.iter().find(|device| device.is_default)?;
    if !is_bluetooth(default) {
        return None;
    }
    devices
        .iter()
        .find(|device| is_built_in(device))
        .map(|device| device.id.clone())
}

/// Turns handy-recorder errors into something a person can act on.
fn mic_error_message(error: &handy_recorder::Error) -> String {
    use handy_recorder::ErrorKind;
    match error.kind() {
        ErrorKind::PermissionDenied => {
            format!("Dictate is not allowed to use the microphone. {MIC_SETTINGS_HINT}, then try again.")
        }
        ErrorKind::DeviceUnavailable => "No microphone was found.".to_string(),
        ErrorKind::DeviceBusy => "Another app is using the microphone.".to_string(),
        _ => format!("The microphone could not be opened: {error}"),
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// --------------------------------------------------------------------------- //
// transcription
// --------------------------------------------------------------------------- //

/// Why a transcription request failed.
pub struct TranscribeFailure {
    /// The HTTP status, or 0 when the request never produced a response.
    pub status: u16,
    pub message: String,
}

impl TranscribeFailure {
    /// The account was refused, not the audio: only a fresh token or a new
    /// sign-in can help, so neither a retry nor a second upload is worth it.
    ///
    /// Only 401 counts. A 403 on this endpoint can also mean the request was
    /// blocked, and telling someone their session expired when it was blocked
    /// sends them to fix something that was never broken.
    pub fn is_auth(&self) -> bool {
        self.status == 401
    }

    /// True for the failures that a second identical request can plausibly fix.
    fn worth_retrying(&self) -> bool {
        self.status == 0 || self.status >= 500
    }
}

pub fn transcribe(path: &Path, credentials: &Credentials) -> Result<String, TranscribeFailure> {
    // One retry smooths over flaky Wi-Fi, VPN switches and the odd edge block.
    // A refusal is not retried: the second request would be refused the same way.
    match transcribe_once(path, credentials) {
        Ok(text) => Ok(text),
        Err(error) if !error.worth_retrying() => Err(error),
        Err(_) => {
            std::thread::sleep(Duration::from_millis(700));
            transcribe_once(path, credentials)
        }
    }
}

/// Splits curl's `-w "\n%{http_code}"` output into the body and the status.
///
/// A status of 0 means the request produced no response at all, which is a
/// different problem from any answer the server could give.
fn split_status(stdout: &str) -> (String, u16) {
    match stdout.rsplit_once('\n') {
        Some((body, code)) => (body.to_string(), code.trim().parse::<u16>().unwrap_or(0)),
        None => (stdout.to_string(), 0),
    }
}

fn transcribe_once(
    path: &Path,
    credentials: &Credentials,
) -> Result<String, TranscribeFailure> {
    let mut config = format!(
        "header = \"Authorization: Bearer {}\"\n",
        credentials.access_token
    );
    // Only send the account header when there is an account id to send: an
    // empty value is a different header than a missing one, and the edge can
    // reject it.
    if let Some(account) = credentials
        .account_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        config.push_str(&format!("header = \"chatgpt-account-id: {account}\"\n"));
    }
    config.push_str(&format!(
        "header = \"originator: {}\"\nheader = \"User-Agent: {}\"\nheader = \"Accept: application/json\"\n",
        TRANSCRIBE_ORIGINATOR,
        transcribe_user_agent(),
    ));
    let upload = format!("file=@{};type=audio/wav", path.display());

    // HTTP/1.1 matters: the edge blocks this endpoint when it arrives over HTTP/2.
    let mut curl = Command::new("curl");
    curl.args([
        "-sS",
        "--max-time",
        "180",
        "--http1.1",
        "-X",
        "POST",
        TRANSCRIBE_URL,
        "-F",
        &upload,
        "-K",
        "-",
        // The status goes after the body, so a refusal can be told apart from a
        // transport failure instead of being guessed at from the prose.
        "-w",
        "\\n%{http_code}",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    hide_console(&mut curl);
    let mut child = curl.spawn().map_err(|e| TranscribeFailure {
        status: 0,
        message: format!("Could not start the upload: {e}"),
    })?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|e| TranscribeFailure {
                status: 0,
                message: format!("Could not send the request: {e}"),
            })?;
    }

    let output = child.wait_with_output().map_err(|e| TranscribeFailure {
        status: 0,
        message: format!("transcription failed: {e}"),
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let (body, status) = split_status(&stdout);

    // Status 0 means curl never got a response at all. Its stderr says why, and
    // that is a connection problem rather than a rejected sign-in.
    if status == 0 {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(TranscribeFailure {
            status: 0,
            message: if detail.is_empty() {
                "Could not reach ChatGPT. Check your connection and try again.".to_string()
            } else {
                format!("Could not reach ChatGPT: {detail}")
            },
        });
    }

    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => {
            let snippet: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let snippet: String = snippet.chars().take(160).collect();
            return Err(TranscribeFailure {
                status,
                message: format!("ChatGPT could not take that audio ({status}): {snippet}"),
            });
        }
    };

    if let Some(transcript) = value.get("text").and_then(|v| v.as_str()) {
        return Ok(transcript.trim().to_string());
    }
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| value.get("detail").and_then(|d| d.as_str()))
        .unwrap_or_else(|| body.trim());
    Err(TranscribeFailure {
        status,
        message: format!("ChatGPT could not transcribe that ({status}): {message}"),
    })
}

// --------------------------------------------------------------------------- //
// clipboard + paste
// --------------------------------------------------------------------------- //

fn clipboard() -> Option<Clipboard> {
    Clipboard::new().ok()
}

pub fn copy_to_clipboard(text: &str) -> bool {
    let Some(mut board) = clipboard() else {
        return false;
    };
    board.set_text(text).is_ok()
}

/// What was on the clipboard before the transcript replaced it.
pub struct ClipboardBackup {
    text: Option<String>,
    html: Option<String>,
    image: Option<arboard::ImageData<'static>>,
}

/// Reads the clipboard before a transcript replaces it. Plain text, rich text
/// and images are covered; a clipboard holding only files yields nothing.
pub fn backup_clipboard() -> Option<ClipboardBackup> {
    let mut board = clipboard()?;
    let text = board.get_text().ok().filter(|text| !text.is_empty());
    let html = board.get().html().ok().filter(|html| !html.is_empty());
    let image = board.get_image().ok();
    if text.is_none() && html.is_none() && image.is_none() {
        None
    } else {
        Some(ClipboardBackup { text, html, image })
    }
}

/// True while the transcript is still what sits on the clipboard. Used to skip
/// the restore when the user copied something else in the meantime.
pub fn clipboard_holds(text: &str) -> bool {
    clipboard()
        .and_then(|mut board| board.get_text().ok())
        .is_some_and(|current| current == text)
}

/// Puts the snapshot back. Only call this while the copied transcript is still
/// what is on the clipboard, so a copy made in the meantime is never lost.
pub fn restore_clipboard(backup: ClipboardBackup) {
    let Some(mut board) = clipboard() else {
        return;
    };
    let _ = board.clear();
    match backup.image {
        // A copied picture is the closest thing to what the user had.
        Some(image) => {
            let _ = board.set_image(image);
        }
        None => {
            if let Some(html) = backup.html {
                let _ = board.set_html(html, backup.text);
            } else if let Some(text) = backup.text {
                let _ = board.set_text(text);
            }
        }
    }
}

/// Sends the paste shortcut. False means the keystroke never left the app, so
/// the transcript is on the clipboard but the frontmost app has not seen it.
pub fn paste_at_cursor() -> bool {
    std::thread::sleep(Duration::from_millis(120));
    let Ok(mut enigo) = Enigo::new(&EnigoSettings::default()) else {
        return false;
    };
    // macOS pastes with Command, Windows and Linux with Control.
    #[cfg(target_os = "macos")]
    let modifier = Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let modifier = Key::Control;
    // Every backend needs the physical V key, never a character: mapping a
    // character asks Text Services for the layout on macOS, and that call
    // asserts it runs on the main thread — this runs on the recording thread
    // and traps the process. 0x09 is the macOS virtual keycode for V.
    #[cfg(target_os = "macos")]
    let letter = Key::Other(0x09);
    #[cfg(target_os = "windows")]
    let letter = Key::V;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let letter = Key::Unicode('v');
    let pressed = enigo.key(modifier, Direction::Press).is_ok();
    let pasted = enigo.key(letter, Direction::Click).is_ok();
    let released = enigo.key(modifier, Direction::Release).is_ok();
    pressed && pasted && released
}

// --------------------------------------------------------------------------- //
// permissions
// --------------------------------------------------------------------------- //

pub fn accessibility_trusted() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_accessibility_client::accessibility::application_is_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Asks macOS to show the "would like to control this computer" prompt and add
/// the app to the Accessibility pane. Without this call the app never appears
/// in that list, so there is nothing for the user to switch on — which is the
/// same path Handy takes.
pub fn prompt_accessibility() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_accessibility_client::accessibility::application_is_trusted_with_prompt()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}


// --------------------------------------------------------------------------- //
// hotkey (handy-keys: system-wide, swallows the trigger, modifier-only support)
// --------------------------------------------------------------------------- //

// The name of each shortcut modifier is the platform's, not macOS's: the key a
// Mac calls Option is Alt on Windows and Linux, and ⌘/⌥ mean nothing outside a
// Mac. The app spells the same keys out the same way in the UI, so the two
// labels never disagree.
const CMD_NAME: &str = if cfg!(target_os = "macos") {
    "Command"
} else if cfg!(target_os = "windows") {
    "Win"
} else {
    "Super"
};
const CTRL_NAME: &str = if cfg!(target_os = "macos") { "Control" } else { "Ctrl" };
const ALT_NAME: &str = if cfg!(target_os = "macos") { "Option" } else { "Alt" };
const SHIFT_NAME: &str = "Shift";

pub fn pretty(name: &str) -> String {
    if name == "fn" {
        return "Fn".to_string();
    }

    // A bare modifier names one physical key, so which side it is survives and
    // only the key's own name follows the platform.
    let (side, key) = if let Some(key) = name.strip_prefix("left_") {
        ("Left", key)
    } else if let Some(key) = name.strip_prefix("right_") {
        ("Right", key)
    } else {
        ("", name)
    };
    let bare = match key {
        "option" => Some(ALT_NAME),
        "ctrl" => Some(CTRL_NAME),
        "cmd" => Some(CMD_NAME),
        "shift" => Some(SHIFT_NAME),
        _ => None,
    };
    if !side.is_empty() {
        if let Some(label) = bare {
            return format!("{side} {label}");
        }
    }

    name.split('+')
        .map(|part| match part {
            "cmd" => CMD_NAME.to_string(),
            "ctrl" => CTRL_NAME.to_string(),
            "alt" => ALT_NAME.to_string(),
            "shift" => SHIFT_NAME.to_string(),
            "space" => "Space".to_string(),
            "up" => "Up".to_string(),
            "down" => "Down".to_string(),
            "left" => "Left".to_string(),
            "right" => "Right".to_string(),
            other => other.to_uppercase(),
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

fn handy_hotkey(name: &str) -> Result<Hotkey, String> {
    let normalized = match name {
        "right_option" => "option_right".to_string(),
        "left_option" => "option_left".to_string(),
        "right_shift" => "shift_right".to_string(),
        "left_shift" => "shift_left".to_string(),
        "right_ctrl" => "ctrl_right".to_string(),
        "left_ctrl" => "ctrl_left".to_string(),
        "right_cmd" => "command_right".to_string(),
        "left_cmd" => "command_left".to_string(),
        "fn" => "fn".to_string(),
        other => other.to_string(),
    };
    normalized
        .parse::<Hotkey>()
        .map_err(|_| "That shortcut isn't supported. Try a modifier on its own or a modifier plus a key.".to_string())
}

pub fn parse_hotkey(name: &str) -> Result<(), String> {
    handy_hotkey(name).map(|_| ())
}

pub struct HotkeyService {
    manager: Arc<Mutex<HotkeyManager>>,
    cancel_id: Arc<Mutex<Option<HotkeyId>>>,
    /// The dictation shortcut registered right now; `None` while the setup
    /// screen is capturing keys.
    main_id: Arc<Mutex<Option<HotkeyId>>>,
    running: Option<Arc<AtomicBool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HotkeyService {
    pub fn start(hotkey: &str, tx: mpsc::Sender<HotkeySignal>) -> Result<Self, String> {
        let hotkey = handy_hotkey(hotkey)?;
        let manager = HotkeyManager::new_with_blocking()
            .or_else(|_| HotkeyManager::new())
            .map_err(|error| {
                format!("The keyboard listener could not start ({error}).{ACCESSIBILITY_HINT}")
            })?;
        let main_id = manager
            .register(hotkey)
            .map_err(|error| format!("That shortcut could not be registered: {error}"))?;

        // The id of the dictation shortcut changes when the user picks another
        // one, so the reader thread looks it up instead of capturing it.
        let main_id = Arc::new(Mutex::new(Some(main_id)));
        let manager = Arc::new(Mutex::new(manager));
        let cancel_id = Arc::new(Mutex::new(None::<HotkeyId>));
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let thread_manager = manager.clone();
        let thread_cancel = cancel_id.clone();
        let thread_main = main_id.clone();
        let thread = std::thread::spawn(move || {
            while flag.load(Ordering::SeqCst) {
                let event = {
                    let manager = thread_manager.lock().unwrap();
                    manager.try_recv()
                };
                if let Some(event) = event {
                    if event.state == HotkeyState::Pressed {
                        let cancel = *thread_cancel.lock().unwrap();
                        let main = *thread_main.lock().unwrap();
                        if Some(event.id) == cancel {
                            let _ = tx.send(HotkeySignal::Cancel);
                        } else if Some(event.id) == main {
                            let _ = tx.send(HotkeySignal::Toggle);
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(8));
                }
            }
        });

        Ok(HotkeyService {
            manager,
            cancel_id,
            main_id,
            running: Some(running),
            thread: Some(thread),
        })
    }

    /// Points the running listener at a different dictation shortcut.
    ///
    /// The manager and its low-level hook are kept for the whole run and only
    /// the registration is swapped. Rebuilding them instead — which is what
    /// this used to do on every shortcut change — leaves Windows delivering
    /// nothing to the replacement hook, so the setup screen's "press it once
    /// more" test waited fifteen seconds for a key that could never arrive.
    pub fn set_hotkey(&self, hotkey: &str) -> Result<(), String> {
        let hotkey = handy_hotkey(hotkey)?;
        // Lock order everywhere is main_id, then manager.
        let mut main_id = self.main_id.lock().unwrap();
        let manager = self.manager.lock().unwrap();
        if main_id.and_then(|id| manager.get_hotkey(id)) == Some(hotkey) {
            return Ok(());
        }
        let id = manager
            .register(hotkey)
            .map_err(|error| format!("That shortcut could not be registered: {error}"))?;
        // Unregister the old one only once the new one is in place, so a
        // rejected shortcut never leaves the app without a working hotkey.
        if let Some(previous) = main_id.replace(id) {
            let _ = manager.unregister(previous);
        }
        Ok(())
    }

    /// Stops reporting the dictation shortcut without closing the listener.
    ///
    /// The setup screen needs the shortcut to reach the window instead of
    /// toggling a recording, and it used to get that by stopping the whole
    /// service — which is what left the replacement hook deaf.
    pub fn pause(&self) {
        let mut main_id = self.main_id.lock().unwrap();
        if let Some(id) = main_id.take() {
            let _ = self.manager.lock().unwrap().unregister(id);
        }
    }

    pub fn is_registered(&self) -> bool {
        self.main_id.lock().unwrap().is_some()
    }

    /// Listen for Escape while a recording runs. Escape is only registered for
    /// that window, so everywhere else it reaches the frontmost app normally.
    pub fn arm_cancel(&self) -> Result<(), String> {
        let mut guard = self.cancel_id.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let escape: Hotkey = "escape"
            .parse()
            .map_err(|_| "Escape could not be parsed as a shortcut".to_string())?;
        let id = self
            .manager
            .lock()
            .unwrap()
            .register(escape)
            .map_err(|error| format!("Escape could not be registered: {error}"))?;
        *guard = Some(id);
        Ok(())
    }

    pub fn disarm_cancel(&self) {
        let Some(id) = self.cancel_id.lock().unwrap().take() else {
            return;
        };
        let _ = self.manager.lock().unwrap().unregister(id);
    }

    pub fn stop(mut self) {
        self.disarm_cancel();
        if let Some(flag) = self.running.take() {
            flag.store(false, Ordering::SeqCst);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// --------------------------------------------------------------------------- //
// hotkey capture
// --------------------------------------------------------------------------- //

/// Streams bare modifier presses while the setup screen is choosing a shortcut.
///
/// The webview cannot be trusted with these: a bare right Option press never
/// reaches the page as a keydown (left Option and right Command do), so a
/// shortcut set to right Option could never be chosen again. The keyboard tap
/// sees every modifier press, and is the same one the hotkey itself uses.
pub struct HotkeyCapture {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn modifier_id(bits: Modifiers) -> Option<&'static str> {
    match bits {
        Modifiers::OPT_RIGHT => Some("right_option"),
        Modifiers::OPT_LEFT => Some("left_option"),
        Modifiers::SHIFT_RIGHT => Some("right_shift"),
        Modifiers::SHIFT_LEFT => Some("left_shift"),
        Modifiers::CTRL_RIGHT => Some("right_ctrl"),
        Modifiers::CTRL_LEFT => Some("left_ctrl"),
        Modifiers::CMD_RIGHT => Some("right_cmd"),
        Modifiers::CMD_LEFT => Some("left_cmd"),
        Modifiers::FN => Some("fn"),
        _ => None,
    }
}

impl HotkeyCapture {
    /// Starts a non-blocking listener that sends the id of every pressed
    /// modifier on its own. Keys held with another key are left to the webview,
    /// which sees those normally.
    pub fn start(tx: mpsc::Sender<String>) -> Result<Self, String> {
        let listener = KeyboardListener::new()
            .map_err(|error| format!("The keyboard listener could not start ({error}).{ACCESSIBILITY_HINT}"))?;
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let thread = std::thread::spawn(move || {
            while flag.load(Ordering::SeqCst) {
                let Ok(event) = listener.recv_timeout(Duration::from_millis(60)) else {
                    continue;
                };
                if event.key.is_some() {
                    continue;
                }
                let Some(bits) = event.changed_modifier else {
                    continue;
                };
                if let Some(id) = modifier_id(bits) {
                    let phase = if event.is_key_down { "down" } else { "up" };
                    if tx.send(format!("{id}:{phase}")).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(HotkeyCapture {
            running,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn level_sink_keeps_real_samples_and_reports_peak() {
        let reported = Arc::new(Mutex::new(Vec::new()));
        let sink_levels = reported.clone();
        let mut sink = LevelSink::new(move |level| sink_levels.lock().unwrap().push(level));

        // Two frames of stereo, then padding that must not be kept.
        let stereo = [0.0, 0.5, -0.25, 0.0, 0.0, 0.0];
        sink.process_chunk(AudioChunk {
            samples: &stereo,
            sample_rate: 16_000,
            channels: 2,
            valid_frames: 2,
        });
        assert_eq!(sink.samples, vec![0.0, 0.5, -0.25, 0.0]);

        // Backdate the clock instead of sleeping, so the report is due now.
        sink.last_emit = Instant::now() - LEVEL_INTERVAL;
        sink.process_chunk(AudioChunk {
            samples: &[0.1, 0.0],
            sample_rate: 16_000,
            channels: 1,
            valid_frames: 2,
        });
        // The first report carries the earlier peak, not just this chunk.
        assert_eq!(*reported.lock().unwrap(), vec![0.5]);

        sink.last_emit = Instant::now() - LEVEL_INTERVAL;
        sink.process_chunk(AudioChunk {
            samples: &[0.9, 0.0],
            sample_rate: 16_000,
            channels: 1,
            valid_frames: 2,
        });
        assert_eq!(*reported.lock().unwrap(), vec![0.5, 0.9]);
        assert_eq!(sink.samples.len(), 8);
    }

    #[test]
    fn the_status_is_taken_from_the_end_of_the_response() {
        assert_eq!(
            split_status("{\"text\":\"hi\"}\n200"),
            ("{\"text\":\"hi\"}".to_string(), 200)
        );
        // A pretty-printed body keeps its own newlines.
        assert_eq!(
            split_status("{\n  \"detail\": \"Unauthorized\"\n}\n401"),
            ("{\n  \"detail\": \"Unauthorized\"\n}".to_string(), 401)
        );
        // Nothing on stdout means no response was received, not a status of 0
        // from the server.
        assert_eq!(split_status(""), (String::new(), 0));
        assert_eq!(split_status("garbage"), ("garbage".to_string(), 0));
    }

    #[test]
    fn shortcut_labels_use_the_platforms_key_names() {
        // A bare modifier keeps its side; only the key's own name follows the
        // platform, so the same physical key reads correctly everywhere.
        assert_eq!(
            pretty("right_option"),
            if cfg!(target_os = "macos") { "Right Option" } else { "Right Alt" }
        );
        let cmd = if cfg!(target_os = "macos") {
            "Left Command"
        } else if cfg!(target_os = "windows") {
            "Left Win"
        } else {
            "Left Super"
        };
        assert_eq!(pretty("left_cmd"), cmd);
        assert_eq!(pretty("fn"), "Fn");

        // A combination is spelled out, and a macOS-only glyph must not survive
        // into a label on any platform.
        assert_eq!(
            pretty("ctrl+shift+t"),
            if cfg!(target_os = "macos") {
                "Control + Shift + T"
            } else {
                "Ctrl + Shift + T"
            }
        );
        let everything = pretty("cmd+alt+ctrl+shift+up");
        for glyph in ['⌘', '⌥', '⇧', '⌃', '↑', '↓', '←', '→'] {
            assert!(!everything.contains(glyph), "{everything} still contains {glyph}");
        }
    }

    #[test]
    fn a_bluetooth_headset_is_never_the_microphone() {
        // What macOS reports for a paired headset, whatever it is called.
        assert!(looks_bluetooth(
            "80-95-3A-F2-3B-D2:input",
            "Zachary's AirPods Pro"
        ));
        // A headset whose UID came out another shape is still named as one.
        assert!(looks_bluetooth("some-other-uid", "AirPods Pro"));
        assert!(looks_bluetooth("some-other-uid", "Beats Fit Pro"));
        // The machine's own microphone, and anything wired, is left alone.
        assert!(!looks_bluetooth(
            "BuiltInMicrophoneDevice",
            "MacBook Air Microphone"
        ));
        assert!(!looks_bluetooth(
            "AppleUSBAudioEngine:Shure:MV7:1",
            "Shure MV7"
        ));
    }

    #[test]
    fn the_machines_own_microphone_is_recognised() {
        assert!(looks_built_in(
            "BuiltInMicrophoneDevice",
            "MacBook Air Microphone"
        ));
        // A UID that differs is still matched by the hardware name, which is
        // not localized and always carries the model.
        assert!(looks_built_in("", "Mac mini Microphone"));
        assert!(!looks_built_in("", "Shure MV7"));
    }

    #[test]
    fn a_refused_token_is_not_retried_and_a_server_error_is() {        let failure = |status| TranscribeFailure {
            status,
            message: String::new(),
        };
        assert!(failure(401).is_auth());
        assert!(!failure(403).is_auth());
        assert!(!failure(401).worth_retrying());
        assert!(!failure(400).worth_retrying() && !failure(400).is_auth());
        assert!(failure(500).worth_retrying());
        assert!(failure(0).worth_retrying() && !failure(0).is_auth());
    }
}

