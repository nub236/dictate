const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let app = {
  signed_in: false,
  email: null,
  hotkey: "right_option",
  onboarded: false,
  engine_running: false,
  accessibility: false,
  platform: "",
};

let perms = { mic: false, accessibility: false };
let keywordList = [];
let snippetList = [];
let pendingHotkey = "right_option";
let bareCandidate = null;
let bareTimer = null;
let permissionsPrompted = false;
let permissionPoll = null;
let signinPoll = null;
let signInReturn = "permissions";
// True only while a browser sign-in is actually running. Without this the
// account screen's poll sees an existing session and jumps forward on its own.
let signInInFlight = false;
// Set by the capture box; lets the Rust listener feed bare modifier presses in.
let injectBare = null;
let awaitingTest = false;
let testTimer = null;
let testPoll = null;
let windowHotkeyTest = null;
let currentView = null;

function log(message) {
  try {
    invoke("debug_log_command", { message });
  } catch {}
}

const $ = (id) => document.getElementById(id);

const bareLabels = {
  right_option: "Right Option",
  left_option: "Left Option",
  right_shift: "Right Shift",
  left_shift: "Left Shift",
  right_ctrl: "Right Control",
  left_ctrl: "Left Control",
  right_cmd: "Right Command",
  left_cmd: "Left Command",
  fn: "Fn",
};

function prettyHotkey(id) {
  if (bareLabels[id]) return bareLabels[id];
  const symbols = { cmd: "⌘", ctrl: "⌃", alt: "⌥", shift: "⇧" };
  return id
    .split("+")
    .map((part) => symbols[part] || (part.length === 1 ? part.toUpperCase() : part[0].toUpperCase() + part.slice(1)))
    .join("");
}

function updateHotkeyState(message) {
  const saved = prettyHotkey(app.hotkey);
  const pending = prettyHotkey(pendingHotkey);
  const changed = pendingHotkey !== app.hotkey;
  $("hotkey-current").textContent = `Saved shortcut: ${saved}`;
  $("capture-value").textContent = pending;
  // Continue also advances to the next step, so it stays enabled even when the
  // shortcut has not changed.
  $("btn-hotkey-save").disabled = false;
  if (message) {
    $("capture-hint").textContent = message;
  } else if (changed) {
    $("capture-hint").textContent = `Changed from ${saved}`;
  } else {
    $("capture-hint").textContent = "Press Continue to keep it";
  }
}

// macOS is the only platform that asks the user for grants before it can
// record; Windows and Linux can record as soon as the app runs, so their setup
// has no permissions screen at all.
function needsPermissions() {
  return (app.platform || perms.platform || "macos") === "macos";
}

function afterSignIn() {
  return needsPermissions() ? "permissions" : "hotkey";
}

function show(view) {
  // After setup the tour has no way back into it: it is the entry point of the
  // setup chain, so Back from the account screen used to land there with no way
  // out but clicking through every screen again.
  if (view === "welcome" && app.onboarded) view = "signin";
  // Nothing to grant on Windows or Linux, so nothing to show there.
  if (view === "permissions" && !needsPermissions()) view = "hotkey";
  if (currentView !== view) signInInFlight = false;
  if (currentView === "hotkey" && view !== "hotkey") {
    invoke("resume_hotkey").catch(() => {});
    invoke("stop_capture").catch(() => {});
  }
  if (view === "hotkey" && currentView !== "hotkey") {
    invoke("pause_hotkey").catch(() => {});
    invoke("start_capture").catch(() => {});
  }
  currentView = view;

  for (const section of document.querySelectorAll(".view")) {
    section.hidden = section.id !== `view-${view}`;
  }
  stopPermissionPolling();
  stopSigninPolling();
  stopHotkeyTest();

  $("capture").classList.remove("active");

  if (view === "hotkey") {
    pendingHotkey = app.hotkey;
    updateHotkeyState();
    setStatus("hotkey-status", "", "");
    setTimeout(() => $("capture").focus(), 50);
  }

  if (view === "permissions") {
    renderPermissions();
    if (!permissionsPrompted) {
      permissionsPrompted = true;
      setTimeout(() => checkPermissions(true), 250);
    } else {
      checkPermissions(false);
    }
    startPermissionPolling();
  }

  if (view === "signin") {
    renderSignin();
  }

  if (view === "keywords") {
    // Keywords always leads on to Snippets, whether the tray opened it after
    // setup or this is first-run onboarding, so the forward button never closes
    // the window from here.
    $("btn-keywords-continue").textContent = "Continue";
    loadKeywords();
    setTimeout(() => $("keyword-input").focus(), 50);
  }

  if (view === "snippets") {
    // Reached from the tray after setup: there is no onboarding left to finish,
    // so the forward button just closes the window.
    $("btn-snippets-continue").textContent = app.onboarded ? "Done" : "Continue";
    loadSnippets();
    setTimeout(() => $("snippet-trigger").focus(), 50);
  }

  if (view === "done") {
    $("done-hotkey").textContent = prettyHotkey(pendingHotkey);
    // Setup counts as finished the moment this screen is reached, so closing the
    // window some other way still remembers it; Done only hides the window.
    invoke("mark_onboarded").catch(() => {});
  }
}

