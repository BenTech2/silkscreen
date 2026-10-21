// Exposes the FFmpeg version ffmpeg-sys-next found, so the code can use
// APIs that newer releases add (and older ones lack, or the reverse).
// With the `pyrowave` feature, also builds PyroWave.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_7_1)");
    if std::env::var("DEP_FFMPEG_FFMPEG_7_1").is_ok_and(|v| v == "true") {
        println!("cargo::rustc-cfg=ffmpeg_7_1");
    }
    #[cfg(feature = "pyrowave")]
    pyrowave::build();
}

#[cfg(feature = "pyrowave")]
mod pyrowave {
    use std::path::{Path, PathBuf};

    pub fn build() {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
        let src = root.join("third_party/pyrowave");
        if !src.join("pyrowave.h").exists() {
            panic!(
                "PyroWave sources missing: run `git submodule update --init --recursive \
                 third_party/pyrowave third_party/granite` (or build with \
                 --no-default-features to leave PyroWave out)"
            );
        }
        println!("cargo::rerun-if-changed=build.rs");
        println!("cargo::rerun-if-changed=src/pyrowave_metal_shim.mm");
        println!(
            "cargo::rerun-if-changed={}",
            src.join("pyrowave.h").display()
        );
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
            metal(&src.join("metal"));
        } else {
            vulkan(&src, &root.join("third_party/granite"));
        }
    }

    /// The Metal port, compiled straight into the binary.
    fn metal(dir: &Path) {
        cc::Build::new()
            .cpp(true)
            .std("c++14")
            .include(dir)
            .file(dir.join("pyrowave_bitstream.cpp"))
            .warnings(false)
            .compile("pyrowave_metal_cpp");
        cc::Build::new()
            .cpp(true)
            .std("c++14")
            .flag("-fobjc-arc")
            .include(dir)
            .include(dir.join("shaders"))
            .file(dir.join("pyrowave_common.mm"))
            .file(dir.join("pyrowave_encoder.mm"))
            .file(dir.join("pyrowave_decoder.mm"))
            .file("src/pyrowave_metal_shim.mm")
            .warnings(false)
            .compile("pyrowave_metal");
        for fw in ["Metal", "IOSurface", "Foundation"] {
            println!("cargo::rustc-link-lib=framework={fw}");
        }
        println!("cargo::rustc-link-lib=objc");
    }

    /// The Vulkan library, built with CMake as a shared library and copied
    /// next to the binaries, which load it when PyroWave is first used (not
    /// linked: a missing or blocked library mustn't stop the app starting).
    fn vulkan(src: &Path, granite: &Path) {
        // PyroWave's CMake expects Granite inside its own tree (which its
        // .gitignore already ignores).
        let link = src.join("Granite");
        if !link.join("CMakeLists.txt").exists() {
            // Also replaces a link made at another path (e.g. in a container).
            let _ = std::fs::remove_file(&link);
            let _ = std::fs::remove_dir(&link);
            link_dir(granite, &link);
        }
        let dst = cmake::Config::new(src)
            .define("PYROWAVE_DEVEL", "OFF")
            .profile("Release")
            .build();
        // target/<triple>/<profile>, where the binaries end up.
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let profile_dir = out.ancestors().nth(3).unwrap().to_path_buf();
        for dir in ["bin", "lib"] {
            let Ok(entries) = std::fs::read_dir(dst.join(dir)) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let runtime = name.ends_with(".dll") || name.contains(".so");
                if name.contains("pyrowave-shared") && runtime {
                    for to in [profile_dir.clone(), profile_dir.join("deps")] {
                        let _ = std::fs::create_dir_all(&to);
                        std::fs::copy(e.path(), to.join(&name))
                            .unwrap_or_else(|err| panic!("copying {name}: {err}"));
                    }
                }
            }
        }
    }

    /// Relative, so the link works wherever the checkout is mounted.
    #[cfg(unix)]
    fn link_dir(_target: &Path, link: &Path) {
        std::os::unix::fs::symlink("../granite", link).expect("linking Granite into PyroWave");
    }

    /// A directory junction: unlike a symlink, needs no special rights.
    #[cfg(windows)]
    fn link_dir(target: &Path, link: &Path) {
        // cmd takes a `/` for the start of a switch, so pass plain absolute
        // paths with backslashes (no `..`, no `\\?\` prefix).
        fn plain(p: &Path) -> String {
            let p = p.canonicalize().expect("resolving a path for mklink");
            let s = p.display().to_string();
            s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
        }
        let link = format!(
            r"{}\{}",
            plain(link.parent().unwrap()),
            link.file_name().unwrap().to_string_lossy()
        );
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(plain(target))
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "linking Granite into PyroWave ({link})");
    }
}
