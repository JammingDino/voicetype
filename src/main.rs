#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod dictation;
mod engine;
mod install;
mod output;
mod ui;
mod update;

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
    /// What goes between phrases, and after the last one unless `end_key` is set.
    after_phrase: output::After,
    custom_after: String,
    /// Pressed once when a recording ends, if anything was typed.
    end_key: output::EndKey,
    end_key_delay_secs: f32,
    /// Whisper ends most phrases with a period, which isn't always wanted.
    drop_period: bool,
    /// Toggle recordings end by themselves after this much silence.
    auto_stop: bool,
    auto_stop_secs: f32,
    /// Real-time: type each phrase as soon as you pause, instead of one recording at the end.
    split_on_pause: bool,
    pause_secs: f32,
    /// Show the words so far in the overlay while you talk.
    live_preview: bool,
    preview_secs: f32,
    wake_word: bool,
    wake_phrase: String,
    wake_match: dictation::WakeMatch,
    /// Wake-word recordings end after this much silence.
    wake_stop_secs: f32,
    /// Play the system sound when the wake phrase is heard.
    wake_chime: bool,
    /// RMS below this is always silence. Raise it in a noisy room.
    silence_threshold: f32,
    close_to_tray: bool,
    overlay: bool,
    overlay_style: ui::OverlayStyle,
    /// The indicator opens at its full size instead of growing to fit the words.
    overlay_fixed: bool,
    /// The monitor's device name; `None` is the primary display.
    overlay_display: Option<String>,
    /// Launch (hidden, in the tray) when the user signs in. Windows only.
    run_at_startup: bool,
    /// Download new releases on launch and restart into them when idle.
    auto_update: bool,
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
            after_phrase: output::After::Space,
            custom_after: String::new(),
            end_key: output::EndKey::None,
            end_key_delay_secs: 0.0,
            drop_period: false,
            auto_stop: true,
            auto_stop_secs: 2.0,
            split_on_pause: false,
            pause_secs: 0.8,
            live_preview: true,
            preview_secs: 1.0,
            wake_word: false,
            wake_phrase: "hey jarvis".into(),
            wake_match: dictation::WakeMatch::LastWord,
            wake_stop_secs: 2.0,
            wake_chime: true,
            silence_threshold: 0.01,
            close_to_tray: true,
            overlay: true,
            overlay_style: ui::OverlayStyle::Classic,
            overlay_fixed: false,
            overlay_display: None,
            run_at_startup: false,
            auto_update: true,
        }
    }
}

impl Config {
    fn separator(&self) -> &str {
        match self.after_phrase {
            output::After::Nothing => "",
            output::After::Space => " ",
            output::After::NewLine => "\n",
            output::After::Custom => &self.custom_after,
        }
    }
}

fn config_path() -> std::path::PathBuf {
    engine::data_dir().join("config.json")
}

fn parse_config(bytes: &[u8]) -> Option<Config> {
    let mut c: Config = serde_json::from_slice(bytes).ok()?;
    // Configs from before `after_phrase` had this on/off instead.
    let old: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if old["trailing_space"] == false {
        c.after_phrase = output::After::Nothing;
    }
    Some(c)
}

fn load_config() -> Config {
    std::fs::read(config_path()).ok().and_then(|b| parse_config(&b)).unwrap_or_default()
}

fn save_config(c: &Config) {
    let _ = std::fs::create_dir_all(engine::data_dir());
    let _ = std::fs::write(config_path(), serde_json::to_vec_pretty(c).unwrap());
}

/// Lifetime totals of what was dictated and typed. Kept out of `Config` because
/// the dictation worker writes these while the UI owns saving the settings.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Stats {
    recordings: u64,
    /// Time the mic was recording, not time spent transcribing.
    recorded_secs: f64,
    chars: u64,
    words: u64,
}

fn stats_path() -> std::path::PathBuf {
    engine::data_dir().join("stats.json")
}

fn load_stats() -> Stats {
    std::fs::read(stats_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Through a temp file: these totals can't be rebuilt if a crash truncates them.
fn save_stats(s: &Stats) {
    let _ = std::fs::create_dir_all(engine::data_dir());
    let tmp = stats_path().with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap()).is_ok() {
        let _ = std::fs::rename(tmp, stats_path());
    }
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
    let flag = |f: &str| args.iter().any(|a| a == f);
    if flag("--uninstall") {
        install::uninstall(flag("--silent"));
        return;
    }
    if install::offer(flag("--silent")) {
        return;
    }
    if install::is_installed_copy() {
        install::refresh();
    }
    update::cleanup();
    let r = ui::run(flag("--hidden"));
    engine::shutdown();
    r.expect("ui");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_spacing_setting_carries_over() {
        assert!(parse_config(br#"{"trailing_space":false}"#).unwrap().separator().is_empty());
        assert_eq!(parse_config(br#"{"trailing_space":true}"#).unwrap().separator(), " ");
        assert_eq!(parse_config(br#"{"model":"tiny.en"}"#).unwrap().separator(), " ");
        let saved = serde_json::to_vec(&Config { after_phrase: output::After::NewLine, ..Default::default() }).unwrap();
        assert_eq!(parse_config(&saved).unwrap().separator(), "\n");
    }
}
