fn main() {
    // Last.fm API credentials live in a gitignored `lastfm.keys` next to this file (the repo is
    // public — they must never be tracked). Format: `LIMUSIC_LASTFM_API_KEY=…` and
    // `LIMUSIC_LASTFM_API_SECRET=…`, one per line. Exported as compile-time env for `option_env!`
    // in lastfm.rs; missing file just means the scrobbler reports "not configured".
    println!("cargo:rerun-if-changed=lastfm.keys");
    if let Ok(keys) = std::fs::read_to_string("lastfm.keys") {
        for line in keys.lines() {
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if k == "LIMUSIC_LASTFM_API_KEY" || k == "LIMUSIC_LASTFM_API_SECRET" {
                    println!("cargo:rustc-env={k}={v}");
                }
            }
        }
    }
    // tauri-build watches tauri.conf.json but not the icons it embeds, so editing a PNG here
    // leaves `generate_context!` emitting the old `default_window_icon` (window, tray, taskbar).
    println!("cargo:rerun-if-changed=icons");

    // Android: libmpv is not a system library there. The playback engine (`libmpv2`) links with a
    // bare `-lmpv`, so the linker needs the prebuilt arm64 libmpv we ship with the app. The same
    // directory is `jniLibs`, which Gradle packs into the APK — at load time the Android linker
    // resolves libmpv and its bundled ffmpeg suite from the APK's own native lib dir.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        let jni_libs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("gen/android/app/src/main/jniLibs/arm64-v8a");
        println!("cargo:rustc-link-search=native={}", jni_libs.display());
        println!("cargo:rerun-if-changed={}", jni_libs.display());
    }

    tauri_build::build()
}
