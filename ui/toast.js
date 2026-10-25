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
// Microphones differ by an order of magnitude in what the same voice produces,
// so a fixed gate and a full-scale assumption leave quiet speakers with a flat
// meter. Instead the meter measures each recording: the room tone sets the floor
// below which nothing counts, and the loudest recent syllable sets the top of
// the scale, so a quiet microphone and a quiet voice are both handled.
//
// Room tone: drops straight to a quieter reading and creeps up toward a louder
// one, so a fan switching on cannot pin it high, and FLOOR_MAX keeps the creep
// from reaching speech level.
const FLOOR_SEED = 0.004;
const FLOOR_MIN = 0.0005;
const FLOOR_MAX = 0.02;
const FLOOR_RISE = 0.0015; // per event: a fraction of the distance to the level
// A level has to clear the floor by this much before any bar moves.
const FLOOR_GATE = 1.8;
// Hard ceiling on the gate. Past this the meter would rather twitch on a noisy
// room than hide a whisper, because a gate raised too high cannot be undone by
// speaking louder.
const GATE_MAX = 0.008;
const GATE_MIN = 0.003;

// Recent peak: rises to a louder level at once and eases back over a few
// seconds, so a whisper after normal speech still fills the bars and a shout
// does not leave them pinned at the top. Seeded mid-scale, so the first moments
// of a recording are neither dead nor pegged while it learns.
const PEAK_SEED = 0.05;
const PEAK_FLOOR = 0.02;
const PEAK_ATTACK = 0.35; // per event: follows a louder level within a frame or two
const PEAK_RELEASE = 0.02; // per event: about two seconds to fall back
// Top of the scale, as a multiple of the recent peak. Above 1 on purpose: a
// level that holds steady sits around four fifths of the way up, so speaking up
// still has somewhere to go instead of pinning the bars at the top.
const HEADROOM = 1.2;
// Gentler than a square root: quiet speech moves the bars without turning room
// tone into jitter.
const CURVE = 0.55;

const level = new Float32Array(bars.length); // smoothed 0..1 per bar
const history = []; // recent shaped levels, newest last

// Learned within a recording, reset when the next one starts.
let noiseFloor = FLOOR_SEED;
let recentPeak = PEAK_SEED;

/// Turns a raw 0..1 microphone level into 0..1 of bar height, scaled to the
/// levels this recording actually contains.
function shapeLevel(raw) {
  raw = Math.min(1, Math.max(0, raw));

  if (raw < noiseFloor) {
    noiseFloor = Math.max(FLOOR_MIN, raw);
  } else {
    noiseFloor = Math.min(
      FLOOR_MAX,
      noiseFloor + (raw - noiseFloor) * FLOOR_RISE,
    );
  }

  recentPeak += (raw - recentPeak) * (raw > recentPeak ? PEAK_ATTACK : PEAK_RELEASE);
  recentPeak = Math.max(PEAK_FLOOR, recentPeak);

  const gate = Math.min(GATE_MAX, Math.max(GATE_MIN, noiseFloor * FLOOR_GATE));
  if (raw <= gate) return 0;
  const top = Math.max(recentPeak * HEADROOM, gate * 2);
  return Math.min(1, Math.pow((raw - gate) / (top - gate), CURVE));
}

function resetMeter() {
  noiseFloor = FLOOR_SEED;
  recentPeak = PEAK_SEED;
}

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
    resetMeter();
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
  history.push(shapeLevel(Number(event.payload) || 0));
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
