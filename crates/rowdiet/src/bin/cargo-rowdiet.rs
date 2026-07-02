use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    // Invoked as `cargo rowdiet ...`: cargo passes the subcommand name as the first argument.
    if args.get(1).map(String::as_str) == Some("rowdiet") {
        args.remove(1);
    }
    rowdiet::cli_main(args)
}
