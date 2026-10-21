//! The egui screens: home (display mode / extend to a display / paired
//! devices), debug (diagnostics, live stats, logs), the display's waiting
//! and pairing screen, and the in-stream overlay.
//!
//! Drawing functions only read state and push [`Action`]s; the app applies
//! them after the frame.

use std::net::IpAddr;
use std::time::UNIX_EPOCH;

use egui::{Align, Color32, Layout, RichText, Ui};
use silkscreen_proto::{Codec, PROTOCOL_VERSION};
use tracing::Level;

use crate::ctx::{Ctx, KeepAwake};
use crate::discovery::FoundDisplay;
use crate::logbuf;
use crate::secure::fingerprint;
use crate::status::{DisplayPhase, DisplayState, HostPhase, HostState};

const ACCENT: Color32 = Color32::from_rgb(110, 168, 254);
const GOOD: Color32 = Color32::from_rgb(120, 200, 140);
const WARN: Color32 = Color32::from_rgb(240, 190, 90);
const BAD: Color32 = Color32::from_rgb(240, 110, 110);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Home,
    Debug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceChoice {
    Virtual,
    Monitor,
    TestPattern,
}

/// Editable UI state that persists between frames.
pub struct UiState {
    pub tab: Tab,
    pub name_edit: String,
    pub manual_addr: String,
    pub code_input: String,
    pub source: SourceChoice,
    pub monitor_name: String,
    pub codec: Option<Codec>,
    /// 0 = automatic.
    pub bitrate_mbps: f32,
    pub resolution: crate::host::Resolution,
    pub display_windowed: bool,
    pub log_level: Level,
    pub log_filter: String,
    pub diagnostics: Option<Vec<(String, String)>>,
}

impl UiState {
    pub fn new(name: String, windowed: bool) -> Self {
        UiState {
            tab: Tab::Home,
            name_edit: name,
            manual_addr: String::new(),
            code_input: String::new(),
            source: SourceChoice::Virtual,
            monitor_name: String::new(),
            codec: None,
            bitrate_mbps: 0.0,
            resolution: crate::host::Resolution::Auto,
            display_windowed: windowed,
            log_level: Level::INFO,
            log_filter: String::new(),
            diagnostics: None,
        }
    }
}

#[derive(Debug)]
pub enum Action {
    StartDisplay,
    StopDisplay,
    Connect(String),
    SubmitCode(String),
    StopHost,
    DismissHost,
    Forget(String),
    Rename(String),
    ToggleOverlay,
    SetKeepAwake(KeepAwake, u32),
}

/// Read-only data for one frame.
pub struct View<'a> {
    pub ctx: &'a Ctx,
    pub displays: Vec<FoundDisplay>,
    pub host: Option<HostState>,
    pub display: Option<DisplayState>,
    pub local_ips: &'a [IpAddr],
    pub monitor: String,
    pub overlay: bool,
    /// The screen is being kept awake right now.
    pub awake: bool,
}

pub fn hotkey_label(key: char) -> String {
    if cfg!(target_os = "macos") {
        format!("Ctrl+Option+Shift+{key} (or Cmd+Option+Shift+{key})")
    } else {
        format!("Ctrl+Alt+Shift+{key}")
    }
}

pub fn style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.visuals.selection.bg_fill = ACCENT.linear_multiply(0.5);
        s.visuals.hyperlink_color = ACCENT;
    });
}

fn card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(14.0)
        .corner_radius(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(17.0).strong());
            ui.add_space(4.0);
            add(ui);
        });
}

// ---------------------------------------------------------------------------
// Home

pub fn home(ui: &mut Ui, st: &mut UiState, v: &View, actions: &mut Vec<Action>) {
    egui::Panel::top("top").show(ui, |ui| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Silkscreen")
                    .size(22.0)
                    .strong()
                    .color(ACCENT),
            );
            ui.add_space(16.0);
            ui.selectable_value(&mut st.tab, Tab::Home, RichText::new("Home").size(15.0));
            ui.selectable_value(&mut st.tab, Tab::Debug, RichText::new("Debug").size(15.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(v.ctx.identity.fingerprint())
                        .monospace()
                        .weak(),
                )
                .on_hover_text("This device's key fingerprint — compare it when pairing");
                let r = ui.add(egui::TextEdit::singleline(&mut st.name_edit).desired_width(180.0));
                if r.lost_focus() && st.name_edit.trim() != v.ctx.name() {
                    actions.push(Action::Rename(st.name_edit.clone()));
                }
                ui.label("This computer:");
            });
        });
        ui.add_space(4.0);
    });
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| match st.tab {
                Tab::Home => home_tab(ui, st, v, actions),
                Tab::Debug => debug_tab(ui, st, v),
            });
    });
}