function setStatus(id, text, kind) {
  const el = $(id);
  el.textContent = text;
  el.className = `status${kind ? ` ${kind}` : ""}`;
}

/* --------------------------------------------------------------------- toast */

let toastTimer = null;
function toast(message) {
  const el = $("toast");
  el.textContent = message;
  el.hidden = false;
  requestAnimationFrame(() => el.classList.add("show"));
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    el.classList.remove("show");
    setTimeout(() => { el.hidden = true; }, 180);
  }, 3200);
}

/* ------------------------------------------------------------------- sign in */

function renderSignin() {
  $("signin-heading").textContent = app.signed_in ? "ChatGPT account" : "Sign in with ChatGPT";
  $("signin-title").textContent = app.signed_in ? (app.email || "ChatGPT account") : "Not signed in";
  // The email and the presence of Sign out already say "signed in"; the subline
  // only earns its space before that.
  $("signin-sub").hidden = app.signed_in;
  $("signin-sub").textContent = "You'll finish in your browser";
  $("signin-hint-a").textContent = app.signed_in
    ? "Sign out to use a different account."
    : "A browser window opens.";
  $("signin-hint-b").textContent = app.signed_in ? "" : "Approve there, then come back.";
  $("btn-signin").textContent = app.signed_in ? "Continue" : "Continue with ChatGPT";
  $("btn-signout").hidden = !app.signed_in;
  // The account screen is where the post-setup chain starts, so Back would only
  // lead to the tour it no longer includes.
  $("btn-signin-back").hidden = app.onboarded;
}

async function beginSignIn(returnView) {
  signInReturn = returnView;
  signInInFlight = true;
  const button = $("btn-signin");
  button.disabled = true;
  button.textContent = "Waiting for browser…";
  $("signin-hint-a").textContent = "Approve in the browser, then come back.";
  $("signin-hint-b").textContent = "This window updates itself.";
  startSigninPolling();
  try {
    await invoke("sign_in");
  } catch (error) {
    button.disabled = false;
    renderSignin();
    toast(String(error));
  }
}

function startSigninPolling() {
  stopSigninPolling();
  signinPoll = setInterval(async () => {
    if (!signInInFlight || $("view-signin").hidden) return;
    try {
      const status = await invoke("auth_status");
      if (status && status.signed_in) {
        stopSigninPolling();
        signInInFlight = false;
        app.signed_in = true;
        app.email = status.email || app.email;
        renderSignin();
        $("btn-signin").disabled = false;
        show(signInReturn);
      }
    } catch { /* keep waiting */ }
  }, 700);
}

function stopSigninPolling() {
  clearInterval(signinPoll);
  signinPoll = null;
}

/* ---------------------------------------------------------------- permissions */

