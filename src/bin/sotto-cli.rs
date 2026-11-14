// SPDX-License-Identifier: GPL-3.0-only
//! Sotto command-line tool: the same engine as the window app, driven by typed
//! commands. Used for testing and measurements.
//!
//! Captures the chosen microphone, removes noise with RNNoise, and writes the result to
//! VB-Cable ("CABLE Input"). Prints processing time, CPU, and RAM once per second.

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sotto::control::{Change, Controller, EngineKind, Hear};
use sotto::params::{EqPreset, Params};
use sotto::tune::{self, Phase};
use sotto::{devices, engine, stats};

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
  eq on|off          tone EQ
  eq natural         EQ preset: natural (flat, default) | warm | clear | broadcast
  eq custom 2 -2 3   your own EQ in dB (each -6 … 6): body ~150 Hz, mud ~350 Hz, presence ~4 kHz
  limiter on|off     limiter (never louder than -1 dBFS)
  strength 0-100     denoise mix: 0 = original, 100 = fully cleaned
  tune               measure your room and voice (5 s quiet, then 5 s talking)
  tune apply         use the suggested gate threshold and auto-gain limit
  record             record 5 s of your cleaned voice (into memory only)
  play               play that clip once on your Windows playback device
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
    Tune,
    TuneApply,
    Record,
    Play,
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
        ["tune"] => return Cmd::Tune,
        ["tune", "apply"] => return Cmd::TuneApply,
        ["record"] => return Cmd::Record,
        ["play"] => return Cmd::Play,
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
        ["eq", v @ ("on" | "off")] => toggle("eq", &p.eq, v),
        ["eq", "custom", b, m, pr] => {
            match (b.parse::<f32>(), m.parse::<f32>(), pr.parse::<f32>()) {
                (Ok(b), Ok(m), Ok(pr)) => {
                    p.set_eq_custom([b, m, pr]);
                    println!("> eq {}", eq_text(p));
                }
                _ => println!("> use: eq custom <body> <mud> <presence>   e.g. eq custom 2 -2 3"),
            }
        }
        ["eq", v] => match EqPreset::parse(v) {
            Some(preset) => {
                p.set_eq_preset(preset);
                println!("> eq {}", eq_text(p));
            }
            None => {
                println!("> use: eq on|off, eq natural|warm|clear|broadcast, or eq custom 2 -2 3")
            }
        },
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

/// e.g. "clear (body +0.0, mud -3.0, presence +3.0 dB)".
fn eq_text(p: &Params) -> String {
    let [b, m, pr] = p.eq_gains();
    format!(
        "{} (body {b:+.1}, mud {m:+.1}, presence {pr:+.1} dB)",
        p.eq_preset().name()
    )
}

