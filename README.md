# midifon
MIDI recorder/player. `mfcli` — terminal REPL, `midifon` — wxdragon GUI. Both use `midifon-core`.

- `crates/midifon-core` — engine (recording, pause split, timer, controller carry-over, SMF writer, playback)
- `crates/mfcli` — interactive terminal (`help` for commands)
- `crates/midifon` — GUI (menu bar, Start/Stop/Play buttons, settings, log)

Build: `cargo build --release` (or `-p mfcli`).
Linux build deps: `libasound2-dev pkg-config`; GUI also `cmake build-essential libgtk-3-dev libclang-dev libssl-dev` (wxWidgets is built from source on the first build, ~15 min).
Config: shared TOML in the OS config dir (`midifon/config.toml`).
