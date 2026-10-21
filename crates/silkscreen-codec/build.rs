// Exposes the FFmpeg version ffmpeg-sys-next found, so the code can use
// APIs that newer releases add (and older ones lack, or the reverse).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_7_1)");
    if std::env::var("DEP_FFMPEG_FFMPEG_7_1").is_ok_and(|v| v == "true") {
        println!("cargo::rustc-cfg=ffmpeg_7_1");
    }
}
