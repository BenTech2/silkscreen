//! `silkscreen` — use any Windows, macOS or Linux computer as a wireless
//! display for another. Without arguments it opens the app window.

// A GUI app on Windows (no console window); CLI use re-attaches to the
// parent terminal in `main`.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod display;
mod host;
mod keymap;

use silkscreen_core::{ctx, discovery, logbuf, net, secure, status};

use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use silkscreen_platform::SourceKind;
use silkscreen_proto::{Codec, DEFAULT_CONTROL_PORT, DEFAULT_DATAGRAM_LEN, PROTOCOL_VERSION};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

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
    /// address) right away — handy for a desktop shortcut.
    #[arg(long)]
    connect: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the app straight in display mode: this computer becomes a
    /// wireless display that hosts can extend onto.
    Display {
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        port: u16,
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

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let Some((w, h)) = s.split_once(['x', 'X']) else {
        bail!("expected WIDTHxHEIGHT, got {s:?}")
    };
    Ok((w.trim().parse()?, h.trim().parse()?))
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
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new(format!(
                "warn,silkscreen={level},silkscreen_codec={level},silkscreen_platform={level}"
            ))
        })
    };
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(filter()),
        )
        .with(logbuf::BufferLayer.with_filter(filter()))
        .init();

    let ctx = ctx::Ctx::load()?;
    if let Some(name) = &cli.name {
        ctx.override_name(name);
    }
    let gui = |start_display, display, windowed, monitor, vsync, mouse, keyboard| {
        app::run(
            app::GuiOptions {
                start_display,
                display,
                windowed,
                monitor,
                vsync,
                forward_mouse: mouse,
                forward_keyboard: keyboard,
                connect: cli.connect.clone(),
            },
            ctx.clone(),
        )
    };

    let Some(command) = cli.command else {
        return gui(
            false,
            display::DisplayOptions::default(),
            false,
            None,
            true,
            true,
            true,
        );
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
            let opts = display::DisplayOptions {
                port,
                hw_decode: !no_hw_decode,
                fixed_code: pairing_code,
            };
            match headless {
                Some(spec) => display::run_headless(opts, ctx.clone(), spec),
                None => gui(
                    true,
                    opts,
                    windowed,
                    monitor,
                    !no_vsync,
                    !no_mouse,
                    !no_keyboard,
                ),
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
            code,
            resolution,
        } => host::run(
            host::HostOptions {
                target: to,
                codec: codec.map(|c| match c {
                    CodecArg::H264 => Codec::H264,
                    CodecArg::Hevc => Codec::Hevc,
                    CodecArg::Av1 => Codec::Av1,
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
        Command::Doctor => {
            println!("silkscreen {}", env!("CARGO_PKG_VERSION"));
            println!(
                "this computer: \"{}\" {}",
                ctx.name(),
                ctx.identity.fingerprint()
            );
            println!("FFmpeg {}", silkscreen_codec::ffmpeg_version());
            for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
                println!(
                    "  {:<5} encoders: {}",
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
