# Dictate

Press a key, talk, press it again — your words appear wherever your cursor is, transcribed by
ChatGPT using your own account.

No API key, no per-word credits, no extra subscription. If you already pay for ChatGPT, Dictate uses
that plan. It's a small native app: a tray/menu-bar icon, a global hotkey, a microphone, and a local
UI for settings.

---

## What it does

1. You press your dictation shortcut anywhere — in a browser, an editor, a chat box, a form.
2. A small pill at the bottom of the screen shows four bars moving with your voice, so you can tell
   the microphone is actually hearing you. A flat meter means it isn't.
3. Press the shortcut again to stop. The audio is sent to ChatGPT, which returns the text; the pill
   drops the bars entirely and pulses a highlight across while that is happening.
4. Dictate copies that text to the clipboard, pastes it at your cursor, and puts your old clipboard
   contents back a moment later (unless you copied something else in the meantime).

Press **Escape** while recording to throw the audio away instead of transcribing it. Errors — a
blocked microphone, an empty recording, a failed upload — appear as a short amber message near the
bottom of the screen.

Nothing runs in the background while idle beyond the hotkey listener; the microphone is open only
between the two presses.

---

## Signing in with ChatGPT

Dictate doesn't ship a server and doesn't ask for an API key. It signs in through ChatGPT's own
browser flow:

- Click **Continue with ChatGPT**. Your browser opens to `auth.openai.com`.
- Approve the request there. Dictate is waiting on `http://localhost:1455/auth/callback` — a tiny
  listener on your own machine that catches the redirect and closes.
- The app stores the resulting tokens and refreshes them automatically as they expire.

It requests only `openid profile email offline_access`: enough to know who you are, keep you signed
in, and call the transcription endpoint. Your ChatGPT email is shown on the sign-in screen so you can
tell which account you used. **Sign out** deletes the stored tokens.

### What Dictate does *not* do with your account

This matters, so plainly:

- It does **not** read your conversations, chats, projects, memory, custom instructions, files, or
  billing details. There is no code that calls any of those APIs.
- Beyond signing in and transcribing, Dictate makes no requests against your account — no reading,
  listing, or writing anything that belongs to it. The only account detail it keeps is the email
  address from the token, and only to show you which account is signed in.
- No analytics, telemetry, crash reporting, or "usage" pings. The app talks to two hosts, ever:
  `auth.openai.com` to sign in and `chatgpt.com/backend-api/transcribe` to transcribe.
- Nothing is synced to a service of ours. There is no "ours" — settings and history are local files.

The only things sent over the network are: the OAuth request when you sign in, and the audio file for
each dictation you finish (plus the token that identifies your account). Recordings are deleted from
disk immediately after transcription, whether it succeeded or not.

---

## Keyword corrections

Some words come back wrong — product names, unusual spellings, surnames, jargon. **Keywords** is the
fix-list for those.

Dictate compares what ChatGPT returned against your list using phonetics (Double Metaphone) and edit
distance, so a near-miss gets corrected without you typing every wrong spelling. Add
`Ghostty`, and "ghosty" or "ghost tea" becomes `Ghostty`. It also preserves your capitalization:
shout a transcript and the correction is shouted too.

This runs locally, in the app, in milliseconds. It is *not* sent to ChatGPT as a hint.

Use it for the obscure stuff. Common words that ChatGPT already handles don't need an entry, and a
keyword that resembles a normal word can cause false corrections — a keyword is a magnet for similar
sounding text.

## Snippets

The opposite of keywords. Say a short phrase, get something longer out: an email address, a signature,
a block of boilerplate.

- Matching is exact (case-insensitive, whole words), not fuzzy — a snippet replaces text that was
  *never said*, so a loose match would rewrite ordinary sentences.
- Commas separate alternatives: `my email, my address` → `you@example.com`.
- An empty **Replace with** field deletes the phrase. Handy for filler: `um, uh, you know` → nothing.
- Longest trigger wins, and one pass only, so a replacement is never re-expanded and a trigger inside
  its own replacement can't loop.

## History

The last ten dictations are kept in a local `history.json`. The tray menu shows the five most recent:
click one to copy it. **Clear History** erases the file's contents.

---

## Menu bar

| Item | What it does |
| --- | --- |
| **History** | Recent transcripts, click to copy; clear all |
| **Keywords** | Edit the correction list |
| **Snippets** | Edit phrase replacements |
| **Settings** | Hotkey, account, permissions |
| **Quit Dictate** | Quit — the hotkey stops with it |