// macOS only: this screen is skipped on Windows and Linux, which need no
// grants from the user.
function renderPermissions() {
  const mic = $("perm-mic-state");
  const ax = $("perm-ax-state");
  const permission = perms.mic_permission || "unknown";
  $("perm-lede").textContent = "macOS needs two. Audio is only recorded while you dictate.";
  $("perm-hint-a").textContent = "macOS adds Dictate to each list the first time it asks.";
  $("perm-hint-b").textContent = "If a switch is off, turn it on and come back.";

  // Words only: the box and cross markers read as stray glyphs at 11.5px, and
  // the word already carries the state without relying on colour.
  const allow = $("btn-perm-mic-allow");
  if (perms.mic) {
    mic.textContent = "Allowed";
    mic.className = "state ok";
    allow.hidden = true;
  } else if (permission === "denied") {
    mic.textContent = "Blocked";
    mic.className = "state bad";
    // A denied app cannot be asked again: TCC holds the decision for the life of
    // the process. The row is still in System Settings, so that is the way back,
    // and the generic "Open" chip would only duplicate it.
    allow.hidden = false;
    allow.textContent = "Open Settings";
    const micOpen = document.querySelector('button[data-open="mic"]');
    if (micOpen) micOpen.hidden = true;
  } else {
    mic.textContent = permission === "not_determined" ? "Needs permission" : "Needed";
    mic.className = "state bad";
    allow.hidden = false;
    allow.textContent = "Allow microphone";
  }
  ax.textContent = perms.accessibility ? "Allowed" : "Needed";
  ax.className = `state ${perms.accessibility ? "ok" : "bad"}`;
  // Once both are granted there is nothing left to check, so the manual nudge
  // only appears while something is actually missing.
  const complete = perms.mic && perms.accessibility;
  $("btn-perm-recheck").hidden = complete;
  $("btn-perm-continue").disabled = !complete;
  // "Open" stays reachable so a grant can be revoked, but stops competing with
  // the state text once the permission is already allowed.
  for (const button of document.querySelectorAll("button[data-open]")) {
    const allowed = button.dataset.open === "mic" ? perms.mic : perms.accessibility;
    button.classList.toggle("is-quiet", Boolean(allowed));
  }
}

async function checkPermissions(prompt) {
  try {
    perms = await invoke("check_permissions", { prompt });
  } catch {
    toast("Could not check permissions");
  }
  renderPermissions();
  if (perms.mic && perms.accessibility) stopPermissionPolling();
}

function startPermissionPolling() {
  stopPermissionPolling();
  permissionPoll = setInterval(() => {
    if ($("view-permissions").hidden) return;
    if (perms.mic && perms.accessibility) {
      stopPermissionPolling();
      return;
    }
    checkPermissions(false);
  }, 2000);
}

function stopPermissionPolling() {
  clearInterval(permissionPoll);
  permissionPoll = null;
}

// Coming back from System Settings is the moment the page used to sit stale:
// the poll only runs while something is missing, and this is usually when it
// was just granted.
window.addEventListener("focus", () => {
  if ($("view-permissions").hidden) return;
  checkPermissions(false);
});

/* ------------------------------------------------------------------- keywords */

function renderKeywords() {
  const list = $("keyword-list");
  list.replaceChildren();
  $("keywords-empty").hidden = keywordList.length > 0;
  keywordList.forEach((word, index) => {
    const row = document.createElement("div");
    row.className = "keyword-row";
    const name = document.createElement("span");
    name.className = "keyword-name";
    name.textContent = word;
    const remove = document.createElement("button");
    remove.className = "chip";
    remove.textContent = "×";
    remove.title = `Remove ${word}`;
    remove.dataset.index = String(index);
    row.append(name, remove);
    list.append(row);
  });
}

async function loadKeywords() {
  try {
    keywordList = await invoke("get_keywords");
  } catch {
    keywordList = [];
  }
  renderKeywords();
}

async function persistKeywords() {
  try {
    keywordList = await invoke("set_keywords", { keywords: keywordList });
  } catch (error) {
    toast(String(error));
  }
  renderKeywords();
}

async function addKeywords(values) {
  const added = values
    .map((value) => value.trim())
    .filter((value) => value && !keywordList.some((word) => word.toLowerCase() === value.toLowerCase()));
  if (!added.length) return;
  for (const value of added) {
    keywordList.push(value);
  }
  await persistKeywords();
  $("keyword-input").focus();
}

async function addKeyword() {
  const input = $("keyword-input");
  const value = input.value.trim();
  if (!value) return;
  input.value = "";
  await addKeywords([value]);
}

/* ------------------------------------------------------------------- snippets */

