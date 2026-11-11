// SPDX-License-Identifier: GPL-3.0-only
//! Sotto — Phase 1 command-line engine.
//!
//! Captures the chosen microphone, removes noise with RNNoise, and writes the result to
//! VB-Cable ("CABLE Input"). Prints processing time, CPU, and RAM once per second.

mod denoise;
mod devices;
mod dfn;
mod dsp;
mod engine;
mod params;
mod stats;

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::denoise::{Denoiser, Off, Rnnoise};
use crate::devices::Dir;
use crate::dsp::EngineSlot;
use crate::params::Params;
use crate::stats::Stats;

const USAGE: &str = "\
Sotto — just your voice.

USAGE:
  sotto --list
  sotto --input \"<mic name>\" [--output \"<VB-Cable name>\"] [--engine rnnoise|dfn|off] [--seconds N] [--quiet]

OPTIONS:
  --list            Show microphones and VB-Cable outputs, then exit
  --input NAME      Microphone to clean (full name or a unique part of it)
  --output NAME     VB-Cable output (default: \"CABLE Input\"). Only VB-Audio devices are allowed.
  --engine NAME     rnnoise (default, \"Light — for gaming\"),
                    dfn (DeepFilterNet, \"High quality — for recording & calls\"),
                    or off (passthrough)
  --seconds N       Stop automatically after N seconds (for measurements)
  --quiet           Don't print the once-a-second report (type \"stats\" to see it)

While running, type a command and press Enter (\"help\" lists them, \"q\" stops).";

const COMMANDS: &str = "\
Commands (type, then Enter):
  engine rnnoise     Light — for gaming (default)
  engine dfn         DeepFilterNet: High quality — for recording & calls
  engine off         no denoising (for before/after comparison)
  hpf on|off         high-pass filter (removes rumble below ~80 Hz)
  gate on|off        noise gate (silence between words)
  gate -45           gate threshold in dBFS (-80 … -10; lower = opens more easily)
  agc on|off         auto gain (steadies your level)
  agc max 12         most the auto gain may boost a quiet voice, in dB (0 … 24, default 12)
  postfilter on|off  DeepFilterNet only: stronger noise removal (voice may sound thinner)
  limiter on|off     limiter (never louder than -1 dBFS)
  strength 0-100     denoise mix: 0 = original, 100 = fully cleaned
  status             show current settings
  stats              show the latest performance line
  report on|off      print that line every second (off = easier typing)
  q                  stop";

/// What the main loop should do after a typed command.
enum Cmd {
    Continue,
    Quit,
    ShowStats,
    Report(bool),
    Engine(EngineKind),
    /// Turn DeepFilterNet's post-filter on/off. Main stores the setting only once it
    /// is applied, so the flag always matches the running model.
    PostFilter(bool),
    ShowStatus,
}

/// Apply one typed command.
fn handle_command(line: &str, p: &Params) -> Cmd {
    let words: Vec<&str> = line.split_whitespace().collect();
    let toggle = |name: &str, flag: &std::sync::atomic::AtomicBool, v: &str| match v {
        "on" | "off" => {
            flag.store(v == "on", Relaxed);
            println!("> {name} {v}");
        }
        _ => println!("> use: {name} on|off"),
    };
    match words.as_slice() {
        ["q" | "quit" | "exit"] => return Cmd::Quit,
        ["stats"] => return Cmd::ShowStats,
        ["report", v @ ("on" | "off")] => return Cmd::Report(*v == "on"),
        ["engine", v] => match EngineKind::parse(v) {
            Some(k) => return Cmd::Engine(k),
            None => println!("> use: engine rnnoise|dfn|off"),
        },
        [] | ["help" | "?"] => println!("{COMMANDS}"),
        ["status"] => return Cmd::ShowStatus,
        ["hpf", v] => toggle("hpf", &p.hpf, v),
        ["gate", v @ ("on" | "off")] => toggle("gate", &p.gate, v),
        ["gate", v] => match v.parse::<f32>() {
            Ok(db) => {
                p.set_gate_threshold_db(db);
                println!("> gate threshold {:.1} dBFS", p.gate_threshold_db());
            }
            Err(_) => println!("> use: gate on|off  or  gate -45"),
        },
        ["agc", "max", v] => match v.trim_end_matches("db").parse::<f32>() {
            Ok(db) => {
                p.set_agc_max_db(db);
                println!("> agc max +{:.1} dB", p.agc_max_db());
            }
            Err(_) => println!("> use: agc max 12"),
        },
        ["agc", v] => toggle("agc", &p.agc, v),
        ["postfilter" | "pf", v @ ("on" | "off")] => return Cmd::PostFilter(*v == "on"),
        ["postfilter" | "pf", _] => println!("> use: postfilter on|off"),
        ["limiter" | "lim", v] => toggle("limiter", &p.limiter, v),
        ["strength", v] => match v.trim_end_matches('%').parse::<f32>() {
            Ok(pct) => {
                p.set_strength(pct / 100.0);
                println!("> strength {:.0}%", p.strength() * 100.0);
            }
            Err(_) => println!("> use: strength 0-100"),
        },
        _ => println!("> unknown command \"{line}\" — type help"),
    }
    Cmd::Continue
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn print_status(p: &Params, engine: EngineKind) {
    let s = |b: &std::sync::atomic::AtomicBool| if b.load(Relaxed) { "on" } else { "off" };
    println!(
        "> engine {} | hpf {} | strength {:.0}% | gate {} at {:.1} dBFS | agc {} (max +{:.1} dB) | limiter {} | postfilter {}",
        engine.name(),
        s(&p.hpf),
        p.strength() * 100.0,
        s(&p.gate),
        p.gate_threshold_db(),
        s(&p.agc),
        p.agc_max_db(),
        s(&p.limiter),
        s(&p.dfn_postfilter),
    );
}

#[derive(Clone, Copy, PartialEq)]
enum EngineKind {
    Rnnoise,
    Dfn,
    Off,
}

impl EngineKind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "rnnoise" | "light" => Some(Self::Rnnoise),
            "dfn" | "deepfilternet" | "hq" => Some(Self::Dfn),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rnnoise => "rnnoise",
            Self::Dfn => "deepfilternet",
            Self::Off => "off",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Rnnoise => "Light — for gaming",
            Self::Dfn => "High quality — for recording & calls",
            Self::Off => "passthrough",
        }
    }
}

struct Args {
    list: bool,
    input: Option<String>,
    output: Option<String>,
    engine: EngineKind,
    seconds: Option<u64>,
    quiet: bool,
}

/// Minimal hand-written argument parser (no `clap` crate needed for five flags).
fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        list: false,
        input: None,
        output: None,
        engine: EngineKind::Rnnoise,
        seconds: None,
        quiet: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--list" => a.list = true,
            "--input" => a.input = Some(value()?),
            "--output" => a.output = Some(value()?),
            "--engine" => {
                let v = value()?;
                a.engine = EngineKind::parse(&v)
                    .ok_or_else(|| format!("unknown engine \"{v}\" (rnnoise, dfn, off)"))?;
            }
            "--seconds" => {
                a.seconds = Some(value()?.parse().map_err(|_| "--seconds needs a number")?)
            }
            "--quiet" => a.quiet = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option \"{other}\"")),
        }
    }
    Ok(a)
}

