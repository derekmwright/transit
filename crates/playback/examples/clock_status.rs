//! Explicitly opt-in hardware diagnostic: silence through the actual Player,
//! clock/telemetry snapshots on main, no profile access or recording files.
#![forbid(unsafe_code)]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use soundcraft_model::Session;
    use soundcraft_playback::{AudioConfig, Player};
    use std::{sync::Arc, time::Duration};

    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--asio-focusrite") {
        return Err("explicit hardware opt-in required: --asio-focusrite [buffer frames]".into());
    }
    if !cfg!(feature = "asio") {
        return Err("build this diagnostic with --features asio".into());
    }
    let buffer: u32 = args.next().as_deref().unwrap_or("256").parse()?;
    let config = AudioConfig {
        system: Some("ASIO".into()),
        output: Some("Focusrite USB ASIO".into()),
        input: None,
        sample_rate: Some(48_000),
        buffer_size: Some(buffer),
        ..Default::default()
    };
    for reopen in 0..2 {
        let player = Player::with_config(Arc::new(Session::default()), &config);
        println!(
            "open={reopen} device={} rate={} buffer={:?} silent={}",
            player.device_name, player.device_rate, player.device_buffer, player.silent
        );
        if player.silent {
            return Err("physical output unavailable".into());
        }
        let mut input = Some(player.open_input(&config)?);
        for poll in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            let _ = player.idle()?;
            let output = player.clock_report();
            let capture = input.as_mut().and_then(|recorder| recorder.clock_report());
            println!("open={reopen} poll={poll} output={output:?} input={capture:?}");
        }
        println!("telemetry={:?}", player.callback_telemetry());
        player.close_input(&mut input)?;
        drop(player);
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {}