function renderSnippets() {
  const list = $("snippet-list");
  list.replaceChildren();
  $("snippets-empty").hidden = snippetList.length > 0;
  snippetList.forEach((snippet, index) => {
    const row = document.createElement("div");
    row.className = "snippet-row";

    const from = document.createElement("span");
    from.className = "snippet-from";
    from.title = snippet.trigger;
    // Each comma-separated alternative gets its own chip, so a filler-word entry
    // reads as three triggers rather than one run-on sentence.
    const alternatives = snippet.trigger
      .split(",")
      .map((part) => part.trim())
      .filter(Boolean);
    for (const [position, spoken] of alternatives.entries()) {
      if (position === 3) {
        const more = document.createElement("span");
        more.className = "snippet-more";
        more.textContent = `+${alternatives.length - 3}`;
        more.title = alternatives.slice(3).join(", ");
        from.append(more);
        break;
      }
      const chip = document.createElement("span");
      chip.className = "snippet-alt";
      chip.textContent = spoken;
      from.append(chip);
    }

    const arrow = document.createElement("span");
    arrow.className = "snippet-arrow";
    arrow.setAttribute("aria-hidden", "true");
    arrow.append(arrowIcon());

    const to = document.createElement("span");
    to.className = "snippet-to";
    to.textContent = snippet.replacement || "deleted";
    to.title = snippet.replacement;
    if (!snippet.replacement) to.classList.add("is-empty");

    const remove = document.createElement("button");
    remove.className = "chip";
    remove.textContent = "×";
    remove.title = `Remove ${snippet.trigger}`;
    remove.setAttribute("aria-label", `Remove ${snippet.trigger}`);
    remove.dataset.index = String(index);

    row.append(from, arrow, to, remove);
    list.append(row);
  });
}

// One drawn arrow everywhere, so the weight matches the row text instead of
// inheriting whatever glyph the font happens to ship.
function arrowIcon() {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 12 12");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("fill", "none");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", "M1.5 6h8M6.75 2.75 10 6l-3.25 3.25");
  path.setAttribute("stroke", "currentColor");
  path.setAttribute("stroke-width", "1.4");
  path.setAttribute("stroke-linecap", "round");
  path.setAttribute("stroke-linejoin", "round");
  svg.append(path);
  return svg;
}

async function loadSnippets() {
  try {
    snippetList = await invoke("get_snippets");
  } catch {
    snippetList = [];
  }
  renderSnippets();
}

async function persistSnippets() {
  try {
    snippetList = await invoke("set_snippets", { snippets: snippetList });
  } catch (error) {
    toast(String(error));
  }
  renderSnippets();
}

async function addSnippet() {
  const trigger = $("snippet-trigger");
  const replacement = $("snippet-replacement");
  const spoken = trigger.value.trim();
  if (!spoken) {
    trigger.focus();
    return;
  }
  // Re-adding the same phrase updates it rather than creating a duplicate.
  snippetList = snippetList.filter(
    (snippet) => snippet.trigger.toLowerCase() !== spoken.toLowerCase()
  );
  snippetList.push({ trigger: spoken, replacement: replacement.value.trim() });
  trigger.value = "";
  replacement.value = "";
  await persistSnippets();
  trigger.focus();
}

/* --------------------------------------------------------------------- hotkey */

function keyName(code) {
  if (code.startsWith("Key")) return code.slice(3).toLowerCase();
  if (code.startsWith("Digit")) return code.slice(5);
  const named = {
    Space: "space",
    Enter: "return",
    Escape: "escape",
    Tab: "tab",
    ArrowUp: "up",
    ArrowDown: "down",
    ArrowLeft: "left",
    ArrowRight: "right",
    Minus: "-",
    Equal: "=",
    BracketLeft: "[",
    BracketRight: "]",
    Backslash: "\\",
    Semicolon: ";",
    Quote: "'",
    Comma: ",",
    Period: ".",
    Slash: "/",
    Backquote: "`",
  };
  if (named[code]) return named[code];
  if (/^F([1-9]|1[0-2])$/.test(code)) return code.toLowerCase();
  return null;
}

const bareByCode = {
  AltRight: "right_option",
  AltLeft: "left_option",
  ShiftRight: "right_shift",
  ShiftLeft: "left_shift",
  ControlRight: "right_ctrl",
  ControlLeft: "left_ctrl",
  MetaRight: "right_cmd",
  MetaLeft: "left_cmd",
  Fn: "fn",
};

function bareModifier(event) {
  if (bareByCode[event.code]) return bareByCode[event.code];
  switch (event.keyCode) {
    case 61: return "right_option";
    case 58: return "left_option";
    case 60: return "right_shift";
    case 56: return "left_shift";
    case 62: return "right_ctrl";
    case 59: return "left_ctrl";
    case 54: return "right_cmd";
    case 55: return "left_cmd";
    case 63: return "fn";
    default: return null;
  }
}

