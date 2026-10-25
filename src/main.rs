mod audio;
mod config;
mod model;
mod runtime;
mod ui;

use std::sync::{Arc, atomic::AtomicBool};

fn main() {
    if let Err(error) = run() {
        eprintln!("pwaudio: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let daemon = match args.as_slice() {
        [] => false,
        [arg] if arg == "daemon" => true,
        [arg] if arg == "--version" || arg == "-V" => {
            println!("pwaudio {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        [arg] if arg == "--help" || arg == "-h" => {
            println!(
                "pwaudio - terminal audio control for PipeWire

Usage: pwaudio [daemon]

  pwaudio          Open the audio panel
  pwaudio daemon   Enforce persistent playback mute in the background

Config: $XDG_CONFIG_HOME/pwaudio/config.toml (or ~/.config/pwaudio/config.toml)
Requires: pw-dump, wpctl, pw-metadata

Keys: j/k navigate, h/l volume, m mute, M persistent mute, d default,
      Tab section, Enter details, t target, p profile, r refresh, q quit"
            );
            return Ok(());
        }
        _ => return Err("unknown arguments. Use pwaudio --help".into()),
    };
    let stop = Arc::new(AtomicBool::new(false));

    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, stop.clone()).map_err(|e| e.to_string())?;
    }

    let paths = config::Paths::discover()?;
    let _daemon_lock = if daemon {
        Some(paths.lock_daemon()?)
    } else {
        None
    };
    let worker = runtime::Worker::start(paths, stop, daemon);

    if daemon {
        worker.wait();
        Ok(())
    } else {
        ui::run(&worker)
    }
}
