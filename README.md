# OpenMicro — stock Codex Micro support

A fork of [conol-ai/openmicrokbd](https://github.com/conol-ai/openmicrokbd)'s companion app (OpenMicro.app 0.15.0) that drives the **Work Louder Codex Micro on its stock firmware** (tested on 0.1.43) from a Mac, without ChatGPT Desktop and without flashing anything.

Upstream expects its own firmware with an on-device keymap. The stock pad has no keymap storage, so this fork runs every binding on the host instead. Everything below is about the changes in this fork; see upstream for the hardware, firmware and original app.

## What's changed

### Stock-firmware backend (`app/src/stock.rs`)
- Falls back to the stock pad (VID `0x303A`, vendor usage page `0xFF00`, JSON-RPC over report 6) when no OpenMicro firmware is found. Opens non-exclusively.
- Maps pad events to app slots: keys → 0–12, knob CW/CCW → 13/14, knob press → 15, joystick U/D/L/R → 16–19. The stock firmware's encoder names are reversed; this is corrected.
- All bindings run on the Mac (keystrokes, media, macros, actions).
- Lighting via `v.oai.rgbcfg` (backlight + ambient) and `v.oai.thstatus` (the six translucent keys can be coloured individually; the rest share one colour).
- **Safety:** firmware update, DFU, bootloader and device-mode commands are refused for the stock pad. Only read-only queries and lighting calls are sent.

### Profiles
- **Per-app switching:** link profiles to apps; the active profile follows the front app, with one Default profile as the fallback.
- **Inherit from Default:** unbound controls fall back to the Default profile's binding; "Do nothing" disables a control explicitly.
- **Hold layers:** hold a trigger key to give the other controls a second set of bindings; a bare tap still runs the trigger's own binding. Two layer keys chord with each other in either order.
- **Hold modifier:** a key can act as Shift/⌃/⌥/⌘ (or a mix) for as long as it's held, including on real mouse clicks. Its own hold layer still runs, with the held modifier lifted for those shortcuts.
- **Per-profile lighting** and per-key colours (the six agent keys on stock hardware).
- **Lighting effects:** the backlight and the ambient glow can each run a firmware effect (Breath, Snake or Rainbow) at slow, medium or fast speed (Settings → Pad → Backlight effect / Ambient effect).
- **Separate ambient brightness:** Settings → Pad → Ambient brightness sets the glow apart from the key backlight; by default it follows the backlight.

### Knob
- **App switcher** rotation preset: turning holds ⌘ and steps through apps (⌘Tab / ⌘⇧Tab). The selection commits when the knob rests for 0.8 s or when you press the knob.

### Claude Code status light
- `agent-hook claude-code` hooks light keys 1–2: blue pulse while working (the pad's own breath effect on stock firmware, so it's smooth), orange flash when it needs you, green when done, red on error. Multiple sessions take turns. Ambient lighting is left alone.

### Shortcut presets
- Multi-step presets: open an app, wait, press keys, type text (e.g. Slack search, Outlook calendar/mail, Obsidian command).
- Added Microsoft Outlook presets.

### Battery, dimming, power
- Battery level from `device.status`, polled every 2 minutes, shown in the menu bar ("Connected · battery N%") and Settings.
- Backlight and ambient dim to 10% after a quiet spell (Settings → Standby after, default 1 min, or Never); live status keys stay bright; any input restores.
- No traffic is sent to the pad while the Mac's display sleeps; the app catches up on wake.
- Pad power state (idle time, standby, BLE, battery, connect/disconnect) and Bluetooth bond/slot state are logged to `~/Library/Logs/OpenMicro/pad-status.log`.

### Other
- Editor: 4×4 board that scales with the window, keys numbered 1–13, stock keycap icons, plain-words summary of what the selected control does; device-mode switcher hidden for the stock pad.
- Menu bar no longer shows the firmware/serial line.
- English-only UI (language picker removed).
- Fix: letter/digit keystroke synthesis no longer crashes on macOS (uses virtual keycodes).
- `openmicro-app probe-rpc <file>`: runs a fixed set of read-only RPC queries and writes the replies to a file.

## Build and install (macOS)

```sh
cd app
cargo test --release
cargo build --release --bin openmicro-app
../scripts/install-local-app.sh          # → ~/Applications/OpenMicro Local.app
open "$HOME/Applications/OpenMicro Local.app"
```

`install-local-app.sh` wraps the build in a separate app bundle (`local.sxie.OpenMicroLocal`, no auto-updates) so it doesn't clash with upstream `/Applications/OpenMicro.app`. Quit upstream OpenMicro first; both share `~/Library/Application Support/OpenMicro/config.json`.

The app needs **Input Monitoring** (to read the pad) and **Accessibility** (to send keystrokes). To keep those grants across rebuilds, create a self-signed code-signing certificate named **"OpenMicro Local Signing"** in your login keychain (Keychain Access → Certificate Assistant → Create a Certificate → Code Signing). The script uses it when present and falls back to ad-hoc signing otherwise; with ad-hoc signing you'll need to re-grant permissions after each build.

## Known limits
- Keys fire on press; no hold-to-repeat.
- Joystick mouse mode isn't implemented for the stock pad.
- The pad itself can wake the Mac over Bluetooth; turn it off when away if that matters.

## License

[MIT](LICENSE), as upstream (© 2026 Tony Huang). Not affiliated with OpenAI or Work Louder.
