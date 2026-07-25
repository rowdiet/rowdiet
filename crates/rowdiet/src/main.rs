//! The `rowdiet` binary: hands argv to [`rowdiet::cli_main`], which is the whole program.

use std::process::ExitCode;

fn main() -> ExitCode {
    rowdiet::cli_main(std::env::args())
}