fn home_tab(ui: &mut Ui, st: &mut UiState, v: &View, actions: &mut Vec<Action>) {
    let wide = ui.available_width() > 900.0;
    let display_card = |ui: &mut Ui, st: &mut UiState, actions: &mut Vec<Action>| {
        card(ui, "Use this computer as a display", |ui| {
            ui.label(
                "Turn this screen into a wireless monitor that another computer can extend onto.",
            );
            ui.label(RichText::new(&v.monitor).weak());
            ui.checkbox(
                &mut st.display_windowed,
                "Run in a window instead of fullscreen",
            );
            keep_awake_setting(ui, v, actions);
            ui.add_space(6.0);
            if ui
                .add(
                    egui::Button::new(RichText::new("Start display mode").size(16.0))
                        .min_size(egui::vec2(200.0, 36.0)),
                )
                .clicked()
            {
                actions.push(Action::StartDisplay);
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "While showing a host's screen, {} disconnects.",
                    hotkey_label('G')
                ))
                .weak(),
            );
        });
    };
    if wide {
        ui.columns(2, |cols| {
            display_card(&mut cols[0], st, actions);
            cols[0].add_space(8.0);
            paired_card(&mut cols[0], v, actions);
            extend_card(&mut cols[1], st, v, actions);
        });
    } else {
        display_card(ui, st, actions);
        ui.add_space(8.0);
        extend_card(ui, st, v, actions);
        ui.add_space(8.0);
        paired_card(ui, v, actions);
    }
}

fn keep_awake_setting(ui: &mut Ui, v: &View, actions: &mut Vec<Action>) {
    let s = v.ctx.settings();
    let (mut mode, mut minutes) = (s.keep_awake, s.idle_minutes);
    let label = |m: KeepAwake| match m {
        KeepAwake::WhileConnected => "While a host is connected",
        KeepAwake::WhileActive => "While the picture is changing",
        KeepAwake::Never => "Never (use system settings)",
    };
    let hint = "Stops this screen from dimming or sleeping while it shows another computer, \
                like a video player does. When it lets go, the system's own sleep timeout applies.";
    ui.horizontal(|ui| {
        ui.label("Keep this screen awake:").on_hover_text(hint);
        egui::ComboBox::from_id_salt("keep-awake")
            .selected_text(label(mode))
            .show_ui(ui, |ui| {
                for m in [
                    KeepAwake::WhileConnected,
                    KeepAwake::WhileActive,
                    KeepAwake::Never,
                ] {
                    ui.selectable_value(&mut mode, m, label(m));
                }
            });
    });
    if mode == KeepAwake::WhileActive {
        ui.horizontal(|ui| {
            ui.label("Then let it sleep after");
            ui.add(
                egui::DragValue::new(&mut minutes)
                    .range(1..=240)
                    .suffix(" min"),
            );
            ui.label("without changes");
        });
    }
    if (mode, minutes) != (s.keep_awake, s.idle_minutes) {
        actions.push(Action::SetKeepAwake(mode, minutes));
    }
}

