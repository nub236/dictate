# Dictate

Press a hotkey, talk, press it again. Your words show up wherever your cursor is.

Dictate transcribes with your existing ChatGPT account. No API key, no per-word credits, no extra subscription. It's a small native app: a tray icon, a global hotkey, and a local settings window.

## How it works

1. Press your shortcut in any app.
2. A pill at the bottom of the screen shows bars moving with your voice. If the meter is flat, the mic isn't hearing you.
3. Press the shortcut again. The audio goes to ChatGPT and the text comes back.
4. Dictate pastes the text at your cursor, then restores your previous clipboard (unless you copied something else in the meantime).

Press **Escape** while recording to discard the audio. Errors show up as a short amber message near the bottom of the screen.

The mic is only open between the two presses.

## Signing in

Click **Continue with ChatGPT** and approve the request in your browser. Dictate catches the redirect on `localhost:1455`, stores the tokens, and refreshes them as needed. **Sign out** deletes them and asks ChatGPT to revoke the refresh token; if a sign-in was still waiting for its browser tab, signing out ends it too.

It requests `openid profile email offline_access` and nothing more.

## Privacy

- Dictate never reads your chats, projects, memory, files, or billing. The only thing it keeps from your account is your email, to show which account is signed in.
- No analytics, telemetry, or crash reporting. The app talks to two hosts: `auth.openai.com` (sign-in) and `chatgpt.com/backend-api/transcribe` (transcription).
- Each dictation uploads one audio file plus your auth token. The recording is deleted from your disk right after, whether or not it worked.
- **Your audio is subject to your ChatGPT data settings.** If "Improve the model for everyone" is turned on in ChatGPT, OpenAI may train on the audio you upload. To opt out, turn it off under ChatGPT → Settings → Data controls.
- Everything else is local. Settings and history never leave your machine.

## Keywords

For words that come back wrong: product names, surnames, jargon. Add `Ghostty` and "ghosty" or "ghost tea" becomes `Ghostty`. Matching is phonetic (Double Metaphone) plus edit distance, and capitalization is preserved. It runs locally and isn't sent to ChatGPT.

Keep it to obscure terms. A keyword that resembles a common word will cause false corrections.

## Snippets

Say a short phrase, get longer text out: an email address, a signature, boilerplate.

- Matching is exact, case-insensitive, whole words.
- Commas separate alternatives: `my email, my address` → `you@example.com`.
- Leave **Replace with** empty to delete the phrase (e.g. `um, uh, you know`).
- Longest trigger wins. One pass only, so replacements are never re-expanded.

## History

The last ten dictations are saved locally. The tray menu shows the five most recent; click one to copy it. **Clear History** wipes them.

## Menu bar

Dictate lives in the tray. There's no Dock icon. The menu has History, Keywords, Snippets, Settings, and Quit.

It adds itself to your login items on first run so the hotkey works after a restart. To turn that off, use **System Settings → General → Login Items**.

## Requirements

- macOS 11+ or Windows (Linux isn't packaged, but you can [build from source](#building))
- A ChatGPT account (the free tier works)
- A microphone and an internet connection

On macOS, setup asks for two permissions:

- **Microphone**, to record.
- **Accessibility**, for the global hotkey and the paste keystroke.

The keyboard listener only watches for your shortcut (and Escape while recording). It never records what you type. Dictate does no screen recording.

## Data on disk

| OS | Folder |
| --- | --- |
| macOS | `~/Library/Application Support/dev.dictate/` |
| Windows | `%APPDATA%\dev.dictate\` |
| Linux | `$XDG_CONFIG_HOME/dev.dictate/` (usually `~/.config/dev.dictate/`) |

| File | Contents |
| --- | --- |
| `config.json` | Hotkey, keywords, snippets, settings |
| `auth.json` | ChatGPT tokens (`0600`) |
| `history.json` | Recent transcripts |
| `debug.log` | Diagnostics, trimmed past 200 KB |

Audio is written to your temp folder as `dictate-<nanos>.wav` and removed after upload. On macOS and Linux, a diagnostic copy of the log also goes to `/tmp/dictate-debug.log`; it's safe to delete.

## Building

You'll need [Node.js](https://nodejs.org), the [Rust toolchain](https://rustup.rs), and the Tauri 2 platform prerequisites (Xcode command line tools on macOS, MSVC + WebView2 on Windows, the usual WebKitGTK packages on Linux).

```bash
npm install
npm run dev     # hot reload
npm run build   # bundled app
```

The frontend is plain HTML/CSS/JS in `ui/`, with no bundler. The Rust side lives in `src-tauri/src/`:

| File | Role |
| --- | --- |
| `main.rs` | App state, tray, setup flow, dictation pipeline |
| `engine.rs` | Mic capture, hotkey, transcription request, paste, permissions |
| `auth.rs` | ChatGPT OAuth and token refresh |
| `keywords.rs` | Phonetic/Levenshtein correction |
| `snippets.rs` | Whole-word phrase replacement |

Bundle targets are set in `src-tauri/tauri.conf.json` (`["app", "dmg"]` on macOS). Windows overrides this in `tauri.windows.conf.json` and builds a per-user NSIS installer, so no admin password is needed.

## Troubleshooting

**Hotkey does nothing.** Check the Accessibility permission, then use *Check again* in Settings. If another app owns the shortcut, pick a different one. `debug.log` shows what was tried.

**"No sound was captured."** The mic permission is off, the input is muted, or the wrong device is selected.

**Text stops appearing.** Your ChatGPT session probably expired. Sign out and back in from Settings.

**A word keeps coming back wrong.** Add it to Keywords. If it's a common word, try a more distinctive spelling of how you say it.

## Disclaimer

Dictate is not an official OpenAI app. It signs in through the ChatGPT OAuth flow and calls the transcription endpoint the ChatGPT clients use. That endpoint isn't a documented public API and could change without notice. It's built for personal use: your account, your machine, your files.
