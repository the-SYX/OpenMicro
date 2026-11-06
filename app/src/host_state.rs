//! Framework-neutral runtime state for the native host application.
//!
//! The GPUI layer owns one `HostState`, renders its public model fields, and
//! feeds it `AppEvent`s from the global event receiver. Background services
//! remain responsible for HID, release downloads, global hotkeys, and the
//! native menu bar; this module is the single main-thread reducer for their
//! results.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use openmicro_layout::BootInfoReply;

use crate::actions;
use crate::behaviors;
use crate::config::{
    self, Action, AppConfig, InputConfig, LedPattern, MediaOp, Profile, ProfileLighting, ProfileMode, Slot, SlotKind,
    KEY_SLOTS, SLOT_COUNT,
};
use crate::device::{BootloaderError, DeviceCmd, DeviceMode, DeviceMsg, PadEvent, UpdateMsg};
use crate::events::AppEvent;
use crate::intercept::Intercept;
use crate::menubar::{Menubar, MenubarMsg};
use crate::release::{DownloadKind, ReleaseCatalog, ReleaseMsg};
use crate::status::ActivityStatus;
use crate::status_ipc::ActivityEvent;

#[cfg(not(test))]
use crate::{device, intercept, release};

/// Physical cells rendered by the device map: 13 keys and three controls.
pub const CELL_COUNT: usize = 16;
pub const CELL_ENCODER: usize = 13;
pub const CELL_JOYSTICK: usize = 14;
pub const CELL_TOUCH: usize = 15;

/// Encoder turns and touch taps have no corresponding release report.
pub const MOMENTARY_RELEASE_DELAY: Duration = Duration::from_millis(250);

const LOG_LIMIT: usize = 8;
const TERMINAL_LED_TTL: Duration = Duration::from_secs(4);
const ACTIVE_LED_TTL: Duration = Duration::from_secs(30 * 60);
// (key index, agent) pairs. Claude Code lights both top-row keys; the other
// agents keep their second-row keys.
const AGENT_LED_KEYS: [(usize, usize); 5] = [(0, 0), (1, 0), (3, 1), (4, 2), (5, 3)];
/// Working breathes over eight 300 ms frames (percent of full brightness).
const WORKING_PULSE: [u16; 8] = [100, 80, 55, 30, 15, 30, 55, 80];
const AGENT_NAMESPACES: [&str; 4] = ["claude-code:", "codex:", "grok:", "octoscode:"];

fn pattern_rgb(pattern: LedPattern) -> (u8, u8, u8) {
    match pattern {
        LedPattern::Rainbow => (0, 96, 255),
        LedPattern::White => (255, 255, 255),
        LedPattern::Solid { r, g, b } => (r, g, b),
    }
}

/// Consumer usages the host can reproduce as a media key.
fn consumer_media_op(usage: u16) -> Option<MediaOp> {
    Some(match usage {
        0xE9 => MediaOp::VolumeUp,
        0xEA => MediaOp::VolumeDown,
        0xE2 => MediaOp::Mute,
        0xCD => MediaOp::PlayPause,
        0xB5 => MediaOp::NextTrack,
        0xB6 => MediaOp::PrevTrack,
        0x6F => MediaOp::BrightnessUp,
        0x70 => MediaOp::BrightnessDown,
        _ => return None,
    })
}

fn supports_agent_leds(version: &str) -> bool {
    if crate::stock::is_stock_version(version) {
        return true;
    }
    let mut parts = version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(major), Some(minor), Some(_)) => major > 0 || minor >= 7,
        _ => false,
    }
}

#[derive(Clone, Debug)]
struct SessionActivity {
    status: ActivityStatus,
    turn_id: Option<String>,
    epoch: u64,
}

/// Side effects which must be performed by the owning UI/event loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEffect {
    ShowWindow,
    OpenSettings,
    Quit,
    /// Clear a momentary device-map flash after `delay`.
    ReleaseCellAfter {
        cell: usize,
        delay: Duration,
    },
    /// Clear an activity colour after either its short completion window or
    /// the failsafe timeout that prevents abandoned agent sessions sticking.
    ExpireActivityAfter {
        session_id: String,
        epoch: u64,
        delay: Duration,
    },
}

/// A pad parked in its bootloader's update mode (1209:0002), as reported by
/// the device worker while no application-mode pad is connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootloaderPad {
    pub serial: String,
    /// The bootloader's BOOT_INFO, or why there is none: the interface
    /// could not be opened (typically a missing udev rule on Linux) or it
    /// did not answer BOOT_INFO usefully (timeout, another protocol).
    pub info: Result<BootInfoReply, BootloaderError>,
}

/// Application model and ownership boundary for all host-side services.
pub struct HostState {
    pub config: AppConfig,
    pub selected_slot: Option<usize>,

    pub device_tx: Option<Sender<DeviceCmd>>,
    pub connected: bool,
    /// Last connected `(firmware version, serial)` pair.
    pub last_conn: Option<(String, String)>,
    /// The pad's battery percentage and charging state, when it reports one.
    pub battery: Option<(u8, bool)>,
    /// The connected pad's boot identity; None while disconnected or on
    /// firmware that predates device modes.
    pub device_mode: Option<DeviceMode>,
    /// The connected pad's BOOT_INFO: Some when it runs on the resident
    /// bootloader (firmware 0.10.0+), so updates can take the driverless
    /// HID path and the firmware sheet offers "Reboot into bootloader".
    pub boot_info: Option<BootInfoReply>,
    /// A pad currently sitting in bootloader mode (no application running).
    pub bootloader: Option<BootloaderPad>,
    /// A SET_MODE is in flight: the pad takes a second or two to save,
    /// reset and reappear, during which the toggle must not fire again.
    pub mode_switch_pending: bool,
    pub pressed_cells: [bool; CELL_COUNT],

    /// Runtime activity reported by local coding-agent integrations.
    /// It is deliberately separate from the persisted LED configuration.
    activities: HashMap<String, SessionActivity>,
    activity_epoch: u64,
    activity_led_frame: usize,
    /// Last per-key LED override sent to the pad. A fresh connection has
    /// none (overrides are runtime-only), so nothing is sent until one is set.
    key_led_sent: [Option<(Option<(u8, u8, u8)>, bool)>; KEY_SLOTS],
    /// The last frontmost app reported, so profiles switch only when the
    /// front app changes and a manual switch sticks until then.
    last_front_app: Option<String>,
    /// The hold layer being edited, by trigger slot. While set, the active
    /// profile's `inputs` and that layer's `inputs` are swapped so every
    /// editor works on the layer unchanged; `stored_config` undoes the swap
    /// for saving and the pad.
    pub editing_layer: Option<usize>,
    /// The hold-layer key currently held on the pad, and whether another
    /// control was used while it was down (a bare tap runs its own action).
    held_layer: Option<(usize, bool)>,
    /// A held-modifier key that is down, and the modifiers it holds, so the
    /// release lifts them even if the profile changed in between.
    held_mods: Option<(usize, u8)>,

    pub intercept: Option<Intercept>,
    pub menubar: Option<Menubar>,

    pub release: Option<ReleaseCatalog>,
    pub updating: bool,
    pub update_phase: Option<String>,
    pub update_progress: f64,
    pub update_error: Option<String>,
    pub firmware_image: Option<PathBuf>,
    pub firmware_expected_version: Option<String>,

    pub app_downloading: bool,
    pub app_download_progress: f64,
    pub app_download: Option<PathBuf>,
    pub app_update_error: Option<String>,
    pub firmware_downloading: bool,
    pub firmware_download_progress: f64,
    pub install_after_download: bool,
    pub release_error: Option<String>,
    pub logs: VecDeque<String>,
    pub app_banner_dismissed: bool,
    pub firmware_banner_dismissed: bool,

    persist_to_disk: bool,
}

impl HostState {
    /// Build the production runtime and start its long-lived services.
    ///
    /// This must be called on the UI thread: both global-hotkey and tray-icon
    /// create platform objects with main-thread requirements on macOS.
    pub fn new() -> Self {
        #[cfg(test)]
        {
            // Unit tests must be hermetic: no USB scan, network request,
            // global shortcut registration, tray, or config-file write.
            Self::detached(AppConfig::default())
        }

        #[cfg(not(test))]
        {
            let mut state = Self::detached(config::load());
            state.persist_to_disk = true;
            // A profile written by an older build can sit on a trigger this
            // build no longer allows -- the macOS brightness keys F14/F15,
            // say, which fire their action *and* move the screen brightness.
            // Re-home those before the hotkey layer or the pad sees the
            // profile, and write the corrected config straight back.
            if state.rehome_stale_triggers() {
                let _ = config::save(&state.config);
            }

            crate::status_ipc::spawn_listener();

            let mut interception = Intercept::new();
            interception.apply(state.active_profile());
            state.intercept = Some(interception);
            intercept::spawn_listener();

            let mut menubar = Menubar::new();
            menubar.set_visible(state.config.show_menubar);
            state.menubar = Some(menubar);

            state.device_tx = Some(device::spawn_worker());
            state.apply_active_profile();
            release::spawn_catalog_check();
            state
        }
    }

    /// Construct a model without starting services or writing configuration.
    ///
    /// Useful for previews and reducer tests. Callers may inject a device
    /// command sender if they want to observe commands without real hardware.
    pub fn detached(mut config: AppConfig) -> Self {
        if config.profiles.is_empty() {
            config.profiles.push(config::default_codex_profile());
        }
        config.active_profile = config.active_profile.min(config.profiles.len() - 1);

        Self {
            config,
            selected_slot: Some(0),
            device_tx: None,
            connected: false,
            last_conn: None,
            battery: None,
            device_mode: None,
            boot_info: None,
            bootloader: None,
            mode_switch_pending: false,
            pressed_cells: [false; CELL_COUNT],
            activities: HashMap::new(),
            activity_epoch: 0,
            activity_led_frame: 0,
            key_led_sent: [Some((None, false)); KEY_SLOTS],
            last_front_app: None,
            editing_layer: None,
            held_layer: None,
            held_mods: None,
            intercept: None,
            menubar: None,
            release: None,
            updating: false,
            update_phase: None,
            update_progress: 0.0,
            update_error: None,
            firmware_image: None,
            firmware_expected_version: None,
            app_downloading: false,
            app_download_progress: 0.0,
            app_download: None,
            app_update_error: None,
            firmware_downloading: false,
            firmware_download_progress: 0.0,
            install_after_download: false,
            release_error: None,
            logs: VecDeque::new(),
            app_banner_dismissed: false,
            firmware_banner_dismissed: false,
            persist_to_disk: false,
        }
    }

    pub fn active_profile(&self) -> &Profile {
        &self.config.profiles[self.config.active_profile]
    }

    pub fn active_profile_mut(&mut self) -> &mut Profile {
        &mut self.config.profiles[self.config.active_profile]
    }

    fn current_profile_mode(&self) -> ProfileMode {
        match self.device_mode {
            Some(DeviceMode::Codex) => ProfileMode::CodexMicro,
            _ => ProfileMode::OpenMicro,
        }
    }

    pub fn visible_profile_indices(&self) -> Vec<usize> {
        let mode = self.current_profile_mode();
        self.config
            .profiles
            .iter()
            .enumerate()
            .filter_map(|(index, profile)| (profile.mode == mode).then_some(index))
            .collect()
    }

