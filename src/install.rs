//! The exe is its own installer (Windows). Run from anywhere else, it offers
//! to install itself per-user into `%LOCALAPPDATA%\Programs\voicetype`, which
//! needs no admin rights and so stays writable for self-updates, adds a Start
//! menu shortcut and an Installed apps entry, and starts the installed copy.
//! `--uninstall` undoes all of it. `--silent` skips the prompts.
//!
//! Settings and models live apart, in `%APPDATA%\voicetype`, so reinstalling or
//! updating never touches them; uninstall asks before deleting them.

use std::path::{Path, PathBuf};

const UNINSTALL_KEY: &str = r"HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Uninstall\voicetype";

pub fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("Programs").join("voicetype")
}

fn installed_exe() -> PathBuf {
    install_dir().join("voicetype.exe")
}

fn shortcut() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join(r"Microsoft\Windows\Start Menu\Programs\voicetype.lnk")
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn is_installed_copy() -> bool {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|p| same_dir(p, &install_dir()))).unwrap_or(false)
}

/// Installing moves the exe, so it is off for `cargo run` builds unless a test asks for it.
fn enabled() -> bool {
    cfg!(windows) && (!cfg!(debug_assertions) || std::env::var_os("VOICETYPE_INSTALL_TEST").is_some())
}

fn hidden(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

fn powershell(script: &str) -> Result<(), String> {
    let out = hidden(std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]))
        .output()
        .map_err(|e| e.to_string())?;
    out.status.success().then_some(()).ok_or_else(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// PowerShell single-quoted string.
fn ps(s: &Path) -> String {
    format!("'{}'", s.display().to_string().replace('\'', "''"))
}

/// Stop every other process whose exe is under `dir`.
fn stop_processes_in(dir: &Path) {
    let script = format!(
        "Get-Process | Where-Object {{ $_.Path -like ({} + '\\*') -and $_.Id -ne {} }} | Stop-Process -Force",
        ps(dir),
        std::process::id()
    );
    let _ = powershell(&script);
}

#[cfg(windows)]
fn ask(text: &str, style: u32) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;
    let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (t, c) = (w(text), w("voicetype"));
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), style) }
}

/// At GUI startup. True means this process is done: it installed and started
/// the installed copy, or the user cancelled. "No" runs this copy as it is.
#[cfg(windows)]
pub fn offer(silent: bool) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    if !enabled() || is_installed_copy() {
        return false;
    }
    if !silent {
        let upgrading = installed_exe().is_file();
        let text = format!(
            "{} voicetype {} for your account?\n\nIt goes in {}, with a Start menu shortcut and an entry in Installed apps. No admin rights needed. Your settings and models are kept.\n\nYes: install   No: run without installing",
            if upgrading { "Reinstall" } else { "Install" },
            env!("CARGO_PKG_VERSION"),
            install_dir().display()
        );
        match ask(&text, MB_YESNOCANCEL | MB_ICONQUESTION) {
            IDYES => {}
            IDNO => return false,
            _ => return true,
        }
    }
    match install() {
        Ok(()) => true,
        Err(e) => {
            if !silent {
                ask(&format!("Installing voicetype failed:\n\n{e}"), MB_OK | MB_ICONERROR);
            }
            true
        }
    }
}

#[cfg(windows)]
fn install() -> Result<(), String> {
    let dir = install_dir();
    stop_processes_in(&dir); // an older installed copy may be running
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = installed_exe();
    // The stopped process can take a moment to let go of its exe.
    let mut tries = 0;
    while let Err(e) = std::fs::copy(&me, &target) {
        tries += 1;
        if tries > 20 {
            return Err(format!("couldn't copy to {}: {e}", target.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    register()?;
    let _ = powershell(&format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath = {}; $s.WorkingDirectory = {}; $s.Save()",
        ps(&shortcut()),
        ps(&target),
        ps(&dir)
    ));
    // Test hooks (fake mic/output) are inherited on purpose; the install override is not.
    hidden(std::process::Command::new(&target).env_remove("VOICETYPE_INSTALL_TEST")).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

/// Write the Installed apps entry. Re-run by every launch of the installed copy,
/// so the version shown there follows self-updates.
pub fn register() -> Result<(), String> {
    let exe = installed_exe();
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let q = |s: &str| format!("\"{}\"", esc(s));
    let exe_s = exe.display().to_string();
    let size_kb = std::fs::metadata(&exe).map(|m| m.len() / 1024).unwrap_or(0);
    let reg = format!(
        "Windows Registry Editor Version 5.00\r\n\r\n[{UNINSTALL_KEY}]\r\n\
         \"DisplayName\"={}\r\n\"DisplayVersion\"={}\r\n\"Publisher\"=\"JammingDino\"\r\n\
         \"DisplayIcon\"={}\r\n\"InstallLocation\"={}\r\n\"URLInfoAbout\"=\"https://github.com/JammingDino/voicetype\"\r\n\
         \"UninstallString\"={}\r\n\"QuietUninstallString\"={}\r\n\
         \"NoModify\"=dword:00000001\r\n\"NoRepair\"=dword:00000001\r\n\"EstimatedSize\"=dword:{:08x}\r\n",
        q("voicetype"),
        q(env!("CARGO_PKG_VERSION")),
        q(&exe_s),
        q(&install_dir().display().to_string()),
        q(&format!("\"{exe_s}\" --uninstall")),
        q(&format!("\"{exe_s}\" --uninstall --silent")),
        size_kb,
    );
    let file = std::env::temp_dir().join("voicetype-install.reg");
    // reg import reads UTF-16 LE with a BOM, which keeps any non-ASCII path intact.
    let bytes: Vec<u8> = std::iter::once(0xFEFFu16).chain(reg.encode_utf16()).flat_map(u16::to_le_bytes).collect();
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    let out = hidden(std::process::Command::new("reg").arg("import").arg(&file)).output().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&file);
    out.status.success().then_some(()).ok_or_else(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// `--uninstall`: remove the app, its shortcut and registry entries; ask before
/// deleting settings and models (kept when silent).
#[cfg(windows)]
pub fn uninstall(silent: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    if !silent && ask("Uninstall voicetype?", MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    let data = crate::engine::data_dir();
    stop_processes_in(&install_dir());
    stop_processes_in(&data); // its whisper-server
    crate::set_autostart(false);
    let _ = hidden(std::process::Command::new("reg").args(["delete", UNINSTALL_KEY, "/f"])).output();
    let _ = std::fs::remove_file(shortcut());
    if !silent && ask("Also delete downloaded models and settings?", MB_YESNO | MB_ICONQUESTION) == IDYES {
        let _ = std::fs::remove_dir_all(&data);
    }
    // A running exe can't delete itself: leave that to a shell that waits for us to exit.
    // raw_arg: cmd doesn't understand Rust's \" escaping of quotes inside an argument.
    use std::os::windows::process::CommandExt;
    let dir = install_dir();
    let _ = hidden(std::process::Command::new("cmd").raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display())))
        .current_dir(std::env::temp_dir())
        .spawn();
}

#[cfg(not(windows))]
pub fn offer(_silent: bool) -> bool {
    false
}

#[cfg(not(windows))]
pub fn uninstall(_silent: bool) {}
