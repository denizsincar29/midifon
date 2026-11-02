use anyhow::Result;
use clap::{Parser, Subcommand};
use midifon_core::devices;
use midifon_core::engine::{Engine, Notice, Notifier, Phase, Started};
use midifon_core::settings::{fmt_secs, parse_secs, Settings};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "mfcli", version, about = "midifon: MIDI recorder, terminal version. Run without arguments for the interactive prompt.")]
struct Cli {
    /// Path to the config file (shared with the midifon GUI by default)
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List MIDI input devices and exit
    Inputs,
    /// List MIDI output devices and exit
    Outputs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Inputs) => {
            print_devices("Inputs", &devices::input_names()?, None);
            return Ok(());
        }
        Some(Cmd::Outputs) => {
            print_devices("Outputs", &devices::output_names()?, None);
            return Ok(());
        }
        None => {}
    }

    let cfg_path = cli.config.or_else(Settings::default_path);
    let settings = match &cfg_path {
        Some(p) => Settings::load(p).unwrap_or_else(|e| {
            eprintln!("Config error: {e:#}. Using defaults.");
            Settings::default()
        }),
        None => Settings::default(),
    };

    let notify: Notifier = Arc::new(|n| {
        println!("\r{}", describe(&n));
        prompt();
    });
    let mut engine = Engine::new(settings, cfg_path, notify);
    auto_connect(&mut engine);

    println!("midifon mfcli. Type help for commands.");
    let stdin = io::stdin();
    loop {
        prompt();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF
        }
        match handle(&mut engine, line.trim()) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => println!("Error: {e:#}"),
        }
    }
    engine.stop();
    Ok(())
}

fn prompt() {
    print!("> ");
    let _ = io::stdout().flush();
}