fn extend_card(ui: &mut Ui, st: &mut UiState, v: &View, actions: &mut Vec<Action>) {
    card(ui, "Extend this computer to a display", |ui| {
        if let Some(h) = &v.host {
            host_status(ui, st, h, actions);
            ui.separator();
        }
        let busy = v
            .host
            .as_ref()
            .is_some_and(|h| !matches!(h.phase, HostPhase::Stopped(_)));
        ui.label(RichText::new("Displays on this network").strong());
        if v.displays.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(
                        "Searching… run Silkscreen in display mode on the other computer.",
                    )
                    .weak(),
                );
            });
        }
        for d in &v.displays {
            display_row(ui, d, v, busy, actions);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Or by address:");
            ui.add(
                egui::TextEdit::singleline(&mut st.manual_addr)
                    .hint_text("192.168.1.20")
                    .desired_width(160.0),
            );
            if ui
                .add_enabled(
                    !busy && !st.manual_addr.trim().is_empty(),
                    egui::Button::new("Connect"),
                )
                .clicked()
            {
                actions.push(Action::Connect(st.manual_addr.trim().to_owned()));
            }
        });
        egui::CollapsingHeader::new("Stream options").show(ui, |ui| {
            egui::Grid::new("opts").num_columns(2).show(ui, |ui| {
                ui.label("Source");
                egui::ComboBox::from_id_salt("source")
                    .selected_text(match st.source {
                        SourceChoice::Virtual => "New virtual monitor",
                        SourceChoice::Monitor => "Existing monitor (dummy plug)",
                        SourceChoice::TestPattern => "Test pattern",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut st.source,
                            SourceChoice::Virtual,
                            "New virtual monitor",
                        );
                        ui.selectable_value(
                            &mut st.source,
                            SourceChoice::Monitor,
                            "Existing monitor (dummy plug)",
                        );
                        ui.selectable_value(
                            &mut st.source,
                            SourceChoice::TestPattern,
                            "Test pattern",
                        );
                    });
                ui.end_row();
                if st.source == SourceChoice::Monitor {
                    ui.label("Monitor");
                    ui.add(
                        egui::TextEdit::singleline(&mut st.monitor_name)
                            .hint_text("auto (the only secondary)"),
                    );
                    ui.end_row();
                }
                ui.label("Resolution");
                {
                    use crate::host::Resolution as R;
                    let label = |r: R| match r {
                        R::Auto => "Automatic (half above 4K)",
                        R::Full => "Full (sharpest, more latency)",
                        R::Half => "Half (lowest latency)",
                    };
                    egui::ComboBox::from_id_salt("resolution")
                        .selected_text(label(st.resolution))
                        .show_ui(ui, |ui| {
                            for r in [R::Auto, R::Full, R::Half] {
                                ui.selectable_value(&mut st.resolution, r, label(r));
                            }
                        });
                }
                ui.end_row();
                ui.label("Codec");
                egui::ComboBox::from_id_salt("codec")
                    .selected_text(st.codec.map_or("Automatic", |c| c.name()))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut st.codec, None, "Automatic");
                        ui.selectable_value(&mut st.codec, Some(Codec::Hevc), "hevc");
                        ui.selectable_value(&mut st.codec, Some(Codec::H264), "h264");
                        ui.selectable_value(&mut st.codec, Some(Codec::Av1), "av1");
                    });
                ui.end_row();
                ui.label("Bitrate");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut st.bitrate_mbps, 0.0..=200.0)
                            .step_by(5.0)
                            .suffix(" Mbit/s"),
                    );
                    if st.bitrate_mbps == 0.0 {
                        ui.label(RichText::new("automatic").weak());
                    }
                });
                ui.end_row();
            });
        });
    });
}

fn display_row(ui: &mut Ui, d: &FoundDisplay, v: &View, busy: bool, actions: &mut Vec<Action>) {
    let own = d.key == Some(v.ctx.identity.public);
    let paired = d.key.is_some_and(|k| v.ctx.paired.knows(&k));
    let compatible = d.protocol == PROTOCOL_VERSION;
    egui::Frame::new()
        .inner_margin(6.0)
        .corner_radius(6.0)
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&d.name).strong().size(15.0));
                        if own {
                            ui.label(RichText::new("(this computer)").weak());
                        } else if paired {
                            ui.label(RichText::new("paired").color(GOOD).strong());
                        }
                    });
                    let mut info = format!(
                        "{}  ·  {}×{} @ {:.0} Hz",
                        d.addr.ip(),
                        d.summary.width,
                        d.summary.height,
                        d.summary.refresh_mhz as f32 / 1000.0
                    );
                    if d.summary.touch {
                        info.push_str("  ·  touch");
                    }
                    if let Some(k) = d.key {
                        info.push_str(&format!("  ·  {}", fingerprint(&k)));
                    }
                    ui.label(RichText::new(info).weak());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !compatible {
                        ui.label(RichText::new("needs update").color(WARN));
                    } else if !own {
                        let label = if paired { "Connect" } else { "Pair…" };
                        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                            actions.push(Action::Connect(d.addr.to_string()));
                        }
                    }
                });
            });
        });
}