// The shortcut a key event represents, in the same ids the capture box and the
// Rust side use. Bare modifiers first, so right Control reads as "right_ctrl"
// rather than as a Control combination.
function hotkeyFromEvent(event) {
  const bare = bareByCode[event.code] || (isModifierKey(event) ? bareModifier(event) : null);
  if (bare) return bare;
  const name = keyName(event.code);
  if (!name) return null;
  const mods = [];
  if (event.metaKey) mods.push("cmd");
  if (event.ctrlKey) mods.push("ctrl");
  if (event.altKey) mods.push("alt");
  if (event.shiftKey) mods.push("shift");
  return mods.length ? [...mods, name].join("+") : name;
}

function isModifierKey(event) {
  return ["Alt", "Shift", "Control", "Meta", "CapsLock", "Fn", "OS"].includes(event.key) || event.keyCode === 63;
}

function attachCapture(boxId, valueId, hintId) {
  const box = $(boxId);
  const value = $(valueId);
  const hint = $(hintId);
  let active = false;

  function deactivate() {
    active = false;
    bareCandidate = null;
    clearTimeout(bareTimer);
    box.classList.remove("active");
  }

  function commit(id, message) {
    pendingHotkey = id;
    // Keep listening: committing one shortcut must not make the box ignore the
    // next key. Blur (clicking Save or leaving) is what stops capture.
    bareCandidate = null;
    clearTimeout(bareTimer);
    active = true;
    box.classList.add("active");
    value.textContent = prettyHotkey(id);
    updateHotkeyState(message);
  }

  // A bare modifier is committed on release, or after a moment if no other
  // key joins it. Used by the keydown handler and by the Rust tap (the page
  // never receives right Option on its own).
  function startBare(id) {
    if (bareCandidate === id) return;
    bareCandidate = id;
    hint.textContent = `Release for ${prettyHotkey(id)}, or keep holding and add a key`;
    clearTimeout(bareTimer);
    bareTimer = setTimeout(() => {
      if (active && bareCandidate) commit(bareCandidate);
    }, 1200);
  }

  injectBare = (id, released) => {
    if (!active) return;
    if (released) {
      if (bareCandidate !== id) return;
      clearTimeout(bareTimer);
      commit(id);
      return;
    }
    startBare(id);
  };

  function activate() {
    active = true;
    bareCandidate = null;
    box.classList.add("active");
    value.textContent = prettyHotkey(pendingHotkey);
    hint.textContent = "Press a modifier on its own, or a combination";
  }

  box.addEventListener("click", activate);
  box.addEventListener("focus", activate);

  box.addEventListener("blur", () => {
    if (!active) return;
    deactivate();
    updateHotkeyState();
  });

  box.addEventListener("keydown", (event) => {
    if (!active) return;
    event.preventDefault();
    log(`capture keydown code=${event.code} key=${event.key} location=${event.location}`);

    if (isModifierKey(event)) {
      const id = bareModifier(event);
      if (!id) return;
      startBare(id);
      return;
    }

    clearTimeout(bareTimer);
    bareCandidate = null;

    const mods = [];
    if (event.metaKey) mods.push("cmd");
    if (event.ctrlKey) mods.push("ctrl");
    if (event.altKey) mods.push("alt");
    if (event.shiftKey) mods.push("shift");

    const name = keyName(event.code);
    if (!name) return;
    if (!mods.length) {
      hint.textContent = "Add ⌘, ⌃, ⌥ or ⇧, or press a modifier by itself";
      return;
    }
    commit([...mods, name].join("+"));
  });

  box.addEventListener("keyup", (event) => {
    if (!active || !bareCandidate) return;
    if (isModifierKey(event)) {
      event.preventDefault();
      commit(bareCandidate);
    }
  });
}

function stopHotkeyTest() {
  awaitingTest = false;
  clearTimeout(testTimer);
  clearInterval(testPoll);
  testTimer = null;
  testPoll = null;
  if (windowHotkeyTest) {
    window.removeEventListener("keydown", windowHotkeyTest, true);
    windowHotkeyTest = null;
  }
  invoke("set_testing", { value: false }).catch(() => {});
}