    fn select_profile_for_current_mode(&mut self) {
        self.exit_layer_edit();
        let mode = self.current_profile_mode();
        let original = self.config.active_profile;
        let original_len = self.config.profiles.len();
        if let Some(profile) = self.config.profiles.get(original) {
            match profile.mode {
                ProfileMode::OpenMicro => self.config.active_openmicro_profile = Some(original),
                ProfileMode::CodexMicro => self.config.active_codex_profile = Some(original),
            }
        }
        let remembered = match mode {
            ProfileMode::OpenMicro => self.config.active_openmicro_profile,
            ProfileMode::CodexMicro => self.config.active_codex_profile,
        };
        if let Some(index) = remembered.filter(|index| {
            self.config
                .profiles
                .get(*index)
                .is_some_and(|profile| profile.mode == mode)
        }) {
            self.config.active_profile = index;
        } else if let Some(index) = self
            .config
            .profiles
            .iter()
            .position(|profile| profile.mode == mode)
        {
            self.config.active_profile = index;
        } else if mode == ProfileMode::CodexMicro {
            self.config
                .profiles
                .push(config::default_codex_micro_profile());
            self.config.active_profile = self.config.profiles.len() - 1;
        }
        match mode {
            ProfileMode::OpenMicro => {
                self.config.active_openmicro_profile = Some(self.config.active_profile)
            }
            ProfileMode::CodexMicro => {
                self.config.active_codex_profile = Some(self.config.active_profile)
            }
        }
        if self.persist_to_disk
            && (self.config.active_profile != original
                || self.config.profiles.len() != original_len)
        {
            let _ = config::save(&self.config);
        }
    }

    /// Re-run the hidden-trigger allocator over every profile. Returns true
    /// if any binding moved, so startup knows whether the config needs
    /// rewriting; the allocator is idempotent, so a healthy config is a
    /// no-op and leaves the file alone.
    pub fn rehome_stale_triggers(&mut self) -> bool {
        let mut moved = false;
        for profile in &mut self.config.profiles {
            let before: Vec<_> = profile.inputs.iter().map(|input| input.emitted).collect();
            crate::behaviors::normalize_hidden_triggers(profile);
            moved |= profile
                .inputs
                .iter()
                .zip(&before)
                .any(|(input, was)| input.emitted != *was);
        }
        moved
    }

    /// Save configuration and re-derive platform integrations from the
    /// active profile. Detached states intentionally skip the disk write.
    pub fn persist(&mut self) -> Result<(), String> {
        let result = if self.persist_to_disk {
            config::save(&self.stored_config())
        } else {
            Ok(())
        };
        if let Err(error) = &result {
            self.push_log(format!("config save failed: {error}"));
        }
        self.apply_active_profile();
        result
    }

    /// Re-register active-profile hotkeys and refresh the native menu.
    pub fn apply_active_profile(&mut self) {
        let profile = self.active_device_profile();
        if let Some(interception) = &mut self.intercept {
            interception.apply(&profile);
        }
        self.refresh_menubar();
    }

    /// Write the active profile and device-wide tuning to pad RAM + flash.
    /// Returns false if the service is unavailable or its command queue shut
    /// down before accepting the request.
    pub fn sync_device(&mut self) -> bool {
        let Some(tx) = &self.device_tx else {
            return false;
        };
        let profile = self.active_device_profile();
        tx.send(DeviceCmd::SyncKeymap {
            slots: profile.slots(),
            joy_threshold: profile.analog.joy_threshold,
            joy_mode: profile.analog.joy_mode,
            joy_mouse_speed: profile.analog.joy_mouse_speed,
            led_brightness: self.config.led_brightness,
            led_key_pattern: self.key_pattern(),
            led_ambient_pattern: self.ambient_pattern(),
        })
        .is_ok()
            && tx
                .send(DeviceCmd::SetStandby {
                    minutes: self.config.standby_minutes,
                })
                .is_ok()
            && self.refresh_activity_led()
    }

    fn active_device_profile(&self) -> Profile {
        let profile = self.effective_profile();
        if self.device_mode == Some(DeviceMode::Codex) {
            behaviors::with_codex_overrides(&profile)
        } else {
            profile
        }
    }

    /// The default profile when the active one is some other profile, i.e.
    /// the profile its empty inputs and unset colours fall back to.
    fn fallback_profile(&self) -> Option<&Profile> {
        let index = self.default_profile_index()?;
        (index != self.config.active_profile).then(|| &self.config.profiles[index])
    }

    /// Whether the active control editor has something to fall back to:
    /// the default profile, or the base bindings while editing a layer.
    pub fn has_fallback(&self) -> bool {
        self.editing_layer.is_some() || self.fallback_profile().is_some()
    }