fn host_status(ui: &mut Ui, st: &mut UiState, h: &HostState, actions: &mut Vec<Action>) {
    let target = h.display.clone().unwrap_or_else(|| "display".into());
    match &h.phase {
        HostPhase::Idle | HostPhase::Connecting(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Connecting to {target}…"));
                if ui.button("Cancel").clicked() {
                    actions.push(Action::StopHost);
                }
            });
        }
        HostPhase::NeedCode(display) => {
            ui.label(
                RichText::new(format!("Pair with \"{display}\""))
                    .strong()
                    .size(15.0),
            );
            ui.label("Enter the 6-digit code shown on that display:");
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut st.code_input)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("000000")
                        .char_limit(7)
                        .desired_width(110.0),
                );
                st.code_input.retain(|c| c.is_ascii_digit());
                if !r.has_focus() && st.code_input.is_empty() {
                    r.request_focus();
                }
                let ready = st.code_input.len() == 6;
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(ready, egui::Button::new("Pair")).clicked() || (ready && enter) {
                    actions.push(Action::SubmitCode(std::mem::take(&mut st.code_input)));
                }
                if ui.button("Cancel").clicked() {
                    actions.push(Action::StopHost);
                }
            });
        }
        HostPhase::Streaming => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Extending to {target}"))
                        .color(GOOD)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Disconnect").clicked() {
                        actions.push(Action::StopHost);
                    }
                });
            });
            host_stats(ui, h);
        }
        HostPhase::Reconnecting(reason) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Reconnecting to {target}…"));
                if ui.button("Stop").clicked() {
                    actions.push(Action::StopHost);
                }
            });
            ui.label(RichText::new(reason).weak());
        }
        HostPhase::Stopped(reason) => {
            ui.horizontal(|ui| {
                match reason {
                    Some(r) => ui.label(RichText::new(format!("Stopped: {r}")).color(WARN)),
                    None => ui.label("Disconnected."),
                };
                if ui.button("OK").clicked() {
                    actions.push(Action::DismissHost);
                }
            });
        }
    }
}

fn host_stats(ui: &mut Ui, h: &HostState) {
    egui::Grid::new("host-stats")
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            let row = |ui: &mut Ui, k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.label(v);
                ui.end_row();
            };
            if let Some(s) = &h.stream {
                row(ui, "Stream", s.clone());
            }
            if let Some(s) = &h.source {
                row(ui, "Source", s.clone());
            }
            row(
                ui,
                "Frame rate",
                format!("{:.1} fps ({} keyframes)", h.fps, h.keyframes),
            );
            row(ui, "Bitrate", format!("{:.1} Mbit/s", h.mbps));
            row(ui, "Encode", format!("{:.1} ms", h.encode_ms));
            row(ui, "Round trip", format!("{:.1} ms", h.rtt_ms));
            row(
                ui,
                "Display",
                format!(
                    "{} frames/s · {} dropped · {} packets lost · {:.1} ms decode",
                    h.receiver.frames_received,
                    h.receiver.frames_dropped,
                    h.receiver.packets_lost,
                    h.receiver.decode_present_us as f32 / 1000.0
                ),
            );
        });
}

fn paired_card(ui: &mut Ui, v: &View, actions: &mut Vec<Action>) {
    card(ui, "Paired devices", |ui| {
        let peers = v.ctx.paired.list();
        if peers.is_empty() {
            ui.label(RichText::new("None yet. Devices pair the first time they connect: the display shows a code, you type it on the host.").weak());
        }
        for p in peers {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&p.name).strong());
                if let Some(k) = p.key_bytes() {
                    ui.label(RichText::new(fingerprint(&k)).monospace().weak());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .button("Forget")
                        .on_hover_text("They will have to pair again")
                        .clicked()
                    {
                        actions.push(Action::Forget(p.key.clone()));
                    }
                });
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Debug

fn debug_tab(ui: &mut Ui, st: &mut UiState, v: &View) {
    card(ui, "This device", |ui| {
        let diag = st.diagnostics.get_or_insert_with(diagnostics);
        egui::Grid::new("diag")
            .num_columns(2)
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Name").weak());
                ui.label(v.ctx.name());
                ui.end_row();
                ui.label(RichText::new("Fingerprint").weak());
                ui.label(RichText::new(v.ctx.identity.fingerprint()).monospace());
                ui.end_row();
                ui.label(RichText::new("Addresses").weak());
                ui.label(
                    v.local_ips
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                ui.end_row();
                for (k, val) in diag.iter() {
                    ui.label(RichText::new(k).weak());
                    ui.label(val);
                    ui.end_row();
                }
            });
    });
    ui.add_space(8.0);
    if let Some(h) = &v.host {
        card(ui, "Host session", |ui| {
            ui.label(format!("{:?}", h.phase));
            host_stats(ui, h);
        });
        ui.add_space(8.0);
    }
    if let Some(d) = &v.display {
        card(ui, "Display session", |ui| {
            display_stats(ui, d);
            ui.label(format!(
                "Screen kept awake: {}",
                if v.awake { "yes" } else { "no" }
            ));
        });
        ui.add_space(8.0);
    }
    card(ui, "Log", |ui| log_view(ui, st, 400.0));
}