// Confirming the shortcut on Windows and Linux.
//
// macOS delivers a bare modifier only to the global keyboard tap, so there the
// confirmation reads the app's hit counter. Windows sends the key to this
// window as an ordinary keydown, and its keyboard hook is not something the
// setup screen can rely on — so the shortcut is paused for the test and the
// page itself recognises the press.
function startWindowHotkeyTest() {
  setStatus("hotkey-status", "Now press your shortcut once more to confirm…", "");
  awaitingTest = true;
  invoke("pause_hotkey").catch(() => {});
  windowHotkeyTest = (event) => {
    if (!awaitingTest) return;
    if (hotkeyFromEvent(event) !== pendingHotkey) return;
    event.preventDefault();
    hotkeyConfirmed();
  };
  window.addEventListener("keydown", windowHotkeyTest, true);

  testTimer = setTimeout(() => {
    if (!awaitingTest) return;
    stopHotkeyTest();
    invoke("resume_hotkey").catch(() => {});
    setStatus("hotkey-status", `Didn't catch it. Try the shortcut again.${hotkeyHint()}`, "bad");
  }, 15000);
}

// Only macOS keeps a list of apps allowed to send keystrokes.
function hotkeyHint() {
  return app.platform === "macos" ? " Or check Accessibility." : "";
}

async function saveHotkey() {
  stopHotkeyTest();
  setStatus("hotkey-status", "Saving…", "");
  const unchanged = pendingHotkey === app.hotkey;

  try {
    const result = await invoke("set_hotkey", { hotkey: pendingHotkey });
    app.hotkey = pendingHotkey;
    app.engine_running = !!result.engine_running;
    updateHotkeyState();
    if (!result.engine_running) {
      setStatus(
        "hotkey-status",
        app.platform === "macos"
          ? "macOS is blocking the hotkey — allow Dictate in Accessibility."
          : "The keyboard listener could not start. Try a different shortcut.",
        "bad",
      );
      return;
    }

    // Pressing the shortcut is what proves the system really delivers it — a new
    // combination may belong to another app. Re-saving the shortcut that already
    // works needs no test.
    if (unchanged && app.onboarded) {
      setStatus("hotkey-status", `✓ ${prettyHotkey(pendingHotkey)} is registered`, "ok");
      show("keywords");
      return;
    }

    if (app.platform !== "macos") {
      startWindowHotkeyTest();
      return;
    }

    await invoke("set_testing", { value: true });
    const baseline = await invoke("hotkey_hits");
    setStatus("hotkey-status", "Now press your shortcut once more to confirm…", "");
    awaitingTest = true;

    testPoll = setInterval(async () => {
      if (!awaitingTest) return;
      try {
        const hits = await invoke("hotkey_hits");
        if (hits > baseline) hotkeyConfirmed();
      } catch {}
    }, 250);

    testTimer = setTimeout(() => {
      if (!awaitingTest) return;
      stopHotkeyTest();
      invoke("set_testing", { value: false }).catch(() => {});
      setStatus("hotkey-status", `Didn't catch it. Try the shortcut again.${hotkeyHint()}`, "bad");
    }, 15000);
  } catch (error) {
    setStatus("hotkey-status", String(error), "bad");
  }
}

function hotkeyConfirmed() {
  if (!awaitingTest) return;
  stopHotkeyTest();
  invoke("set_testing", { value: false }).catch(() => {});
  invoke("resume_hotkey").catch(() => {});
  setStatus("hotkey-status", `✓ ${prettyHotkey(pendingHotkey)} is registered`, "ok");
  show("keywords");
}

/* --------------------------------------------------------------------- wiring */

