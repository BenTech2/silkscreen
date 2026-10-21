//! Silkscreen for Android: the phone or tablet becomes a wireless display.
//!
//! The Kotlin app (`android/`) owns the UI, the full-screen `Surface` and
//! input; everything else — pairing, encryption, the protocol, discovery and
//! hardware decoding with MediaCodec — is the same Rust code the desktop app
//! uses, reached through the JNI functions below (`dev.silkscreen.Native`).
//! State is handed to the UI as small JSON snapshots it polls; video never
//! passes through Kotlin.

#[cfg(target_os = "android")]
mod codec;
mod nal;

#[cfg(target_os = "android")]
mod jni_api {
    use std::sync::{Arc, Mutex, OnceLock};

    use jni::JNIEnv;
    use jni::objects::{JClass, JObject, JString};
    use jni::sys::{jboolean, jfloat, jint, jstring};
    use serde_json::json;
    use silkscreen_core::ctx::{self, Ctx, KeepAwake};
    use silkscreen_core::display::DisplayRole;
    use silkscreen_core::secure::{fingerprint, set_config_dir};
    use silkscreen_core::status::DisplayPhase;
    use silkscreen_core::{logbuf, net};
    use silkscreen_proto::{
        Codec, DEFAULT_CONTROL_PORT, DisplayCaps, InputEvent, MouseButton, TouchPhase,
    };
    use tracing::{Level, error, info};
    use tracing_subscriber::Layer;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    use crate::codec::CodecSink;

    struct App {
        ctx: Arc<Ctx>,
        sink: Arc<CodecSink>,
        display: Option<DisplayRole>,
    }

    static APP: OnceLock<Mutex<App>> = OnceLock::new();

