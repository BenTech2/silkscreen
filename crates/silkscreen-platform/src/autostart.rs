//! Starting Silkscreen when the user logs in: the per-user `Run` registry
//! key on Windows, a LaunchAgent on macOS, an XDG autostart entry on Linux.
//! Only the current user is affected, so no administrator rights are needed.

use anyhow::Result;

/// The argument the login entry passes, so the app knows how it was started.
pub const LOGIN_ARG: &str = "--autostart";

/// Adds (or refreshes, e.g. after the app moved) or removes the login entry.
pub fn set_enabled(enabled: bool) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    imp::set(enabled, &exe)
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use anyhow::{Result, bail};
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
    };
    use windows::core::w;

    const RUN: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const VALUE: windows::core::PCWSTR = w!("Silkscreen");

    pub fn set(enabled: bool, exe: &Path) -> Result<()> {
        // canonicalize() gives a \\?\ path; the Run key wants a plain one.
        let exe = exe.display().to_string();
        let exe = exe.strip_prefix(r"\\?\").unwrap_or(&exe);
        unsafe {
            if enabled {
                let cmd = format!("\"{exe}\" {}", super::LOGIN_ARG);
                let data: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
                let r = RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    RUN,
                    VALUE,
                    REG_SZ.0,
                    Some(data.as_ptr().cast()),
                    (data.len() * 2) as u32,
                );
                if r.is_err() {
                    bail!("cannot write the Run registry key: {r:?}");
                }
            } else {
                let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, VALUE);
                if r.is_err() && r != ERROR_FILE_NOT_FOUND {
                    bail!("cannot remove the Run registry value: {r:?}");
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    const LABEL: &str = "dev.silkscreen";

    fn plist_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("no home directory")?;
        Ok(PathBuf::from(home).join(format!("Library/LaunchAgents/{LABEL}.plist")))
    }

    fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub fn set(enabled: bool, exe: &Path) -> Result<()> {
        let path = plist_path()?;
        if !enabled {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
            escape(&exe.display().to_string()),
            super::LOGIN_ARG
        );
        if std::fs::read_to_string(&path).is_ok_and(|c| c == plist) {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, plist)?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    fn entry_path() -> Result<PathBuf> {
        let dirs = directories::BaseDirs::new().context("no home directory")?;
        Ok(dirs.config_dir().join("autostart/silkscreen.desktop"))
    }

    pub fn set(enabled: bool, exe: &Path) -> Result<()> {
        let path = entry_path()?;
        if !enabled {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        // Desktop entry Exec quoting: double quotes, with \ " ` $ escaped.
        let mut quoted = String::from('"');
        for c in exe.display().to_string().chars() {
            if matches!(c, '\\' | '"' | '`' | '$') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        let contents = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Silkscreen\n\
             Comment=Use another computer as a wireless display\n\
             Exec={quoted} {}\n\
             Icon=silkscreen\n\
             X-GNOME-Autostart-enabled=true\n",
            super::LOGIN_ARG
        );
        if std::fs::read_to_string(&path).is_ok_and(|c| c == contents) {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, contents)?;
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    pub fn set(_enabled: bool, _exe: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("starting at login isn't supported on this system")
    }
}