function wire() {
  $("btn-welcome-continue").addEventListener("click", () => {
    show("signin");
  });

  $("btn-signin").addEventListener("click", () => {
    if (app.signed_in) show(afterSignIn());
    else beginSignIn(afterSignIn());
  });
  $("btn-signin-back").addEventListener("click", () => show("welcome"));
  $("btn-signout").addEventListener("click", async () => {
    const button = $("btn-signout");
    button.disabled = true;
    try {
      await invoke("sign_out");
      invoke("pause_hotkey").catch(() => {});
      app.signed_in = false;
      app.email = null;
      renderSignin();
      toast("Signed out");
    } catch (error) {
      toast(String(error));
    }
    button.disabled = false;
  });

  $("btn-perm-back").addEventListener("click", () => show("signin"));
  $("btn-perm-recheck").addEventListener("click", () => checkPermissions(true));
  $("btn-perm-mic-allow").addEventListener("click", async () => {
    const button = $("btn-perm-mic-allow");
    // Blocked means macOS will not prompt again, so send people to the switch.
    if (perms.mic_permission === "denied") {
      invoke("open_privacy", { kind: "mic" }).catch(() => {});
      toast("Turn Dictate on under Microphone, then come back.");
      return;
    }
    button.disabled = true;
    button.textContent = "Waiting for you…";
    try {
      const answer = await invoke("request_mic_access");
      if (answer && answer.granted) {
        toast("Microphone allowed");
      } else {
        toast("macOS did not allow the microphone");
      }
    } catch (error) {
      toast(String(error));
    }
    button.disabled = false;
    await checkPermissions(false);
  });
  $("btn-perm-continue").addEventListener("click", () => show("hotkey"));
  for (const chip of document.querySelectorAll("[data-open]")) {
    chip.addEventListener("click", () => {
      invoke("open_privacy", { kind: chip.dataset.open });
      toast("Turn Dictate on in System Settings, then come back");
    });
  }

  // Back is one step, not "the screen after signing in": on Windows and Linux
  // the shortcut follows the account screen directly.
  $("btn-hotkey-back").addEventListener("click", () => show(needsPermissions() ? "permissions" : "signin"));
  $("btn-hotkey-save").addEventListener("click", saveHotkey);

  $("btn-keyword-add").addEventListener("click", addKeyword);
  $("keyword-input").addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      addKeyword();
    }
  });
  $("keyword-input").addEventListener("paste", (event) => {
    const text = event.clipboardData.getData("text");
    if (!/\s*[\n,]+\s*/.test(text)) return;
    event.preventDefault();
    const values = text.split(/[\n,]+/).map((value) => value.trim()).filter(Boolean);
    addKeywords(values);
  });
  $("keyword-list").addEventListener("click", (event) => {
    const button = event.target.closest("button[data-index]");
    if (!button) return;
    keywordList.splice(Number(button.dataset.index), 1);
    persistKeywords();
  });
  $("btn-keywords-back").addEventListener("click", () => show("hotkey"));
  $("btn-keywords-continue").addEventListener("click", () => show("snippets"));

  $("btn-snippet-add").addEventListener("click", addSnippet);
  for (const id of ["snippet-trigger", "snippet-replacement"]) {
    $(id).addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        addSnippet();
      }
    });
  }
  $("snippet-list").addEventListener("click", (event) => {
    const button = event.target.closest("button[data-index]");
    if (!button) return;
    snippetList.splice(Number(button.dataset.index), 1);
    persistSnippets();
  });
  $("btn-snippets-back").addEventListener("click", () => show("keywords"));
  $("btn-snippets-continue").addEventListener("click", () => {
    if (app.onboarded) {
      invoke("complete_onboarding").catch(() => {});
      return;
    }
    app.onboarded = true;
    show("done");
  });
  // The Ready screen closes on an explicit click: a window that vanishes on its
  // own a few seconds in reads as a crash. This also marks setup complete, so
  // closing the window any other way leaves that for the next run.
  $("btn-done").addEventListener("click", () => {
    const button = $("btn-done");
    button.disabled = true;
    invoke("complete_onboarding").catch(() => {});
  });

  attachCapture("capture", "capture-value", "capture-hint");

  listen("sign-in-finished", (event) => {
    const payload = event.payload || {};
    const button = $("btn-signin");
    button.disabled = false;
    if (payload.ok) {
      stopSigninPolling();
      signInInFlight = false;
      app.signed_in = true;
      app.email = payload.email || app.email;
      renderSignin();
      if (currentView === "signin") show(signInReturn);
    } else {
      renderSignin();
      toast(payload.message ? String(payload.message).split("\n")[0] : "Sign-in failed");
    }
  });

  listen("capture-key", (event) => {
    if (currentView !== "hotkey" || !injectBare) return;
    const [id, phase] = String(event.payload).split(":");
    injectBare(id, phase === "up");
  });

  listen("hotkey-fired", hotkeyConfirmed);
  listen("goto", (event) => show(event.payload || "welcome"));
}

async function init() {
  wire();
  try {
    const fresh = await invoke("bootstrap");
    app = { ...app, ...fresh };
  } catch { /* first run, defaults are fine */ }
  renderSignin();
  show("welcome");
}

init();
