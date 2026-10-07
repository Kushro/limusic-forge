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
    // Discord application id for rich presence, read by `option_env!` in discord.rs. Cargo already
    // tracks env vars that `option_env!` reads, but saying it here keeps the build script the one
    // place that lists what a release build takes from its environment (docs/RELEASING-FORK.md).
    println!("cargo:rerun-if-env-changed=LIMUSIC_DISCORD_APP_ID");
    // A mistyped id builds and ships fine, and Discord then refuses every connection: v1.2.0-forge.1
    // went out with a digit pasted twice. Same check as `is_app_id` in discord.rs: a u64 whose
    // snowflake timestamp is not in the future. Empty means "no Discord" and is allowed.
    // No trimming: `option_env!` hands the app the value verbatim, spaces included.
    if let Ok(id) = std::env::var("LIMUSIC_DISCORD_APP_ID") {
        let ok = id.is_empty()
            || (id.bytes().all(|b| b.is_ascii_digit())
                && id.parse::<u64>().is_ok_and(|n| {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(u64::MAX, |d| d.as_millis() as u64);
                    (n >> 22).saturating_add(1_420_070_400_000) <= now_ms.saturating_add(86_400_000)
                }));
        assert!(
            ok,
            "LIMUSIC_DISCORD_APP_ID is not a Discord application id ({} digits). Copy it again from \
             the Developer Portal, General Information > Application ID",
            id.len()
        );
    }
    // tauri-build watches tauri.conf.json but not the icons it embeds, so editing a PNG here
    // leaves `generate_context!` emitting the old `default_window_icon` (window, tray, taskbar).
    println!("cargo:rerun-if-changed=icons");

    tauri_build::build()
}