The main window is just for setup and settings. Dictate lives in the tray: it has no Dock icon and
no entry in the app switcher, so the menu bar icon is how you reach it. Opening a screen from that
menu brings the window to the front as usual.

Dictate adds itself to your login items on first run, so the hotkey is live after a restart without
opening the app. There is no in-app switch for this — macOS announces the new login item itself, and
**System Settings → General → Login Items** is where you turn it off.

---

## Requirements

- macOS 11 or newer (Windows and Linux builds are possible; see below)
- A ChatGPT account with access to transcription
- An internet connection for each dictation
- A microphone

On macOS the app needs two permissions, and setup walks you through both:

- **Microphone** — to record you. Requested properly through AVFoundation, so the system prompt
  appears; a previous "Don't Allow" is reset only for this app so it can ask again.
- **Accessibility** — for the global hotkey and for sending the paste keystroke.

Neither is used for anything else. Dictate does no screen recording. Its keyboard listener only ever
looks for your shortcut (and Escape while recording) — it never records what you type. On disk it
touches its own data folder plus the temporary files described below.

---

## Data on disk

Everything of yours lives in one folder:

| OS | Path |
| --- | --- |
| macOS | `~/Library/Application Support/dev.dictate/` |
| Windows | `%APPDATA%\dev.dictate\` |
| Linux | `$XDG_CONFIG_HOME/dev.dictate/` (usually `~/.config/dev.dictate/`) |

| File | Contents |
| --- | --- |
| `config.json` | Hotkey, setup-complete flag, keywords, snippets, open-at-login flag |
| `auth.json` | ChatGPT tokens (created `0600`, readable only by you) |
| `history.json` | Recent transcripts and their timestamps |
| `debug.log` | Diagnostic lines, trimmed once it passes 200 KB |

Audio is written to the system temp folder as `dictate-<nanos>.wav` and removed right after the
upload. Nothing keeps your recordings. There is no database. One extra diagnostic mirror lives at
`/tmp/dictate-debug.log` (macOS/Linux) so logs exist before the data folder is created; it is plain
text and can be deleted at any time.

---

## Building from source

Prerequisites: [Node.js](https://nodejs.org), the [Rust toolchain](https://rustup.rs), and the
platform toolchain for Tauri 2 (Xcode command line tools on macOS, MSVC + WebView2 on Windows).

```bash
npm install
npm run dev     # run with hot reload
npm run build   # produce a signed/bundled app
```

The frontend is plain HTML/CSS/JS in `ui/`, loaded directly — no bundler, no framework. The Rust side
is in `src-tauri/src/`:

| File | Role |
| --- | --- |
| `main.rs` | App state, tray menu, setup flow, the dictation pipeline |
| `engine.rs` | Microphone capture, hotkey listener, transcription request, paste, permissions |
| `auth.rs` | ChatGPT OAuth: login, token storage, refresh |
| `keywords.rs` | Phonetic/Levenshtein correction of names and terms |
| `snippets.rs` | Exact whole-word phrase replacement |

Bundle targets are set in `src-tauri/tauri.conf.json` under `bundle.targets` — `["app"]` for macOS.
Windows overrides that in `src-tauri/tauri.windows.conf.json`, which builds only an NSIS installer
and installs per user, so no Administrator password is needed. macOS builds use the signing identity
and entitlements named in the same file; both are ignored on other platforms.

---

## Troubleshooting

**The hotkey does nothing.** Check the Accessibility permission, then reopen Settings → permissions
and press *Check again*. If another app owns the shortcut, choose a different one in Settings. The
app writes what it tried into `debug.log`.

**"No sound was captured."** The microphone permission is off, the input device is muted, or the
wrong device is selected. Fix it in system settings and dictate again.

**Text stops appearing after a while.** The ChatGPT session expired. Sign out and back in from
Settings; refreshing normally handles it, but a revoked session can't be refreshed.

**A word keeps coming back wrong.** Add it to Keywords. If your keyword is a common word, a
more distinctive spelling of what you actually say will match better.

---

## A note on what this is

Dictate is not an official OpenAI app, and it doesn't use a documented public API — it signs in with
the ChatGPT OAuth flow and calls the transcription endpoint the ChatGPT clients use. That can change
without notice. It's built for personal, local use: your account, your machine, your files.
