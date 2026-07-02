// Headless verification of the browser loader path under node: same shim, same loader, same
// module the page ships. Run web/build.sh first, then: node web/smoke.mjs
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { initRowdiet } from "./loader.js";

const here = dirname(fileURLToPath(import.meta.url));
const bytes = readFileSync(join(here, "rowdiet.wasm"));
const api = await initRowdiet(bytes);
const out = api.lint({
  sources: [{
    name: "V1__m.sql",
    sql: "DO $$ BEGIN NULL; END $$; CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);",
  }],
  fail_over: 0,
});
const t = out.analysis.tables[0];
const checks = [
  ["parser is pg-exact", out.parser === "pg-exact"],
  ["gate exceeded", out.gate_exceeded === true],
  ["avoidable 8", t.avoidable_bytes_per_row === 8],
  ["footprint 56→48", t.current.footprint === 56 && t.suggested.footprint === 48],
  ["DO block parsed (no notes)", out.analysis.notes.length === 0],
];
const err = api.lint({ sources: [{ name: "x.sql", sql: "CREATE TABLE broken (" }] });
checks.push(["parse error surfaces as note", err.analysis.notes.length === 1]);
const reuse = api.lint({ sources: [{ name: "y.sql", sql: "CREATE TABLE ok (a bigint NOT NULL);" }] });
checks.push(["instance survives for reuse", reuse.analysis.tables.length === 1]);
let failed = 0;
for (const [name, ok] of checks) {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}`);
  if (!ok) failed += 1;
}
if (failed > 0) {
  console.error(JSON.stringify(out, null, 2).slice(0, 2000));
  process.exit(1);
}
console.log("loader smoke green");
