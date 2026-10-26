//! `silkscreen` — use any Windows, macOS or Linux computer as a wireless
//! display for another. Without arguments it opens the app window.

// A GUI app on Windows (no console window); CLI use re-attaches to the
// parent terminal in `main`.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod display;
mod host;
mod keymap;
mod mirror;
mod pacer;

use silkscreen_core::{ctx, discovery, logbuf, net, secure, status};

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use silkscreen_platform::SourceKind;
use silkscreen_proto::{Codec, DEFAULT_CONTROL_PORT, DEFAULT_DATAGRAM_LEN, PROTOCOL_VERSION};
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, reload};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// More logging (-v debug, -vv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// Name other devices see (default: saved name or hostname).
    #[arg(long, global = true)]
    name: Option<String>,
    /// Open the app and extend this computer to the given display (name or
    /// address) right away — handy for a desktop shortcut. Repeat it to
    /// extend to several displays.
    #[arg(long)]
    connect: Vec<String>,
    /// Like --connect, but the display shows a copy of this computer's main
    /// screen (or the one picked in Stream options). Repeatable.
    #[arg(long)]
    mirror: Vec<String>,
    /// Started at login: open as the app's login setting says.
    #[arg(long, hide = true)]
    autostart: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the app straight in display mode: this computer becomes a
    /// wireless display that hosts can extend onto.
    Display {
        /// Port to listen on (default: the app's setting, normally 47810).
        #[arg(long)]
        port: Option<u16>,
        /// Run in a window instead of fullscreen.
        #[arg(long)]
        windowed: bool,
        /// Monitor index to use when this computer has several.
        #[arg(long)]
        monitor: Option<usize>,
        /// Decode in software only.
        #[arg(long)]
        no_hw_decode: bool,
        /// Present immediately, allowing tearing, for the lowest latency.
        #[arg(long)]
        no_vsync: bool,
        /// Don't forward this computer's mouse to the host.
        #[arg(long)]
        no_mouse: bool,
        /// Don't forward this computer's keyboard to the host.
        #[arg(long)]
        no_keyboard: bool,
        /// Skip queued frames to catch up when the picture is more than this
        /// many milliseconds behind the host (e.g. 150 on Wi-Fi).
        #[arg(long, value_name = "MS")]
        skip_behind: Option<u32>,
        /// Keep this screen from sleeping while it shows a host (saved).
        #[arg(long, value_enum)]
        keep_awake: Option<KeepAwakeArg>,
        /// With `--keep-awake while-active`: minutes of a still picture
        /// before the screen may sleep (saved).
        #[arg(long)]
        idle_minutes: Option<u32>,
        /// Use a fixed pairing code instead of a random one (scripted setups).
        #[arg(long, hide = true)]
        pairing_code: Option<String>,
        /// Decode without a window (testing): WIDTHxHEIGHT[@HZ].
        #[arg(long, value_name = "MODE")]
        headless: Option<String>,
        /// With --headless: exit after this many seconds.
        #[arg(long)]
        duration: Option<u64>,
        /// With --headless: move the host's pointer in a circle (tests input).
        #[arg(long, hide = true)]
        input_test: bool,
    },
    /// Extend this computer's desktop onto a wireless display.
    Host {
        /// Display name (or part of it) or IP address. Optional when there is
        /// only one display on the network.
        #[arg(long)]
        to: Option<String>,
        #[arg(long, value_enum)]
        codec: Option<CodecArg>,
        /// Video bitrate in Mbit/s (default: scaled to resolution).
        #[arg(long)]
        bitrate: Option<f32>,
        /// Force an FFmpeg encoder, e.g. h264_nvenc, hevc_vaapi, libx264.
        #[arg(long)]
        encoder: Option<String>,
        /// What to stream.
        #[arg(long, value_enum, default_value_t = SourceArg::Virtual)]
        source: SourceArg,
        /// With --source monitor: which monitor (name/connector).
        #[arg(long)]
        monitor: Option<String>,
        /// Override the resolution, e.g. 2560x1440.
        #[arg(long, value_name = "WxH")]
        size: Option<String>,
        /// Stream resolution: auto = native up to 4K, half above (lowest
        /// latency); full = always native (sharpest); half = always half.
        #[arg(long, value_enum, default_value_t = ResolutionArg::Auto)]
        resolution: ResolutionArg,
        /// Override the refresh rate in Hz.
        #[arg(long)]
        refresh: Option<f32>,
        /// UDP datagram size; raise to ~8900 on a jumbo-frame network.
        #[arg(long, default_value_t = DEFAULT_DATAGRAM_LEN)]
        mtu: usize,
        /// Send at most this many frames a second (e.g. 30 on Wi-Fi); the
        /// screen keeps its refresh rate.
        #[arg(long, value_name = "FPS")]
        max_fps: Option<f32>,
        /// Full-resolution colour (4:4:4): sharper coloured text, more data;
        /// uses software encoding unless the hardware supports it.
        #[arg(long)]
        full_color: bool,
        /// Exit when the display disconnects instead of reconnecting.
        #[arg(long)]
        once: bool,
        /// Pairing code shown on the display (otherwise asked for when needed).
        #[arg(long)]
        code: Option<String>,
    },
    /// List or forget paired devices.
    Paired {
        /// Forget the device with this name or fingerprint.
        #[arg(long)]
        forget: Option<String>,
    },
    /// List displays on the local network.
    List {
        #[arg(long, default_value_t = 2)]
        seconds: u64,
    },
    /// Show codecs, hardware acceleration and capture backends available here.
    Doctor,
    /// Try PyroWave on this GPU (run by the app in a separate process, so a
    /// driver crash can't take the app down). Exit code 0 if it works.
    #[command(hide = true)]
    PyrowaveProbe,
}

