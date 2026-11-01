//! Встраивает Windows-манифест: Common Controls v6 (иначе wxWidgets ругается и контролы выглядят по-старому),
//! per-monitor DPI, UTF-8 code page, длинные пути.
use embed_manifest::manifest::{ActiveCodePage, DpiAwareness, Setting, SupportedOS::*};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        let manifest = new_manifest("Midifon.Recorder")
            .supported_os(Windows7..=Windows10)
            .active_code_page(ActiveCodePage::Utf8)
            .dpi_awareness(DpiAwareness::PerMonitorV2)
            .long_path_aware(Setting::Enabled);
        if let Err(e) = embed_manifest(manifest) {
            println!("cargo:warning=Failed to embed manifest: {e}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
