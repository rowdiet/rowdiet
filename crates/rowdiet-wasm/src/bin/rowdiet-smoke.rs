//! stdin → lint_json → stdout: the wasmtime smoke vehicle for the wasip1 build (a *command*
//! module, unlike the cdylib reactor — this one runs via plain `wasmtime run`).

use std::io::Read as _;

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).expect("read stdin");
    println!("{}", rowdiet_wasm::lint_json(&input));
}
