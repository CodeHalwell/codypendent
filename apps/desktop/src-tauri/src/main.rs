// The packaged desktop application. Behind the `app` feature (see
// Cargo.toml): `tauri::generate_context!` embeds the built frontend, so this
// binary only compiles once `npm run build` has produced `apps/desktop/dist`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    codypendent_desktop::bridge::register(tauri::Builder::default())
        .build(tauri::generate_context!())
        .expect("building the Codypendent desktop shell")
        .run(|_, _event| {
            #[cfg(target_os = "macos")]
            if matches!(_event, tauri::RunEvent::Ready) {
                set_dock_icon();
            }
        });
}

#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let main_thread = MainThreadMarker::new().expect("Dock icon must be set on the main thread");
    let data = NSData::with_bytes(include_bytes!("../icons/icon.png"));
    let image =
        NSImage::initWithData(NSImage::alloc(), &data).expect("loading the Codypendent icon");
    // Use this build's icon even when LaunchServices has cached an older app with the same ID.
    unsafe { NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&image)) };
}