fn diagnostics() -> Vec<(String, String)> {
    let mut out = vec![("FFmpeg".to_string(), silkscreen_codec::ffmpeg_version())];
    for codec in [Codec::Hevc, Codec::H264, Codec::Av1] {
        out.push((
            format!("{} encoders", codec.name()),
            silkscreen_codec::compiled_encoders(codec).join(", "),
        ));
    }
    let dec: Vec<_> = silkscreen_codec::decodable_codecs()
        .iter()
        .map(|c| c.name())
        .collect();
    out.push(("Decoders".into(), dec.join(", ")));
    out.push((
        "Touch points".into(),
        silkscreen_platform::touch_points().to_string(),
    ));
    out.extend(silkscreen_platform::diagnostics());
    out.push((
        "Config".into(),
        crate::secure::config_dir().display().to_string(),
    ));
    out
}

fn display_stats(ui: &mut Ui, d: &DisplayState) {
    egui::Grid::new("display-stats")
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            let row = |ui: &mut Ui, k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.label(v);
                ui.end_row();
            };
            row(ui, "State", format!("{:?}", d.phase));
            if let Some(s) = &d.stream {
                row(ui, "Stream", s.clone());
            }
            if let Some(s) = &d.decoder {
                row(ui, "Decoder", s.clone());
            }
            row(ui, "Frame rate", format!("{:.1} fps", d.fps));
            row(ui, "Receive to decoded", format!("{:.2} ms", d.latency_ms));
            row(
                ui,
                "Dropped / lost",
                format!("{} frames · {} packets", d.frames_dropped, d.packets_lost),
            );
        });
}

fn level_color(l: Level) -> Color32 {
    match l {
        Level::ERROR => BAD,
        Level::WARN => WARN,
        Level::INFO => Color32::from_gray(210),
        _ => Color32::from_gray(140),
    }
}

fn format_line(l: &logbuf::LogLine) -> String {
    let secs = l
        .time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02} UTC {:5} {}", l.level, l.text)
}

fn log_view(ui: &mut Ui, st: &mut UiState, height: f32) {
    let lines: Vec<_> = logbuf::snapshot()
        .into_iter()
        .filter(|l| l.level <= st.log_level)
        .filter(|l| {
            st.log_filter.is_empty()
                || l.text
                    .to_lowercase()
                    .contains(&st.log_filter.to_lowercase())
        })
        .collect();
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("level")
            .selected_text(st.log_level.as_str())
            .show_ui(ui, |ui| {
                for l in [
                    Level::ERROR,
                    Level::WARN,
                    Level::INFO,
                    Level::DEBUG,
                    Level::TRACE,
                ] {
                    ui.selectable_value(&mut st.log_level, l, l.as_str());
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut st.log_filter)
                .hint_text("filter")
                .desired_width(200.0),
        );
        if ui.button("Copy").clicked() {
            ui.ctx()
                .copy_text(lines.iter().map(format_line).collect::<Vec<_>>().join("\n"));
        }
        if ui.button("Clear").clicked() {
            logbuf::clear();
        }
        ui.label(RichText::new(format!("{} lines", lines.len())).weak());
    });
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::vertical()
        .id_salt("log")
        .max_height(height)
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .show_rows(ui, row_h, lines.len(), |ui, range| {
            for l in &lines[range] {
                ui.label(
                    RichText::new(format_line(l))
                        .monospace()
                        .color(level_color(l.level)),
                );
            }
        });
}

// ---------------------------------------------------------------------------
// Display mode

