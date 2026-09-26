//! Embeds assets/icons/icon.ico as the Windows executable icon so
//! Explorer, the taskbar and shortcuts show the Shrinkr logo.
fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/icons/icon.ico");
        winresource::WindowsResource::new()
            .set_icon("assets/icons/icon.ico")
            .compile()
            .expect("failed to embed Windows icon resource");
    }
}
