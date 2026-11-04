//! whisper.cpp's prebuilt `whisper-server`, downloaded, verified and run as a
//! child process. Ported from MultiZone's `local_stt.rs` to blocking code.
//! See that file for the reasoning behind the pinned release, the CUDA 12.4
//! package (not 11.8: it lacks cuBLAS) and the sidecar design.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const WHISPER_RELEASE: &str = "v1.9.2";
/// The model repository commit the hashes below were taken from.
const MODELS_COMMIT: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    pub size: u64,
    sha256: &'static str,
    pub multilingual: bool,
}

pub const MODELS: &[Model] = &[
    Model { id: "tiny.en", label: "Tiny (English)", size: 77_704_715, sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f", multilingual: false },
    Model { id: "base.en", label: "Base (English)", size: 147_964_211, sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002", multilingual: false },
    Model { id: "base", label: "Base (multilingual)", size: 147_951_465, sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe", multilingual: true },
    Model { id: "small.en", label: "Small (English)", size: 487_614_201, sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d", multilingual: false },
    Model { id: "small", label: "Small (multilingual)", size: 487_601_967, sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b", multilingual: true },
    Model { id: "large-v3-turbo-q5_0", label: "Large v3 Turbo (multilingual)", size: 574_041_195, sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2", multilingual: true },
];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// The prebuilt CPU engine for this platform: asset, SHA-256, server path inside the archive.
fn engine_asset() -> Option<(&'static str, &'static str, &'static str)> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some(("whisper-bin-x64.zip", "49dcc16de826f20bd53d44f947a1ae49dfa81f86cad67a64d80820cb192d674a", "Release/whisper-server.exe"))
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(("whisper-bin-ubuntu-x64.tar.gz", "46811a3ecf584307480a220b9ef5ff81b7b22dc41577cbc274ce3afc61f753b1", "whisper-bin-ubuntu-x64/whisper-server"))
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(("whisper-bin-ubuntu-arm64.tar.gz", "7e26fa6a36d9174d5c0bf033ccbc026c3b5e569e2ee787058241346ef5392719", "whisper-bin-ubuntu-arm64/whisper-server"))
    } else {
        None
    }
}

/// The CUDA build: asset, SHA-256, server path, download size.
fn cuda_asset() -> Option<(&'static str, &'static str, &'static str, u64)> {
    cfg!(all(windows, target_arch = "x86_64")).then_some((
        "whisper-cublas-12.4.0-bin-x64.zip",
        "443110ddaad70d4290ab2e77179e31cf712035bbc4fad56bb4519a90c917b39c",
        "Release/whisper-server.exe",
        670_611_449,
    ))
}


pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("voicetype")
}

fn root() -> PathBuf {
    data_dir().join("whisper")
}

fn model_path(id: &str) -> PathBuf {
    root().join("models").join(format!("ggml-{id}.bin"))
}

pub fn installed(id: &str) -> bool {
    model_path(id).is_file()
}

fn cuda_server() -> Option<PathBuf> {
    let (_, _, rel, _) = cuda_asset()?;
    let p = root().join(format!("{WHISPER_RELEASE}-cuda")).join(rel);
    p.is_file().then_some(p)
}

pub fn gpu_engine_installed() -> bool {
    cuda_server().is_some()
}

fn cpu_server() -> Option<PathBuf> {
    let (_, _, rel) = engine_asset()?;
    let p = root().join(WHISPER_RELEASE).join(rel);
    p.is_file().then_some(p)
}

fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// The NVIDIA GPU's name, when there is one (Windows only).
pub fn nvidia_gpu() -> Option<String> {
    static GPU: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    GPU.get_or_init(|| {
        if cuda_asset().is_none() {
            return None;
        }
        let sys = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join("System32");
        if !sys.join("nvcuda.dll").is_file() {
            return None;
        }
        let name = no_window(std::process::Command::new(sys.join("nvidia-smi.exe")).args(["--query-gpu=name", "--format=csv,noheader"]))
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
            .filter(|s| !s.is_empty());
        Some(name.unwrap_or_else(|| "NVIDIA GPU".into()))
    })
    .clone()
}

/// The CUDA build if downloaded (it runs on the CPU too), then the CPU build, then PATH.
fn server_path() -> Option<PathBuf> {
    if let Some(p) = cuda_server().or_else(cpu_server) {
        return Some(p);
    }
    if engine_asset().is_some() {
        return None;
    }
    let exe = if cfg!(windows) { "whisper-server.exe" } else { "whisper-server" };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(exe)).find(|p| p.is_file())
}

// Download progress, polled by the UI.
pub static DOWNLOADING: AtomicBool = AtomicBool::new(false);
pub static CANCEL: AtomicBool = AtomicBool::new(false);
pub static RECEIVED: AtomicU64 = AtomicU64::new(0);
pub static TOTAL: AtomicU64 = AtomicU64::new(0);

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().timeout(None).build().expect("http client")
}

/// One download at a time; resets the cancel flag.
pub fn exclusive<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("a download is already running".into());
    }
    CANCEL.store(false, Ordering::Relaxed);
    let r = f();
    DOWNLOADING.store(false, Ordering::SeqCst);
    r
}

