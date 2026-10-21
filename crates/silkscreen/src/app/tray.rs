//! The system tray icon (the menu bar on macOS): shows what Silkscreen is
//! doing and lets the user disconnect, reopen the window or quit while the
//! window is closed.

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Open,
    Disconnect,
    /// Start display mode, or stop it if running.
    ToggleDisplay,
    Quit,
}

/// What the menu shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub status: String,
    pub can_disconnect: bool,
    pub in_display_mode: bool,
}

pub struct Tray {
    icon: TrayIcon,
    status: MenuItem,
    disconnect: MenuItem,
    display: MenuItem,
    shown: Option<TrayState>,
}

impl Tray {
    /// Creates the icon. `send` is called from the platform's tray thread or
    /// event loop with each command.
    pub fn new(send: impl Fn(TrayCommand) + Send + Sync + 'static) -> Result<Self> {
        let status = MenuItem::with_id("status", "Silkscreen", false, None);
        let open = MenuItem::with_id("open", "Open Silkscreen", true, None);
        let disconnect = MenuItem::with_id("disconnect", "Disconnect", false, None);
        let display = MenuItem::with_id("display", "Use this computer as a display", true, None);
        let quit = MenuItem::with_id("quit", "Quit Silkscreen", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &status,
            &PredefinedMenuItem::separator(),
            &open,
            &disconnect,
            &display,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;

        // macOS: a one-colour template image it tints like the other menu
        // bar icons. Elsewhere trays show the colour app icon.
        #[cfg(target_os = "macos")]
        let builder = TrayIconBuilder::new().with_icon_templated(tray_icon::Icon::from_rgba(
            include_bytes!("../../../../assets/tray-macos-36.rgba").to_vec(),
            36,
            36,
        )?);
        #[cfg(not(target_os = "macos"))]
        let builder = TrayIconBuilder::new().with_icon(tray_icon::Icon::from_rgba(
            include_bytes!("../../../../assets/icon-64.rgba").to_vec(),
            64,
            64,
        )?);
        let icon = builder
            .with_tooltip("Silkscreen")
            .with_menu(Box::new(menu))
            // Windows and Linux: left click opens the window, right click the
            // menu. On macOS any click opens the menu, as menu bar items do.
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .build()?;

        let send = std::sync::Arc::new(send);
        let on_menu = send.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let cmd = match e.id.0.as_str() {
                "open" => TrayCommand::Open,
                "disconnect" => TrayCommand::Disconnect,
                "display" => TrayCommand::ToggleDisplay,
                "quit" => TrayCommand::Quit,
                _ => return,
            };
            on_menu(cmd);
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
                && !cfg!(target_os = "macos")
            {
                send(TrayCommand::Open);
            }
        }));
        Ok(Tray {
            icon,
            status,
            disconnect,
            display,
            shown: None,
        })
    }

    pub fn update(&mut self, state: TrayState) {
        if self.shown.as_ref() == Some(&state) {
            return;
        }
        self.status.set_text(&state.status);
        self.disconnect.set_enabled(state.can_disconnect);
        self.display.set_text(if state.in_display_mode {
            "Stop display mode"
        } else {
            "Use this computer as a display"
        });
        let _ = self
            .icon
            .set_tooltip(Some(format!("Silkscreen — {}", state.status)));
        self.shown = Some(state);
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
    }
}