#[derive(Clone, Copy, ValueEnum)]
enum KeepAwakeArg {
    /// While a host is streaming (default).
    WhileConnected,
    /// While the picture is changing; then the system's sleep timeout applies.
    WhileActive,
    /// Follow the system's power settings.
    Never,
}

#[derive(Clone, Copy, ValueEnum)]
enum ResolutionArg {
    Auto,
    Full,
    Half,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    H264,
    Hevc,
    Av1,
    /// Experimental GPU wavelet codec; needs a few hundred Mbit/s.
    Pyrowave,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SourceArg {
    /// A new virtual monitor, sized to the display (like plugging one in).
    Virtual,
    /// An existing monitor, e.g. one with a dummy HDMI/DP plug.
    Monitor,
    /// Animated test pattern (draws touch points); needs no permissions.
    TestPattern,
}

static SET_LOG_LEVEL: OnceLock<Box<dyn Fn(Level) + Send + Sync>> = OnceLock::new();

/// Changes how much is logged at runtime: the GUI's `-v` / `-vv`.
pub fn set_log_level(level: Level) {
    if let Some(set) = SET_LOG_LEVEL.get() {
        set(level);
    }
}

fn log_filter(level: Level) -> EnvFilter {
    let l = level.as_str().to_lowercase();
    EnvFilter::new(format!(
        "warn,silkscreen={l},silkscreen_codec={l},silkscreen_platform={l}"
    ))
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let Some((w, h)) = s.split_once(['x', 'X']) else {
        bail!("expected WIDTHxHEIGHT, got {s:?}")
    };
    Ok((w.trim().parse()?, h.trim().parse()?))
}

/// Opens this run's log file, keeping the previous run's as `.old.log`.
fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    let _ = std::fs::create_dir_all(path.parent()?);
    let _ = std::fs::rename(path, path.with_extension("old.log"));
    std::fs::File::create(path).ok()
}

