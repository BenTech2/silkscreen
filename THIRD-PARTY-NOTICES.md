# Third-party software in Silkscreen

Silkscreen itself is licensed under the MIT License or the Apache License 2.0,
at your option ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
It builds on the work below; thank you to everyone who made it.

The Rust crates compiled into Silkscreen, with their full license texts, are
listed in [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md).

## FFmpeg

Video encoding and decoding on Windows, macOS and Linux. <https://ffmpeg.org>
FFmpeg is a trademark of Fabrice Bellard. Silkscreen uses FFmpeg's libraries
unmodified, as separate shared libraries.

- **Windows and Linux downloads** include FFmpeg 8.1 shared libraries
  (`avcodec`, `avutil`, `swresample`) from
  [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds) (the `lgpl-shared`
  builds), licensed under the **GNU Lesser General Public License v3**; the
  license is included in each download as `FFMPEG-LICENSE.txt`. You may replace
  these libraries with your own build of the same FFmpeg version. Source code:
  <https://github.com/FFmpeg/FFmpeg> (tag `n8.1`) and the build scripts at
  BtbN/FFmpeg-Builds.
- **The macOS app** includes Homebrew's FFmpeg build
  (<https://formulae.brew.sh/formula/ffmpeg>), which enables GPL components
  such as x264 and x265 and is licensed under the **GNU General Public License
  v3 or later**. That copy of Silkscreen.app is therefore distributed under the
  GPL v3 as a whole; Silkscreen's own MIT/Apache-2.0 code is compatible with
  it, and all of its source code is in this repository. FFmpeg's source code:
  <https://github.com/FFmpeg/FFmpeg>; the formula lists the exact version and
  build options. `silkscreen doctor` prints the FFmpeg version in use.
- **The macOS app for Intel Macs** includes FFmpeg 8.1 built for it by
  `scripts/package-macos-intel.sh` (VideoToolbox, no other external
  libraries), licensed under the **GNU Lesser General Public License v2.1 or
  later** (included as `FFMPEG-LICENSE.txt`). Source:
  <https://ffmpeg.org/releases/ffmpeg-8.1.tar.xz>; the script shows exactly
  how it was built.
- **The Android app** does not include FFmpeg; it decodes with Android's
  MediaCodec.

## PyroWave

The experimental PyroWave codec. <https://github.com/Themaister/pyrowave>
Copyright (c) Hans-Kristian Arntzen. **MIT License** (included in the
downloads as `PYROWAVE-LICENSE.txt`). The macOS app includes PyroWave's Metal
port; the Windows and Linux downloads include `libpyrowave-shared`, built from
PyroWave's Vulkan implementation together with the parts below.

- **Granite** — <https://github.com/Themaister/Granite>, Copyright (c)
  2017-2026 Hans-Kristian Arntzen, **MIT License**.
- **volk** — <https://github.com/zeux/volk>, Copyright (c) 2018-2026 Arseny
  Kapoulkine, **MIT License**.
- **Vulkan-Headers** — <https://github.com/KhronosGroup/Vulkan-Headers>,
  Copyright The Khronos Group Inc., **Apache License 2.0 or MIT License**.

The MIT License text (with each project's copyright line as above):

```
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Android app libraries

The Android app uses AndroidX (core-ktx, activity-compose) and Jetpack Compose
(UI, Foundation, Material 3), Copyright The Android Open Source Project,
**Apache License 2.0** (see [LICENSE-APACHE](LICENSE-APACHE)).
<https://developer.android.com/jetpack/androidx>

## Parsec Virtual Display Driver

On Windows, Silkscreen creates virtual monitors through the Parsec Virtual
Display Driver, which is installed separately and not included with
Silkscreen. Silkscreen's code that talks to the driver follows the interface
documented by <https://github.com/nomi-san/parsec-vdd> (BSD 3-Clause License).

## Fonts

The app's interface uses the fonts bundled with egui (`epaint_default_fonts`):
Ubuntu Light (Ubuntu Font Licence 1.0), Hack (MIT), Noto Emoji (SIL Open Font
License 1.1), emoji-icon-font (MIT) and egui's icon font. Their licenses are in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md).
