//! Host action engine: executes an `Action` when its pad input fires (or when
//! the user presses Test in the editor).
//!
//! Everything here runs detached — `execute` spawns a worker thread per
//! invocation, so a slow shell command or a macro full of delays can never
//! stall the UI or the device reader. Within one macro, steps still run in
//! order on that single worker thread, because ordering is the whole point of
//! a macro.
//!
//! Keystroke and media-key synthesis go through the `enigo` crate and are
//! deliberately confined to this module: on macOS synthesis needs the
//! Accessibility permission, and keeping every enigo call here means the
//! permission story (`accessibility_trusted` / `needs_permission` /
//! `open_permission_settings`) lives next to the code it gates.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use crate::config::{Action, MacroStep, MediaOp};
use crate::events;
use crate::keycodes::{keyboard_name, mods_label};

/// Posted when an input bound to `Action::AppSettings` fires (the SETUP key
/// default). The UI listens for it and opens the settings sheet.
#[derive(Debug, Clone)]
pub struct OpenAppSettings;

/// HID modifier bitmask -> the enigo key that holds it. Order matters: we
/// press in this order and release in reverse, like fingers would.
const HID_MOD_KEYS: [(u8, Key); 4] = [
    (1 << 0, Key::Control), // LCtrl
    (1 << 1, Key::Shift),   // LShift
    (1 << 2, Key::Alt),     // LAlt
    (1 << 3, Key::Meta),    // LGui (Cmd / Win)
];

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Run one action, detached. Returns immediately; the work happens on a
/// throwaway thread so nothing here can block the caller.
pub fn execute(action: &Action) {
    if matches!(action, Action::None) {
        return;
    }
    let action = action.clone();
    thread::spawn(move || run_blocking(&action));
}

/// The blocking half of `execute`; only ever called on a worker thread.
fn run_blocking(action: &Action) {
    match action {
        Action::None => {}
        Action::Keystroke { mods, key } => synthesize_keystroke(*mods, *key),
        Action::Macro { steps } => {
            for entry in steps {
                if entry.enabled {
                    run_step(&entry.step);
                }
            }
        }
        Action::Run { command } => run_command(command),
        Action::Open { target } => open_target(target),
        Action::Media { op } => media_op(*op),
        Action::AppSettings => events::post(OpenAppSettings),
        Action::AppSwitch { reverse } => app_switch(*reverse),
        Action::Disabled => {}
    }
}

/// One macro step, in sequence on the macro's thread. `Run` spawns detached
/// and does NOT wait — a macro should not hang on a long-running command;
/// insert a `Delay` step if the next step depends on it.
fn run_step(step: &MacroStep) {
    match step {
        MacroStep::Keystroke { mods, key } => synthesize_keystroke(*mods, *key),
        MacroStep::Delay { ms } => thread::sleep(Duration::from_millis(*ms)),
        MacroStep::Run { command } => run_command(command),
        MacroStep::Open { target } => open_target(target),
        MacroStep::Media { op } => media_op(*op),
    }
}

