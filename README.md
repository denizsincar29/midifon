# midifon
MIDI recorder/player. `mfcli` — terminal (done), `midifon` — wxdragon GUI (next).
Workspace: `crates/midifon-core` (engine, shared by both), `crates/mfcli`.
Build: `cargo build --release -p mfcli` (Linux needs libasound2-dev).
Config: shared TOML in the OS config dir (`midifon/config.toml`).