/// Build an engine. DeepFilterNet is loaded only here, i.e. only when selected
/// ("lazy loading"); it takes a moment, so say so.
fn make_engine(kind: EngineKind, p: &Params) -> Result<EngineSlot, String> {
    let den: Box<dyn Denoiser> = match kind {
        EngineKind::Rnnoise => Box::new(Rnnoise::new()),
        EngineKind::Off => Box::new(Off),
        EngineKind::Dfn => {
            println!("> loading DeepFilterNet…");
            let t0 = Instant::now();
            let d = dfn::DeepFilter::start(p.dfn_postfilter.load(Relaxed))?;
            println!(
                "> DeepFilterNet ready in {:.0} ms (adds {:.0} ms latency)",
                t0.elapsed().as_secs_f64() * 1e3,
                d.latency() as f64 / 48.0
            );
            Box::new(d)
        }
    };
    Ok(EngineSlot::new(den))
}

/// Drop engines the worker has finished with. Doing it here, on the main thread,
/// frees their memory (DeepFilterNet's model thread is stopped and its model released)
/// without touching the audio path.
fn drain_retired(worker: &engine::Worker) {
    while let Ok(old) = worker.retired.try_recv() {
        let name = old.name();
        drop(old);
        if name == "deepfilternet" {
            let (_, private) = stats::process_memory().unwrap_or((0, 0));
            println!(
                "> DeepFilterNet unloaded (private memory now {:.1} MB)",
                private as f64 / 1_048_576.0
            );
        }
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
    println!("Engine: {} ({})", args.engine.name(), args.engine.label());
    let params = Arc::new(Params::new());
    let first_engine = make_engine(args.engine, &params)?;
    // The engine last asked for; `switch_seq` numbers the switch requests.
    let mut engine_kind = args.engine;
    let mut switch_seq: u64 = 0;

    // Typed commands. A separate thread blocks reading lines and sends each one on the
    // channel (like `queue.Queue` between Python threads).
    let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if cmd_tx.send(line).is_err() {
                return;
            }
        }
        // No console attached (stdin closed): keep `cmd_tx` alive and never send, so
        // only --seconds or Ctrl+C stops the program.
        loop {
            std::thread::park();
        }
    });

    let stats = Arc::new(Stats::new());
    let worker = engine::Worker::spawn(first_engine, Arc::clone(&params), Arc::clone(&stats))?;
    let mut engine: Option<engine::Engine> = None;
    let started = Instant::now();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let mut last_wall = Instant::now();
    let mut last_cpu = stats::process_cpu_time().unwrap_or_default();
    let mut lost_reported = false;
    let mut drift = stats::DriftEstimator::default();
    let mut drift_start = Instant::now();
    let mut report_every_second = !args.quiet;
    let mut last_report = String::new();

    print_status(&params, engine_kind);
    println!("Running. Type \"help\" for commands, \"q\" to stop.\n");
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
                    stats.gate_open_frames.store(0, Relaxed);
                    stats.limit_cdb.store(0, Relaxed);
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

        // Handle typed commands for up to one second, then report.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut quit = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match cmd_rx.recv_timeout(left) {
                Ok(line) => match handle_command(line.trim(), &params) {
                    Cmd::Continue => {}
                    Cmd::Quit => {
                        quit = true;
                        break;
                    }
                    Cmd::ShowStats if last_report.is_empty() => println!("> no data yet"),
                    Cmd::ShowStats => println!("{last_report}"),
                    Cmd::ShowStatus => print_status(&params, engine_kind),
                    Cmd::PostFilter(on) if on == params.dfn_postfilter.load(Relaxed) => {
                        println!("> postfilter already {}", on_off(on))
                    }
                    Cmd::PostFilter(on) if engine_kind != EngineKind::Dfn => {
                        params.dfn_postfilter.store(on, Relaxed);
                        println!(
                            "> postfilter {} (applies when you select engine dfn)",
                            on_off(on)
                        )
                    }
                    // One switch at a time (they take ~0.1–0.2 s). So at most two models
                    // run at once: the old and the new one, during the warm-up and 30 ms
                    // crossfade of a switch.
                    Cmd::Engine(_) | Cmd::PostFilter(_)
                        if stats.switch_done.load(Relaxed) != switch_seq =>
                    {
                        println!("> still switching — try again in a moment")
                    }
                    Cmd::Engine(k) if k == engine_kind => {
                        println!("> already using {}", k.name())
                    }
                    // Changing the post-filter rebuilds DeepFilterNet and crossfades to
                    // it, exactly like an engine switch (no click).
                    c @ (Cmd::Engine(_) | Cmd::PostFilter(_)) => {
                        let (k, pf) = match c {
                            Cmd::Engine(k) => (k, None),
                            Cmd::PostFilter(on) => (EngineKind::Dfn, Some(on)),
                            _ => (engine_kind, None),
                        };
                        // The new model is built with the new setting; restored on failure.
                        let old_pf = pf.map(|on| params.dfn_postfilter.swap(on, Relaxed));
                        // Free a just-retired model before loading another one.
                        drain_retired(&worker);
                        match make_engine(k, &params) {
                            Ok(slot) => {
                                switch_seq += 1;
                                worker.set_engine(slot, switch_seq)?;
                                engine_kind = k;
                                match pf {
                                    Some(on) => println!("> postfilter {}", on_off(on)),
                                    None => {
                                        println!("> switching to {} ({})", k.name(), k.label())
                                    }
                                }
                            }
                            Err(e) => {
                                if let Some(old) = old_pf {
                                    params.dfn_postfilter.store(old, Relaxed);
                                }
                                println!("> could not switch: {e}")
                            }
                        }
                    }
                    Cmd::Report(on) => {
                        report_every_second = on;
                        println!("> report {}", if on { "on" } else { "off" });
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    quit = true;
                    break;
                }
            }
        }
        if quit {
            break;
        }
        drain_retired(&worker);
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
            let gate_pct = if t.frames > 0 {
                stats.gate_open_frames.swap(0, Relaxed) as f64 / t.frames as f64 * 100.0
            } else {
                0.0
            };
            let agc_db = f32::from_bits(stats.agc_gain_db.load(Relaxed));
            let limit_db = stats.limit_cdb.swap(0, Relaxed) as f32 / 100.0;
            let drift_txt = match drift.ppm(engine::SAMPLE_RATE as f64) {
                Some((ppm, se)) if drift.points() >= 30 => format!("{ppm:+6.1} ±{se:.1} ppm"),
                _ => "  (measuring)".into(),
            };
            last_report = format!(
                "{:<7} | frames {:>3} | proc p50 {:>5.3} ms  p99 {:>5.3} ms  max {:>6.3} ms | \
                 CPU {:>5.2}% | RAM {:>5.1} MB (private {:>5.1}) | buffer avg {:>4.1} ms | \
                 gate open {gate_pct:>3.0}% | agc {agc_db:+5.1} dB | limit {limit_db:>4.1} dB | \
                 drift {drift_txt} | overflow {} underrun {} resync {} | engine errors {}",
                // The engine actually heard right now (published by the worker).
                match stats.active_engine.load(Relaxed) {
                    1 => "rnnoise",
                    2 => "dfn",
                    3 => "off",
                    _ => "?",
                },
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
                stats.engine_errors.load(Relaxed),
            );
            if report_every_second {
                println!("{last_report}");
            }
        }
    }

    drop(engine); // stop streams first, then the worker they feed
    worker.stop();
    println!("Stopped.");
    Ok(())
}
