// Build web/rowdiet-standalone.html: one double-clickable file that works from file:// —
// all JS bundled inline (esbuild via npx) and the wasm embedded as base64.
// Run web/build.sh first so web/rowdiet.wasm exists. Usage: node web/build-standalone.mjs
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const bundle = execFileSync(
  "npx",
  ["--yes", "esbuild", join(here, "app.js"), "--bundle", "--format=esm", "--minify"],
  { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
);
const wasmB64 = readFileSync(join(here, "rowdiet.wasm")).toString("base64");
const html = readFileSync(join(here, "index.html"), "utf8");
const marker = '<script type="module" src="./app.js"></script>';
if (!html.includes(marker)) throw new Error("index.html script marker not found");
const inline = [
  `<script>globalThis.ROWDIET_WASM_B64 = ${JSON.stringify(wasmB64)};</script>`,
  `<script type="module">${bundle}</script>`,
].join("\n");
const out = join(here, "rowdiet-standalone.html");
writeFileSync(out, html.replace(marker, inline));
console.log(`${out}: ${(statSync(out).size / 1e6).toFixed(1)} MB (double-clickable, works from file://)`);
