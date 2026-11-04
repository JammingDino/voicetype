//! Put text into whatever window has focus: paste (default) or synthetic keystrokes.

use enigo::{Direction::*, Enigo, Key, Keyboard, Settings};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Paste,
    Keys,
}

pub fn emit(text: &str, mode: Mode) -> Result<(), String> {
    // The hotkey's modifiers may still be held; Ctrl+V with Alt down is Ctrl+Alt+V.
    wait_for_modifiers_up();
    // Test hook: append to a file instead of typing into whatever has focus.
    if let Ok(path) = std::env::var("VOICETYPE_FAKE_OUTPUT") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| e.to_string())?;
        return f.write_all(text.as_bytes()).map_err(|e| e.to_string());
    }
    inject(text, mode)?;
    // Tried anyway in case the check is wrong; UIPI drops input silently, so say why nothing appeared.
    #[cfg(windows)]
    if win::foreground_elevated() {
        return Err("couldn't type into an admin window (run voicetype as administrator to do that)".into());
    }
    Ok(())
}

fn inject(text: &str, mode: Mode) -> Result<(), String> {
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    match mode {
        Mode::Keys => enigo.text(text).map_err(|e| e.to_string()),
        Mode::Paste => {
            let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
            // ponytail: text only; images/files on the clipboard are lost. Save them via arboard's image API if anyone cares.
            let saved = cb.get_text().ok();
            cb.set_text(text).map_err(|e| e.to_string())?;
            let r = enigo
                .key(Key::Control, Press)
                .and_then(|_| enigo.key(Key::Unicode('v'), Click))
                .map_err(|e| e.to_string());
            let _ = enigo.key(Key::Control, Release);
            std::thread::sleep(Duration::from_millis(150)); // let the target read it
            if let Some(s) = saved {
                let _ = cb.set_text(s);
            }
            r
        }
    }
}

fn wait_for_modifiers_up() {
    #[cfg(windows)]
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while win::modifier_down() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    #[cfg(not(windows))]
    std::thread::sleep(Duration::from_millis(80));
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    pub fn modifier_down() -> bool {
        [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN].iter().any(|&k| unsafe { GetAsyncKeyState(k as i32) } < 0)
    }

    /// Some(elevated) when the token can be read.
    fn elevated(process: HANDLE) -> Option<bool> {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0;
            let ok = GetTokenInformation(token, TokenElevation, &mut e as *mut _ as _, size_of::<TOKEN_ELEVATION>() as u32, &mut len);
            CloseHandle(token);
            (ok != 0).then_some(e.TokenIsElevated != 0)
        }
    }

    /// UIPI silently drops synthetic input into a more privileged window.
    pub fn foreground_elevated() -> bool {
        if elevated(unsafe { GetCurrentProcess() }) == Some(true) {
            return false;
        }
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            // An unreadable token from a normal-integrity process means it's elevated.
            let r = elevated(h).unwrap_or(true);
            CloseHandle(h);
            r
        }
    }
}
