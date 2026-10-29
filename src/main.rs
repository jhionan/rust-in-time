use clap::Parser;

fn main() {
    let cli = match rust_in_time::cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() { 64 } else { 0 };
            let _ = error.print();
            std::process::exit(code);
        }
    };
    if let Err(error) = rust_in_time::cli::run(cli) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
