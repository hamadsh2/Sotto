// SPDX-License-Identifier: GPL-3.0-only
//! Sotto — Phase 1 command-line engine.
//!
//! Captures the chosen microphone, removes noise with RNNoise, and writes the result to
//! VB-Cable ("CABLE Input"). Prints processing time, CPU, and RAM once per second.

mod denoise;
mod devices;
mod engine;
mod stats;

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::denoise::{Denoiser, Off, Rnnoise};
use crate::devices::Dir;
use crate::stats::Stats;

const USAGE: &str = "\
Sotto — just your voice.

USAGE:
  sotto --list
  sotto --input \"<mic name>\" [--output \"<VB-Cable name>\"] [--engine rnnoise|off] [--seconds N]

OPTIONS:
  --list            Show microphones and VB-Cable outputs, then exit
  --input NAME      Microphone to clean (full name or a unique part of it)
  --output NAME     VB-Cable output (default: \"CABLE Input\"). Only VB-Audio devices are allowed.
  --engine NAME     rnnoise (default, \"Light — for gaming\") or off (passthrough)
  --seconds N       Stop automatically after N seconds (for measurements)

Press Enter to stop.";

#[derive(Clone, Copy, PartialEq)]
enum EngineKind {
    Rnnoise,
    Off,
}

struct Args {
    list: bool,
    input: Option<String>,
    output: Option<String>,
    engine: EngineKind,
    seconds: Option<u64>,
}

/// Minimal hand-written argument parser (no `clap` crate needed for five flags).
fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        list: false,
        input: None,
        output: None,
        engine: EngineKind::Rnnoise,
        seconds: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--list" => a.list = true,
            "--input" => a.input = Some(value()?),
            "--output" => a.output = Some(value()?),
            "--engine" => {
                a.engine = match value()?.as_str() {
                    "rnnoise" => EngineKind::Rnnoise,
                    "off" => EngineKind::Off,
                    other => return Err(format!("unknown engine \"{other}\"")),
                }
            }
            "--seconds" => {
                a.seconds = Some(value()?.parse().map_err(|_| "--seconds needs a number")?)
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option \"{other}\"")),
        }
    }
    Ok(a)
}

fn make_denoiser(kind: EngineKind) -> Box<dyn Denoiser> {
    match kind {
        EngineKind::Rnnoise => Box::new(Rnnoise::new()),
        EngineKind::Off => Box::new(Off),
    }
}