/// Shown on the display while no host is streaming.
pub fn display_waiting(ui: &mut Ui, st: &mut UiState, v: &View, actions: &mut Vec<Action>) {
    let d = v.display.clone().unwrap_or_default();
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(Color32::from_rgb(14, 16, 22)).inner_margin(24.0))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.18).max(10.0));
                ui.label(RichText::new(v.ctx.name()).size(44.0).strong());
                ui.add_space(12.0);
                match &d.phase {
                    DisplayPhase::Pairing { host, code } => {
                        ui.label(RichText::new(format!("\"{host}\" wants to use this display")).size(22.0));
                        ui.label(RichText::new("Enter this code on that computer:").size(18.0).weak());
                        ui.add_space(8.0);
                        let spaced = format!("{} {}", &code[..3.min(code.len())], &code[3.min(code.len())..]);
                        ui.label(RichText::new(spaced).size(96.0).monospace().strong().color(ACCENT));
                    }
                    _ => {
                        centered_row(ui, "status", |ui| {
                            ui.spinner();
                            ui.label(RichText::new("Ready — waiting for a host").size(22.0));
                        });
                        ui.label(
                            RichText::new("On the other computer, open Silkscreen and pick this display.")
                                .size(16.0)
                                .weak(),
                        );
                    }
                }
                ui.add_space(24.0);
                let ips = v.local_ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join("  ·  ");
                ui.label(RichText::new(format!("{ips}    {}", v.ctx.identity.fingerprint())).monospace().weak());
                ui.add_space(24.0);
                centered_row(ui, "buttons", |ui| {
                    if ui.button("Stop display mode").clicked() {
                        actions.push(Action::StopDisplay);
                    }
                    if ui.button(if v.overlay { "Hide debug" } else { "Show debug" }).clicked() {
                        actions.push(Action::ToggleOverlay);
                    }
                });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "{} disconnects / leaves display mode  ·  {} debug overlay  ·  {} fullscreen",
                        hotkey_label('G'),
                        hotkey_label('D'),
                        hotkey_label('F')
                    ))
                    .weak(),
                );
                if v.overlay {
                    ui.add_space(16.0);
                    // A centred block, left-aligned inside (it holds a log).
                    // (Measure outside the row: inside it egui reports less width.)
                    let available = ui.available_width();
                    let width = available.min(1000.0);
                    let height = ui.available_height();
                    ui.horizontal(|ui| {
                        ui.add_space(((available - width) / 2.0).max(0.0));
                        ui.allocate_ui_with_layout(
                            egui::vec2(width, height),
                            Layout::top_down(Align::Min),
                            |ui| {
                                display_stats(ui, &d);
                                log_view(ui, st, 220.0);
                            },
                        );
                    });
                }
            });
        });
}

/// A horizontal row centred in the available width.
///
/// egui lays a row out left to right without knowing its width in advance,
/// so the row is measured on one (invisible) frame and centred using that
/// width from then on.
fn centered_row(ui: &mut Ui, salt: &str, add: impl FnOnce(&mut Ui)) {
    let id = ui.scope_id().with(salt);
    let ctx = ui.ctx().clone();
    let measured: Option<f32> = ctx.data(|d| d.get_temp(id));
    let available = ui.available_width();
    ui.horizontal(|ui| {
        match measured {
            Some(w) => ui.add_space(((available - w) / 2.0).max(0.0)),
            None => ui.set_invisible(),
        }
        let start = ui.cursor().min.x;
        add(ui);
        let width = ui.min_rect().max.x - start;
        if measured.is_none_or(|w| (w - width).abs() > 0.5) {
            ctx.data_mut(|d| d.insert_temp(id, width));
            ctx.request_repaint();
        }
    });
}

/// Drawn over the video while streaming: a short hint and/or the debug overlay.
pub fn stream_overlay(ctx: &egui::Context, st: &mut UiState, v: &View, show_hint: bool) {
    if show_hint {
        egui::Area::new(egui::Id::unique("silkscreen-hint"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -40.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(200))
                    .stroke(egui::Stroke::new(1.0, Color32::from_gray(90)))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(18, 10))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} disconnects  ·  {} shows debug info",
                                hotkey_label('G'),
                                hotkey_label('D')
                            ))
                            .size(16.0)
                            .color(Color32::WHITE),
                        );
                    });
            });
    }
    if v.overlay {
        egui::Area::new(egui::Id::unique("silkscreen-overlay"))
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(16.0, 16.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(210))
                    .corner_radius(8.0)
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.set_max_width(760.0);
                        ui.label(RichText::new("Silkscreen debug").strong().color(ACCENT));
                        display_stats(ui, &v.display.clone().unwrap_or_default());
                        log_view(ui, st, 180.0);
                    });
            });
    }
}
