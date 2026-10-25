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
use handy_recorder::{AudioChunk, Recorder, RecorderConfig, Sink};

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
/// Dictate is a GUI binary and owns no console, so Windows hands every console
/// program it starts a new console window — with curl that is a black rectangle
/// flashing on screen for as long as the request runs. CREATE_NO_WINDOW is the
/// documented way to ask for no console at all. Everywhere else this is a no-op.
pub fn hide_console(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = command;
    }
}

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
}

impl Recording {
    /// `on_level` receives the loudest sample of each interval while the
    /// microphone is open, from 0 to 1. It runs on the audio delivery thread, so
    /// it must return quickly; the app uses it to drive the recording meter.
    pub fn start(mut on_level: impl FnMut(f32) + Send + 'static) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<Result<Finished, String>>();

        std::thread::spawn(move || {
            let result = capture(ready_tx, stop_rx, move |level| on_level(level));
            let _ = done_tx.send(result);
        });

        match ready_rx.recv_timeout(Duration::from_secs(6)) {
            Ok(Ok(())) => Ok(Recording {
                stop: stop_tx,
                done: done_rx,
            }),
            Ok(Err(message)) => Err(message),
            Err(_) => Err(format!("The microphone did not start.{MIC_HINT}")),
        }
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
    ready: mpsc::Sender<Result<(), String>>,
    stop: mpsc::Receiver<()>,
    on_level: impl FnMut(f32) + Send + 'static,
) -> Result<Finished, String> {
    let recorder = Recorder::open_with_failure_handler(RecorderConfig::speech(), |error| {
        eprintln!("audio: recorder failed: {error}");
    })
    .map_err(|error| mic_error_message(&error))?;

    let info = recorder.info().clone();
    let sink = LevelSink::new(on_level);
    recorder
        .start(sink)
        .map_err(|_| "The microphone could not start recording.".to_string())?;
    let _ = ready.send(Ok(()));

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

/// Turns handy-recorder errors into something a person can act on.
fn mic_error_message(error: &handy_recorder::Error) -> String {
    use handy_recorder::ErrorKind;
    match error.kind() {
        ErrorKind::PermissionDenied => {
            "Dictate is not allowed to use the microphone. Open System Settings > Privacy & Security > Microphone, turn Dictate on, then try again.".to_string()
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

pub fn pretty(name: &str) -> String {
    let bare = match name {
        "right_option" => Some("Right Option"),
        "left_option" => Some("Left Option"),
        "right_shift" => Some("Right Shift"),
        "left_shift" => Some("Left Shift"),
        "right_ctrl" => Some("Right Control"),
        "left_ctrl" => Some("Left Control"),
        "right_cmd" => Some("Right Command"),
        "left_cmd" => Some("Left Command"),
        "fn" => Some("Fn"),
        _ => None,
    };
    if let Some(label) = bare {
        return label.to_string();
    }

    let mut out = String::new();
    for part in name.split('+') {
        let token = match part {
            "cmd" => "⌘",
            "ctrl" => "⌃",
            "alt" => "⌥",
            "shift" => "⇧",
            "space" => "Space",
            "up" => "↑",
            "down" => "↓",
            "left" => "←",
            "right" => "→",
            other => {
                out.push_str(&other.to_uppercase());
                continue;
            }
        };
        out.push_str(token);
    }
    out
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
    fn a_refused_token_is_not_retried_and_a_server_error_is() {
        let failure = |status| TranscribeFailure {
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