    /// The config as saved: with any hold-layer edit swap undone.
    pub fn stored_config(&self) -> std::borrow::Cow<'_, AppConfig> {
        if self.editing_layer.is_none() {
            return std::borrow::Cow::Borrowed(&self.config);
        }
        let mut config = self.config.clone();
        let active = config.active_profile;
        unswap_layer(&mut config.profiles[active], self.editing_layer);
        std::borrow::Cow::Owned(config)
    }

    /// The active profile as stored (base bindings, not a layer being edited).
    fn stored_profile(&self) -> Profile {
        let mut profile = self.active_profile().clone();
        unswap_layer(&mut profile, self.editing_layer);
        profile
    }

    pub fn has_layer(&self, trigger: usize) -> bool {
        self.active_profile().layers.iter().any(|layer| layer.trigger == trigger)
    }

    /// Turn `trigger` into a hold-layer key, or back into a plain key.
    pub fn toggle_layer(&mut self, trigger: usize) {
        if trigger >= KEY_SLOTS {
            return;
        }
        self.exit_layer_edit();
        let profile = self.active_profile_mut();
        if let Some(index) = profile.layers.iter().position(|layer| layer.trigger == trigger) {
            profile.layers.remove(index);
        } else {
            profile.layers.push(config::HoldLayer::new(trigger));
        }
        let _ = self.persist();
        self.sync_device();
    }

    /// Point the editors at `trigger`'s layer.
    pub fn enter_layer_edit(&mut self, trigger: usize) {
        if self.editing_layer == Some(trigger) || !self.has_layer(trigger) {
            return;
        }
        self.exit_layer_edit();
        let profile = self.active_profile_mut();
        if let Some(layer) = profile.layers.iter_mut().find(|layer| layer.trigger == trigger) {
            std::mem::swap(&mut profile.inputs, &mut layer.inputs);
            self.editing_layer = Some(trigger);
        }
    }

    pub fn exit_layer_edit(&mut self) {
        let editing = self.editing_layer.take();
        if editing.is_some() {
            let active = self.config.active_profile;
            unswap_layer(&mut self.config.profiles[active], editing);
        }
    }

    /// The binding an empty control falls back to in the current editor.
    fn fallback_input(&self, slot: usize) -> Option<InputConfig> {
        if self.editing_layer.is_some() {
            return self.stored_profile().inputs.get(slot).cloned();
        }
        self.fallback_profile()?.inputs.get(slot).cloned()
    }

    /// Where a control's binding comes from in the active profile.
    pub fn input_source(&self, slots: &[usize]) -> InputSource {
        let inputs = &self.active_profile().inputs;
        if slots.iter().all(|&slot| inputs[slot].action == Action::Disabled) {
            InputSource::Disabled
        } else if slots.iter().all(|&slot| inputs[slot].is_unbound()) {
            InputSource::Default
        } else {
            InputSource::Own
        }
    }

    /// Cycle a control between following the default profile, its own
    /// binding (starting from a copy of the default's) and doing nothing.
    pub fn cycle_input_source(&mut self, slots: &[usize]) {
        let next = match self.input_source(slots) {
            InputSource::Default => InputSource::Own,
            InputSource::Own => InputSource::Disabled,
            InputSource::Disabled => InputSource::Default,
        };
        let inherited: Vec<InputConfig> = slots
            .iter()
            .map(|&slot| self.fallback_input(slot).unwrap_or_else(config::unbound_input))
            .collect();
        let inputs = &mut self.active_profile_mut().inputs;
        for (&slot, inherited) in slots.iter().zip(inherited) {
            inputs[slot] = match next {
                InputSource::Default => config::unbound_input(),
                InputSource::Own => inherited,
                InputSource::Disabled => InputConfig {
                    action: Action::Disabled,
                    ..config::unbound_input()
                },
            };
        }
    }

    /// What `slot` inherits from the default profile, when the active
    /// profile leaves it empty.
    pub fn inherited_input(&self, slot: usize) -> Option<InputConfig> {
        let own = self.active_profile().inputs.get(slot)?;
        if !own.is_unbound() {
            return None;
        }
        self.fallback_input(slot).filter(|inherited| !inherited.is_unbound())
    }

    /// The active profile with empty inputs filled from the default profile
    /// and "Do nothing" inputs cleared, as the pad and host should run it.
    pub fn effective_profile(&self) -> Profile {
        let mut profile = self.stored_profile();
        if let Some(fallback) = self.fallback_profile() {
            for (input, inherited) in profile.inputs.iter_mut().zip(&fallback.inputs) {
                if input.is_unbound() {
                    *input = inherited.clone();
                }
            }
            // Hold layers the profile doesn't define come from the default;
            // empty controls in a layer it does define inherit per control.
            for layer in &fallback.layers {
                match profile.layers.iter_mut().find(|own| own.trigger == layer.trigger) {
                    Some(own) => {
                        for (input, inherited) in own.inputs.iter_mut().zip(&layer.inputs) {
                            if input.is_unbound() {
                                *input = inherited.clone();
                            }
                        }
                    }
                    None => profile.layers.push(layer.clone()),
                }
            }
        }
        for input in &mut profile.inputs {
            if input.action == Action::Disabled {
                input.emitted = Slot::default();
                input.action = Action::None;
            }
        }
        profile.lighting = self.effective_lighting();
        profile
    }

    /// The active profile's lighting, with unset key colours (or all of it)
    /// taken from the default profile.
    fn effective_lighting(&self) -> Option<ProfileLighting> {
        let own = self.active_profile().lighting.clone();
        let Some(inherited) = self.fallback_profile().and_then(|p| p.lighting.clone()) else {
            return own;
        };
        let Some(mut own) = own else {
            return Some(inherited);
        };
        for index in 0..KEY_SLOTS {
            if own.key_color(index).is_none() {
                if let Some(color) = inherited.key_color(index) {
                    own.set_key_color(index, Some(color));
                }
            }
        }
        Some(own)
    }

    pub fn select_slot(&mut self, slot: usize) -> bool {
        if slot >= SLOT_COUNT {
            return false;
        }
        self.selected_slot = Some(slot);
        true
    }

    /// Append a profile built from `template`, named so it does not collide
    /// with the existing ones, and return its index. Nothing is persisted or
    /// activated here; `switch_profile` does both.
    pub fn add_profile(&mut self, template: config::ProfileTemplate) -> usize {
        self.exit_layer_edit();
        let mut profile = template.profile(&self.config.profiles);
        profile.mode = self.current_profile_mode();
        profile.is_default = false;
        profile.apps.clear();
        profile.layers.clear();
        self.config.profiles.push(profile);
        self.config.profiles.len() - 1
    }

    /// The profile used when the frontmost app has no profile of its own.
    pub fn default_profile_index(&self) -> Option<usize> {
        let visible = self.visible_profile_indices();
        visible
            .iter()
            .copied()
            .find(|&index| self.config.profiles[index].is_default)
            .or_else(|| visible.first().copied())
    }

    /// Make `index` the default for its mode.
    pub fn set_default_profile(&mut self, index: usize) {
        let Some(mode) = self.config.profiles.get(index).map(|p| p.mode) else {
            return;
        };
        for (i, profile) in self.config.profiles.iter_mut().enumerate() {
            if profile.mode == mode {
                profile.is_default = i == index;
            }
        }
        let _ = self.persist();
    }

    /// Link `path` to the active profile, unlinking it from any other.
    pub fn link_app(&mut self, path: &str) {
        let active = self.config.active_profile;
        for (i, profile) in self.config.profiles.iter_mut().enumerate() {
            profile.apps.retain(|app| app != path);
            if i == active {
                profile.apps.push(path.to_string());
            }
        }
        let _ = self.persist();
    }

    pub fn unlink_app(&mut self, path: &str) {
        self.active_profile_mut().apps.retain(|app| app != path);
        let _ = self.persist();
    }

    /// Follow the frontmost app: its linked profile, else the default.
    /// Returns true when the active profile changed.
    pub fn on_frontmost_app(&mut self, path: Option<&str>) -> bool {
        let Some(path) = path else {
            return false;
        };
        if !self.config.auto_switch || is_own_app(path) {
            return false;
        }
        if self.last_front_app.as_deref() == Some(path) {
            return false;
        }
        self.last_front_app = Some(path.to_string());
        let target = self
            .visible_profile_indices()
            .into_iter()
            .find(|&index| self.config.profiles[index].apps.iter().any(|app| app == path))
            .or_else(|| self.default_profile_index());
        let before = self.config.active_profile;
        match target {
            Some(index) => self.switch_profile(index) && self.config.active_profile != before,
            None => false,
        }
    }

    /// Activate a profile, persist it, update host interception/menu state,
    /// and make the pad follow the switch.
    pub fn switch_profile(&mut self, index: usize) -> bool {
        if self.config.active_profile != index {
            self.exit_layer_edit();
        }
        if index >= self.config.profiles.len() {
            return false;
        }
        if self.config.profiles[index].mode != self.current_profile_mode() {
            return false;
        }
        if self.config.active_profile == index {
            return true;
        }
        self.config.active_profile = index;
        match self.current_profile_mode() {
            ProfileMode::OpenMicro => self.config.active_openmicro_profile = Some(index),
            ProfileMode::CodexMicro => self.config.active_codex_profile = Some(index),
        }
        let _ = self.persist();
        self.sync_device();
        true
    }

    pub fn release_cell(&mut self, cell: usize) {
        if let Some(pressed) = self.pressed_cells.get_mut(cell) {
            *pressed = false;
        }
    }

    /// Begin a firmware update through the device worker. The same worker
    /// accepts this while the normal HID device is offline so DFU recovery
    /// can resume.
    pub fn start_firmware_update(
        &mut self,
        image: PathBuf,
        expected_version: Option<String>,
    ) -> bool {
        self.firmware_image = Some(image.clone());
        self.firmware_expected_version = expected_version.clone();
        self.install_after_download = false;
        self.update_progress = 0.0;
        self.update_error = None;
        self.update_phase = Some("Starting…".into());

        let Some(tx) = &self.device_tx else {
            self.update_failed("device service is unavailable".into());
            return false;
        };
        if tx
            .send(DeviceCmd::StartUpdate {
                image,
                expected_version,
            })
            .is_err()
        {
            self.update_failed("device service stopped before the update began".into());
            return false;
        }
        self.updating = true;
        true
    }

    /// True when the connected pad answered BOOT_INFO, i.e. it runs on the
    /// resident bootloader and can be rebooted into update mode.
    pub fn pad_has_bootloader(&self) -> bool {
        self.connected && self.boot_info.is_some()
    }

    /// Ask the pad in bootloader mode to validate and start its firmware.
    pub fn boot_firmware(&mut self) -> bool {
        let Some(tx) = &self.device_tx else {
            return false;
        };
        let sent = tx.send(DeviceCmd::BootRun).is_ok();
        if sent {
            self.push_log("asking the bootloader to start the firmware");
        }
        sent
    }

    /// Reduce one service event and return effects for the UI/event loop.
    pub fn handle_event(&mut self, event: AppEvent) -> Vec<HostEffect> {
        let mut effects = Vec::new();
        match event {
            AppEvent::Device(message) => self.handle_device(message, &mut effects),
            AppEvent::Update(message) => self.handle_update(message),
            AppEvent::Release(message) => self.handle_release(message),
            AppEvent::Hotkey(message) => self.handle_hotkey(message.hotkey_id, &mut effects),
            AppEvent::Menubar(message) => self.handle_menubar(message, &mut effects),
            AppEvent::OpenSettings => push_open_settings(&mut effects),
            AppEvent::Activity(message) => self.handle_activity(message, &mut effects),
        }
        effects
    }

    /// Ask the pad to boot as a Codex Micro, or back as itself. The pad
    /// persists the choice, acks, and resets; it returns as a fresh
    /// connection under the other USB identity (`find_raw` knows both).
    pub fn set_device_mode(&mut self, mode: DeviceMode) -> bool {
        if self.device_tx.is_none() || self.mode_switch_pending {
            return false;
        }
        self.push_log(format!("switching the pad to {mode} mode"));
        let sent = self
            .device_tx
            .as_ref()
            .is_some_and(|tx| tx.send(DeviceCmd::SetDeviceMode { mode }).is_ok());
        // Cleared by the Connected that follows the pad's reset (or by
        // Disconnected, after which the toggle is disabled anyway).
        self.mode_switch_pending = sent;
        sent
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        self.logs.push_back(line.into());
        while self.logs.len() > LOG_LIMIT {
            self.logs.pop_front();
        }
    }

    fn refresh_menubar(&mut self) {
        let indices = self.visible_profile_indices();
        let names: Vec<String> = indices
            .iter()
            .map(|index| self.config.profiles[*index].name.clone())
            .collect();
        let active = indices
            .iter()
            .position(|index| *index == self.config.active_profile)
            .unwrap_or(0);
        let Some(menubar) = &mut self.menubar else {
            return;
        };
        let (version, serial) = self
            .last_conn
            .clone()
            .unwrap_or_else(|| ("?".into(), "?".into()));
        let version = match self.device_mode {
            Some(DeviceMode::Codex) => format!("{version} · Codex Micro mode"),
            _ => version,
        };
        menubar.update(self.connected, &version, &serial, self.battery, &names, active);
    }

    fn handle_device(&mut self, message: DeviceMsg, effects: &mut Vec<HostEffect>) {
        match message {
            DeviceMsg::Connected {
                version,
                serial,
                mode,
                boot,
            } => {
                self.connected = true;
                self.key_led_sent = [Some((None, false)); KEY_SLOTS];
                self.last_conn = Some((version, serial));
                self.device_mode = mode;
                self.select_profile_for_current_mode();
                self.boot_info = boot;
                // An application came up: whichever pad was in bootloader
                // mode either booted (this one) or is no longer the story.
                self.bootloader = None;
                self.mode_switch_pending = false;
                self.firmware_banner_dismissed = false;
                self.apply_active_profile();
                self.refresh_menubar();
                self.refresh_activity_led();
            }
            DeviceMsg::Disconnected => {
                self.connected = false;
                self.last_conn = None;
                self.battery = None;
                self.device_mode = None;
                self.boot_info = None;
                self.pressed_cells.fill(false);
                self.refresh_menubar();
            }
            DeviceMsg::BootloaderPresent { serial, info } => {
                let summary = match &info {
                    Ok(info) => format!(
                        "bootloader {} · firmware {} ({})",
                        crate::boot::version_string(info.boot_version),
                        if info.app_version_str().is_empty() {
                            "-"
                        } else {
                            info.app_version_str()
                        },
                        crate::boot::describe_app_valid(info.app_valid)
                    ),
                    Err(e) => format!("bootloader {e}"),
                };
                self.push_log(format!("pad {serial} is in bootloader mode — {summary}"));
                self.bootloader = Some(BootloaderPad { serial, info });
            }
            DeviceMsg::BootloaderGone => {
                self.bootloader = None;
            }
            DeviceMsg::Event(event) => {
                if let Some(signal) = cell_signal(event) {
                    self.pressed_cells[signal.cell] = signal.pressed;
                    if signal.momentary && signal.pressed {
                        effects.push(HostEffect::ReleaseCellAfter {
                            cell: signal.cell,
                            delay: MOMENTARY_RELEASE_DELAY,
                        });
                    }
                }
            }
            DeviceMsg::Keymap {
                slots,
                joy_threshold,
                joy_mode,
                joy_mouse_speed,
                led_brightness,
                led_key_pattern,
                led_ambient_pattern,
            } => {
                let profile = self.active_device_profile();
                let differs = slots != profile.slots()
                    || joy_threshold != profile.analog.joy_threshold
                    || joy_mode != profile.analog.joy_mode
                    || joy_mouse_speed != profile.analog.joy_mouse_speed
                    || led_brightness != self.config.led_brightness
                    || led_key_pattern != self.key_pattern()
                    || led_ambient_pattern != self.ambient_pattern();
                if differs {
                    self.push_log("pad keymap differs from the active profile — syncing");
                    self.sync_device();
                }
                self.refresh_activity_led();
            }
            DeviceMsg::SyncDone { ok, detail } => {
                if ok {
                    self.push_log(format!("pad: {detail}"));
                } else {
                    self.push_log(format!("pad sync failed: {detail}"));
                }
            }
            DeviceMsg::HostInput { slot, pressed } => self.handle_host_input(slot, pressed, effects),
            DeviceMsg::Battery { percent, charging } => {
                if self.battery != Some((percent, charging)) {
                    self.battery = Some((percent, charging));
                    self.refresh_menubar();
                }
            }
        }
    }

    /// A stock-firmware pad emits nothing itself, so the host does what the
    /// firmware keymap would have: run the slot's action, or else type the
    /// code it is set to emit.
    /// Route a stock-pad input through hold layers: a layer key runs nothing
    /// on press; controls used while it is held run the layer's binding (or
    /// the normal one where the layer is empty); releasing a layer key that
    /// was only tapped runs its own binding.
    fn handle_host_input(&mut self, slot: usize, pressed: bool, effects: &mut Vec<HostEffect>) {
        // Pressing the knob while the app switcher is open picks the app.
        if slot == config::SLOT_ENC_PRESS && pressed && actions::commit_app_switch() {
            return;
        }
        let profile = self.active_device_profile();
        // A held-modifier key is down exactly as long as the control is.
        if !pressed {
            if let Some((_, mods)) = self.held_mods.filter(|(held, _)| *held == slot) {
                self.held_mods = None;
                actions::hold_modifiers(mods, false);
                return;
            }
        }
        // The held-modifier key's layer runs without the held modifiers, so
        // it can carry shortcuts too.
        if let Some((held, mods)) = self.held_mods.filter(|_| pressed) {
            let layered = profile
                .layers
                .iter()
                .find(|layer| layer.trigger == held)
                .and_then(|layer| layer.inputs.get(slot))
                .filter(|input| !input.is_unbound())
                .cloned();
            if let Some(input) = layered {
                actions::hold_modifiers(mods, false);
                self.run_input_now(input, effects);
                actions::hold_modifiers(mods, true);
                return;
            }
        }
        // While a layer key is held, its binding for this key beats the hold.
        let layer_binds = |held: usize| {
            profile
                .layers
                .iter()
                .find(|layer| layer.trigger == held)
                .and_then(|layer| layer.inputs.get(slot))
                .is_some_and(|input| !input.is_unbound())
        };
        let held_layer_binds = self.held_layer.is_some_and(|(held, _)| layer_binds(held));
        if let Some(Action::HoldModifier { mods }) = profile
            .inputs
            .get(slot)
            .map(|i| &i.action)
            .filter(|_| !held_layer_binds)
        {
            if pressed {
                self.held_mods = Some((slot, *mods));
                actions::hold_modifiers(*mods, true);
            }
            return;
        }
        // Another layer key pressed while one is held is a normal control of
        // the held layer, so two layer keys can chord with each other.
        let is_trigger = profile.layers.iter().any(|layer| layer.trigger == slot)
            && self.held_layer.map_or(true, |(held, _)| held == slot);
        if is_trigger {
            if pressed {
                self.held_layer = Some((slot, false));
            } else if let Some((held, used)) = self.held_layer {
                if held == slot {
                    self.held_layer = None;
                    if !used {
                        self.run_host_input(slot, effects);
                    }
                }
            }
            return;
        }
        if !pressed {
            return;
        }
        if let Some((held, _)) = self.held_layer {
            self.held_layer = Some((held, true));
            let layered = profile
                .layers
                .iter()
                .find(|layer| layer.trigger == held)
                .and_then(|layer| layer.inputs.get(slot))
                .filter(|input| !input.is_unbound())
                .cloned();
            if let Some(input) = layered {
                self.run_input(input, effects);
                return;
            }
        }
        self.run_host_input(slot, effects);
    }

    fn run_host_input(&mut self, slot: usize, effects: &mut Vec<HostEffect>) {
        let Some(input) = self.active_device_profile().inputs.get(slot).cloned() else {
            return;
        };
        self.run_input(input, effects);
    }

    fn run_input(&mut self, input: InputConfig, effects: &mut Vec<HostEffect>) {
        self.dispatch_input(input, effects, actions::execute);
    }

    /// `run_input`, but finished before returning.
    fn run_input_now(&mut self, input: InputConfig, effects: &mut Vec<HostEffect>) {
        self.dispatch_input(input, effects, actions::execute_now);
    }

    fn dispatch_input(
        &mut self,
        input: InputConfig,
        effects: &mut Vec<HostEffect>,
        execute: fn(&Action),
    ) {
        match input.action {
            Action::AppSettings => push_open_settings(effects),
            Action::None => {
                let emitted = input.emitted;
                match emitted.kind {
                    SlotKind::None => {}
                    SlotKind::Keyboard => execute(&Action::Keystroke {
                        mods: emitted.mods,
                        key: emitted.code,
                    }),
                    SlotKind::Consumer => match consumer_media_op(emitted.code) {
                        Some(op) => execute(&Action::Media { op }),
                        None => self.push_log(format!(
                            "consumer usage 0x{:02X} can't be sent from the Mac",
                            emitted.code
                        )),
                    },
                }
            }
            action => execute(&action),
        }
    }

    fn handle_activity(&mut self, event: ActivityEvent, effects: &mut Vec<HostEffect>) {
        let session_id = if event.session_id.trim().is_empty() {
            "default".to_string()
        } else {
            event.session_id
        };

        // A late status hook from an earlier turn must not replace the state
        // of a newer turn in the same agent session. UserPromptSubmit is the
        // event used by supported hook clients to establish a different turn;
        // clients that omit turn_id remain compatible and are accepted.
        if !event.begins_turn
            && event.turn_id.is_some()
            && self
                .activities
                .get(&session_id)
                .and_then(|activity| activity.turn_id.as_ref())
                .is_some_and(|current| Some(current) != event.turn_id.as_ref())
        {
            return;
        }

        self.activity_epoch = self.activity_epoch.wrapping_add(1);
        let epoch = self.activity_epoch;
        match event.status {
            ActivityStatus::Idle => {
                self.activities.remove(&session_id);
            }
            status => {
                self.activities.insert(
                    session_id.clone(),
                    SessionActivity {
                        status,
                        turn_id: event.turn_id,
                        epoch,
                    },
                );
                let delay = if matches!(status, ActivityStatus::Success | ActivityStatus::Error) {
                    TERMINAL_LED_TTL
                } else {
                    ACTIVE_LED_TTL
                };
                effects.push(HostEffect::ExpireActivityAfter {
                    session_id,
                    epoch,
                    delay,
                });
            }
        }
        self.activity_led_frame = 0;
        self.refresh_activity_led();
    }

    /// Remove an activity state if no newer event superseded it.
    /// Returns whether an LED refresh was needed.
    pub fn expire_activity(&mut self, session_id: &str, epoch: u64) -> bool {
        let should_remove = self
            .activities
            .get(session_id)
            .is_some_and(|activity| activity.epoch == epoch);
        if should_remove {
            self.activities.remove(session_id);
            self.refresh_activity_led();
            true
        } else {
            false
        }
    }

    pub fn activity_status(&self) -> ActivityStatus {
        self.activities
            .values()
            .max_by_key(|activity| activity.status.priority())
            .map(|activity| activity.status)
            .unwrap_or(ActivityStatus::Idle)
    }

    /// Re-apply the current transient status after a status-colour setting
    /// changes. When idle this simply restores the configured idle patterns.
    pub fn refresh_activity_led(&mut self) -> bool {
        let Some(tx) = &self.device_tx else {
            return false;
        };
        let status = self.activity_status();
        let (status_key_pattern, ambient_pattern) = status
            .patterns_with(&self.config.activity_status_colors)
            .unwrap_or((self.key_pattern(), self.ambient_pattern()));
        let mapped_active = self
            .last_conn
            .as_ref()
            .is_some_and(|(version, _)| supports_agent_leds(version))
            && self
                .activities
                .keys()
                .any(|id| AGENT_NAMESPACES.iter().any(|prefix| id.starts_with(prefix)));
        let mut sent = tx
            .send(DeviceCmd::SetTransientLedPattern {
                // Legacy/generic integrations keep the original whole-keyboard
                // status behavior. The four mapped agents use dedicated keys.
                key_pattern: if mapped_active {
                    self.key_pattern()
                } else {
                    status_key_pattern
                },
                // A mapped agent shows its status on its own keys only; the
                // ambient light keeps its own colour.
                ambient_pattern: if mapped_active {
                    self.ambient_pattern()
                } else {
                    ambient_pattern
                },
            })
            .is_ok();
        // Per-key colours: a mapped agent's status wins on its key, else the
        // profile's own colour for that key. Only changes are sent.
        let lighting = self.effective_lighting().unwrap_or_default();
        for index in 0..KEY_SLOTS {
            let agent = AGENT_LED_KEYS
                .iter()
                .find(|(key, _)| *key == index)
                .map(|(_, agent)| *agent);
            let status_color = agent
                .filter(|_| mapped_active)
                .and_then(|agent| self.agent_led_color(agent))
                .map(pattern_rgb);
            let status = status_color.is_some();
            let color = status_color.or_else(|| lighting.key_color(index));
            if self.key_led_sent[index] != Some((color, status)) {
                let ok = tx
                    .send(DeviceCmd::SetKeyLedOverride {
                        index: index as u8,
                        color,
                        status,
                    })
                    .is_ok();
                sent &= ok;
                if ok {
                    self.key_led_sent[index] = Some((color, status));
                }
            }
        }
        sent
    }

    /// The active profile's key backlight, or the global one.
    pub fn key_pattern(&self) -> LedPattern {
        self.effective_lighting()
            .map_or(self.config.led_key_pattern, |l| l.key_pattern)
    }

    /// The active profile's ambient light, or the global one.
    pub fn ambient_pattern(&self) -> LedPattern {
        self.effective_lighting()
            .map_or(self.config.led_ambient_pattern, |l| l.ambient_pattern)
    }

    /// The active profile's lighting, created on first edit from what it
    /// currently shows (the default profile's, or the global settings).
    pub fn active_lighting_mut(&mut self) -> &mut ProfileLighting {
        let (key_pattern, ambient_pattern) = (self.key_pattern(), self.ambient_pattern());
        self.active_profile_mut()
            .lighting
            .get_or_insert_with(|| ProfileLighting {
                key_pattern,
                ambient_pattern,
                key_colors: Vec::new(),
            })
    }

    /// Advance the deterministic per-agent session carousel by one 300 ms
    /// frame. A single session stays steady; multiple sessions are separated
    /// by a short dark frame so repeated colours still reveal their count.
    pub fn advance_activity_led_frame(&mut self) -> bool {
        self.activity_led_frame = self.activity_led_frame.wrapping_add(1);
        self.refresh_activity_led()
    }

    /// A status colour for the current frame: Working breathes, Attention
    /// flashes on and off, Success and Error hold steady until they expire.
    fn animated_status_color(&self, status: ActivityStatus) -> LedPattern {
        let color = self.config.activity_status_colors.get(status);
        let frame = self.activity_led_frame;
        let percent = match status {
            ActivityStatus::Working => WORKING_PULSE[frame % WORKING_PULSE.len()],
            ActivityStatus::Attention if frame % 2 == 1 => 0,
            _ => 100,
        };
        let (r, g, b) = match color {
            LedPattern::Solid { r, g, b } => (r, g, b),
            LedPattern::White => (255, 255, 255),
            LedPattern::Rainbow => return color,
        };
        let scale = |c: u8| (c as u16 * percent / 100) as u8;
        LedPattern::Solid {
            r: scale(r),
            g: scale(g),
            b: scale(b),
        }
    }

    fn agent_led_color(&self, agent: usize) -> Option<LedPattern> {
        let prefix = AGENT_NAMESPACES[agent];
        let mut sessions: Vec<_> = self
            .activities
            .iter()
            .filter(|(id, _)| id.starts_with(prefix))
            .collect();
        sessions.sort_by(|(left, _), (right, _)| left.cmp(right));
        if sessions.is_empty() {
            return None;
        }
        // Claude shows its most urgent session; others show a lone session.
        if agent == 0 || sessions.len() == 1 {
            let status = sessions
                .iter()
                .map(|(_, activity)| activity.status)
                .max_by_key(|status| status.priority())?;
            return Some(self.animated_status_color(status));
        }

        let overflow = sessions.len() > 4;
        sessions.truncate(4);
        let mut frames = Vec::new();
        for (_, activity) in sessions {
            let dwell = match activity.status {
                ActivityStatus::Attention | ActivityStatus::Error => 3,
                _ => 2,
            };
            frames.extend(std::iter::repeat_n(Some(activity.status), dwell));
            frames.push(None);
        }
        if overflow {
            frames.extend([None, None]); // replaced below with purple sentinel
            let frame = self.activity_led_frame % frames.len();
            if frame >= frames.len() - 2 {
                return Some(LedPattern::Solid {
                    r: 160,
                    g: 64,
                    b: 255,
                });
            }
        }
        frames[self.activity_led_frame % frames.len()]
            .map(|status| self.config.activity_status_colors.get(status))
    }

    fn handle_update(&mut self, message: UpdateMsg) {
        match message {
            UpdateMsg::Phase(phase) => self.update_phase = Some(phase),
            UpdateMsg::Log(line) => self.push_log(line),
            UpdateMsg::Progress(fraction) => {
                self.update_progress = normalized_fraction(fraction);
            }
            UpdateMsg::Done { version } => {
                self.updating = false;
                self.update_progress = 1.0;
                self.update_error = None;
                self.update_phase = Some(format!("Up to date — firmware {version}"));
                self.push_log(format!("update complete — firmware {version}"));
            }
            UpdateMsg::Failed(error) => self.update_failed(error),
        }
    }

    fn update_failed(&mut self, error: String) {
        self.updating = false;
        self.update_error = Some(error.clone());
        self.update_phase = Some(format!("Failed — {error}"));
        self.push_log(format!("failed: {error}"));
    }

    fn handle_release(&mut self, message: ReleaseMsg) {
        match message {
            ReleaseMsg::Catalog(catalog) => {
                let app_changed = self
                    .release
                    .as_ref()
                    .map(|current| current.app.version != catalog.app.version)
                    .unwrap_or(true);
                let firmware_changed = self
                    .release
                    .as_ref()
                    .map(|current| current.firmware.version != catalog.firmware.version)
                    .unwrap_or(true);

                if app_changed {
                    self.app_banner_dismissed = false;
                    self.app_downloading = false;
                    self.app_download_progress = 0.0;
                    self.app_download = None;
                    self.app_update_error = None;
                }
                if firmware_changed {
                    self.firmware_banner_dismissed = false;
                    self.firmware_downloading = false;
                    self.firmware_download_progress = 0.0;
                    self.install_after_download = false;
                    if !self.updating
                        && self
                            .firmware_expected_version
                            .as_deref()
                            .is_some_and(|version| version != catalog.firmware.version)
                    {
                        self.firmware_image = None;
                        self.firmware_expected_version = None;
                    }
                }
                self.release = Some(catalog);
                self.release_error = None;
            }
            ReleaseMsg::CatalogUnavailable(error) => {
                self.release_error = Some(error.clone());
                self.push_log(format!("release check failed: {error}"));
            }
            ReleaseMsg::Warning(warning) => {
                self.push_log(format!("release manifest: {warning}"));
            }
            ReleaseMsg::DownloadProgress {
                kind,
                version,
                fraction,
            } => {
                if !self.is_current_download(kind, &version) {
                    return;
                }
                let fraction = normalized_fraction(fraction);
                match kind {
                    DownloadKind::App => self.app_download_progress = fraction,
                    DownloadKind::Firmware => self.firmware_download_progress = fraction,
                }
            }
            ReleaseMsg::DownloadReady {
                kind,
                version,
                path,
            } => {
                if !self.is_current_download(kind, &version) {
                    return;
                }
                match kind {
                    DownloadKind::App => {
                        self.app_downloading = false;
                        self.app_download_progress = 1.0;
                        self.app_update_error = None;
                        // Keep the verified image ready for a deliberate user
                        // action. Release bundles use Sparkle instead; this is
                        // the source/ad-hoc build fallback and must not surprise
                        // the user by opening a disk image in the background.
                        self.app_download = Some(path);
                    }
                    DownloadKind::Firmware => {
                        self.firmware_downloading = false;
                        self.firmware_download_progress = 1.0;
                        self.firmware_image = Some(path.clone());
                        self.firmware_expected_version = Some(version.clone());
                        if self.install_after_download {
                            self.start_firmware_update(path, Some(version));
                        }
                    }
                }
            }
            ReleaseMsg::DownloadFailed {
                kind,
                version,
                error,
            } => {
                if !self.is_current_download(kind, &version) {
                    return;
                }
                match kind {
                    DownloadKind::App => {
                        self.app_downloading = false;
                        self.app_update_error = Some(error);
                    }
                    DownloadKind::Firmware => {
                        self.release_error = Some(error.clone());
                        self.firmware_downloading = false;
                        self.install_after_download = false;
                        self.update_error = Some(error.clone());
                        self.update_phase = Some(format!("Download failed — {error}"));
                        self.push_log(format!("firmware download failed: {error}"));
                    }
                }
            }
        }
    }

    fn is_current_download(&self, kind: DownloadKind, version: &str) -> bool {
        self.release.as_ref().is_some_and(|catalog| match kind {
            DownloadKind::App => catalog.app.version == version,
            DownloadKind::Firmware => catalog.firmware.version == version,
        })
    }

    fn handle_hotkey(&mut self, hotkey_id: u32, effects: &mut Vec<HostEffect>) {
        let slots: Vec<usize> = self
            .intercept
            .as_ref()
            .map(|interception| interception.slots_for_id(hotkey_id).collect())
            .unwrap_or_default();

        for slot in slots {
            let Some(input) = self.active_device_profile().inputs.get(slot).cloned() else {
                continue;
            };
            // Synthesized chords re-enter the global grab. Consuming the
            // guard prevents a host action from recursively firing itself.
            if actions::was_just_synthesized(input.emitted.mods, input.emitted.code) {
                continue;
            }
            if matches!(input.action, Action::AppSettings) {
                push_open_settings(effects);
            } else {
                actions::execute(&input.action);
            }
        }
    }

    fn handle_menubar(&mut self, message: MenubarMsg, effects: &mut Vec<HostEffect>) {
        if let Some(index) = message
            .id
            .strip_prefix("profile:")
            .and_then(|index| index.parse::<usize>().ok())
        {
            if let Some(profile_index) = self.visible_profile_indices().get(index).copied() {
                self.switch_profile(profile_index);
            }
        } else if message.id == "open" {
            effects.push(HostEffect::ShowWindow);
        } else if message.id == "quit" {
            let _ = self.persist();
            effects.push(HostEffect::Quit);
        }
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellSignal {
    cell: usize,
    pressed: bool,
    momentary: bool,
}

fn cell_signal(event: PadEvent) -> Option<CellSignal> {
    let signal = match event {
        PadEvent::Key { index, pressed } if index < 13 => CellSignal {
            cell: index as usize,
            pressed,
            momentary: false,
        },
        PadEvent::Key { .. } => return None,
        PadEvent::Encoder { .. } => CellSignal {
            cell: CELL_ENCODER,
            pressed: true,
            momentary: true,
        },
        PadEvent::EncoderButton { pressed } => CellSignal {
            cell: CELL_ENCODER,
            pressed,
            momentary: false,
        },
        PadEvent::Joystick { active, .. } => CellSignal {
            cell: CELL_JOYSTICK,
            pressed: active,
            momentary: false,
        },
        PadEvent::Touch => CellSignal {
            cell: CELL_TOUCH,
            pressed: true,
            momentary: true,
        },
    };
    Some(signal)
}

fn normalized_fraction(fraction: f64) -> f64 {
    if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn push_open_settings(effects: &mut Vec<HostEffect>) {
    if !effects.contains(&HostEffect::OpenSettings) {
        effects.push(HostEffect::OpenSettings);
    }
}

/// Put a profile's base inputs back from the layer being edited.
fn unswap_layer(profile: &mut Profile, editing: Option<usize>) {
    let Some(trigger) = editing else {
        return;
    };
    if let Some(layer) = profile.layers.iter_mut().find(|layer| layer.trigger == trigger) {
        std::mem::swap(&mut profile.inputs, &mut layer.inputs);
    }
}

/// Where a control's binding comes from in a non-default profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// Empty here, so the default profile's binding runs.
    Default,
    /// Bound in this profile.
    Own,
    /// Explicitly does nothing in this profile.
    Disabled,
}

/// True when `path` is this app (its bundle, or the bare binary when run
/// outside one), so focusing our own window never switches profiles.
fn is_own_app(path: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy();
    exe == path || exe.starts_with(&format!("{}/", path.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::config::{LedPattern, Slot, SlotKind};
    use crate::release::{AppRelease, FirmwareRelease, MacOsRelease, ReleaseAsset, ReleaseCatalog};

    fn state() -> HostState {
        HostState::detached(AppConfig::default())
    }

    #[test]
    fn device_modes_keep_separate_active_profiles() {
        let mut host = state();
        let openmicro_profile = host.config.active_profile;

        host.handle_event(AppEvent::Device(DeviceMsg::Connected {
            version: "0.10.1".into(),
            serial: "TEST".into(),
            mode: Some(DeviceMode::Codex),
            boot: None,
        }));
        let codex_profile = host.config.active_profile;
        assert_ne!(codex_profile, openmicro_profile);
        assert_eq!(host.active_profile().mode, ProfileMode::CodexMicro);

        host.handle_event(AppEvent::Device(DeviceMsg::Connected {
            version: "0.10.1".into(),
            serial: "TEST".into(),
            mode: Some(DeviceMode::OpenMicro),
            boot: None,
        }));
        assert_eq!(host.config.active_profile, openmicro_profile);
        assert_eq!(host.active_profile().mode, ProfileMode::OpenMicro);
    }

    #[test]
    fn the_front_app_picks_its_linked_profile_else_the_default() {
        let mut host = state();
        let default = host.config.active_profile;
        let zoom = host.add_profile(config::ProfileTemplate::Empty);
        host.switch_profile(zoom);
        host.link_app("/Applications/Zoom.app");
        host.switch_profile(default);

        assert!(host.on_frontmost_app(Some("/Applications/Zoom.app")));
        assert_eq!(host.config.active_profile, zoom);
        assert!(host.on_frontmost_app(Some("/System/Applications/Notes.app")));
        assert_eq!(host.config.active_profile, default);
    }

    #[test]
    fn a_manual_switch_sticks_until_the_front_app_changes() {
        let mut host = state();
        let other = host.add_profile(config::ProfileTemplate::Empty);
        host.on_frontmost_app(Some("/Applications/Safari.app"));
        host.switch_profile(other);

        assert!(!host.on_frontmost_app(Some("/Applications/Safari.app")));
        assert_eq!(host.config.active_profile, other);
        assert!(host.on_frontmost_app(Some("/Applications/Mail.app")));
        assert_ne!(host.config.active_profile, other);
    }

    #[test]
    fn auto_switch_off_and_our_own_window_leave_the_profile_alone() {
        let mut host = state();
        let other = host.add_profile(config::ProfileTemplate::Empty);
        host.switch_profile(other);

        let own = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        assert!(!host.on_frontmost_app(Some(&own)));
        assert_eq!(host.config.active_profile, other);

        host.config.auto_switch = false;
        assert!(!host.on_frontmost_app(Some("/Applications/Mail.app")));
        assert_eq!(host.config.active_profile, other);
    }

    #[test]
    fn linking_an_app_moves_it_and_one_default_is_kept_per_mode() {
        let mut host = state();
        let first = host.config.active_profile;
        let second = host.add_profile(config::ProfileTemplate::Empty);
        host.link_app("/Applications/Zoom.app");
        host.switch_profile(second);
        host.link_app("/Applications/Zoom.app");
        assert!(host.config.profiles[first].apps.is_empty());
        assert_eq!(host.config.profiles[second].apps, ["/Applications/Zoom.app"]);

        assert_eq!(host.default_profile_index(), Some(first));
        host.set_default_profile(second);
        assert_eq!(host.default_profile_index(), Some(second));
        assert!(!host.config.profiles[first].is_default);
    }

    #[test]
    fn empty_inputs_follow_the_default_profile_and_do_nothing_blocks_them() {
        let mut host = state();
        let default = host.config.active_profile;
        let bound: Vec<usize> = (0..SLOT_COUNT)
            .filter(|&slot| !host.config.profiles[default].inputs[slot].is_unbound())
            .take(2)
            .collect();
        let (inherit, blocked) = (bound[0], bound[1]);
        let app = host.add_profile(config::ProfileTemplate::Empty);
        host.switch_profile(app);

        let effective = host.effective_profile();
        assert_eq!(effective.inputs[inherit], host.config.profiles[default].inputs[inherit]);
        assert!(host.inherited_input(inherit).is_some());
        assert_eq!(host.input_source(&[inherit]), InputSource::Default);

        host.cycle_input_source(&[blocked]); // Default -> Own (a copy)
        assert_eq!(host.input_source(&[blocked]), InputSource::Own);
        assert_eq!(host.active_profile().inputs[blocked], host.config.profiles[default].inputs[blocked]);
        host.cycle_input_source(&[blocked]); // Own -> Do nothing
        assert_eq!(host.input_source(&[blocked]), InputSource::Disabled);
        assert!(host.effective_profile().inputs[blocked].is_unbound());
        host.cycle_input_source(&[blocked]); // back to Default
        assert_eq!(host.input_source(&[blocked]), InputSource::Default);

        // The default profile itself never inherits.
        host.switch_profile(default);
        assert!(host.inherited_input(inherit).is_none());
    }

    #[test]
    fn unset_lighting_follows_the_default_profile() {
        let red = LedPattern::Solid { r: 255, g: 0, b: 0 };
        let mut host = state();
        host.active_lighting_mut().ambient_pattern = red;
        host.active_lighting_mut().set_key_color(1, Some((0, 255, 60)));
        let app = host.add_profile(config::ProfileTemplate::Empty);
        host.switch_profile(app);
        assert_eq!(host.ambient_pattern(), red);

        host.active_lighting_mut().set_key_color(0, Some((0, 0, 255)));
        let lighting = host.effective_profile().lighting.unwrap();
        assert_eq!(lighting.key_color(0), Some((0, 0, 255)));
        assert_eq!(lighting.key_color(1), Some((0, 255, 60)));
        assert_eq!(lighting.ambient_pattern, red);
    }

    fn host_press(host: &mut HostState, slot: usize, pressed: bool) -> Vec<HostEffect> {
        let mut effects = Vec::new();
        host.handle_host_input(slot, pressed, &mut effects);
        effects
    }

    #[test]
    fn a_hold_layer_rebinds_controls_and_a_bare_tap_runs_the_key() {
        let settings = InputConfig {
            action: Action::AppSettings,
            ..config::unbound_input()
        };
        let mut host = state();
        let profile = host.add_profile(config::ProfileTemplate::Empty);
        host.switch_profile(profile);
        host.set_default_profile(profile);
        host.active_profile_mut().inputs[0] = settings.clone();
        host.toggle_layer(0);
        host.enter_layer_edit(0);
        host.active_profile_mut().inputs[config::SLOT_JOY_LEFT] = settings.clone();
        host.exit_layer_edit();
        assert_eq!(host.active_profile().inputs[0], settings);
        assert!(host.active_profile().inputs[config::SLOT_JOY_LEFT].is_unbound());

        // Hold + left runs the layer binding; releasing doesn't tap the key.
        assert!(host_press(&mut host, 0, true).is_empty());
        assert_eq!(host_press(&mut host, config::SLOT_JOY_LEFT, true), [HostEffect::OpenSettings]);
        assert!(host_press(&mut host, 0, false).is_empty());
        // Left without the hold key uses the (empty) normal binding.
        assert!(host_press(&mut host, config::SLOT_JOY_LEFT, true).is_empty());
        // A bare tap of the hold key runs its own binding on release.
        assert!(host_press(&mut host, 0, true).is_empty());
        assert_eq!(host_press(&mut host, 0, false), [HostEffect::OpenSettings]);
    }

    #[test]
    fn two_layer_keys_chord_with_each_other() {
        let settings = InputConfig {
            action: Action::AppSettings,
            ..config::unbound_input()
        };
        let mut host = state();
        let profile = host.add_profile(config::ProfileTemplate::Empty);
        host.switch_profile(profile);
        host.set_default_profile(profile);
        host.toggle_layer(0);
        host.toggle_layer(1);
        host.enter_layer_edit(1);
        host.active_profile_mut().inputs[0] = settings.clone();
        host.exit_layer_edit();

        // Hold key 2, press key 1: key 2's layer binding for key 1 runs and
        // neither release taps anything.
        assert!(host_press(&mut host, 1, true).is_empty());
        assert_eq!(host_press(&mut host, 0, true), [HostEffect::OpenSettings]);
        assert!(host_press(&mut host, 0, false).is_empty());
        assert!(host_press(&mut host, 1, false).is_empty());
        // Afterwards key 1 is a layer key again.
        assert!(host_press(&mut host, 0, true).is_empty());
        assert!(host_press(&mut host, 0, false).is_empty());
    }

    #[test]
    fn editing_a_layer_never_saves_or_syncs_the_swapped_inputs() {
        let mut host = state();
        let base = host.active_profile().inputs.clone();
        host.toggle_layer(1);
        host.enter_layer_edit(1);
        assert!(host.active_profile().inputs.iter().all(InputConfig::is_unbound));
        assert_eq!(host.stored_config().profiles[host.config.active_profile].inputs, base);
        assert_eq!(host.effective_profile().inputs, base);

        // Switching profile ends the edit and restores the base inputs.
        let other = host.add_profile(config::ProfileTemplate::Empty);
        assert_eq!(host.editing_layer, None);
        host.switch_profile(other);
        assert_eq!(host.config.profiles[0].inputs, base);
    }

    #[test]
    fn profiles_cannot_be_switched_across_device_modes() {
        let mut host = state();
        let openmicro_profile = host.add_profile(config::ProfileTemplate::Empty);
        assert_eq!(host.config.profiles[openmicro_profile].mode, ProfileMode::OpenMicro);

        host.device_mode = Some(DeviceMode::Codex);
        host.select_profile_for_current_mode();
        let codex_profile = host.add_profile(config::ProfileTemplate::Empty);
        assert_eq!(host.config.profiles[codex_profile].mode, ProfileMode::CodexMicro);
        assert!(!host.switch_profile(openmicro_profile));

        host.device_mode = Some(DeviceMode::OpenMicro);
        host.select_profile_for_current_mode();
        assert!(!host.switch_profile(codex_profile));
    }

    fn catalog(app_version: &str, firmware_version: &str) -> ReleaseCatalog {
        let asset = |name: &str| ReleaseAsset {
            name: name.into(),
            url: format!("https://example.com/{name}"),
            sha256: "0".repeat(64),
            size: 1024,
        };
        ReleaseCatalog {
            schema: 1,
            product: "openmicrokbd".into(),
            release_url: "https://example.com/release".into(),
            app: AppRelease {
                version: app_version.into(),
                macos: MacOsRelease {
                    aarch64: asset("arm.dmg"),
                    x86_64: asset("intel.dmg"),
                },
                windows: None,
            },
            firmware: FirmwareRelease {
                version: firmware_version.into(),
                board: "openmicro-stm32f072cb".into(),
                protocol: 2,
                bootloader_version: None,
                layout: None,
                asset: asset("firmware.bin"),
            },
        }
    }

    fn device_event(event: PadEvent) -> AppEvent {
        AppEvent::Device(DeviceMsg::Event(event))
    }

    fn boot_info(app_valid: u8) -> BootInfoReply {
        BootInfoReply {
            status: 0,
            protocol: 1,
            boot_version: [1, 0, 0],
            app_base: openmicro_layout::APP_BASE,
            app_size: openmicro_layout::APP_SIZE,
            app_valid,
            app_version: openmicro_layout::version_bytes("0.10.0"),
            page: Some(2048),
            max_chunk: Some(56),
        }
    }

    #[test]
    fn bootloader_presence_is_tracked_until_it_leaves_or_the_app_boots() {
        let mut host = state();
        assert!(host.bootloader.is_none());

        let info = boot_info(openmicro_layout::app_valid::NONE);
        host.handle_event(AppEvent::Device(DeviceMsg::BootloaderPresent {
            serial: "ABC".into(),
            info: Ok(info),
        }));
        assert_eq!(
            host.bootloader,
            Some(BootloaderPad {
                serial: "ABC".into(),
                info: Ok(info),
            })
        );
        assert!(!host.connected);
        let line = host.logs.back().unwrap();
        assert!(
            line.contains("ABC") && line.contains("no valid firmware"),
            "{line}"
        );

        host.handle_event(AppEvent::Device(DeviceMsg::BootloaderGone));
        assert!(host.bootloader.is_none());

        // An un-openable bootloader is still shown, with the reason.
        host.handle_event(AppEvent::Device(DeviceMsg::BootloaderPresent {
            serial: "ABC".into(),
            info: Err(BootloaderError::Open("Permission denied".into())),
        }));
        assert!(host
            .bootloader
            .as_ref()
            .is_some_and(|pad| pad.info.is_err()));
        let line = host.logs.back().unwrap();
        assert!(line.contains("Permission denied"), "{line}");
        // So is one that opened but did not answer BOOT_INFO usefully.
        host.handle_event(AppEvent::Device(DeviceMsg::BootloaderPresent {
            serial: "ABC".into(),
            info: Err(BootloaderError::Info("bootloader speaks protocol 2".into())),
        }));
        let line = host.logs.back().unwrap();
        assert!(line.contains("protocol 2"), "{line}");

        // The application coming up replaces the bootloader card.
        host.handle_event(AppEvent::Device(DeviceMsg::Connected {
            version: "0.10.0".into(),
            serial: "ABC".into(),
            mode: Some(DeviceMode::OpenMicro),
            boot: Some(boot_info(openmicro_layout::app_valid::VALID)),
        }));
        assert!(host.bootloader.is_none());
        assert!(host.connected);
        assert!(host.pad_has_bootloader());

        host.handle_event(AppEvent::Device(DeviceMsg::Disconnected));
        assert!(!host.pad_has_bootloader());
        assert!(host.boot_info.is_none());
    }

    #[test]
    fn a_failed_install_from_bootloader_mode_shows_the_slot_as_it_is_now() {
        // The worker announces the pad again with fresh BOOT_INFO after an
        // Install that erased the slot and then failed; the card must follow
        // that, not keep the "valid" it showed before.
        let mut host = state();
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);
        host.handle_event(AppEvent::Device(DeviceMsg::BootloaderPresent {
            serial: "ABC".into(),
            info: Ok(boot_info(openmicro_layout::app_valid::VALID)),
        }));
        assert!(host.start_firmware_update(PathBuf::from("fw.bin"), Some("0.11.0".into())));
        assert!(matches!(rx.try_recv(), Ok(DeviceCmd::StartUpdate { .. })));
        assert!(host.updating);
        host.handle_event(AppEvent::Update(UpdateMsg::Failed(
            "UPDATE_END: CRC mismatch".into(),
        )));
        assert!(!host.updating);
        assert!(host
            .update_error
            .as_deref()
            .is_some_and(|e| e.contains("CRC")));
        host.handle_event(AppEvent::Device(DeviceMsg::BootloaderPresent {
            serial: "ABC".into(),
            info: Ok(boot_info(openmicro_layout::app_valid::NONE)),
        }));
        let pad = host.bootloader.as_ref().expect("still in bootloader mode");
        assert_eq!(
            pad.info.as_ref().map(|info| info.app_valid),
            Ok(openmicro_layout::app_valid::NONE)
        );
        // The error stays visible on the card next to the fresh state.
        assert!(host.update_error.is_some());
    }

    #[test]
    fn pads_without_boot_info_do_not_offer_the_bootloader_actions() {
        let mut host = state();
        host.handle_event(AppEvent::Device(DeviceMsg::Connected {
            version: "0.9.0".into(),
            serial: "OLD".into(),
            mode: Some(DeviceMode::OpenMicro),
            boot: None,
        }));
        assert!(host.connected);
        assert!(!host.pad_has_bootloader());
    }

    #[test]
    fn boot_firmware_sends_boot_run_to_the_worker() {
        let mut host = state();
        assert!(!host.boot_firmware());
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);
        assert!(host.boot_firmware());
        assert!(matches!(rx.try_recv(), Ok(DeviceCmd::BootRun)));
        assert!(host.logs.back().unwrap().contains("start the firmware"));
    }

    #[test]
    fn device_events_map_to_the_exact_sixteen_physical_cells() {
        let mut host = state();

        assert!(host
            .handle_event(device_event(PadEvent::Key {
                index: 12,
                pressed: true,
            }))
            .is_empty());
        assert!(host.pressed_cells[12]);

        let effects = host.handle_event(device_event(PadEvent::Encoder { cw: true }));
        assert!(host.pressed_cells[CELL_ENCODER]);
        assert_eq!(
            effects,
            vec![HostEffect::ReleaseCellAfter {
                cell: CELL_ENCODER,
                delay: MOMENTARY_RELEASE_DELAY,
            }]
        );

        host.handle_event(device_event(PadEvent::EncoderButton { pressed: false }));
        assert!(!host.pressed_cells[CELL_ENCODER]);

        host.handle_event(device_event(PadEvent::Joystick {
            dir: 3,
            active: true,
        }));
        assert!(host.pressed_cells[CELL_JOYSTICK]);

        let effects = host.handle_event(device_event(PadEvent::Touch));
        assert!(host.pressed_cells[CELL_TOUCH]);
        assert_eq!(
            effects,
            vec![HostEffect::ReleaseCellAfter {
                cell: CELL_TOUCH,
                delay: MOMENTARY_RELEASE_DELAY,
            }]
        );

        host.release_cell(CELL_TOUCH);
        assert!(!host.pressed_cells[CELL_TOUCH]);

        let before = host.pressed_cells;
        host.handle_event(device_event(PadEvent::Key {
            index: 13,
            pressed: true,
        }));
        assert_eq!(host.pressed_cells, before);
    }

    #[test]
    fn differing_device_keymap_resyncs_the_visible_profile() {
        let mut host = state();
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);
        let profile = host.active_profile().clone();

        host.handle_event(AppEvent::Device(DeviceMsg::Keymap {
            slots: profile.slots(),
            joy_threshold: profile.analog.joy_threshold,
            joy_mode: profile.analog.joy_mode,
            joy_mouse_speed: profile.analog.joy_mouse_speed,
            led_brightness: host.config.led_brightness,
            led_key_pattern: host.config.led_key_pattern,
            led_ambient_pattern: host.config.led_ambient_pattern,
        }));
        assert!(matches!(
            rx.try_recv(),
            Ok(DeviceCmd::SetTransientLedPattern { .. })
        ));

        let mut changed = profile.slots();
        changed[0] = Slot {
            kind: SlotKind::None,
            mods: 0,
            code: 0,
        };
        host.handle_event(AppEvent::Device(DeviceMsg::Keymap {
            slots: changed,
            joy_threshold: profile.analog.joy_threshold,
            joy_mode: profile.analog.joy_mode,
            joy_mouse_speed: profile.analog.joy_mouse_speed,
            led_brightness: host.config.led_brightness,
            led_key_pattern: host.config.led_key_pattern,
            led_ambient_pattern: host.config.led_ambient_pattern,
        }));

        match rx.try_recv().expect("profile sync command") {
            DeviceCmd::SyncKeymap { slots, .. } => assert_eq!(slots, profile.slots()),
            _ => panic!("unexpected device command"),
        }
        assert!(host.logs.back().unwrap().contains("differs"));
    }

    #[test]
    fn activity_events_override_leds_and_ignore_stale_terminal_hooks() {
        let mut host = state();
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);

        let working_effects = host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "session".into(),
            turn_id: Some("turn-2".into()),
            status: ActivityStatus::Working,
            begins_turn: true,
        }));
        assert!(matches!(
            working_effects.as_slice(),
            [HostEffect::ExpireActivityAfter {
                delay: ACTIVE_LED_TTL,
                ..
            }]
        ));
        assert_eq!(host.activity_status(), ActivityStatus::Working);
        match rx.try_recv().expect("working LED override") {
            DeviceCmd::SetTransientLedPattern { key_pattern, .. } => assert_eq!(
                key_pattern,
                LedPattern::Solid {
                    r: 0,
                    g: 96,
                    b: 255,
                }
            ),
            _ => panic!("unexpected device command"),
        }

        // A delayed Stop for turn-1 must not terminate the newer turn-2.
        assert!(host
            .handle_event(AppEvent::Activity(ActivityEvent {
                session_id: "session".into(),
                turn_id: Some("turn-1".into()),
                status: ActivityStatus::Success,
                begins_turn: false,
            }))
            .is_empty());
        assert_eq!(host.activity_status(), ActivityStatus::Working);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        // The same stale-turn guard applies to a delayed approval request.
        assert!(host
            .handle_event(AppEvent::Activity(ActivityEvent {
                session_id: "session".into(),
                turn_id: Some("turn-1".into()),
                status: ActivityStatus::Attention,
                begins_turn: false,
            }))
            .is_empty());
        assert_eq!(host.activity_status(), ActivityStatus::Working);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        // A PostToolUse from an older turn also reports Working, but it must
        // not masquerade as a new UserPromptSubmit event.
        assert!(host
            .handle_event(AppEvent::Activity(ActivityEvent {
                session_id: "session".into(),
                turn_id: Some("turn-1".into()),
                status: ActivityStatus::Working,
                begins_turn: false,
            }))
            .is_empty());
        assert_eq!(host.activity_status(), ActivityStatus::Working);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        let effects = host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "session".into(),
            turn_id: Some("turn-2".into()),
            status: ActivityStatus::Success,
            begins_turn: false,
        }));
        let (session_id, epoch) = match effects.as_slice() {
            [HostEffect::ExpireActivityAfter {
                session_id, epoch, ..
            }] => (session_id.clone(), *epoch),
            other => panic!("unexpected effects: {other:?}"),
        };
        assert_eq!(host.activity_status(), ActivityStatus::Success);
        let _ = rx.try_recv().expect("success LED override");

        assert!(host.expire_activity(&session_id, epoch));
        assert_eq!(host.activity_status(), ActivityStatus::Idle);
        match rx.try_recv().expect("idle pattern restore") {
            DeviceCmd::SetTransientLedPattern {
                key_pattern,
                ambient_pattern,
            } => {
                assert_eq!(key_pattern, host.config.led_key_pattern);
                assert_eq!(ambient_pattern, host.config.led_ambient_pattern);
            }
            _ => panic!("unexpected device command"),
        }
    }

    #[test]
    fn attention_has_priority_over_background_work() {
        let mut host = state();
        host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "background".into(),
            turn_id: None,
            status: ActivityStatus::Working,
            begins_turn: false,
        }));
        host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "foreground".into(),
            turn_id: None,
            status: ActivityStatus::Attention,
            begins_turn: false,
        }));
        assert_eq!(host.activity_status(), ActivityStatus::Attention);
    }

    #[test]
    fn four_agent_namespaces_drive_the_second_row_leds() {
        let mut host = state();
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);
        host.last_conn = Some(("0.7.0".into(), "test".into()));
        for (session_id, status) in [
            ("claude-code:a", ActivityStatus::Attention),
            ("codex:b", ActivityStatus::Working),
            ("grok:c", ActivityStatus::Success),
            ("octoscode:d", ActivityStatus::Error),
        ] {
            host.handle_event(AppEvent::Activity(ActivityEvent {
                session_id: session_id.into(),
                turn_id: None,
                status,
                begins_turn: true,
            }));
        }
        host.refresh_activity_led();

        // Overrides are sent only on change, so check each key's latest.
        let latest = latest_key_overrides(&rx);
        assert_eq!(latest[0], Some(Some((255, 150, 0))));
        assert_eq!(latest[1], Some(Some((255, 150, 0))));
        assert_eq!(latest[3], Some(Some((0, 96, 255))));
        assert_eq!(latest[4], Some(Some((0, 210, 90))));
        assert_eq!(latest[5], Some(Some((255, 30, 50))));
    }

    /// Drain the queue; per key index, the last override colour sent.
    fn latest_key_overrides(
        rx: &mpsc::Receiver<DeviceCmd>,
    ) -> [Option<Option<(u8, u8, u8)>>; KEY_SLOTS] {
        let mut latest = [None; KEY_SLOTS];
        while let Ok(cmd) = rx.try_recv() {
            if let DeviceCmd::SetKeyLedOverride { index, color, .. } = cmd {
                latest[index as usize] = Some(color);
            }
        }
        latest
    }

    #[test]
    fn profile_lighting_overrides_the_global_patterns() {
        let mut host = state();
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);
        host.config.led_key_pattern = LedPattern::Rainbow;
        host.config.led_ambient_pattern = LedPattern::Rainbow;
        let red = LedPattern::Solid { r: 255, g: 0, b: 0 };
        let blue = LedPattern::Solid { r: 0, g: 80, b: 255 };
        {
            let lighting = host.active_lighting_mut();
            lighting.key_pattern = red;
            lighting.ambient_pattern = blue;
            lighting.set_key_color(1, Some((0, 255, 60)));
        }
        host.sync_device();
        let mut synced = None;
        let mut latest = [None; KEY_SLOTS];
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                DeviceCmd::SyncKeymap {
                    led_key_pattern,
                    led_ambient_pattern,
                    ..
                } => synced = Some((led_key_pattern, led_ambient_pattern)),
                DeviceCmd::SetKeyLedOverride { index, color, .. } => {
                    latest[index as usize] = Some(color)
                }
                _ => {}
            }
        }
        assert_eq!(synced, Some((red, blue)));
        assert_eq!(latest[1], Some(Some((0, 255, 60))));
        assert_eq!(latest[0], None); // never set, never sent
        // The global settings are untouched.
        assert_eq!(host.config.led_key_pattern, LedPattern::Rainbow);

        // Nothing changed: a second refresh sends no key overrides.
        host.refresh_activity_led();
        assert!(latest_key_overrides(&rx).iter().all(Option::is_none));
    }

    #[test]
    fn agent_status_beats_the_profile_key_colour_until_idle() {
        let mut host = state();
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);
        host.last_conn = Some(("0.7.0".into(), "test".into()));
        host.active_lighting_mut().set_key_color(0, Some((150, 0, 255)));
        host.refresh_activity_led();
        assert_eq!(latest_key_overrides(&rx)[0], Some(Some((150, 0, 255))));

        let event = |status| {
            AppEvent::Activity(ActivityEvent {
                session_id: "claude-code:a".into(),
                turn_id: None,
                status,
                begins_turn: true,
            })
        };
        host.handle_event(event(ActivityStatus::Working));
        let during = latest_key_overrides(&rx)[0];
        assert!(matches!(during, Some(Some(c)) if c != (150, 0, 255)));

        host.handle_event(event(ActivityStatus::Idle));
        assert_eq!(latest_key_overrides(&rx)[0], Some(Some((150, 0, 255))));
    }

    #[test]
    fn claude_status_animates_and_shows_the_most_urgent_session() {
        let mut host = state();
        let colors = host.config.activity_status_colors;
        for (id, status) in [
            ("claude-code:a", ActivityStatus::Working),
            ("claude-code:b", ActivityStatus::Attention),
        ] {
            host.handle_event(AppEvent::Activity(ActivityEvent {
                session_id: id.into(),
                turn_id: None,
                status,
                begins_turn: true,
            }));
        }
        let black = LedPattern::Solid { r: 0, g: 0, b: 0 };
        host.activity_led_frame = 0;
        assert_eq!(host.agent_led_color(0), Some(colors.attention));
        host.activity_led_frame = 1;
        assert_eq!(host.agent_led_color(0), Some(black));

        host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "claude-code:b".into(),
            turn_id: None,
            status: ActivityStatus::Idle,
            begins_turn: false,
        }));
        host.activity_led_frame = 0;
        let bright = host.agent_led_color(0);
        host.activity_led_frame = 4;
        let dim = host.agent_led_color(0);
        assert_eq!(bright, Some(colors.working));
        assert_ne!(dim, bright);
        assert_ne!(dim, Some(black));
    }

    #[test]
    fn multiple_sessions_cycle_with_dark_separators() {
        let mut host = state();
        for (id, status) in [
            ("codex:a", ActivityStatus::Working),
            ("codex:b", ActivityStatus::Attention),
        ] {
            host.handle_event(AppEvent::Activity(ActivityEvent {
                session_id: id.into(),
                turn_id: None,
                status,
                begins_turn: true,
            }));
        }
        let frames: Vec<_> = (0..7)
            .map(|frame| {
                host.activity_led_frame = frame;
                host.agent_led_color(1)
            })
            .collect();
        assert_eq!(frames[0], Some(host.config.activity_status_colors.working));
        assert_eq!(frames[1], Some(host.config.activity_status_colors.working));
        assert_eq!(frames[2], None);
        assert_eq!(
            frames[3],
            Some(host.config.activity_status_colors.attention)
        );
        assert_eq!(frames[6], None);
    }

    #[test]
    fn expiring_an_old_timer_does_not_clear_a_newer_agent_state() {
        let mut host = state();
        let first = host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "claude-code:session".into(),
            turn_id: Some("prompt".into()),
            status: ActivityStatus::Working,
            begins_turn: true,
        }));
        let first_epoch = match first.as_slice() {
            [HostEffect::ExpireActivityAfter { epoch, .. }] => *epoch,
            other => panic!("unexpected effects: {other:?}"),
        };

        let second = host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "claude-code:session".into(),
            turn_id: Some("prompt".into()),
            status: ActivityStatus::Attention,
            begins_turn: false,
        }));
        let second_epoch = match second.as_slice() {
            [HostEffect::ExpireActivityAfter { epoch, .. }] => *epoch,
            other => panic!("unexpected effects: {other:?}"),
        };

        assert!(!host.expire_activity("claude-code:session", first_epoch));
        assert_eq!(host.activity_status(), ActivityStatus::Attention);
        assert!(host.expire_activity("claude-code:session", second_epoch));
        assert_eq!(host.activity_status(), ActivityStatus::Idle);
    }

    #[test]
    fn one_client_going_idle_does_not_clear_another_client() {
        let mut host = state();
        host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "codex:same".into(),
            turn_id: Some("codex-turn".into()),
            status: ActivityStatus::Working,
            begins_turn: true,
        }));
        host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "claude-code:same".into(),
            turn_id: Some("claude-prompt".into()),
            status: ActivityStatus::Attention,
            begins_turn: true,
        }));
        assert_eq!(host.activity_status(), ActivityStatus::Attention);

        host.handle_event(AppEvent::Activity(ActivityEvent {
            session_id: "claude-code:same".into(),
            turn_id: Some("claude-prompt".into()),
            status: ActivityStatus::Idle,
            begins_turn: false,
        }));
        assert_eq!(host.activity_status(), ActivityStatus::Working);
    }

    #[test]
    fn release_reducer_ignores_stale_versions_and_clamps_progress() {
        let mut host = state();
        host.handle_event(AppEvent::Release(ReleaseMsg::Catalog(catalog(
            "1.2.3", "4.5.6",
        ))));

        host.handle_event(AppEvent::Release(ReleaseMsg::DownloadProgress {
            kind: DownloadKind::App,
            version: "0.9.0".into(),
            fraction: 0.75,
        }));
        assert_eq!(host.app_download_progress, 0.0);

        host.handle_event(AppEvent::Release(ReleaseMsg::DownloadProgress {
            kind: DownloadKind::App,
            version: "1.2.3".into(),
            fraction: 3.0,
        }));
        assert_eq!(host.app_download_progress, 1.0);

        let path = PathBuf::from("OpenMicro.dmg");
        let effects = host.handle_event(AppEvent::Release(ReleaseMsg::DownloadReady {
            kind: DownloadKind::App,
            version: "1.2.3".into(),
            path: path.clone(),
        }));
        assert_eq!(host.app_download, Some(path.clone()));
        assert!(effects.is_empty());
    }

    #[test]
    fn app_download_errors_are_scoped_and_only_reset_for_a_new_app_release() {
        let mut host = state();
        host.handle_event(AppEvent::Release(ReleaseMsg::Catalog(catalog(
            "1.2.3", "4.5.6",
        ))));
        host.handle_event(AppEvent::Release(ReleaseMsg::DownloadFailed {
            kind: DownloadKind::App,
            version: "1.2.3".into(),
            error: "network unavailable".into(),
        }));

        assert_eq!(
            host.app_update_error.as_deref(),
            Some("network unavailable")
        );
        assert!(host.release_error.is_none());

        host.handle_event(AppEvent::Release(ReleaseMsg::Catalog(catalog(
            "1.2.3", "4.5.7",
        ))));
        assert_eq!(
            host.app_update_error.as_deref(),
            Some("network unavailable")
        );

        host.handle_event(AppEvent::Release(ReleaseMsg::Catalog(catalog(
            "1.2.4", "4.5.7",
        ))));
        assert!(host.app_update_error.is_none());
    }

    #[test]
    fn downloaded_firmware_can_flow_directly_into_the_update_worker() {
        let mut host = state();
        let (tx, rx) = mpsc::channel();
        host.device_tx = Some(tx);
        host.install_after_download = true;
        host.handle_event(AppEvent::Release(ReleaseMsg::Catalog(catalog(
            "1.2.3", "4.5.6",
        ))));
        // A catalog refresh deliberately clears an old install intent; arm
        // it after accepting the current release, as the UI download action
        // does.
        host.install_after_download = true;

        let path = PathBuf::from("firmware.bin");
        host.handle_event(AppEvent::Release(ReleaseMsg::DownloadReady {
            kind: DownloadKind::Firmware,
            version: "4.5.6".into(),
            path: path.clone(),
        }));

        match rx.try_recv().expect("firmware update command") {
            DeviceCmd::StartUpdate {
                image,
                expected_version,
            } => {
                assert_eq!(image, path);
                assert_eq!(expected_version.as_deref(), Some("4.5.6"));
            }
            _ => panic!("unexpected device command"),
        }
        assert!(host.updating);
        assert_eq!(host.update_phase.as_deref(), Some("Starting…"));

        host.handle_event(AppEvent::Update(UpdateMsg::Progress(0.6)));
        assert_eq!(host.update_progress, 0.6);
        host.handle_event(AppEvent::Update(UpdateMsg::Done {
            version: "4.5.6".into(),
        }));
        assert!(!host.updating);
        assert_eq!(host.update_progress, 1.0);
        assert!(host
            .logs
            .back()
            .is_some_and(|line| line.contains("update complete")));
    }

    #[test]
    fn tray_profile_open_and_quit_commands_are_reduced_without_process_exit() {
        let mut host = state();
        let mut second = config::default_codex_profile();
        second.name = "Second".into();
        host.config.profiles.push(second);

        assert!(host
            .handle_event(AppEvent::Menubar(MenubarMsg {
                id: "profile:1".into(),
            }))
            .is_empty());
        assert_eq!(host.config.active_profile, 1);

        assert_eq!(
            host.handle_event(AppEvent::Menubar(MenubarMsg { id: "open".into() })),
            vec![HostEffect::ShowWindow]
        );

        assert_eq!(
            host.handle_event(AppEvent::Menubar(MenubarMsg { id: "quit".into() })),
            vec![HostEffect::Quit]
        );
    }

    #[test]
    fn add_profile_appends_uniquely_named_template_without_activating_it() {
        let mut host = state();
        assert_eq!(host.config.profiles.len(), 1);

        let first = host.add_profile(config::ProfileTemplate::ClaudeCode);
        let second = host.add_profile(config::ProfileTemplate::ClaudeCode);
        let empty = host.add_profile(config::ProfileTemplate::Empty);

        assert_eq!((first, second, empty), (1, 2, 3));
        assert_eq!(host.config.profiles[first].name, "Claude Code");
        assert_eq!(host.config.profiles[second].name, "Claude Code 2");
        assert_eq!(host.config.profiles[empty].name, "Profile 4");
        assert_eq!(host.config.active_profile, 0);

        assert!(host.switch_profile(empty));
        assert_eq!(host.config.active_profile, empty);
    }

    #[test]
    fn fractions_reject_non_finite_values() {
        assert_eq!(normalized_fraction(f64::NAN), 0.0);
        assert_eq!(normalized_fraction(f64::INFINITY), 0.0);
        assert_eq!(normalized_fraction(-0.2), 0.0);
        assert_eq!(normalized_fraction(0.4), 0.4);
        assert_eq!(normalized_fraction(1.2), 1.0);
    }
}