/// Download (if needed) the CPU engine and the model.
pub fn install_model(id: &str) -> Result<(), String> {
    let m = model(id).ok_or_else(|| format!("unknown model {id}"))?;
    exclusive(|| {
        ensure_engine()?;
        let path = model_path(m.id);
        if !path.is_file() {
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{MODELS_COMMIT}/ggml-{}.bin", m.id);
            download(&url, &path, m.sha256, m.size)?;
        }
        Ok(())
    })
}

fn ensure_engine() -> Result<(), String> {
    if server_path().is_some() {
        return Ok(());
    }
    let (asset, sha, _) = engine_asset().ok_or(
        "No prebuilt whisper.cpp server for this platform. Put `whisper-server` on PATH (macOS: brew install whisper-cpp).",
    )?;
    fetch_engine(asset, sha, root().join(WHISPER_RELEASE), 0)?;
    server_path().map(|_| ()).ok_or_else(|| "the whisper.cpp download did not contain its server".into())
}

pub fn install_gpu_engine() -> Result<(), String> {
    let (asset, sha, _, size) = cuda_asset().ok_or("the GPU engine is Windows x64 only")?;
    exclusive(|| {
        fetch_engine(asset, sha, root().join(format!("{WHISPER_RELEASE}-cuda")), size)?;
        cuda_server().map(|_| ()).ok_or_else(|| "the GPU download did not contain its server".into())
    })
}

fn fetch_engine(asset: &str, sha: &str, dir: PathBuf, size: u64) -> Result<(), String> {
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let archive = dir.join(asset);
    let url = format!("https://github.com/ggml-org/whisper.cpp/releases/download/{WHISPER_RELEASE}/{asset}");
    download(&url, &archive, sha, size)?;
    let r = extract(&archive, &dir);
    let _ = std::fs::remove_file(&archive);
    r
}

