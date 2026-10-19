//! Lists MIDI ports, opens the first input whose name contains the argument, and prints every
//! event with its backend time, host arrival time and the gaps between successive events.
//!
//! ```text
//! cargo run -p soundcraft-midi-io --example midi_monitor -- KeyStep [seconds]
//! ```
//!
//! Runs until Enter is pressed, or for `seconds` if given, then prints the port's counters.
//! It never sends anything to an output.

use soundcraft_midi_io::{InputConfig, MidiIo};
use std::io::BufRead;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let wanted = args.next().unwrap_or_default();
    let seconds: Option<f64> = args.next().and_then(|s| s.parse().ok());

    let io = MidiIo::new("SoundCraft midi_monitor");
    println!("backend: {}", io.backend().as_str());
    let inputs = io.list_inputs()?;
    for p in &inputs {
        println!("  in : {:?} (key: {:?} #{})", p.display_name, p.key.name, p.key.ordinal);
    }
    for p in io.list_outputs()? {
        println!("  out: {:?}", p.display_name);
    }
    let Some(port) = inputs.iter().find(|p| p.key.name.to_lowercase().contains(&wanted.to_lowercase())) else {
        return Err(format!("no input port contains {wanted:?}").into());
    };

    let opened = Instant::now();
    let (handle, mut consumer) = io.open_input(&port.key, InputConfig::default())?;
    println!("opened {:?} in {:.1} ms; play something (Enter to stop)", port.display_name, opened.elapsed().as_secs_f64() * 1e3);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        // EOF (no console attached) is not a request to stop.
        if std::io::stdin().lock().read_line(&mut line).is_ok_and(|n| n > 0) {
            let _ = tx.send(());
        }
    });
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let mut watcher = io.watcher()?;
    let mut last: Option<(u64, u64)> = None;
    let mut next_poll = Instant::now();
    loop {
        while let Some(e) = consumer.pop() {
            let (db, dh) = match last {
                Some((b, h)) => (e.backend_time_us as i128 - b as i128, e.host_arrival_ns as i128 - h as i128),
                None => (0, 0),
            };
            println!(
                "#{:<6} {:<9} backend {:>12} us  host {:>15} ns  Δbackend {:>8} us  Δhost {:>12} ns",
                e.seq,
                hex(e.message()),
                e.backend_time_us,
                e.host_arrival_ns,
                db,
                dh
            );
            last = Some((e.backend_time_us, e.host_arrival_ns));
        }
        if Instant::now() >= next_poll {
            for change in watcher.poll()? {
                println!("port change: {change:?}");
            }
            next_poll = Instant::now() + Duration::from_millis(500);
        }
        if rx.try_recv().is_ok() || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let counters = handle.counters();
    let disconnected = handle.is_disconnected();
    let closing = Instant::now();
    handle.close();
    println!("closed in {:.1} ms; consumer sees closed: {}", closing.elapsed().as_secs_f64() * 1e3, consumer.is_closed());
    println!("disconnected: {disconnected}");
    println!("{counters:#?}");
    Ok(())
}