fn main() {
    // Like `sys.exit(main())` in Python: run, and turn an error into exit code 1.
    if let Err(e) = run() {
        if e.is_empty() {
            println!("{USAGE}");
        } else {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let host = cpal::default_host(); // WASAPI on Windows

    if args.list {
        return devices::print_list(&host);
    }

    if devices::default_playback_is_cable(&host) {
        println!(
            "Heads up: \"CABLE Input\" is set as your Windows default playback device, so \
             your games and music are going into the cable instead of your headphones.\n\
             To fix it: Settings → System → Sound → Output → pick your headphones or speakers.\n\
             Sotto will not change this for you.\n"
        );
    }

    let Some(input_query) = args.input.as_deref() else {
        devices::print_list(&host)?;
        return Err("choose a microphone with --input \"<name>\"".into());
    };

    // Resolve once by name, then remember the exact Windows endpoint IDs. Reconnecting
    // only ever accepts those same IDs.
    let mic = devices::find_input(&host, input_query)?;
    let cable = devices::find_output(&host, args.output.as_deref())?;
    let (input_id, input_name) = (mic.id, mic.name);
    let (output_id, output_name) = (cable.id, cable.name);
    println!("Input : {input_name}");
    println!("Output: {output_name}");
    let label = match args.engine {
        EngineKind::Rnnoise => "Light — for gaming",
        EngineKind::Off => "passthrough",
    };
    let denoiser = make_denoiser(args.engine);
    println!("Engine: {} ({label})", denoiser.name());

    // Enter on stdin stops the program. A separate thread blocks on reading the line
    // and sends a message on the channel (like `queue.Queue` between Python threads).
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        match std::io::stdin().lock().lines().next() {
            Some(Ok(_)) => {
                let _ = stop_tx.send(());
            }
            // No console attached (stdin closed): keep `stop_tx` alive and never send,
            // so only --seconds or Ctrl+C stops the program.
            _ => loop {
                std::thread::park();
            },
        }
    });

    let stats = Arc::new(Stats::new());
    let worker = engine::Worker::spawn(denoiser, Arc::clone(&stats))?;
    let mut engine: Option<engine::Engine> = None;
    let started = Instant::now();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let mut last_wall = Instant::now();
    let mut last_cpu = stats::process_cpu_time().unwrap_or_default();
    let mut lost_reported = false;
    let mut drift = stats::DriftEstimator::default();
    let mut drift_start = Instant::now();

    println!("Running. Press Enter to stop.\n");
    loop {
        // (Re)start the streams if they aren't running. Only the two remembered endpoint
        // IDs are ever looked up; if either is missing we wait — no fallback device.
        if engine.is_none() {
            let attempt =
                devices::find_by_id(&host, Dir::Input, &input_id, &input_name).and_then(|i| {
                    let o = devices::find_by_id(&host, Dir::Output, &output_id, &output_name)?;
                    stats.input_failed.store(false, Relaxed);
                    stats.output_failed.store(false, Relaxed);
                    devices::no_panic("opening streams", || {
                        engine::start(&i, &o, &worker, Arc::clone(&stats))
                    })
                });
            match attempt {
                Ok(e) => {
                    drift = stats::DriftEstimator::default(); // new streams, new clocks
                    drift_start = Instant::now();
                    // Drop numbers left over from before a disconnect.
                    let _ = stats.take_timing();
                    let _ = stats.take_buffer_avg();
                    println!(
                        "Streams open: mic {}ch → 48 kHz mono → CABLE {}ch (shared mode)",
                        e.input_channels, e.output_channels
                    );
                    engine = Some(e);
                    lost_reported = false;
                }
                Err(e) if !lost_reported => {
                    println!("Waiting for devices: {e}");
                    lost_reported = true;
                }
                Err(_) => {}
            }
        }

        match stop_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if args
            .seconds
            .is_some_and(|s| started.elapsed() >= Duration::from_secs(s))
        {
            break;
        }

        if stats.worker_failed.load(Relaxed) {
            drop(engine);
            return Err("the denoise engine stopped unexpectedly".into());
        }
        // Only while streams are running: the flags stay set until the device is back,
        // and we report the loss once, not every second.
        if engine.is_some()
            && (stats.input_failed.load(Relaxed) || stats.output_failed.load(Relaxed))
        {
            let which = if stats.input_failed.load(Relaxed) {
                "Microphone"
            } else {
                "VB-Cable"
            };
            println!("{which} disconnected — stopping streams and waiting for it to come back.");
            engine = None; // drops (stops) both streams
            lost_reported = true;
            continue;
        }

        // ---- once-per-second report ----
        let now = Instant::now();
        let cpu = stats::process_cpu_time().unwrap_or_default();
        let wall = now.duration_since(last_wall).as_secs_f64();
        let cpu_pct = (cpu - last_cpu).as_secs_f64() / (wall * cores) * 100.0;
        (last_wall, last_cpu) = (now, cpu);

        if engine.is_some() {
            let t = stats.take_timing();
            let (ws, private) = stats::process_memory().unwrap_or((0, 0));
            let buf_ms = stats.take_buffer_avg().unwrap_or(0.0) / 48.0;
            let diff = stats.captured.load(Relaxed) as f64 - stats.rendered.load(Relaxed) as f64;
            drift.add(drift_start.elapsed().as_secs_f64(), diff);
            // Too few points → the slope is mostly jitter; wait 30 s before showing it.
            let drift_txt = match drift.ppm(engine::SAMPLE_RATE as f64) {
                Some((ppm, se)) if drift.points() >= 30 => format!("{ppm:+6.1} ±{se:.1} ppm"),
                _ => "  (measuring)".into(),
            };
            println!(
                "frames {:>3} | proc p50 {:>5.3} ms  p99 {:>5.3} ms  max {:>6.3} ms | \
                 CPU {:>5.2}% | RAM {:>5.1} MB (private {:>5.1}) | buffer avg {:>4.1} ms | \
                 drift {drift_txt} | overflow {} underrun {} resync {}",
                t.frames,
                t.p50_ms,
                t.p99_ms,
                t.max_ms,
                cpu_pct,
                ws as f64 / 1_048_576.0,
                private as f64 / 1_048_576.0,
                buf_ms,
                stats.overflows.load(Relaxed),
                stats.underruns.load(Relaxed),
                stats.resyncs.load(Relaxed),
            );
        }
    }

    drop(engine); // stop streams first, then the worker they feed
    worker.stop();
    println!("Stopped.");
    Ok(())
}
