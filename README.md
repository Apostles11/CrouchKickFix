# Crouch Kick Fix

8ms input buffer for crouch kicks — makes them land consistently. **Native Northstar plugin: no `midimap.dll`, no manual game-root install.**

Buffers your jump & crouch key presses by up to 8ms; if both land within the window they re-emit in order so the crouch kick registers. A port of the crouch-kick fix from FzzyMod / TF2SR-Menu-Mod to a self-contained Northstar plugin (hooks `inputsystem`'s `PostEvent`; the per-frame flush runs in the plugin's runframe).

## Features

- 8ms crouch-kick input buffer
- **Auto-detects your jump/crouch binds** from the engine — works for any rebind (multiple crouch keys, mouse/scroll); no key config
- Optional **on-screen speed-gain readout** (`+N`/`-N` at screen centre) on each kick, via a native wall-run detector
- **Mod Settings menu** integration (Enable CKF + Enable UI Feedback)

## Settings (Mod Settings menu)

Requires the [ModSettings](https://thunderstore.io/c/northstar/p/EladNLG/ModSettings/) mod (listed as a dependency). Under **Mods → CrouchKickFix**:

- **Enable CKF** (`ckf_enabled`) — the 8ms crouch-kick buffer (the fix). Default On.
- **Enable UI Feedback** (`ckf_ui_feedback`) — flashes the speed gain (`+N` / `-N`) at screen centre on each detected kick. Default On.

## Keys

No key config needed — the plugin reads your **actual** jump/crouch binds from the engine (via the [tf2-input](https://github.com/FromWau/tf2-input-lib) crate), so it works for any rebind, including multiple crouch keys (e.g. LCtrl + C) and mouse/scroll.

## Install

Thunderstore-layout package — install via the Thunderstore mod manager (pulls in ModSettings automatically), or drop the folder into `<profile>/packages/` so it becomes `packages/FromWau-CrouchKickFix-2.0.0/` (`mods/FromWau.CrouchKickFix/` + `plugins/crouchkick_plugin.dll`). Restart the game.

> Upgrading from 1.x? Delete the old `midimap.dll` from your Titanfall2 root — it's no longer used.

## Build

Cross-compiles Linux → Windows:

```
rustup target add x86_64-pc-windows-gnu
sudo pacman -S mingw-w64-gcc
cargo build --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/crouchkick_plugin.dll
```

Pure buffer logic lives in `crates/crouchkick-core` (host-testable: `cargo test`); `crates/crouchkick-plugin` is the rrplug + `retour` glue that hooks `PostEvent`. Bind resolution comes from the [`tf2-input`](https://github.com/FromWau/tf2-input-lib) crate (a git dependency).

## Credits

Port of the crouch-kick fix from [FzzyMod](https://github.com/Fzzy2j/FzzyMod) (Fzzy2j) / [TF2SR-Menu-Mod](https://github.com/zweek/TF2SR-Menu-Mod) (zweek).

## Any Issues? / Not working

Open an issue here: https://github.com/FromWau/CrouchKickFix/issues

## Changelog

### 2.0.0
- **Native rewrite** — self-contained Northstar plugin; **no more `midimap.dll` / manual install**.
- Native **crouch-kick detector** with an optional on-screen **speed-gain readout** (`+N`/`-N` at screen centre), built on the wall-run flag read from `client.dll`.
- **Mod Settings** menu integration (Enable CKF + Enable UI Feedback).

### 1.0.0
- Initial release (bundled FzzyMod `midimap.dll` + `voice_forcemicrecord` enable bit)