/// Spawn a shell command detached (same pattern as the old hotkeys module):
/// the shell parses the line, we never wait on it.
fn run_command(command: &str) {
    if command.trim().is_empty() {
        return;
    }
    #[cfg(target_os = "windows")]
    let spawned = {
        use std::os::windows::process::CommandExt;

        // The companion app uses the GUI subsystem, so a child cmd.exe would
        // otherwise flash a console window for every Run action.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/D", "/C", command])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    };
    #[cfg(not(target_os = "windows"))]
    let spawned = std::process::Command::new("sh")
        .args(["-c", command])
        .spawn();
    if let Err(e) = spawned {
        eprintln!("actions: run '{command}': spawn failed: {e}");
    }
}

/// Open a URL / file / app with the OS default handler.
fn open_target(target: &str) {
    if target.trim().is_empty() {
        return;
    }
    if let Err(e) = open::that(target) {
        eprintln!("actions: open '{target}': {e}");
    }
}

// ---------------------------------------------------------------------------
// Synthesis (all enigo usage lives below this line)
// ---------------------------------------------------------------------------

/// Chords we synthesized in the immediate past. OS-level hotkey grabs match
/// SYNTHETIC events too, so an action that types a chord some pad slot also
/// emits would re-enter our own interception and loop; the dispatcher asks
/// `was_just_synthesized` before running an intercepted slot's action.
static SYNTH_GUARD: std::sync::Mutex<Vec<(u8, u16, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());

/// True if this exact (modifiers, usage) chord was synthesized by us within
/// the last few hundred milliseconds. Consumes the entry.
pub fn was_just_synthesized(mods: u8, usage: u16) -> bool {
    let Ok(mut guard) = SYNTH_GUARD.lock() else {
        return false;
    };
    let now = std::time::Instant::now();
    guard.retain(|&(_, _, t)| now.duration_since(t).as_millis() < 500);
    if let Some(pos) = guard.iter().position(|&(m, u, _)| m == mods && u == usage) {
        guard.remove(pos);
        return true;
    }
    false
}

/// Hold the HID-bitmask modifiers, tap the key, release modifiers in reverse
/// — the same shape a human chord has, which is what shortcut-matching apps
/// expect to see.
fn synthesize_keystroke(mods: u8, usage: u16) {
    let Some(key) = hid_to_enigo(usage) else {
        eprintln!("actions: HID usage 0x{usage:02X} has no host mapping; keystroke skipped");
        return;
    };
    if let Ok(mut guard) = SYNTH_GUARD.lock() {
        guard.push((mods, usage, std::time::Instant::now()));
    }
    let Some(mut enigo) = new_enigo() else {
        return;
    };
    let held: Vec<Key> = HID_MOD_KEYS
        .iter()
        .filter(|(bit, _)| mods & bit != 0)
        .map(|&(_, k)| k)
        .collect();
    for &m in &held {
        if let Err(e) = enigo.key(m, Direction::Press) {
            eprintln!("actions: modifier press failed: {e}");
        }
    }
    if let Err(e) = enigo.key(key, Direction::Click) {
        eprintln!("actions: key tap failed: {e}");
    }
    // Release unconditionally, even after an error: a stuck modifier is far
    // worse than a missed tap.
    for &m in held.iter().rev() {
        let _ = enigo.key(m, Direction::Release);
    }
}

/// The live app-switcher session: each step is sent here while Cmd is held.
static APP_SWITCH: Mutex<Option<Sender<bool>>> = Mutex::new(None);

/// How long Cmd stays held after the last step, so the switcher stays open
/// between knob detents and commits once the knob rests.
const APP_SWITCH_HOLD: Duration = Duration::from_millis(800);

/// One step of the Cmd-Tab switcher. The first step opens a session thread
/// that holds Cmd; later steps join it, so the switcher walks through every
/// app instead of flipping between the last two.
fn app_switch(reverse: bool) {
    let Ok(mut session) = APP_SWITCH.lock() else {
        return;
    };
    if let Some(tx) = session.as_ref() {
        if tx.send(reverse).is_ok() {
            return;
        }
    }
    let (tx, rx) = mpsc::channel();
    let _ = tx.send(reverse);
    *session = Some(tx);
    drop(session);
    thread::spawn(move || {
        let Some(mut enigo) = new_enigo() else {
            if let Ok(mut session) = APP_SWITCH.lock() {
                *session = None;
            }
            return;
        };
        let tab = |enigo: &mut Enigo, reverse: bool| {
            if reverse {
                let _ = enigo.key(Key::Shift, Direction::Press);
            }
            if let Err(e) = enigo.key(Key::Tab, Direction::Click) {
                eprintln!("actions: app switcher step failed: {e}");
            }
            if reverse {
                let _ = enigo.key(Key::Shift, Direction::Release);
            }
        };
        if let Err(e) = enigo.key(Key::Meta, Direction::Press) {
            eprintln!("actions: app switcher Cmd press failed: {e}");
        }
        loop {
            match rx.recv_timeout(APP_SWITCH_HOLD) {
                Ok(reverse) => tab(&mut enigo, reverse),
                Err(RecvTimeoutError::Timeout) => {
                    // Close the session under the lock so no step is lost
                    // between the timeout and the release, then run any
                    // that arrived meanwhile.
                    if let Ok(mut session) = APP_SWITCH.lock() {
                        *session = None;
                    }
                    while let Ok(reverse) = rx.try_recv() {
                        tab(&mut enigo, reverse);
                    }
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = enigo.key(Key::Meta, Direction::Release);
    });
}

/// Pick the app highlighted in an open switcher now instead of waiting for
/// the knob to rest. Returns false when no switcher is open.
pub fn commit_app_switch() -> bool {
    let Ok(mut session) = APP_SWITCH.lock() else {
        return false;
    };
    // Dropping the sender ends the session thread, which releases Cmd.
    session.take().is_some()
}

/// Tap one media key.
fn media_op(op: MediaOp) {
    let Some(key) = media_key(op) else {
        eprintln!("actions: {op:?} is not supported on this platform; skipped");
        return;
    };
    let Some(mut enigo) = new_enigo() else {
        return;
    };
    if let Err(e) = enigo.key(key, Direction::Click) {
        eprintln!("actions: media key {op:?} failed: {e}");
    }
}

/// A fresh enigo connection per tap. Cheap enough, and it means a long macro
/// (Delay / Run steps) never holds platform input handles across its idle
/// time. Failure here usually means missing Accessibility permission.
fn new_enigo() -> Option<Enigo> {
    match Enigo::new(&Settings::default()) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("actions: input synthesis unavailable ({e}); is Accessibility granted?");
            None
        }
    }
}

/// The enigo key for a media op, if the platform has one. Display brightness
/// only has key events on macOS; elsewhere the op degrades to a logged no-op.
fn media_key(op: MediaOp) -> Option<Key> {
    match op {
        MediaOp::VolumeUp => Some(Key::VolumeUp),
        MediaOp::VolumeDown => Some(Key::VolumeDown),
        MediaOp::Mute => Some(Key::VolumeMute),
        MediaOp::PlayPause => Some(Key::MediaPlayPause),
        MediaOp::NextTrack => Some(Key::MediaNextTrack),
        MediaOp::PrevTrack => Some(Key::MediaPrevTrack),
        #[cfg(target_os = "macos")]
        MediaOp::BrightnessUp => Some(Key::BrightnessUp),
        #[cfg(target_os = "macos")]
        MediaOp::BrightnessDown => Some(Key::BrightnessDown),
        #[cfg(not(target_os = "macos"))]
        MediaOp::BrightnessUp | MediaOp::BrightnessDown => None,
    }
}

/// HID keyboard usage -> enigo key. Letters and digits go through
/// `Key::Unicode` so the *character* survives non-US layouts; named keys use
/// their enigo variants. Unmapped usages return None and the keystroke is
/// skipped (never guessed).
fn hid_to_enigo(usage: u16) -> Option<Key> {
    // On macOS, enigo resolves `Key::Unicode` through the keyboard layout
    // (TSM), which current macOS only allows on the main queue; from the
    // action worker thread it aborts the app. Raw virtual keycodes skip that
    // lookup, and HID usages are positional anyway.
    #[cfg(target_os = "macos")]
    if let Some(code) = mac_virtual_keycode(usage) {
        return Some(Key::Other(code));
    }
    Some(match usage {
        // a..z (0x04..=0x1D), 1..9 (0x1E..=0x26), 0 (0x27)
        0x04..=0x1D => Key::Unicode((b'a' + (usage - 0x04) as u8) as char),
        0x1E..=0x26 => Key::Unicode((b'1' + (usage - 0x1E) as u8) as char),
        0x27 => Key::Unicode('0'),
        0x28 => Key::Return,
        0x29 => Key::Escape,
        0x2A => Key::Backspace,
        0x2B => Key::Tab,
        0x2C => Key::Space,
        // US-layout punctuation, also via Unicode so the shown character is
        // the typed character.
        0x2D => Key::Unicode('-'),
        0x2E => Key::Unicode('='),
        0x2F => Key::Unicode('['),
        0x30 => Key::Unicode(']'),
        0x31 => Key::Unicode('\\'),
        0x33 => Key::Unicode(';'),
        0x34 => Key::Unicode('\''),
        0x35 => Key::Unicode('`'),
        0x36 => Key::Unicode(','),
        0x37 => Key::Unicode('.'),
        0x38 => Key::Unicode('/'),
        // F1..F12 (0x3A..=0x45)
        0x3A => Key::F1,
        0x3B => Key::F2,
        0x3C => Key::F3,
        0x3D => Key::F4,
        0x3E => Key::F5,
        0x3F => Key::F6,
        0x40 => Key::F7,
        0x41 => Key::F8,
        0x42 => Key::F9,
        0x43 => Key::F10,
        0x44 => Key::F11,
        0x45 => Key::F12,
        // Navigation cluster
        0x4A => Key::Home,
        0x4B => Key::PageUp,
        0x4C => Key::Delete, // forward delete
        0x4D => Key::End,
        0x4E => Key::PageDown,
        0x4F => Key::RightArrow,
        0x50 => Key::LeftArrow,
        0x51 => Key::DownArrow,
        0x52 => Key::UpArrow,
        // F13..F20 (0x68..=0x6F); F21+ is not portable across enigo backends
        0x68 => Key::F13,
        0x69 => Key::F14,
        0x6A => Key::F15,
        0x6B => Key::F16,
        0x6C => Key::F17,
        0x6D => Key::F18,
        0x6E => Key::F19,
        0x6F => Key::F20,
        _ => return None,
    })
}

/// macOS ANSI virtual keycodes (`kVK_ANSI_*`) for the HID usages that enigo
/// would otherwise resolve through the keyboard layout.
#[cfg(target_os = "macos")]
fn mac_virtual_keycode(usage: u16) -> Option<u32> {
    // a..z in HID order
    const LETTERS: [u32; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D,
        0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    // 1..9, 0 in HID order
    const DIGITS: [u32; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D];
    Some(match usage {
        0x04..=0x1D => LETTERS[(usage - 0x04) as usize],
        0x1E..=0x27 => DIGITS[(usage - 0x1E) as usize],
        0x2D => 0x1B, // -
        0x2E => 0x18, // =
        0x2F => 0x21, // [
        0x30 => 0x1E, // ]
        0x31 => 0x2A, // \
        0x33 => 0x29, // ;
        0x34 => 0x27, // '
        0x35 => 0x32, // `
        0x36 => 0x2B, // ,
        0x37 => 0x2F, // .
        0x38 => 0x2C, // /
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Permissions (macOS Accessibility)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// Returns a C `Boolean` (u8): nonzero when this process may synthesize
    /// input events.
    fn AXIsProcessTrusted() -> u8;
}

/// Whether the OS will let us synthesize keystrokes. Always true off macOS —
/// other platforms don't gate synthetic input behind a permission.
pub fn accessibility_trusted() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Safety: no arguments, no state; plain query into ApplicationServices.
        unsafe { AXIsProcessTrusted() != 0 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Jump the user straight to Privacy & Security > Accessibility so granting
/// the permission is one click, not a settings scavenger hunt. No-op off
/// macOS.
pub fn open_permission_settings() {
    #[cfg(target_os = "macos")]
    if let Err(e) =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
    {
        eprintln!("actions: could not open Accessibility settings: {e}");
    }
}

/// True when the action (or any step of a macro) synthesizes input and is
/// therefore gated by Accessibility. Run/Open/AppSettings never are — they go
/// through the OS launcher or stay inside the app.
pub fn needs_permission(action: &Action) -> bool {
    match action {
        Action::Keystroke { .. } | Action::Media { .. } | Action::AppSwitch { .. } => true,
        Action::Macro { steps } => steps.iter().any(|e| {
            e.enabled
                && matches!(
                    e.step,
                    MacroStep::Keystroke { .. } | MacroStep::Media { .. }
                )
        }),
        Action::None
        | Action::Run { .. }
        | Action::Open { .. }
        | Action::AppSettings
        | Action::Disabled => false,
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

/// Short human summary of an action for list rows and key caps:
/// "⇧⌘P", "Macro · 4 steps", "Run · deploy.sh", "Open · linear.app",
/// "Vol +", "App settings", "—".
pub fn describe(action: &Action) -> String {
    match action {
        Action::None => "—".to_string(),
        Action::Keystroke { mods, key } => {
            format!(
                "{}{}",
                mods_label(*mods),
                keyboard_name(*key).unwrap_or("—")
            )
        }
        Action::Macro { steps } if steps.len() == 1 => "Macro · 1 step".to_string(),
        Action::Macro { steps } => format!("Macro · {} steps", steps.len()),
        Action::Run { command } => format!("Run · {}", short_command(command)),
        Action::Open { target } => format!("Open · {}", short_target(target)),
        Action::Media { op } => media_label(*op).to_string(),
        Action::AppSettings => "App settings".to_string(),
        Action::AppSwitch { reverse: false } => "Next app".to_string(),
        Action::AppSwitch { reverse: true } => "Previous app".to_string(),
        Action::Disabled => "Do nothing".to_string(),
    }
}

fn media_label(op: MediaOp) -> &'static str {
    match op {
        MediaOp::VolumeUp => "Vol +",
        MediaOp::VolumeDown => "Vol −",
        MediaOp::Mute => "Mute",
        MediaOp::PlayPause => "Play / Pause",
        MediaOp::NextTrack => "Next track",
        MediaOp::PrevTrack => "Prev track",
        MediaOp::BrightnessUp => "Bright +",
        MediaOp::BrightnessDown => "Bright −",
    }
}

/// First token of a command line, path stripped:
/// "~/bin/deploy.sh --prod" -> "deploy.sh".
fn short_command(command: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
    if base.is_empty() {
        "—".to_string()
    } else {
        base.to_string()
    }
}

/// Compact form of an open target: URLs shrink to their host, paths to their
/// last component.
fn short_target(target: &str) -> String {
    let t = target.trim();
    if t.is_empty() {
        return "—".to_string();
    }
    if let Some((_, rest)) = t.split_once("://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        if host.is_empty() {
            t.to_string()
        } else {
            host.to_string()
        }
    } else {
        t.rsplit(['/', '\\'])
            .find(|s| !s.is_empty())
            .unwrap_or(t)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_map_covers_the_basics() {
        if cfg!(target_os = "macos") {
            assert_eq!(hid_to_enigo(0x04), Some(Key::Other(0x00))); // kVK_ANSI_A
            assert_eq!(hid_to_enigo(0x27), Some(Key::Other(0x1D))); // kVK_ANSI_0
            assert_eq!(hid_to_enigo(0x38), Some(Key::Other(0x2C))); // kVK_ANSI_Slash
        } else {
            assert_eq!(hid_to_enigo(0x04), Some(Key::Unicode('a')));
            assert_eq!(hid_to_enigo(0x27), Some(Key::Unicode('0')));
        }
        assert_eq!(hid_to_enigo(0x28), Some(Key::Return));
        assert_eq!(hid_to_enigo(0x3A), Some(Key::F1));
        assert_eq!(hid_to_enigo(0x6F), Some(Key::F20));
        assert_eq!(hid_to_enigo(0x0000), None);
        assert_eq!(hid_to_enigo(0x00E0), None); // modifiers travel in `mods`, not as usages
    }

    #[test]
    fn permission_gating_follows_synthesis() {
        assert!(needs_permission(&Action::Keystroke { mods: 0, key: 0x04 }));
        assert!(needs_permission(&Action::Media { op: MediaOp::Mute }));
        assert!(!needs_permission(&Action::Run {
            command: "ls".into()
        }));
        assert!(!needs_permission(&Action::AppSettings));
        assert!(needs_permission(&Action::Macro {
            steps: vec![
                MacroStep::Delay { ms: 5 }.into(),
                MacroStep::Media {
                    op: MediaOp::PlayPause
                }
                .into(),
            ],
        }));
        assert!(!needs_permission(&Action::Macro {
            steps: vec![MacroStep::Open {
                target: "https://example.com".into()
            }
            .into()],
        }));
        // A disabled synthesis step must not demand the permission.
        assert!(!needs_permission(&Action::Macro {
            steps: vec![crate::config::MacroStepEntry {
                enabled: false,
                step: MacroStep::Media {
                    op: MediaOp::PlayPause
                },
            }],
        }));
    }

    #[test]
    fn describe_is_compact() {
        assert_eq!(describe(&Action::None), "—");
        assert_eq!(describe(&Action::AppSettings), "App settings");
        assert_eq!(
            describe(&Action::Run {
                command: "~/bin/deploy.sh --prod".into()
            }),
            "Run · deploy.sh"
        );
        assert_eq!(
            describe(&Action::Open {
                target: "https://linear.app/team/board".into()
            }),
            "Open · linear.app"
        );
        assert_eq!(
            describe(&Action::Macro {
                steps: vec![MacroStep::Delay { ms: 1 }.into()]
            }),
            "Macro · 1 step"
        );
    }
}
