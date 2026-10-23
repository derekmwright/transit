use audio_bench::cli::{Command, USAGE, parse};
use audio_bench::{calibration, command_line, list_devices, report, run, run_midi_roundtrip};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// `--midi-roundtrip`: install the Ctrl+C handler (the first press ends the run and releases the
/// note; a second one exits at once), measure, print, write the JSON.
fn midi_roundtrip(config: &audio_bench::Config, midi: &audio_bench::midi_roundtrip::Options, args: &[String]) -> Result<(), String> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
    })
    .map_err(|e| format!("the Ctrl+C handler could not be installed, so a note could be left sounding: {e}"))?;
    let command = format!("cargo run -p audio-bench --features native-asio --release -- {}", command_line(args));
    let report = run_midi_roundtrip(config, midi, &interrupted)?;
    for line in report.lines() {
        println!("{line}");
    }
    println!("command: {command}");
    if let Some(path) = &midi.json {
        let json = report.json(&command, &report::today(), &report::machine());
        let text = serde_json::to_string_pretty(&json).map_err(|e| format!("JSON: {e}"))?;
        std::fs::write(path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("wrote {}", path.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = parse(args.clone()).and_then(|command| match command {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::List(system) => list_devices(system.as_deref()).map(|text| print!("{text}")),
        Command::MidiRoundTrip { config, midi } => midi_roundtrip(&config, &midi, &args),
        Command::Run(config) => {
            let row = run(&config)?;
            let rows = [row];
            print!("{}", report::table(&rows));
            for line in rows[0].stats_lines() {
                println!("{line}");
            }
            for line in rows[0].asio_calibration_lines() {
                println!("{line}");
            }
            match &rows[0].calibration {
                Some(Ok(figures)) => {
                    println!("{}", figures.describe());
                    println!("{}", calibration::input_peak_line(&rows[0].measurements.input_peaks, config.request.in_ch));
                }
                Some(Err(why)) => println!("calibration: none ({why})"),
                None => {}
            }
            if let Some(line) = rows[0].failed_detection_peaks() {
                println!("{line}");
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
