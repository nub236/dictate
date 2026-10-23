const { listen } = window.__TAURI__.event;

const el = document.getElementById("toast");
const pill = document.getElementById("pill");
const bars = [...pill.querySelectorAll("i")];

// Bar geometry in px. DOT is both the bar width and its resting height, so a
// silent microphone draws round dots rather than a flat line.
const DOT = 4;
// Loudest the bars ever go: 50% of the pill's 30px inner height, so a shout
// never crowds the rounded ends.
const CAP = 15;

// Uneven on purpose, so it reads as a waveform and not one bar drawn four times.
const GAIN = [0.55, 0.85, 1, 0.7];
// Frames of delay per bar, so the bars never move in lockstep.
const PHASE = [2, 1, 0, 1];
// Rise fast, fall slow: 50ms up, 150ms down. Raw peaks are spiky, and this is
// what turns them into fluid motion without lagging behind your voice.
const ATTACK_MS = 50;
const DECAY_MS = 150;
// Room tone, fans and hard drives sit below this, so the bars stay collapsed
// instead of twitching at nothing.
const GATE = 0.02;

const level = new Float32Array(bars.length); // smoothed 0..1 per bar
const history = []; // recent gated levels, newest last

let state = "idle";
let lastLevel = 0;
let fadeTimer = null;
let frameHandle = null;
let lastFrame = 0;

function flatten() {
  level.fill(0);
  history.length = 0;
  for (const bar of bars) bar.style.height = `${DOT}px`;
}

function draw(now) {
  // Clamp dt so a stalled frame cannot snap the bars to the top in one step.
  const dt = Math.min(64, now - lastFrame || 16);
  lastFrame = now;
  bars.forEach((bar, index) => {
    const sample = history[Math.max(0, history.length - 1 - PHASE[index])] ?? 0;
    const target = sample * GAIN[index];
    const tau = target > level[index] ? ATTACK_MS : DECAY_MS;
    level[index] += (target - level[index]) * (1 - Math.exp(-dt / tau));
    bar.style.height = `${(DOT + (CAP - DOT) * level[index]).toFixed(2)}px`;
  });
  frameHandle = requestAnimationFrame(draw);
}

function setState(next) {
  state = next;
  pill.classList.toggle("show", next !== "idle");
  pill.classList.toggle("working", next === "working");
  if (next === "listening") {
    flatten();
    if (frameHandle === null) {
      lastFrame = 0;
      frameHandle = requestAnimationFrame(draw);
    }
  } else {
    if (frameHandle !== null) cancelAnimationFrame(frameHandle);
    frameHandle = null;
    // While transcribing the bars are hidden by CSS, so only the sweep shows.
    flatten();
  }
}

listen("toast", (event) => {
  const payload = event.payload;
  const message =
    typeof payload === "string" ? payload : String(payload?.message ?? "");
  el.textContent = message;
  el.classList.toggle(
    "error",
    typeof payload === "object" && payload !== null && Boolean(payload.error),
  );
  // The window is hidden by Rust, not by this page, so the .show class is
  // often still on the element when the next message arrives. Strip it and
  // force a reflow so the fade-in plays every time.
  el.classList.remove("show");
  void el.offsetWidth;
  el.classList.add("show");

  clearTimeout(fadeTimer);
  fadeTimer = setTimeout(() => el.classList.remove("show"), 2300);
});

listen("pill", (event) => {
  const next = String(event.payload ?? "idle");
  setState(next);
  if (next === "listening") {
    // A new dictation: the message before it is stale.
    el.classList.remove("show");
    clearTimeout(fadeTimer);
    lastLevel = Date.now();
  }
});

listen("recording-level", (event) => {
  if (state !== "listening") return;
  lastLevel = Date.now();
  const raw = Math.min(1, Math.max(0, Number(event.payload) || 0));
  // Gate first, then the same curve as before, so quiet speech still shows.
  const shaped =
    raw <= GATE
      ? 0
      : Math.min(1, Math.pow((raw - GATE) / (1 - GATE), 0.45) * 1.5);
  history.push(shaped);
  if (history.length > 8) history.shift();
});

// Audio that stops arriving means the recording ended without the pill being
// told — the five-minute limit, or a microphone that died. Without this the
// pill would sit at the bottom of the screen forever.
setInterval(() => {
  if (state === "listening" && Date.now() - lastLevel > 1200) {
    setState("idle");
  }
}, 500);
