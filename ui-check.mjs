// Syntax check for the browser scripts in ui/.
//
// There is no bundler and no framework here: `ui/` is served exactly as it sits
// on disk, so a broken script is only discovered when a window loads it — and
// main.js *is* the settings window, so a syntax error there leaves the app with
// no way to sign in. `npm run check:ui` parses each script with the same Node
// that runs the dev server, and confirms every file a page points at still
// exists, which is the other way a page silently comes up empty.
//
// Node 18+, no dependencies. CI can call it before `tauri build`.

import { access, readdir, readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(fileURLToPath(new URL(".", import.meta.url)), "ui");

const names = (await readdir(ROOT)).sort();
const scripts = names.filter((name) => extname(name) === ".js");
const pages = names.filter((name) => extname(name) === ".html");

const failures = [];

for (const name of scripts) {
  const check = spawnSync(process.execPath, ["--check", join(ROOT, name)], {
    encoding: "utf8",
  });
  if (check.status === 0) {
    console.log(`ok    ${name}`);
    continue;
  }
  // Node reports parse errors with CRLF on Windows; normalise before indenting
  // so a failure reads as a block instead of one long line of carriage returns.
  const reason = (check.stderr || check.stdout || "could not be parsed")
    .replace(/\r\n/g, "\n")
    .trim();
  failures.push(`${name}\n${reason.replace(/^/gm, "      ")}`);
}

// Only same-directory references: anything with a scheme, a protocol-relative
// prefix, or a #fragment is not a file this folder has to contain.
const REFERENCE = /<(?:script|link)\b[^>]*?\b(?:src|href)="([^"]+)"/g;
const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:|\/\/|#)/i;

for (const name of pages) {
  const html = await readFile(join(ROOT, name), "utf8");
  for (const [, reference] of html.matchAll(REFERENCE)) {
    if (EXTERNAL.test(reference)) continue;
    try {
      await access(join(ROOT, reference));
      console.log(`ok    ${name} -> ${reference}`);
    } catch {
      failures.push(`${name}: ${reference} is missing`);
    }
  }
}

if (failures.length > 0) {
  console.error(`\n${failures.length} problem(s) in ui/:`);
  for (const failure of failures) console.error(`  ${failure}`);
  process.exit(1);
}

console.log(`\n${scripts.length} script(s) and ${pages.length} page(s) check out.`);
