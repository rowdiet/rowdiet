use std::process::ExitCode;

fn main() -> ExitCode {
    rowdiet::cli_main(std::env::args())
}
