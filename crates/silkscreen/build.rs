// Embeds the app icon (assets/icon.ico) in silkscreen.exe.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    let ico = ico.canonicalize().expect("assets/icon.ico");
    let rc = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("silkscreen.rc");
    let path = ico.display().to_string().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