fn fname(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn describe(n: &Notice) -> String {
    match n {
        Notice::TakeStarted { path } => format!("Note detected. Recording to {}.", fname(path)),
        Notice::PauseSplit { saved } => {
            format!("Detected pause, saved {}. Waiting for the next notes.", fname(saved))
        }
        Notice::TimerElapsed { saved: Some(p) } => format!("Timer elapsed, saved {}. Recording stopped.", fname(p)),
        Notice::TimerElapsed { saved: None } => "Timer elapsed. Recording stopped, nothing was recorded.".into(),
        Notice::PauseStopped { saved } => format!("Detected pause, saved {}. Recording stopped.", fname(saved)),
        Notice::PlaybackFinished => "Playback finished.".into(),
        Notice::Error(e) => format!("Error: {e}"),
    }
}

/// Подключает сохранённые устройства; если устройство одно и ничего не выбрано — берёт его.
fn auto_connect(engine: &mut Engine) {
    let s = engine.settings();
    match (&s.input, devices::input_names()) {
        (Some(name), _) => {
            if let Err(e) = engine.set_input(name) {
                println!("Saved input \"{name}\" is unavailable: {e:#}");
            } else {
                println!("Input: {name}");
            }
        }
        (None, Ok(names)) if names.len() == 1 => {
            if engine.set_input(&names[0]).is_ok() {
                println!("Input: {} (the only device)", names[0]);
            }
        }
        _ => {}
    }
    match (&s.output, devices::output_names()) {
        (Some(name), _) => println!("Output: {name}"),
        (None, Ok(names)) if names.len() == 1 => {
            if engine.set_output(&names[0]).is_ok() {
                println!("Output: {} (the only device)", names[0]);
            }
        }
        _ => {}
    }
}

fn print_devices(title: &str, names: &[String], selected: Option<&str>) {
    if names.is_empty() {
        println!("{title}: no devices found.");
        return;
    }
    println!("{title}:");
    for (i, n) in names.iter().enumerate() {
        let mark = if selected == Some(n.as_str()) { " (selected)" } else { "" };
        println!("{}: {}{}", i + 1, n, mark);
    }
}

fn parse_onoff(arg: &str) -> Option<bool> {
    match arg.to_lowercase().as_str() {
        "on" | "1" | "yes" | "true" => Some(true),
        "off" | "0" | "no" | "false" => Some(false),
        _ => None,
    }
}

fn onoff(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn handle(engine: &mut Engine, line: &str) -> Result<bool> {
    let mut it = line.splitn(2, char::is_whitespace);
    let cmd = it.next().unwrap_or("").to_lowercase();
    let arg = it.next().unwrap_or("").trim();

    match cmd.as_str() {
        "" => {}
        "help" | "?" | "h" => print_help(),
        "quit" | "exit" | "q" => return Ok(false),

        "inputs" | "ins" => {
            print_devices("Inputs", &devices::input_names()?, engine.settings().input.as_deref())
        }
        "outputs" | "outs" => {
            print_devices("Outputs", &devices::output_names()?, engine.settings().output.as_deref())
        }
        "in" | "input" => {
            let names = devices::input_names()?;
            if arg.is_empty() {
                print_devices("Inputs", &names, engine.settings().input.as_deref());
            } else {
                let name = names[devices::resolve(&names, arg)?].clone();
                engine.set_input(&name)?;
                println!("Input: {name}");
            }
        }
        "out" | "output" => {
            let names = devices::output_names()?;
            if arg.is_empty() {
                print_devices("Outputs", &names, engine.settings().output.as_deref());
            } else {
                let name = names[devices::resolve(&names, arg)?].clone();
                engine.set_output(&name)?;
                println!("Output: {name}");
            }
        }

        "rec" | "record" => {
            if !engine.input_connected() {
                println!("No input device. Use inputs, then in <number>.");
            } else {
                let name = if arg.is_empty() { None } else { Some(arg.to_string()) };
                match engine.start(name)? {
                    Started::Armed => println!("Armed. Waiting for the first note."),
                    Started::Recording(p) => println!("Recording to {}.", fname(&p)),
                }
            }
        }
        "stop" => {
            let o = engine.stop();
            match (o.was_recording, o.saved) {
                (true, Some(p)) => println!("Stopped, saved {}.", fname(&p)),
                (true, None) => println!("Stopped. Nothing was recorded."),
                (false, _) if o.was_playing => println!("Playback stopped."),
                _ => println!("Nothing is running."),
            }
        }
        "status" => {
            let st = engine.status();
            match st.phase {
                Phase::Idle => println!("Idle."),
                Phase::Armed => println!("Armed, waiting for notes."),
                Phase::Recording => println!("Recording to {}.", st.path.as_deref().map(fname).unwrap_or_default()),
            }
            if engine.is_playing() {
                println!("Playing.");
            }
        }

        "pause" => {
            if arg.is_empty() {
                let s = engine.settings();
                println!("Pause split is {}, threshold {} s.", onoff(s.pause_split), s.pause_secs);
            } else if let Some(v) = parse_onoff(arg) {
                engine.update(|s| s.pause_split = v)?;
                println!("Pause split {}.", onoff(v));
            } else if let Ok(sec) = arg.trim_end_matches('s').trim().parse::<f64>() {
                if sec <= 0.0 {
                    println!("Pause must be more than 0 seconds.");
                } else {
                    engine.update(|s| {
                        s.pause_secs = sec;
                        s.pause_split = true;
                    })?;
                    println!("Pause split on, threshold {sec} s.");
                }
            } else {
                println!("Usage: pause on | off | <seconds>");
            }
        }
        "auto" => match parse_onoff(arg) {
            Some(v) => {
                engine.update(|s| s.auto_start = v)?;
                println!("Auto start on first note {}.", onoff(v));
            }
            None => println!("Auto start is {}. Usage: auto on | off", onoff(engine.settings().auto_start)),
        },
        "controllers" | "cc" | "keep" => match parse_onoff(arg) {
            Some(v) => {
                engine.update(|s| s.keep_controllers = v)?;
                println!("Keeping controllers between recordings {}.", onoff(v));
            }
            None => println!(
                "Keeping controllers is {}. Usage: controllers on | off",
                onoff(engine.settings().keep_controllers)
            ),
        },
        "timer" => {
            if arg.is_empty() {
                match engine.settings().timer_secs {
                    Some(t) => println!("Timer: {}.", fmt_secs(t)),
                    None => println!("Timer is off. Usage: timer <time> | off. Time: 90, 90s, 2m, 1:30."),
                }
            } else if parse_onoff(arg) == Some(false) {
                engine.update(|s| s.timer_secs = None)?;
                println!("Timer off.");
            } else if let Some(t) = parse_secs(arg).filter(|t| *t > 0) {
                engine.update(|s| s.timer_secs = Some(t))?;
                println!("Timer: {}.", fmt_secs(t));
            } else {
                println!("Usage: timer <time> | off. Time: 90, 90s, 2m, 1:30.");
            }
        }
        "bpm" => match arg.parse::<f64>() {
            Ok(b) if (20.0..=400.0).contains(&b) => {
                engine.update(|s| s.bpm = b)?;
                println!("Tempo in files: {b} bpm.");
            }
            _ => println!("Tempo is {} bpm. Usage: bpm <20-400>", engine.settings().bpm),
        },
        "dir" => {
            if arg.is_empty() {
                println!("Folder: {}", engine.settings().dir.display());
            } else {
                let p = PathBuf::from(arg.trim_matches('"'));
                engine.update(|s| s.dir = p.clone())?;
                println!("Folder: {}", p.display());
            }
        }
        "name" => {
            if arg.is_empty() {
                println!("File name base: {}", engine.settings().name);
            } else {
                let n = arg.to_string();
                engine.update(|s| s.name = n.clone())?;
                println!("Files will be named {n}1.mid, {n}2.mid, ...");
            }
        }
        "settings" => {
            let s = engine.settings();
            println!("Input: {}", s.input.as_deref().unwrap_or("not selected"));
            println!("Output: {}", s.output.as_deref().unwrap_or("not selected"));
            println!("Folder: {}", s.dir.display());
            println!("File name base: {}", s.name);
            println!("Auto start on first note: {}", onoff(s.auto_start));
            println!("Pause split: {}, threshold {} s", onoff(s.pause_split), s.pause_secs);
            match s.timer_secs {
                Some(t) => println!("Timer: {}", fmt_secs(t)),
                None => println!("Timer: off"),
            }
            println!("Keep controllers: {}", onoff(s.keep_controllers));
            println!("Tempo in files: {} bpm", s.bpm);
        }

        "play" => {
            let path = if arg.is_empty() || arg.eq_ignore_ascii_case("last") {
                match engine.status().last_saved {
                    Some(p) => p,
                    None => {
                        println!("No recording in this session. Usage: play <file> | last");
                        return Ok(true);
                    }
                }
            } else {
                let p = PathBuf::from(arg.trim_matches('"'));
                if p.is_absolute() || p.exists() { p } else { engine.settings().dir.join(p) }
            };
            engine.play(&path)?;
            println!("Playing {}.", fname(&path));
        }

        other => println!("Unknown command: {other}. Type help."),
    }
    Ok(true)
}

fn print_help() {
    println!(
        "Commands:
inputs, outputs        list MIDI devices
in <n|name>            select input device (output <n|name> / out for playback)
rec [name]             start recording; files are name1.mid, name2.mid, ...
stop                   stop recording (saves the file) or playback
status                 what is running now
pause on|off|<sec>     split recording on silence; a number sets the threshold
auto on|off            start recording on the first note
timer <time>|off       stop after this time (90, 2m, 1:30)
controllers on|off     write last controller values at the start of each new file
bpm <n>                tempo stored in files
dir <path>, name <base>  folder and file name base
settings               show all settings
play [file|last]       play a file on the selected output
quit                   exit"
    );
}
