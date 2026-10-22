use audio_bench::cli::{Command, USAGE, parse};
use audio_bench::{calibration, list_devices, report, run};
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
            match &rows[0].calibration {
                Some(Ok(figures)) => {
                    println!("{}", figures.describe());
                    println!("{}", calibration::input_peak_line(&rows[0].measurements.input_peaks, config.request.in_ch));
                }
                Some(Err(why)) => println!("calibration: none ({why})"),
                None => {}
            }
            if let Some(path) = &config.settings {
                eprintln!("calibration merged into {}", path.display());
            }
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
