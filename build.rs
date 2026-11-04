//! Embeds the icon in the exe, which is where the Start menu, Installed apps and
//! Explorer read it from; the window and tray icons are set at runtime in ui.rs.

fn main() {
    println!("cargo:rerun-if-changed=assets");
    embed_resource::compile("assets/voicetype.rc", embed_resource::NONE).manifest_optional().unwrap();
}