    fn app() -> std::sync::MutexGuard<'static, App> {
        APP.get().expect("Native.init not called").lock().unwrap()
    }

    fn jstr(env: &mut JNIEnv, s: &JString) -> String {
        env.get_string(s).map(Into::into).unwrap_or_default()
    }

    fn out(env: &mut JNIEnv, s: impl AsRef<str>) -> jstring {
        env.new_string(s)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }

    /// Sets up logging, the config directory and the device identity.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_init(
        mut env: JNIEnv,
        _: JClass,
        files_dir: JString,
        default_name: JString,
    ) {
        if APP.get().is_some() {
            return; // activity recreated; keep the running state
        }
        // Our own crates at info; everything else (jni, mdns…) only warnings.
        let filter = || {
            Targets::new()
                .with_default(Level::WARN)
                .with_target("silkscreen_core", Level::INFO)
                .with_target("silkscreen_android", Level::INFO)
                .with_target("silkscreen_proto", Level::INFO)
        };
        let _ = tracing_subscriber::registry()
            .with(paranoid_android::layer("Silkscreen").with_filter(filter()))
            .with(logbuf::BufferLayer.with_filter(filter()))
            .try_init();
        set_config_dir(jstr(&mut env, &files_dir).into());
        ctx::set_default_name(&jstr(&mut env, &default_name));
        match Ctx::load() {
            Ok(ctx) => {
                info!(
                    "Silkscreen {} ready as \"{}\"",
                    env!("CARGO_PKG_VERSION"),
                    ctx.name()
                );
                let _ = APP.set(Mutex::new(App {
                    ctx,
                    sink: CodecSink::new(),
                    display: None,
                }));
            }
            Err(e) => error!("cannot load identity: {e:#}"),
        }
    }

    /// Device name, fingerprint, settings and paired devices.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_info(mut env: JNIEnv, _: JClass) -> jstring {
        let a = app();
        let s = a.ctx.settings();
        let paired: Vec<_> = a
            .ctx
            .paired
            .list()
            .into_iter()
            .map(|p| {
                let fp = p.key_bytes().map(|k| fingerprint(&k)).unwrap_or_default();
                json!({ "name": p.name, "key": p.key, "fingerprint": fp })
            })
            .collect();
        let v = json!({
            "name": a.ctx.name(),
            "fingerprint": a.ctx.identity.fingerprint(),
            "keepAwake": match s.keep_awake {
                KeepAwake::WhileConnected => 0,
                KeepAwake::WhileActive => 1,
                KeepAwake::Never => 2,
            },
            "idleMinutes": s.idle_minutes,
            "paired": paired,
        });
        drop(a);
        out(&mut env, v.to_string())
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_setName(
        mut env: JNIEnv,
        _: JClass,
        name: JString,
    ) {
        let name = jstr(&mut env, &name);
        app().ctx.set_name(&name);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_forget(
        mut env: JNIEnv,
        _: JClass,
        key: JString,
    ) {
        let key = jstr(&mut env, &key);
        app().ctx.paired.remove(&key);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_setKeepAwake(
        _: JNIEnv,
        _: JClass,
        mode: jint,
        minutes: jint,
    ) {
        app().ctx.update_settings(|s| {
            s.keep_awake = match mode {
                1 => KeepAwake::WhileActive,
                2 => KeepAwake::Never,
                _ => KeepAwake::WhileConnected,
            };
            s.idle_minutes = minutes.max(1) as u32;
        });
    }

    /// Starts display mode. Returns an error message, or null on success.
    #[unsafe(no_mangle)]
    #[allow(clippy::too_many_arguments)]
    pub extern "system" fn Java_dev_silkscreen_Native_startDisplay(
        mut env: JNIEnv,
        _: JClass,
        width: jint,
        height: jint,
        refresh_mhz: jint,
        scale: jfloat,
        touch_points: jint,
        codecs: JString,
    ) -> jstring {
        let codecs = jstr(&mut env, &codecs);
        let mut a = app();
        if a.display.is_some() {
            return std::ptr::null_mut();
        }
        let decoders = codecs
            .split(',')
            .filter_map(|c| match c.trim() {
                "hevc" => Some(Codec::Hevc),
                "h264" => Some(Codec::H264),
                "av1" => Some(Codec::Av1),
                _ => None,
            })
            .collect();
        let caps = DisplayCaps {
            name: a.ctx.name(),
            width: width as u32 & !1,
            height: height as u32 & !1,
            refresh_mhz: refresh_mhz as u32,
            scale,
            physical_size_mm: None,
            touch: touch_points > 0,
            max_touch_points: touch_points.clamp(0, 255) as u8,
            pen: true,
            decoders,
            hdr: false,
            video_port: DEFAULT_CONTROL_PORT + 1,
        };
        info!(
            "display mode: {}x{} @ {:.2} Hz, scale {scale}",
            caps.width,
            caps.height,
            refresh_mhz as f32 / 1000.0
        );
        match DisplayRole::start(
            DEFAULT_CONTROL_PORT,
            None,
            a.ctx.clone(),
            caps,
            a.sink.clone(),
        ) {
            Ok(role) => {
                a.display = Some(role);
                std::ptr::null_mut()
            }
            Err(e) => {
                error!("cannot start display mode: {e:#}");
                drop(a);
                out(&mut env, format!("{e:#}"))
            }
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_stopDisplay(_: JNIEnv, _: JClass) {
        let role = app().display.take();
        drop(role); // outside the lock: ending the session takes a moment
    }

    /// Ends the current session; the display stays available.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_disconnect(_: JNIEnv, _: JClass) {
        if let Some(d) = &app().display {
            d.service.disconnect();
        }
    }

    /// The video surface was created (non-null) or destroyed (null).
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_setSurface(
        env: JNIEnv,
        _: JClass,
        surface: JObject,
    ) {
        let window = if surface.is_null() {
            None
        } else {
            unsafe {
                ndk::native_window::NativeWindow::from_surface(env.get_raw(), surface.as_raw())
            }
        };
        let sink = app().sink.clone();
        sink.set_surface(window);
    }

    /// Live state for the UI.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_state(mut env: JNIEnv, _: JClass) -> jstring {
        let a = app();
        let d = a
            .display
            .as_ref()
            .map(|d| d.service.state.get())
            .unwrap_or_default();
        let video = *a.sink.video.lock().unwrap();
        let (phase, host, code) = match (&a.display, &d.phase) {
            (None, _) => ("off", None, None),
            (_, DisplayPhase::Pairing { host, code }) => {
                ("pairing", Some(host.clone()), Some(code.clone()))
            }
            (_, DisplayPhase::Streaming { host }) => ("streaming", Some(host.clone()), None),
            _ => ("waiting", None, None),
        };
        let v = json!({
            "phase": phase,
            "host": host,
            "code": code,
            "stream": d.stream,
            "decoder": d.decoder,
            "fps": d.fps,
            "latencyMs": d.latency_ms,
            "dropped": d.frames_dropped,
            "lost": d.packets_lost,
            "videoWidth": video.width,
            "videoHeight": video.height,
            "secondsSinceFrame": a.sink.since_last_frame().map(|t| t.as_secs_f32()),
            "address": net::local_ip().map(|i| i.to_string()),
        });
        drop(a);
        out(&mut env, v.to_string())
    }

    /// Recent log lines, newest last.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_logs(mut env: JNIEnv, _: JClass) -> jstring {
        let lines = logbuf::snapshot();
        let text = lines
            .iter()
            .rev()
            .take(300)
            .rev()
            .map(|l| format!("{:5} {}", l.level, l.text))
            .collect::<Vec<_>>()
            .join("\n");
        out(&mut env, text)
    }

    fn send(ev: InputEvent) {
        if let Some(d) = &app().display {
            d.service.send_input(ev);
        }
    }

    fn phase(p: jint) -> TouchPhase {
        match p {
            0 => TouchPhase::Down,
            1 => TouchPhase::Move,
            2 => TouchPhase::Up,
            _ => TouchPhase::Cancel,
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_touch(
        _: JNIEnv,
        _: JClass,
        id: jint,
        p: jint,
        x: jfloat,
        y: jfloat,
        pressure: jfloat,
    ) {
        send(InputEvent::Touch {
            id: id as u32,
            phase: phase(p),
            x,
            y,
            pressure: (pressure > 0.0).then_some(pressure.min(1.0)),
        });
    }

    #[unsafe(no_mangle)]
    #[allow(clippy::too_many_arguments)]
    pub extern "system" fn Java_dev_silkscreen_Native_pen(
        _: JNIEnv,
        _: JClass,
        p: jint,
        x: jfloat,
        y: jfloat,
        pressure: jfloat,
        tilt_x: jfloat,
        tilt_y: jfloat,
        barrel: jboolean,
        eraser: jboolean,
        hover: jboolean,
    ) {
        send(InputEvent::Pen {
            phase: phase(p),
            x,
            y,
            pressure: pressure.clamp(0.0, 1.0),
            tilt_x,
            tilt_y,
            barrel: barrel != 0,
            eraser: eraser != 0,
            hover: hover != 0,
        });
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_mouseMove(
        _: JNIEnv,
        _: JClass,
        x: jfloat,
        y: jfloat,
    ) {
        send(InputEvent::MouseMove { x, y });
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_mouseButton(
        _: JNIEnv,
        _: JClass,
        button: jint,
        pressed: jboolean,
    ) {
        let button = match button {
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            3 => MouseButton::Back,
            4 => MouseButton::Forward,
            _ => MouseButton::Left,
        };
        send(InputEvent::MouseButton {
            button,
            pressed: pressed != 0,
        });
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_scroll(
        _: JNIEnv,
        _: JClass,
        dx: jfloat,
        dy: jfloat,
    ) {
        send(InputEvent::Scroll {
            dx,
            dy,
            pixels: false,
        });
    }

    /// A hardware-keyboard key by Linux scancode (`KeyEvent.getScanCode()`),
    /// which is exactly the protocol's evdev encoding.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_silkscreen_Native_key(
        _: JNIEnv,
        _: JClass,
        scancode: jint,
        pressed: jboolean,
    ) {
        if (1..0x300).contains(&scancode) {
            send(InputEvent::Key {
                evdev_code: scancode as u16,
                pressed: pressed != 0,
            });
        }
    }
}
