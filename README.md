# Silkscreen

Use any Windows, macOS or Linux computer as a **wireless monitor** for another —
a real extended display, not a remote-desktop window. Built for a Surface Studio
as a second screen: native resolution, 60 Hz, multitouch, over Ethernet or Wi-Fi.

Every machine runs the same `silkscreen` binary and can be either side:

- **display** — the computer that acts as the screen (e.g. the Surface Studio)
- **host** — the computer whose desktop is extended onto it

```
host                                               display
┌──────────────────────────────┐   UDP video   ┌──────────────────────────┐
│ virtual monitor (OS-native)  │ ────────────▶ │ reassemble → HW decode   │
│  → capture (zero-copy)       │               │  → GPU YUV→RGB → present │
│  → BT.709 NV12 → HW encode   │ ◀──────────── │ touch / pen / mouse      │
│ ← inject touch/pen/mouse     │  TCP control  │                          │
└──────────────────────────────┘               └──────────────────────────┘
```

When a host connects it **plugs in a virtual monitor** sized to the display's
native panel (resolution, refresh rate, and — on KDE/macOS — scale). The OS
treats it like any other monitor: arrange it, drag windows to it, set it as
primary. When the session ends the monitor is unplugged again.

## Download

Grab the file for your system from the [Releases](https://github.com/BenTech2/silkscreen/releases) page:

| System | File | Install |
|---|---|---|
| Windows 10/11 (x64) | `silkscreen-windows-x64.zip` | Unzip anywhere, run `silkscreen.exe`. Allow it on *Private* networks when the firewall asks. |
| macOS 12.3+ (Apple Silicon) | `silkscreen-macos-arm64.zip` | Unzip, then run `xattr -dr com.apple.quarantine silkscreen-macos-arm64` once (the build isn't notarized) and open `Silkscreen.app`. From a terminal: `Silkscreen.app/Contents/MacOS/silkscreen`. |
| Linux x86_64 | `silkscreen-linux-x86_64.tar.gz` | `tar xf` it and run `./silkscreen`. Bundles its own FFmpeg; needs glibc 2.36+ (Debian 12, Ubuntu 22.10, Fedora 37 or newer) and PipeWire. |
| Raspberry Pi 4 / 5 (64-bit Raspberry Pi OS) | `silkscreen-linux-aarch64.tar.gz` | Same as Linux. Best as a **display**: the Pi 4 decodes H.264 in hardware, the Pi 5 in software (fine at 1080p60). |
| Android 11+ phones and tablets | `silkscreen-android.apk` | Install the APK (allow installs from your browser/file manager). Display only. |

## Quick start

Open **Silkscreen** on both computers (run `silkscreen` with no arguments).

1. On the computer that should become the screen, click **Start display mode**.
2. On the main computer it appears under *Displays on this network* — click **Pair…**.
3. The display shows a 6-digit code; type it on the main computer. Done — the
   main computer gets a new monitor. Next time, just click **Connect**.

While a host is showing on the display, the display's **keyboard, mouse, touch
and pen all control the host**. Hotkeys on the display (never sent to the host):

| Windows / Linux | Mac | Action |
|---|---|---|
| **Ctrl+Alt+Shift+G** | **Ctrl+Option+Shift+G** or **Cmd+Option+Shift+G** | disconnect (the host shows *disconnected from the display* and waits; click **Connect** to resume). Outside a session it leaves display mode |
| Ctrl+Alt+Shift+D | Ctrl/Cmd+Option+Shift+D | debug overlay: live stats and log |
| Ctrl+Alt+Shift+F | Ctrl/Cmd+Option+Shift+F | toggle fullscreen |

The **Debug** tab shows the same log the terminal does, plus codecs, hardware
acceleration, capture backends and live stats for both sides.

### Pairing and security

Each device has its own key (shown as a fingerprint like `48c6-4328-caa5`).
Pairing uses the display's one-time code with SPAKE2, so the code can't be
guessed offline even by someone recording the network; it seeds a Noise
`XXpsk3` handshake that exchanges and authenticates both devices' keys.
Paired devices reconnect with Noise `KK` (mutual authentication, no prompt).
All control traffic (including keyboard input) is encrypted, and every video
packet is encrypted and authenticated with ChaCha20-Poly1305. Forget a device
in *Paired devices* (or `silkscreen paired --forget NAME`) to revoke it.

### Command line

The same binary works without the GUI:

```sh
silkscreen display            # open straight in display mode
silkscreen list               # see displays on the network
silkscreen host --to Surface  # extend to a display (asks for the code if not paired)
silkscreen --connect Surface  # open the app and connect right away (desktop shortcut)
silkscreen paired             # list paired devices
```

| Command | Option | Meaning |
|---|---|---|
| `host` | `--to "Surface"` | pick a display by (part of its) name, or an IP |
| `host` | `--code 123456` | pairing code shown on the display (otherwise prompted) |
| `host` | `--source monitor [--monitor DISPLAY3]` | stream an existing monitor, e.g. one with a **dummy HDMI/DP plug** |
| `host` | `--resolution auto\|full\|half` | `auto` (default) streams panels above 4K at half resolution — a 4500×3000 Surface Studio as 2250×1500 — for low latency; `full` is sharpest but adds latency (also in the app under *Stream options*) |
| `host` | `--codec hevc\|h264\|av1`, `--bitrate 80` | override codec / Mbit/s |
| `host` | `--size 2560x1440 --refresh 60` | override the display's native mode |
| `host` | `--mtu 8900` | larger UDP datagrams on a jumbo-frame Ethernet network |
| `host` | `--source test-pattern` | animated pattern that draws touch points — tests everything except capture |
| `display` | `--windowed`, `--monitor N` | window instead of fullscreen / which monitor |
| `display` | `--no-vsync` | lowest latency (may tear) |
| `display` | `--no-keyboard`, `--no-mouse` | don't forward this computer's keyboard / mouse |
| `display` | `--keep-awake while-connected\|while-active\|never`, `--idle-minutes 10` | keep the screen from sleeping while it shows a host (default: while connected), like a video player; `while-active` lets it sleep after N minutes of a still picture. Saved; also in the app |
| any | `--name "Surface Studio"` | the name other devices see (also editable in the app) |
| any | `doctor` | show codecs, hardware acceleration, capture backends |

## Android

An Android phone or tablet (Android 11+) can be a **display**: install
`silkscreen-android.apk`, tap **Start display mode**, and pick it from
Silkscreen on your computer like any other display (same pairing code flow).

- Hardware HEVC/H.264/AV1 decoding with MediaCodec, rendered straight to
  the screen (zero-copy, low-latency mode).
- Touch (multi-finger), **stylus** with pressure, tilt, hover, barrel button
  and eraser, mouse and scroll, and hardware keyboards are forwarded to the
  computer.
- **Back** opens a menu (disconnect, debug info, leave display mode); with a
  keyboard, Ctrl+Alt+Shift+G / D work as on desktop.
- The "keep this screen awake" setting applies here too.

Build it with `scripts/build-android.sh` (needs the Android SDK + NDK, JDK 17–21,
`cargo-ndk`, and the `aarch64-linux-android` / `x86_64-linux-android` Rust
targets); the APK lands in `dist/`. Install with `adb install dist/silkscreen-android.apk`.

Android can only be a display: it has no way to give apps an extra desktop
monitor of its own to extend onto.

## Platform support

| | Virtual monitor | Capture | Touch → host | Keyboard → host |
|---|---|---|---|---|
| **Linux / KDE Plasma 6** | KWin virtual output, exact size *and* scale, no prompt¹ | PipeWire (zero-copy) | real multitouch (KWin fake-input) | KWin fake-input |
| **Linux / GNOME & others** | xdg-desktop-portal *virtual* source (one-time prompt) | PipeWire (zero-copy) | real multitouch (RemoteDesktop portal) | RemoteDesktop portal |
| **Windows 10 1809+ / 11** | Parsec Virtual Display Driver² or a dummy plug | DXGI Desktop Duplication (+ cursor) | real multitouch **and pen** with pressure/tilt | scancodes (layout-independent) |
| **macOS 12.3+** | `CGVirtualDisplay` (HiDPI) | ScreenCaptureKit (zero-copy) | pointer (macOS has no touch injection API) | CGEvent key codes |

Keys are sent by *physical position*, so the host's keyboard layout decides
what they type. A Mac's Command key arrives as the Windows/Super key on a
Windows or Linux host.

Every platform can be a **display** (winit + wgpu renderer, hardware decode via
D3D11VA / VideoToolbox / VAAPI / CUDA).

¹ On first use Silkscreen installs `~/.local/share/applications/silkscreen.desktop`,
which is how KWin grants a program its virtual-output and input interfaces
(the same mechanism `krfb-virtualmonitor` uses). If that is not possible it
falls back to the portal.

² Install the Parsec VDD driver once ([parsec-vdd](https://github.com/nomi-san/parsec-vdd),
v0.41 recommended). To get the display's *exact* resolution (e.g. 4500×3000) run
`silkscreen host` once as Administrator so it can register the mode with the
driver. Without the driver, use a dummy plug and `--source monitor`.

Permissions: macOS needs **Screen Recording** (host) and **Accessibility** (input)
in System Settings › Privacy & Security.

Firewall: the **display** listens on TCP 47810 (control) and UDP 47811 (video);
discovery uses mDNS (UDP 5353). On Windows, allow Silkscreen on *Private*
networks when prompted. If discovery is blocked, `silkscreen host --to <ip>` still works.

## Performance

Measured on the development machine (RTX 5070, NVENC HEVC → CUDA decode, loopback):

| Mode | Encode (incl. RGB→NV12) | Receive → decoded | Rate |
|---|---|---|---|
| 1920×1080 @ 60 | 1.4 ms | ~1 ms | 60 fps |
| 2560×1600 @ 60 (KWin virtual output) | 3.0 ms | 1.4 ms | 60 fps |
| 3840×2160 @ 60 | 5.4 ms | 2.5 ms | 60 fps |
| 4500×3000 @ 60 (Surface Studio panel) | 8.9 ms | 4.0 ms | 60 fps |

Network time is on top (≈0.2 ms on gigabit Ethernet; 2–10 ms on Wi-Fi).

Latency design:
- **Every frame is decodable on arrival**: no B-frames, 1-frame VBV, zero-latency encoder presets, slice threading only.
- **Loss → freeze, not garbage**: a lost packet drops frames until the next IDR, which the display requests immediately (rate-limited); a static screen is re-encoded as IDR on request. Periodic IDR every 30 s as a backstop.
- **Only changed frames are sent**: the compositor/duplication API reports damage, so an idle desktop costs nothing.
- **Zero-copy capture** on Linux and macOS: the compositor's buffer is converted directly to NV12 (parallel BT.709 converter, 2 ms at 13.5 MP) and the buffer is returned as soon as the encoder is done.
- **Mailbox presentation** on the display: the newest frame replaces any queued one.
- **HEVC by default**: most hardware H.264 encoders stop at 4096 px wide (too small for a 4500 px Surface Studio panel); Silkscreen falls back through the display's codec list automatically.

## Building

Needs Rust (stable), clang, and FFmpeg 7.1+ development files (8.x tested).

```sh
# Fedora:  dnf install ffmpeg-devel pipewire-devel clang   (ffmpeg-devel from RPM Fusion for x264/NVENC)
# Ubuntu:  apt install libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev \
#                      libavdevice-dev libswscale-dev libswresample-dev libpipewire-0.3-dev clang
# macOS:   brew install ffmpeg pkg-config
# Windows: download an FFmpeg *shared* build (e.g. BtbN win64-lgpl-shared) and set FFMPEG_DIR
cargo build --release
```

Release bundles (each ships its own FFmpeg):

- Windows, cross-compiled from Linux: `FFMPEG_DIR=… scripts/package-windows.sh`
- Linux x86_64 / Raspberry Pi: `scripts/package-linux.sh x86_64|aarch64` (in a Debian 12 container; aarch64 via qemu)
- Android: `scripts/build-android.sh`
- macOS: built by `.github/workflows/release.yml` when a release is published

Workspace layout:

| Crate | Purpose |
|---|---|
| `silkscreen-proto` | wire protocol: framed control messages, UDP packetizer/reassembler |
| `silkscreen-codec` | FFmpeg hardware encode/decode, BT.709 RGB→NV12 converter |
| `silkscreen-platform` | per-OS virtual monitor, capture, input injection; test pattern |
| `silkscreen-core` | shared by desktop and Android: identity and pairing, encryption, mDNS discovery, settings, the display's network service |
| `silkscreen` | the desktop app: GUI (egui), CLI, host sessions, FFmpeg decoding, wgpu renderer |
| `silkscreen-android` | the Android display: JNI bridge + MediaCodec decoding (UI in `android/`, Kotlin + Compose) |

## Status and limitations

Tested end to end on Linux (KDE Plasma 6.7, Wayland): virtual output creation,
PipeWire capture, encode/decode, rendering, pointer and keyboard injection,
pairing, the GUI, hotkeys and the debug overlay. The Windows and
macOS backends compile against the real platform APIs but **have not yet been
run on those systems** — expect some first-run fixes there.

Not done yet:
- Pen pressure on Linux/macOS hosts (KWin's fake-input and CGEvent have no tablet API; the pen drives the pointer).
- Distinguishing pen from touch on the *display* side (winit reports pen contacts as touches).
- System shortcuts the display's OS keeps for itself (e.g. Alt+Tab, the Windows key on a Windows
  display) are not forwarded.
- Audio, HDR/10-bit, forward error correction for lossy Wi-Fi, adaptive bitrate (receiver stats
  are already reported to the host), GPU zero-copy encode on Windows.

## License

MIT OR Apache-2.0. FFmpeg is used as a shared library under the LGPL.