/// Runs `silkscreen pyrowave-probe` and reports whether PyroWave worked
/// there (a crash or a hang counts as no).
#[cfg(feature = "pyrowave")]
fn probe_pyrowave_in_child() -> bool {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut child = match Command::new(exe)
        .arg("pyrowave-probe")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("cannot run the PyroWave check: {e}");
            return false;
        }
    };
    let mut stderr = child.stderr.take();
    let output = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                break None;
            }
        }
    };
    let ok = status.is_some_and(|s| s.success());
    if !ok {
        let out = output.join().unwrap_or_default();
        let tail: Vec<_> = out.lines().rev().take(15).collect();
        tracing::warn!(
            "PyroWave check {} on this GPU:\n{}",
            match status {
                Some(s) => format!("failed ({s})"),
                None => "hung".to_owned(),
            },
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    ok
}

#[cfg(windows)]
fn attach_console() {
    unsafe extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

fn main() -> Result<()> {
    #[cfg(windows)]
    if std::env::args_os().len() > 1 {
        attach_console();
    }
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| log_filter(level));
    let (term_filter, term_reload) = reload::Layer::new(filter());
    let (buf_filter, buf_reload) = reload::Layer::new(filter());
    let (file_filter, file_reload) = reload::Layer::new(filter());
    // The app (not one-shot commands) also logs to a file: on Windows it
    // has no console, so that is where a crash can be looked into.
    let gui_run = matches!(
        cli.command,
        None | Some(Command::Display { headless: None, .. })
    );
    let log_path = secure::config_dir().join("silkscreen.log");
    let log_file = gui_run.then(|| open_log(&log_path)).flatten();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(term_filter),
        )
        .with(logbuf::BufferLayer.with_filter(buf_filter))
        .with(log_file.map(|f| {
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(f))
                .with_filter(file_filter)
        }))
        .init();
    let _ = SET_LOG_LEVEL.set(Box::new(move |l| {
        let _ = term_reload.reload(log_filter(l));
        let _ = buf_reload.reload(log_filter(l));
        let _ = file_reload.reload(log_filter(l));
    }));
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!("crashed: {info}\n{bt}");
        default_hook(info);
        if gui_run {
            silkscreen_platform::alert(
                "Silkscreen stopped working",
                &format!("{info}\n\nDetails are in {}", log_path.display()),
            );
        }
    }));
    tracing::info!(
        "silkscreen {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    #[cfg(feature = "pyrowave")]
    if !matches!(cli.command, Some(Command::PyrowaveProbe)) {
        silkscreen_codec::pyrowave::set_probe(probe_pyrowave_in_child);
    }

    let ctx = ctx::Ctx::load()?;
    if let Some(name) = &cli.name {
        ctx.override_name(name);
    }
    let gui = |start_display, prefs, fixed_code| {
        app::run(
            app::GuiOptions {
                start_display,
                prefs,
                fixed_code,
                connect: cli.connect.clone(),
                mirror: cli.mirror.clone(),
                log_level: level,
                autostart: cli.autostart,
            },
            ctx.clone(),
        )
    };

    let Some(command) = cli.command else {
        return gui(false, app::Prefs::load(), None);
    };
    match command {
        Command::Display {
            port,
            windowed,
            monitor,
            no_hw_decode,
            no_vsync,
            no_mouse,
            no_keyboard,
            skip_behind,
            keep_awake,
            idle_minutes,
            pairing_code,
            headless,
            duration,
            input_test,
        } => {
            let headless = headless
                .map(|m| -> Result<_> {
                    let (size, hz) = m.split_once('@').unwrap_or((&m, "60"));
                    let (width, height) = parse_size(size)?;
                    Ok(display::HeadlessSpec {
                        width,
                        height,
                        refresh_mhz: (hz.parse::<f32>()? * 1000.0) as u32,
                        duration: duration.map(Duration::from_secs),
                        input_test,
                    })
                })
                .transpose()?;
            if keep_awake.is_some() || idle_minutes.is_some() {
                ctx.update_settings(|s| {
                    if let Some(k) = keep_awake {
                        s.keep_awake = match k {
                            KeepAwakeArg::WhileConnected => ctx::KeepAwake::WhileConnected,
                            KeepAwakeArg::WhileActive => ctx::KeepAwake::WhileActive,
                            KeepAwakeArg::Never => ctx::KeepAwake::Never,
                        };
                    }
                    if let Some(m) = idle_minutes {
                        s.idle_minutes = m.max(1);
                    }
                });
            }
            match headless {
                Some(spec) => display::run_headless(
                    display::DisplayOptions {
                        port: port.unwrap_or(DEFAULT_CONTROL_PORT),
                        hw_decode: !no_hw_decode,
                        fixed_code: pairing_code,
                        catch_up: skip_behind.map(|ms| Duration::from_millis(ms as u64)),
                    },
                    ctx.clone(),
                    spec,
                ),
                None => {
                    // Flags override the app's saved settings for this run.
                    let mut prefs = app::Prefs::load();
                    let d = &mut prefs.display;
                    d.port = port.unwrap_or(d.port);
                    d.windowed |= windowed;
                    d.monitor = monitor.or(d.monitor);
                    d.hw_decode &= !no_hw_decode;
                    d.vsync &= !no_vsync;
                    d.forward_mouse &= !no_mouse;
                    d.forward_keyboard &= !no_keyboard;
                    d.skip_behind_ms = skip_behind.or(d.skip_behind_ms);
                    gui(true, prefs, pairing_code)
                }
            }
        }
        Command::Host {
            to,
            codec,
            bitrate,
            encoder,
            source,
            monitor,
            size,
            refresh,
            mtu,
            once,
            max_fps,
            full_color,
            code,
            resolution,
        } => host::run(
            host::HostOptions {
                target: to,
                codec: codec.map(|c| match c {
                    CodecArg::H264 => Codec::H264,
                    CodecArg::Hevc => Codec::Hevc,
                    CodecArg::Av1 => Codec::Av1,
                    CodecArg::Pyrowave => Codec::PyroWave,
                }),
                bitrate_kbps: bitrate.map(|m| (m * 1000.0) as u32),
                encoder,
                source: match source {
                    SourceArg::Virtual => SourceKind::Virtual,
                    SourceArg::Monitor => SourceKind::Monitor(monitor),
                    SourceArg::TestPattern => SourceKind::TestPattern,
                },
                size: size.as_deref().map(parse_size).transpose()?,
                refresh_hz: refresh,
                datagram_len: mtu,
                persist: !once,
                max_fps,
                chroma444: full_color,
                resolution: match resolution {
                    ResolutionArg::Auto => host::Resolution::Auto,
                    ResolutionArg::Full => host::Resolution::Full,
                    ResolutionArg::Half => host::Resolution::Half,
                },
            },
            ctx.clone(),
            code.map_or(host::CodeSource::Stdin, host::CodeSource::Fixed),
            host::Control::standalone(),
        ),
        Command::Paired { forget } => {
            let peers = ctx.paired.list();
            if let Some(f) = forget {
                let f = f.to_lowercase();
                let hits: Vec<_> = peers
                    .iter()
                    .filter(|p| {
                        p.name.to_lowercase() == f
                            || p.key_bytes().is_some_and(|k| secure::fingerprint(&k) == f)
                    })
                    .collect();
                match hits.as_slice() {
                    [p] => {
                        ctx.paired.remove(&p.key);
                        println!("Forgot \"{}\".", p.name);
                    }
                    [] => bail!("no paired device named or fingerprinted {f:?}"),
                    _ => bail!("several devices match {f:?}; use the fingerprint"),
                }
                return Ok(());
            }
            println!(
                "This computer: \"{}\" {}",
                ctx.name(),
                ctx.identity.fingerprint()
            );
            if peers.is_empty() {
                println!("No paired devices.");
            }
            for p in peers {
                let fp = p
                    .key_bytes()
                    .map(|k| secure::fingerprint(&k))
                    .unwrap_or_default();
                println!("  {:<28} {fp}", p.name);
            }
            Ok(())
        }
        Command::List { seconds } => {
            let found = discovery::browse(Duration::from_secs(seconds))?;
            if found.is_empty() {
                println!(
                    "No displays found. Run `silkscreen display` on the computer you want to use as a screen."
                );
            }
            for d in found {
                println!(
                    "{:<24} {:<21} {}x{} @ {:.0} Hz{}{}",
                    d.name,
                    d.addr,
                    d.summary.width,
                    d.summary.height,
                    d.summary.refresh_mhz as f32 / 1000.0,
                    if d.summary.touch { ", touch" } else { "" },
                    if d.protocol != PROTOCOL_VERSION {
                        "  (incompatible version)"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        Command::PyrowaveProbe => {
            #[cfg(feature = "pyrowave")]
            let ok = silkscreen_codec::pyrowave::probe_here();
            #[cfg(not(feature = "pyrowave"))]
            let ok = false;
            std::process::exit(if ok { 0 } else { 1 });
        }
        Command::Doctor => {
            println!("silkscreen {}", env!("CARGO_PKG_VERSION"));
            println!(
                "this computer: \"{}\" {}",
                ctx.name(),
                ctx.identity.fingerprint()
            );
            println!("FFmpeg {}", silkscreen_codec::ffmpeg_version());
            for codec in [Codec::H264, Codec::Hevc, Codec::Av1, Codec::PyroWave] {
                println!(
                    "  {:<8} encoders: {}",
                    codec.name(),
                    silkscreen_codec::compiled_encoders(codec).join(", ")
                );
            }
            let dec: Vec<_> = silkscreen_codec::decodable_codecs()
                .iter()
                .map(|c| c.name())
                .collect();
            println!("  decoders: {}", dec.join(", "));
            println!("touch points: {}", silkscreen_platform::touch_points());
            for (k, v) in silkscreen_platform::diagnostics() {
                println!("{k}: {v}");
            }
            Ok(())
        }
    }
}
