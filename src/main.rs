#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod dictation;
mod engine;
mod output;
mod ui;

use std::time::Instant;

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    model: String,
    gpu: bool,
    hotkey: String,
    /// "hold" (push-to-talk) or "toggle".
    mode: String,
    output: output::Mode,
    language: String,
    device: Option<String>,
    trailing_space: bool,
    /// Toggle and wake-word recordings end by themselves after this much silence.
    auto_stop: bool,
    auto_stop_secs: f32,
    /// Real-time: type each phrase as soon as you pause, instead of one recording at the end.
    split_on_pause: bool,
    pause_secs: f32,
    /// Show the words so far in the overlay while you talk.
    live_preview: bool,
    wake_word: bool,
    wake_phrase: String,
    /// RMS below this is always silence. Raise it in a noisy room.
    silence_threshold: f32,
    close_to_tray: bool,
    overlay: bool,
    /// Launch (hidden, in the tray) when the user signs in. Windows only.
    run_at_startup: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: "base.en".into(),
            gpu: true,
            hotkey: "ctrl+alt+Space".into(),
            mode: "hold".into(),
            output: output::Mode::Paste,
            language: "auto".into(),
            device: None,
            trailing_space: true,
            auto_stop: true,
            auto_stop_secs: 2.0,
            split_on_pause: false,
            pause_secs: 0.8,
            live_preview: true,
            wake_word: false,
            wake_phrase: "hey jarvis".into(),
            silence_threshold: 0.01,
            close_to_tray: true,
            overlay: true,
            run_at_startup: false,
        }
    }
}

fn config_path() -> std::path::PathBuf {
    engine::data_dir().join("config.json")
}

fn load_config() -> Config {
    std::fs::read(config_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_config(c: &Config) {
    let _ = std::fs::create_dir_all(engine::data_dir());
    let _ = std::fs::write(config_path(), serde_json::to_vec_pretty(c).unwrap());
}

pub struct Row {
    model: String,
    start_s: f32,
    secs: f32,
    audio_s: f32,
    gpu: bool,
    text: Result<String, String>,
}

/// Start the server and transcribe one clip, timing each half; used by the Test tab and the CLI.
fn run(model: &str, gpu: bool, samples: &[f32], language: &str) -> Row {
    let t0 = Instant::now();
    let r = engine::ensure_running(model, gpu);
    let start_s = t0.elapsed().as_secs_f32();
    let t1 = Instant::now();
    let text = r.and_then(|url| engine::transcribe(&url, audio::encode_wav(samples), model, language, ""));
    Row {
        model: model.into(),
        start_s,
        secs: t1.elapsed().as_secs_f32(),
        audio_s: samples.len() as f32 / audio::RATE as f32,
        gpu: engine::running().is_some_and(|r| r.1),
        text,
    }
}

/// Add or remove voicetype in the current user's Run key. Re-run on every launch
/// while enabled, so the entry follows the exe if it is moved.
fn set_autostart(on: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let mut cmd = std::process::Command::new("reg");
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            cmd.args(["add", KEY, "/v", "voicetype", "/t", "REG_SZ", "/f", "/d"]).arg(format!("\"{}\" --hidden", exe.display()));
        } else {
            cmd.args(["delete", KEY, "/v", "voicetype", "/f"]);
        }
        let _ = cmd.creation_flags(0x0800_0000).output();
    }
    #[cfg(not(windows))]
    let _ = on;
}

fn cli(args: &[String]) -> Result<(), String> {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    let cfg = load_config();
    let model = args.get(1).cloned().unwrap_or(cfg.model.clone());
    if !engine::installed(&model) {
        eprintln!("downloading {model}…");
        engine::install_model(&model)?;
    }
    let samples = if args[0] == "--test" {
        audio::load_wav(std::path::Path::new(args.get(2).ok_or("usage: voicetype --test <model> <file.wav>")?))?
    } else {
        let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
        let cap = audio::start_capture(cfg.device.as_deref())?;
        eprintln!("recording {secs} s…");
        std::thread::sleep(std::time::Duration::from_secs(secs));
        cap.stop()
    };
    let r = run(&model, cfg.gpu, &samples, &cfg.language);
    engine::shutdown();
    eprintln!("{} on {}: start {:.2} s, transcribe {:.2} s", r.model, if r.gpu { "GPU" } else { "CPU" }, r.start_s, r.secs);
    println!("{}", r.text?);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--test" | "--record")) {
        // `--test <model> <wav>` / `--record [model] [secs]`
        if let Err(e) = cli(&args) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    let r = ui::run(args.iter().any(|a| a == "--hidden"));
    engine::shutdown();
    r.expect("ui");
}