/// Stream to a `.part` file, hashing on the way; rename only once the SHA-256 matches.
pub fn download(url: &str, dest: &Path, sha256: &str, size: u64) -> Result<(), String> {
    let mut res = http().get(url).send().map_err(|e| format!("download of {url} failed: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("download of {url} failed: {}", res.status()));
    }
    TOTAL.store(res.content_length().unwrap_or(size), Ordering::Relaxed);
    RECEIVED.store(0, Ordering::Relaxed);
    let part = dest.with_extension("part");
    let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let result = loop {
        if CANCEL.load(Ordering::Relaxed) {
            break Err("download cancelled".to_string());
        }
        let n = match res.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(format!("download of {url} failed: {e}")),
        };
        hasher.update(&buf[..n]);
        if let Err(e) = file.write_all(&buf[..n]) {
            break Err(e.to_string());
        }
        RECEIVED.fetch_add(n as u64, Ordering::Relaxed);
    };
    drop(file);
    let result = result.and_then(|()| {
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        (got == sha256).then_some(()).ok_or_else(|| format!("{url} did not match its published checksum, so it was discarded"))
    });
    match result {
        Ok(()) => std::fs::rename(&part, dest).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// The system `tar` (bsdtar on Windows 10+) reads zip as well as tar.gz.
fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    let tar = if cfg!(windows) {
        PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\tar.exe")
    } else {
        PathBuf::from("tar")
    };
    let out = no_window(std::process::Command::new(tar).arg("-xf").arg(archive).arg("-C").arg(into))
        .output()
        .map_err(|e| format!("could not run tar: {e}"))?;
    if !out.status.success() {
        return Err(format!("unpacking failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

pub fn remove_model(id: &str) -> Result<(), String> {
    if running().is_some_and(|(m, _)| m == id) {
        shutdown();
    }
    let p = model_path(id);
    if p.exists() {
        std::fs::remove_file(p).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Delete every `<release>-cuda` folder. The running server stops first:
/// Windows won't delete files a live process has open.
pub fn remove_gpu_engine() -> Result<(), String> {
    if DOWNLOADING.load(Ordering::Relaxed) {
        return Err("a download is running; cancel it first".into());
    }
    shutdown();
    for e in std::fs::read_dir(root()).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-cuda")) {
            std::fs::remove_dir_all(&p).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

struct Server {
    child: std::process::Child,
    port: u16,
    model: String,
    gpu: bool,
    on_gpu: bool,
    last_used: Instant,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// Held across a whole start so two near-simultaneous requests can't leave a second server untracked.
static STARTING: Mutex<()> = Mutex::new(());
const IDLE: Duration = Duration::from_secs(10 * 60);

/// (model, actually on GPU) of the running server.
pub fn running() -> Option<(String, bool)> {
    SERVER.lock().ok()?.as_ref().map(|s| (s.model.clone(), s.on_gpu))
}

fn log_path() -> PathBuf {
    root().join("server.log")
}

/// Base URL of a server running `model_id`, starting one if needed.
pub fn ensure_running(model_id: &str, gpu: bool) -> Result<String, String> {
    let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut guard = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = guard.as_mut() {
            if matches!(s.child.try_wait(), Ok(None)) && s.model == model_id && s.gpu == gpu {
                s.last_used = Instant::now();
                return Ok(format!("http://127.0.0.1:{}/v1", s.port));
            }
        }
        if let Some(mut old) = guard.take() {
            let _ = old.child.kill();
            let _ = old.child.wait();
        }
    }

    let exe = server_path().ok_or("The whisper engine is not installed: download a model first.")?;
    let m = model(model_id).ok_or_else(|| format!("unknown model {model_id}"))?;
    let weights = model_path(m.id);
    if !weights.is_file() {
        return Err(format!("The {} model is not downloaded.", m.label));
    }

    let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(|e| e.to_string())?.port();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 9) - 1;
    let log = std::fs::File::create(log_path()).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("-m").arg(&weights)
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .args(["--inference-path", "/v1/audio/transcriptions"])
        .args(["-l", if m.multilingual { "auto" } else { "en" }])
        .args(["-t", &threads.to_string()])
        .args(if gpu { &[][..] } else { &["-ng"][..] })
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
        #[cfg(target_os = "linux")]
        cmd.env("LD_LIBRARY_PATH", dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
    }
    let mut child = no_window(&mut cmd).spawn().map_err(|e| format!("could not start whisper-server: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let log = std::fs::read_to_string(log_path()).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(6).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            return Err(format!("whisper-server exited ({status}):\n{}", tail.join("\n")));
        }
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("whisper-server did not start within a minute".into());
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let on_gpu = gpu && std::fs::read_to_string(log_path()).is_ok_and(|l| l.contains("found GPU device"));
    *SERVER.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Server { child, port, model: model_id.to_string(), gpu, on_gpu, last_used: Instant::now() });
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(60));
        let mut guard = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(s) if s.last_used.elapsed() < IDLE => {}
            Some(_) => {
                if let Some(mut s) = guard.take() {
                    let _ = s.child.kill();
                }
                return;
            }
            None => return,
        }
    });
    Ok(format!("http://127.0.0.1:{port}/v1"))
}

/// Kill the server. Windows doesn't take children down with their parent.
pub fn shutdown() {
    if let Some(mut s) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = s.child.kill();
        let _ = s.child.wait();
    }
}

/// POST the WAV to the OpenAI-style endpoint. `language` "auto"/"" lets the server decide.
/// `prompt` (may be empty) biases spelling, e.g. toward the wake phrase.
pub fn transcribe(base_url: &str, wav: Vec<u8>, model_id: &str, language: &str, prompt: &str) -> Result<String, String> {
    let part = reqwest::blocking::multipart::Part::bytes(wav).file_name("dictation.wav").mime_str("audio/wav").map_err(|e| e.to_string())?;
    let mut form = reqwest::blocking::multipart::Form::new().text("model", model_id.to_string()).part("file", part);
    if model(model_id).is_some_and(|m| m.multilingual) && !language.is_empty() && language != "auto" {
        form = form.text("language", language.to_string());
    }
    if !prompt.is_empty() {
        form = form.text("prompt", prompt.to_string());
    }
    let res = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?
        .post(format!("{base_url}/audio/transcriptions"))
        .multipart(form)
        .send()
        .map_err(|e| format!("transcription request failed: {e}"))?;
    let status = res.status();
    let body = res.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("whisper-server answered {status}: {body}"));
    }
    let text = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or(body);
    Ok(clean(&text))
}

/// whisper-server joins segments with "\n "; it also emits tags like
/// `[BLANK_AUDIO]` or `(music)` for non-speech, which are never wanted as text.
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in text.replace("\n ", " ").chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn clean_strips_tags_and_joins_segments() {
        assert_eq!(super::clean(" [BLANK_AUDIO]"), "");
        assert_eq!(super::clean(" Hello there.\n (music) How are you?"), "Hello there. How are you?");
    }

    #[test]
    fn every_model_has_a_full_sha256() {
        for m in super::MODELS {
            assert_eq!(m.sha256.len(), 64, "{}", m.id);
        }
    }
}
