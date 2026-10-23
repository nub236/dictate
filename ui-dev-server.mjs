// Dev-only static server for ui/.
//
// Without a `devUrl`, Tauri embeds frontendDist into the binary, so a CSS tweak
// needs a full Rust rebuild. Pointing `devUrl` here serves the same files
// straight from disk and reloads the window the moment one of them changes.
// `npm run build` still bundles ui/ the normal way; this file is never used
// outside `tauri dev`.

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { watch } from "node:fs";
import { extname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// fileURLToPath rather than import.meta.dirname: that property needs Node 20.11+,
// and Tauri itself only asks for 18+.
const ROOT = resolve(fileURLToPath(new URL(".", import.meta.url)), "ui");
const PORT = 1420;

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".woff2": "font/woff2",
};

// Injected into every HTML page so a change can reach the webviews.
const RELOAD = `<script>new EventSource("/__reload").onmessage = () => location.reload();</script>`;

/** Open reload connections, one per window. */
const clients = new Set();

const server = createServer(async (req, res) => {
  const path = decodeURIComponent(new URL(req.url, "http://localhost").pathname);

  if (path === "/__reload") {
    res.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-store",
      connection: "keep-alive",
    });
    res.write(": connected\n\n");
    clients.add(res);
    req.on("close", () => clients.delete(res));
    return;
  }

  const file = resolve(ROOT, `.${path}${path.endsWith("/") ? "index.html" : ""}`);
  if (!file.startsWith(ROOT)) {
    res.writeHead(403).end("forbidden");
    return;
  }

  try {
    const raw = await readFile(file);
    const type = TYPES[extname(file)] ?? "application/octet-stream";
    const body = type.startsWith("text/html")
      ? Buffer.from(raw.toString("utf8").replace("</body>", `${RELOAD}</body>`))
      : raw;
    console.log(`[ui] ${path}`);
    res.writeHead(200, { "content-type": type, "cache-control": "no-store" }).end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

// Saves arrive in bursts (editor writes, then renames), so debounce briefly.
let timer;
watch(ROOT, { recursive: true }, () => {
  clearTimeout(timer);
  timer = setTimeout(() => {
    console.log(`[ui] changed — reloading ${clients.size} window(s)`);
    for (const client of clients) client.write("data: reload\n\n");
  }, 80);
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`[ui] serving ${ROOT} on http://127.0.0.1:${PORT}`);
});