fn print_status(p: &Params, engine: EngineKind) {
    let s = |b: &std::sync::atomic::AtomicBool| if b.load(Relaxed) { "on" } else { "off" };
    println!(
        "> engine {} | hpf {} | strength {:.0}% | eq {} {} | gate {} at {:.1} dBFS | agc {} (max +{:.1} dB) | limiter {} | postfilter {}",
        engine.name(),
        s(&p.hpf),
        p.strength() * 100.0,
        s(&p.eq),
        eq_text(p),
        s(&p.gate),
        p.gate_threshold_db(),
        s(&p.agc),
        p.agc_max_db(),
        s(&p.limiter),
        s(&p.dfn_postfilter),
    );
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

    if args.list {
        return devices::print_list(&cpal::default_host());
    }
    // Only one Sotto may write to CABLE Input. `_claim` holds the shared lock until
    // `run` returns.
    let _claim = match sotto::instance::claim() {
        sotto::instance::Outcome::Taken => {
            return Err(
                "Sotto is already running (the window app or another sotto-cli). \
                        Quit it from its tray icon first."
                    .into(),
            );
        }
        sotto::instance::Outcome::Alone(c) => Some(c),
        sotto::instance::Outcome::Unknown => None,
    };
    let Some(input_query) = args.input.as_deref() else {
        devices::print_list(&cpal::default_host())?;
        return Err("choose a microphone with --input \"<name>\"".into());
    };

    if args.engine == EngineKind::Dfn {
        println!("> loading DeepFilterNet…");
    }
    let params = Arc::new(Params::new());
    let mut ctl = Controller::new(input_query, args.output.as_deref(), args.engine, params)?;
    if ctl.cable_is_default() {
        println!(
            "Heads up: \"CABLE Input\" is set as your Windows default playback device, so \
             your games and music are going into the cable instead of your headphones.\n\
             To fix it: Settings → System → Sound → Output → pick your headphones or speakers.\n\
             Sotto will not change this for you.\n"
        );
    }
    println!("Input : {}", ctl.input_name());
    println!("Output: {}", ctl.output_name());
    println!("Engine: {} ({})", args.engine.name(), args.engine.label());

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

    let started = Instant::now();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let mut last_wall = Instant::now();
    let mut last_cpu = stats::process_cpu_time().unwrap_or_default();
    let mut drift = stats::DriftEstimator::default();
    let mut drift_start = Instant::now();
    let mut generation = 0;
    let mut report_every_second = !args.quiet;
    let mut last_report = String::new();
    let print_notes = |ctl: &mut Controller| {
        for n in ctl.take_notes() {
            println!("{n}");
        }
    };

    let mut tuning: Option<tune::Session> = None;
    let mut suggestion: Option<tune::Suggestion> = None;

    print_status(&ctl.params, ctl.engine_kind());
    println!("Running. Type \"help\" for commands, \"q\" to stop.\n");
    loop {
        ctl.tick()?;
        print_notes(&mut ctl);
        if ctl.generation() != generation {
            generation = ctl.generation();
            drift = stats::DriftEstimator::default(); // new streams, new clocks
            drift_start = Instant::now();
        }

        // Handle typed commands for up to one second, then report.
        // While recording or playing, come back every 50 ms so the playback stream is
        // closed as soon as the clip ends.
        let wait = if matches!(ctl.hear(), Hear::Recording | Hear::Playing) {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(1)
        };
        let deadline = Instant::now() + wait;
        let mut quit = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match cmd_rx.recv_timeout(left) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    quit = true;
                    break;
                }
            };
            let result = match handle_command(line.trim(), &ctl.params) {
                Cmd::Continue => continue,
                Cmd::Quit => {
                    quit = true;
                    break;
                }
                Cmd::ShowStats => {
                    if last_report.is_empty() {
                        println!("> no data yet");
                    } else {
                        println!("{last_report}");
                    }
                    continue;
                }
                Cmd::ShowStatus => {
                    print_status(&ctl.params, ctl.engine_kind());
                    continue;
                }
                Cmd::Tune => {
                    if !ctl.is_running() {
                        println!("> the microphone isn't running");
                        continue;
                    }
                    tuning = Some(tune::Session::start(Arc::clone(&ctl.stats)));
                    suggestion = None;
                    println!("> tune: stay quiet for 5 s (leave your usual sounds on)…");
                    continue;
                }
                Cmd::Record => {
                    match ctl.record() {
                        Ok(()) => println!("> recording 5 s — talk now…"),
                        Err(e) => println!("> {e}"),
                    }
                    continue;
                }
                Cmd::Play => {
                    match ctl.play() {
                        Ok(()) => println!("> playing the clip once…"),
                        Err(e) => println!("> {e}"),
                    }
                    continue;
                }
                Cmd::TuneApply => {
                    match &suggestion {
                        Some(s) => {
                            ctl.params.set_gate_threshold_db(s.gate_db);
                            ctl.params.set_agc_max_db(s.agc_max_db);
                            println!(
                                "> applied: gate {:.0} dBFS, agc max +{:.0} dB",
                                s.gate_db, s.agc_max_db
                            );
                        }
                        None => println!("> nothing to apply — type \"tune\" first"),
                    }
                    continue;
                }
                Cmd::Report(on) => {
                    report_every_second = on;
                    println!("> report {}", on_off(on));
                    continue;
                }
                Cmd::Engine(k) => {
                    if k == EngineKind::Dfn && ctl.engine_kind() != k {
                        println!("> loading DeepFilterNet…");
                    }
                    ctl.set_engine(k).map(|c| match c {
                        Change::Started => format!("switching to {} ({})", k.name(), k.label()),
                        Change::AlreadySo => format!("already using {}", k.name()),
                        Change::Busy => "still switching — try again in a moment".into(),
                        Change::Stored => String::new(),
                    })
                }
                Cmd::PostFilter(on) => ctl.set_postfilter(on).map(|c| match c {
                    Change::Started => format!("postfilter {}", on_off(on)),
                    Change::AlreadySo => format!("postfilter already {}", on_off(on)),
                    Change::Busy => "still switching — try again in a moment".into(),
                    Change::Stored => {
                        format!(
                            "postfilter {} (applies when you select engine dfn)",
                            on_off(on)
                        )
                    }
                }),
            };
            print_notes(&mut ctl);
            match result {
                Ok(msg) => println!("> {msg}"),
                Err(e) => println!("> could not switch: {e}"),
            }
        }
        if quit {
            break;
        }
        if args
            .seconds
            .is_some_and(|s| started.elapsed() >= Duration::from_secs(s))
        {
            break;
        }

        let before = ctl.hear();
        ctl.poll_hear();
        print_notes(&mut ctl);
        match (before, ctl.hear()) {
            (Hear::Recording, Hear::Ready) => println!("> recorded — type \"play\" to hear it"),
            (Hear::Playing, Hear::Ready) => println!("> played"),
            _ => {}
        }

        if let Some(t) = tuning.as_mut() {
            let was_quiet = *t.phase() == Phase::Quiet;
            match t.tick().clone() {
                Phase::Talk if was_quiet => println!("> tune: now talk normally for 5 s…"),
                Phase::Done(r) => {
                    match r {
                        Ok(s) => {
                            println!(
                                "> tune: noise {:.0} dB, voice {:.0} dB → gate {:.0} dBFS (now {:.0}), agc max +{:.0} dB (now +{:.0}){}{}{}",
                                s.noise_db,
                                s.voice_db,
                                s.gate_db,
                                ctl.params.gate_threshold_db(),
                                s.agc_max_db,
                                ctl.params.agc_max_db(),
                                if s.noisy { " | noisy room" } else { "" },
                                if s.too_quiet { " | mic too quiet" } else { "" },
                                if s.clipping { " | mic clipping" } else { "" },
                            );
                            println!("> type \"tune apply\" to use these values");
                            suggestion = Some(s);
                        }
                        Err(f) => println!("> tune: {}", f.message()),
                    }
                    tuning = None;
                }
                _ => {}
            }
        }

        // ---- once-per-second report ----
        let now = Instant::now();
        let cpu = stats::process_cpu_time().unwrap_or_default();
        let wall = now.duration_since(last_wall).as_secs_f64();
        let cpu_pct = (cpu - last_cpu).as_secs_f64() / (wall * cores) * 100.0;
        (last_wall, last_cpu) = (now, cpu);

        if ctl.is_running() {
            let st = &ctl.stats;
            let t = st.take_timing();
            let (ws, private) = stats::process_memory().unwrap_or((0, 0));
            let buf_ms = st.take_buffer_avg().unwrap_or(0.0) / 48.0;
            let diff = st.captured.load(Relaxed) as f64 - st.rendered.load(Relaxed) as f64;
            drift.add(drift_start.elapsed().as_secs_f64(), diff);
            let gate_pct = if t.frames > 0 {
                st.gate_open_frames.swap(0, Relaxed) as f64 / t.frames as f64 * 100.0
            } else {
                0.0
            };
            let agc_db = f32::from_bits(st.agc_gain_db.load(Relaxed));
            let limit_db = st.limit_cdb.swap(0, Relaxed) as f32 / 100.0;
            // Too few points → the slope is mostly jitter; wait 30 s before showing it.
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
                match st.active_engine.load(Relaxed) {
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
                st.overflows.load(Relaxed),
                st.underruns.load(Relaxed),
                st.resyncs.load(Relaxed),
                st.engine_errors.load(Relaxed),
            );
            if report_every_second {
                println!("{last_report}");
            }
        }
    }

    ctl.stop();
    println!("Stopped.");
    Ok(())
}
