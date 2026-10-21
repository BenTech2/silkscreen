//! Keeping the screen awake while it shows a host — the same OS mechanism
//! video players use, so the display neither dims nor sleeps nor starts the
//! screen saver. Dropping the [`WakeLock`] hands control back to the OS's
//! normal power settings.

use anyhow::Result;

pub struct WakeLock {
    #[cfg(target_os = "linux")]
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    #[cfg(target_os = "linux")]
    thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(windows)]
    request: windows::Win32::Foundation::HANDLE,
    #[cfg(target_os = "macos")]
    assertion: u32,
}

// The Windows handle is only used to clear the request.
#[cfg(windows)]
unsafe impl Send for WakeLock {}

impl WakeLock {
    /// Keeps the display on until dropped. `reason` may be shown by the OS
    /// (e.g. `powercfg /requests`, GNOME's "inhibitors").
    pub fn acquire(reason: &str) -> Result<Self> {
        imp::acquire(reason)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use anyhow::{Context, Result, anyhow};
    use tracing::debug;

    use super::WakeLock;

    const SERVICE: &str = "org.freedesktop.ScreenSaver";
    const PATH: &str = "/org/freedesktop/ScreenSaver";

    /// `org.freedesktop.ScreenSaver.Inhibit` (KDE, GNOME and most desktops).
    /// The inhibition lives as long as the D-Bus connection, so a small
    /// thread holds it and un-inhibits on drop.
    pub fn acquire(reason: &str) -> Result<WakeLock> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let reason = reason.to_owned();
        let thread = std::thread::Builder::new()
            .name("wake-lock".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.into()));
                        return;
                    }
                };
                rt.block_on(async move {
                    let inhibit = async {
                        let conn = zbus::Connection::session().await?;
                        let reply = conn
                            .call_method(
                                Some(SERVICE),
                                PATH,
                                Some(SERVICE),
                                "Inhibit",
                                &("Silkscreen", reason.as_str()),
                            )
                            .await?;
                        let cookie: u32 = reply.body().deserialize()?;
                        anyhow::Ok((conn, cookie))
                    };
                    let (conn, cookie) = match inhibit.await {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));
                    let _ = stop_rx.await;
                    if let Err(e) = conn
                        .call_method(Some(SERVICE), PATH, Some(SERVICE), "UnInhibit", &(cookie,))
                        .await
                    {
                        debug!("UnInhibit: {e}");
                    }
                });
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("wake-lock thread exited"))?
            .context("org.freedesktop.ScreenSaver.Inhibit")?;
        Ok(WakeLock {
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    impl Drop for WakeLock {
        fn drop(&mut self) {
            if let Some(s) = self.stop.take() {
                let _ = s.send(());
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::Result;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Power::{
        PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired,
        PowerRequestSystemRequired, PowerSetRequest,
    };
    use windows::Win32::System::SystemServices::POWER_REQUEST_CONTEXT_VERSION;
    use windows::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };
    use windows::core::PWSTR;

    use super::WakeLock;

    /// A power request (what Chrome uses for video): display and system stay on.
    pub fn acquire(reason: &str) -> Result<WakeLock> {
        let mut text: Vec<u16> = reason.encode_utf16().chain([0]).collect();
        let ctx = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(text.as_mut_ptr()),
            },
        };
        unsafe {
            let request = PowerCreateRequest(&ctx)?;
            PowerSetRequest(request, PowerRequestDisplayRequired)?;
            let _ = PowerSetRequest(request, PowerRequestSystemRequired);
            Ok(WakeLock { request })
        }
    }

    impl Drop for WakeLock {
        fn drop(&mut self) {
            unsafe {
                let _ = PowerClearRequest(self.request, PowerRequestDisplayRequired);
                let _ = PowerClearRequest(self.request, PowerRequestSystemRequired);
                let _ = CloseHandle(self.request);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Result, bail};
    use objc2_core_foundation::CFString;

    use super::WakeLock;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: &CFString,
            level: u32,
            name: &CFString,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    const LEVEL_ON: u32 = 255;

    /// An IOKit power assertion that prevents idle display sleep.
    pub fn acquire(reason: &str) -> Result<WakeLock> {
        let kind = CFString::from_str("PreventUserIdleDisplaySleep");
        let name = CFString::from_str(reason);
        let mut id = 0u32;
        let r = unsafe { IOPMAssertionCreateWithName(&kind, LEVEL_ON, &name, &mut id) };
        if r != 0 {
            bail!("IOPMAssertionCreateWithName failed ({r:#x})");
        }
        Ok(WakeLock { assertion: id })
    }

    impl Drop for WakeLock {
        fn drop(&mut self) {
            unsafe { IOPMAssertionRelease(self.assertion) };
        }
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod imp {
    use super::WakeLock;
    pub fn acquire(_reason: &str) -> anyhow::Result<WakeLock> {
        anyhow::bail!("keeping the screen awake is not supported on this platform")
    }
}
