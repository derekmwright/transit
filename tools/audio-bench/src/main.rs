use audio_bench::cli::{Command, USAGE, parse};
use audio_bench::{list_devices, report, run};
use std::process::ExitCode;

fn main() -> ExitCode {
    let result = parse(std::env::args().skip(1)).and_then(|command| match command {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::List(system) => list_devices(system.as_deref()).map(|text| print!("{text}")),
        Command::Run(config) => {
            let row = run(&config)?;
            let rows = [row];
            print!("{}", report::table(&rows));
            if let Some(path) = &config.out {
                report::write(path, &rows)?;
                eprintln!("wrote {}", path.display());
            }
            Ok(())
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("audio-bench: {error}");
            ExitCode::FAILURE
        }
    }
}
