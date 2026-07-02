// rowdiet wasm loader — the ~100-line hand-rolled glue the plan called for.
// Contract (wasm/README.md "Browser loader contract"): reactor via wasi.initialize(), never
// start(); packed-u64 return from rowdiet_lint; FRESH memory views after every call (the parser
// may grow memory); WASIProcExit means the parser aborted, not a normal return.

import { WASI, WASIProcExit, File, OpenFile, ConsoleStdout } from "./vendor/browser_wasi_shim/index.js";

export async function initRowdiet(wasmBytes) {
  const wasi = new WASI([], [], [
    new OpenFile(new File([])),
    ConsoleStdout.lineBuffered((line) => console.log("[rowdiet stdout]", line)),
    ConsoleStdout.lineBuffered((line) => console.warn("[rowdiet stderr]", line)),
  ]);
  const { instance } = await WebAssembly.instantiate(wasmBytes, {
    wasi_snapshot_preview1: wasi.wasiImport,
  });
  wasi.initialize(instance);
  const exports = instance.exports;
  function lint(inputObject) {
    const input = new TextEncoder().encode(JSON.stringify(inputObject));
    const inPtr = exports.rowdiet_alloc(input.length);
    new Uint8Array(exports.memory.buffer).set(input, inPtr);
    let packed;
    try {
      packed = exports.rowdiet_lint(inPtr, input.length);
    } catch (e) {
      exports.rowdiet_free(inPtr, input.length);
      if (e instanceof WASIProcExit) {
        return { error: `parser aborted (exit ${e.code})` };
      }
      throw e;
    }
    const outPtr = Number(packed >> 32n);
    const outLen = Number(packed & 0xffffffffn);
    const out = new Uint8Array(exports.memory.buffer).slice(outPtr, outPtr + outLen);
    exports.rowdiet_free(inPtr, input.length);
    exports.rowdiet_free(outPtr, outLen);
    return JSON.parse(new TextDecoder().decode(out));
  }
  return { lint };
}
