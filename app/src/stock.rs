//! Backend for a Codex Micro still running its stock (Work Louder / OpenAI)
//! firmware, so the app works without reflashing the pad.
//!
//! The stock firmware has no keymap storage and none of the vendor protocol
//! in device.rs. What it does expose is the JSON-RPC channel ChatGPT Desktop
//! uses, on the vendor interface (usage page 0xFF00, report 6):
//!
//! ```text
//! report:  [0x06][0x02 channel][len 0..=61][UTF-8 fragment]
//! message: compact JSON + "\n", split across consecutive reports
//! ```
//!
//! Inputs arrive as `v.oai.hid {k, act}` (keys `AG00`..`AG05`, `ACT06`..
//! `ACT12`, `ENC`, `ENC_CW`/`ENC_CC`) and `v.oai.rad {a, d}` (joystick angle
//! in turns, deflection). LEDs are driven with `v.oai.rgbcfg` (key and
//! ambient groups) and `v.oai.thstatus` (the six agent keys).
//!
//! So the keymap lives on the host: SyncKeymap is held in memory and every
//! input is posted as `DeviceMsg::HostInput`, which host_state resolves
//! against the active profile (run its action, or synthesize the emitted
//! code). Firmware update, DFU and bootloader commands are always refused —
//! this backend never writes to the pad's flash.

use hidapi::{HidApi, HidDevice};
use serde_json::Value;
use std::ffi::CString;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::{
    JoyMode, LedEffect, LedPattern, Slot, DEFAULT_JOY_MOUSE_SPEED, DEFAULT_LED_BRIGHTNESS, KEY_SLOTS,
    SLOT_COUNT, SLOT_ENC_CCW, SLOT_ENC_CW, SLOT_ENC_PRESS, SLOT_JOY_DOWN, SLOT_JOY_LEFT,
    SLOT_JOY_RIGHT, SLOT_JOY_UP,
};
use crate::device::{DeviceCmd, DeviceMsg, PadEvent, UpdateMsg};
use crate::events;

pub const STOCK_VID: u16 = 0x303A;
const STOCK_USAGE_PAGE: u16 = 0xFF00;
const REPORT_ID: u8 = 0x06;
const CHANNEL_RPC: u8 = 0x02;
const MAX_CHUNK: usize = 61;
const REPORT_SIZE: usize = 64;
const EVENT_READ_MS: i32 = 30;
const PRESENCE_PERIOD: Duration = Duration::from_secs(2);
/// How often the battery is read with `device.status`.
const BATTERY_PERIOD: Duration = Duration::from_secs(120);
/// Dimmed brightness, as a fraction of the configured brightness.
const DIM_FRACTION: u16 = 10;
/// How often the display's sleep state is checked.
const DISPLAY_CHECK_PERIOD: Duration = Duration::from_secs(1);

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
}

/// True while the Mac's display sleeps (display off, system asleep or in a
/// background dark wake). The pad answers every request, and macOS counts
/// those answers from a Bluetooth keyboard as input that wakes the Mac, so
/// nothing is sent to the pad meanwhile.
fn display_asleep() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        CGDisplayIsAsleep(CGMainDisplayID()) != 0
    }
    #[cfg(not(target_os = "macos"))]
    false
}
const AGENT_KEYS: usize = 6;
/// Marker appended to the version so the UI can tell this pad apart.
pub const STOCK_SUFFIX: &str = " (stock Codex)";

/// True for a version string reported by this backend.
pub fn is_stock_version(version: &str) -> bool {
    version.ends_with(STOCK_SUFFIX)
}

/// Locate the stock firmware's vendor JSON-RPC interface.
pub fn find(api: &HidApi) -> Option<(CString, String)> {
    api.device_list()
        .find(|info| info.vendor_id() == STOCK_VID && info.usage_page() == STOCK_USAGE_PAGE)
        .map(|info| {
            let serial = info.serial_number().unwrap_or("?").to_string();
            (info.path().to_owned(), serial)
        })
}

/// Open the interface non-exclusively, so other tools (and the OS keyboard
/// interface of the same pad) keep working alongside us.
pub fn open(api: &HidApi, path: &CString) -> Option<HidDevice> {
    #[cfg(target_os = "macos")]
    api.set_open_exclusive(false);
    let dev = api.open_path(path).ok();
    #[cfg(target_os = "macos")]
    api.set_open_exclusive(true);
    dev
}

/// Split one message into 64-byte output reports on the RPC channel.
pub fn frame_rpc(message: &str) -> Vec<[u8; REPORT_SIZE]> {
    message
        .as_bytes()
        .chunks(MAX_CHUNK)
        .map(|chunk| {
            let mut report = [0u8; REPORT_SIZE];
            report[0] = REPORT_ID;
            report[1] = CHANNEL_RPC;
            report[2] = chunk.len() as u8;
            report[3..3 + chunk.len()].copy_from_slice(chunk);
            report
        })
        .collect()
}

/// One pad input, already resolved to the app's slot model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StockInput {
    pub slot: usize,
    pub pressed: bool,
    pub event: Option<PadEventKind>,
}

/// A comparable mirror of `PadEvent`, so the parser can be unit tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadEventKind {
    Key(u8, bool),
    Encoder(bool),
    EncoderButton(bool),
    Joystick(u8, bool),
}

impl PadEventKind {
    fn to_pad_event(self) -> PadEvent {
        match self {
            Self::Key(index, pressed) => PadEvent::Key { index, pressed },
            Self::Encoder(cw) => PadEvent::Encoder { cw },
            Self::EncoderButton(pressed) => PadEvent::EncoderButton { pressed },
            Self::Joystick(dir, active) => PadEvent::Joystick { dir, active },
        }
    }
}

/// Joystick state across `v.oai.rad` notifications: the held direction, so
/// a deflection fires once and its release goes to the same slot.
#[derive(Default)]
pub struct Stick {
    held: Option<u8>,
}

/// Translate one notification into inputs (a joystick flick across
/// directions can release one slot and press another).
pub fn parse_notification(msg: &Value, stick: &mut Stick) -> Vec<StockInput> {
    let Some(params) = msg.get("params") else {
        return Vec::new();
    };
    match msg.get("method").and_then(Value::as_str) {
        Some("v.oai.hid") => {
            let Some(k) = params.get("k").and_then(Value::as_str) else {
                return Vec::new();
            };
            let act = params.get("act").and_then(Value::as_i64).unwrap_or(0);
            let pressed = act != 0;
            let input = match k {
                // Encoder detents arrive as act 2 with no release. The stock
                // firmware's names are reversed: ENC_CW is a physical
                // anticlockwise turn.
                "ENC_CW" => (SLOT_ENC_CCW, Some(PadEventKind::Encoder(false))),
                "ENC_CC" => (SLOT_ENC_CW, Some(PadEventKind::Encoder(true))),
                "ENC" => (SLOT_ENC_PRESS, Some(PadEventKind::EncoderButton(pressed))),
                _ => {
                    let index = k
                        .strip_prefix("AG")
                        .or_else(|| k.strip_prefix("ACT"))
                        .and_then(|n| n.parse::<usize>().ok());
                    match index {
                        Some(i) if i < KEY_SLOTS => {
                            (i, Some(PadEventKind::Key(i as u8, pressed)))
                        }
                        _ => return Vec::new(),
                    }
                }
            };
            let pressed = if act == 2 { true } else { pressed };
            vec![StockInput {
                slot: input.0,
                pressed,
                event: input.1,
            }]
        }
        Some("v.oai.rad") => {
            let d = params.get("d").and_then(Value::as_f64).unwrap_or(0.0);
            let dir = (d >= 0.5).then(|| {
                // a: 0 right, .25 down, .5 left, .75 up -> PadEvent dir
                // (0 up, 1 down, 2 left, 3 right).
                let a = params.get("a").and_then(Value::as_f64).unwrap_or(0.0);
                match ((a * 4.0).round() as i64).rem_euclid(4) {
                    0 => 3,
                    1 => 1,
                    2 => 2,
                    _ => 0,
                }
            });
            let mut out = Vec::new();
            if stick.held == dir {
                return out;
            }
            if let Some(old) = stick.held.take() {
                out.push(StockInput {
                    slot: joy_slot(old),
                    pressed: false,
                    event: Some(PadEventKind::Joystick(old, false)),
                });
            }
            if let Some(new) = dir {
                stick.held = Some(new);
                out.push(StockInput {
                    slot: joy_slot(new),
                    pressed: true,
                    event: Some(PadEventKind::Joystick(new, true)),
                });
            }
            out
        }
        _ => Vec::new(),
    }
}

fn joy_slot(dir: u8) -> usize {
    match dir {
        0 => SLOT_JOY_UP,
        1 => SLOT_JOY_DOWN,
        2 => SLOT_JOY_LEFT,
        _ => SLOT_JOY_RIGHT,
    }
}

/// Firmware effect code, colour and speed for a pattern.
fn pattern_wire(pattern: LedPattern) -> (u8, u32, f32) {
    match pattern {
        LedPattern::Rainbow => (3, 0xFFFFFF, 0.5),
        LedPattern::White => (1, 0xFFFFFF, 0.5),
        LedPattern::Solid { r, g, b } => (1, rgb(r, g, b), 0.5),
        LedPattern::Animated { effect, r, g, b, speed } => {
            let (e, c) = match effect {
                LedEffect::Breath => (EFFECT_BREATH, rgb(r, g, b)),
                LedEffect::Snake => (EFFECT_SNAKE, rgb(r, g, b)),
                LedEffect::Rainbow => (EFFECT_RAINBOW, 0xFFFFFF),
            };
            (e, c, speed.wire())
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn level(brightness: u8) -> f32 {
    brightness as f32 / 255.0
}

fn group_json(pattern: LedPattern, brightness: u8) -> String {
    let (e, c, s) = pattern_wire(pattern);
    let e = if brightness == 0 { 0 } else { e };
    format!(r#"{{"e":{e},"b":{:.2},"s":{s},"c":{c}}}"#, level(brightness))
}

/// `v.oai.rgbcfg` for the key and ambient groups.
pub fn rgbcfg_request(
    keys: LedPattern,
    ambient: LedPattern,
    brightness: u8,
    ambient_brightness: u8,
    id: u16,
) -> String {
    format!(
        r#"{{"method":"v.oai.rgbcfg","params":{{"ambient":{},"keys":{}}},"id":{id}}}"#,
        group_json(ambient, ambient_brightness),
        group_json(keys, brightness)
    )
}

/// Firmware effects (the device kit's numbering) and the Working speed.
const EFFECT_SNAKE: u8 = 2;
const EFFECT_RAINBOW: u8 = 3;
const EFFECT_BREATH: u8 = 4;
const BREATH_SPEED: f32 = 0.3;

/// `v.oai.thstatus` for the six agent keys: an override colour where set,
/// otherwise the key pattern. Status overrides show at `override_brightness`,
/// everything else at `brightness`; `breathe` keys use the pad's breath effect.
pub fn thstatus_request(
    keys: LedPattern,
    overrides: &[Option<(u8, u8, u8)>; AGENT_KEYS],
    status: &[bool; AGENT_KEYS],
    breathe: &[bool; AGENT_KEYS],
    brightness: u8,
    override_brightness: u8,
    id: u16,
) -> String {
    let entries: Vec<String> = overrides
        .iter()
        .enumerate()
        .map(|(i, color)| {
            let (e, c, brightness, speed) = match color {
                Some((r, g, b)) => {
                    let level = if status[i] { override_brightness } else { brightness };
                    if breathe[i] {
                        (EFFECT_BREATH, rgb(*r, *g, *b), level, BREATH_SPEED)
                    } else {
                        (1, rgb(*r, *g, *b), level, 0.5)
                    }
                }
                None => {
                    let (e, c, s) = pattern_wire(keys);
                    (e, c, brightness, s)
                }
            };
            let e = if brightness == 0 { 0 } else { e };
            format!(
                r#"{{"id":{i},"c":{c},"b":{:.2},"e":{e},"s":{speed},"sk":0,"sa":0}}"#,
                level(brightness)
            )
        })
        .collect();
    format!(
        r#"{{"method":"v.oai.thstatus","params":[{}],"id":{id}}}"#,
        entries.join(",")
    )
}

/// Host-held keymap and LED state for one connected pad.
struct State {
    slots: [Slot; SLOT_COUNT],
    joy_threshold: u16,
    joy_mode: JoyMode,
    joy_mouse_speed: u8,
    led_brightness: u8,
    /// Underglow level; None follows `led_brightness`.
    ambient_brightness: Option<u8>,
    led_key_pattern: LedPattern,
    led_ambient_pattern: LedPattern,
    /// What the LEDs currently show (a transient pattern may differ from the
    /// configured one above).
    shown_keys: LedPattern,
    shown_ambient: LedPattern,
    overrides: [Option<(u8, u8, u8)>; AGENT_KEYS],
    /// Overrides that show a live status (kept bright while dimmed).
    status_keys: [bool; AGENT_KEYS],
    /// Overrides the pad breathes itself.
    breathe_keys: [bool; AGENT_KEYS],
    /// Overrides changed since the last `v.oai.thstatus`; queued changes
    /// are coalesced into one write so neighbouring keys change together.
    agent_leds_dirty: bool,
    /// The LEDs dim after a quiet spell; keys showing a live status keep
    /// full brightness.
    dimmed: bool,
    /// Idle time before dimming; `None` never dims.
    dim_after: Option<Duration>,
    last_input: Instant,
    /// No writes while the display sleeps; the LEDs are refreshed on resume.
    paused: bool,
    next_id: u16,
}

impl State {
    fn new() -> Self {
        State {
            slots: [Slot::default(); SLOT_COUNT],
            joy_threshold: 1024,
            joy_mode: JoyMode::default(),
            joy_mouse_speed: DEFAULT_JOY_MOUSE_SPEED,
            led_brightness: DEFAULT_LED_BRIGHTNESS,
            ambient_brightness: None,
            led_key_pattern: LedPattern::default(),
            led_ambient_pattern: LedPattern::default(),
            shown_keys: LedPattern::default(),
            shown_ambient: LedPattern::default(),
            overrides: [None; AGENT_KEYS],
            status_keys: [false; AGENT_KEYS],
            breathe_keys: [false; AGENT_KEYS],
            agent_leds_dirty: false,
            dimmed: false,
            dim_after: Some(Duration::from_secs(60)),
            last_input: Instant::now(),
            paused: false,
            next_id: 0,
        }
    }

    fn keymap_msg(&self) -> DeviceMsg {
        DeviceMsg::Keymap {
            slots: self.slots,
            joy_threshold: self.joy_threshold,
            joy_mode: self.joy_mode,
            joy_mouse_speed: self.joy_mouse_speed,
            led_brightness: self.led_brightness,
            led_key_pattern: self.led_key_pattern,
            led_ambient_pattern: self.led_ambient_pattern,
        }
    }

    /// RPC ids must stay under 1000 for the firmware.
    fn id(&mut self) -> u16 {
        self.next_id = self.next_id % 900 + 1;
        self.next_id
    }

    /// `level` as shown right now (reduced while idle-dimmed).
    fn dim(&self, level: u8) -> u8 {
        if self.dimmed && level > 0 {
            ((level as u16 * DIM_FRACTION / 100) as u8).max(1)
        } else {
            level
        }
    }

    /// The brightness the backlight shows right now.
    fn shown_brightness(&self) -> u8 {
        self.dim(self.led_brightness)
    }

    fn shown_ambient_brightness(&self) -> u8 {
        self.dim(self.ambient_brightness.unwrap_or(self.led_brightness))
    }

    /// Note pad input: restart the idle clock and undim.
    fn note_input(&mut self, dev: &HidDevice) {
        self.last_input = Instant::now();
        if self.dimmed {
            self.dimmed = false;
            self.apply_leds(dev);
        }
    }

    fn apply_leds(&mut self, dev: &HidDevice) {
        if self.paused {
            return;
        }
        let id = self.id();
        let rgbcfg = rgbcfg_request(
            self.shown_keys,
            self.shown_ambient,
            self.shown_brightness(),
            self.shown_ambient_brightness(),
            id,
        );
        let _ = write_msg(dev, &rgbcfg);
        self.apply_agent_leds(dev);
    }

    fn apply_agent_leds(&mut self, dev: &HidDevice) {
        if self.paused {
            return;
        }
        self.agent_leds_dirty = false;
        let id = self.id();
        let th = thstatus_request(
            self.shown_keys,
            &self.overrides,
            &self.status_keys,
            &self.breathe_keys,
            self.shown_brightness(),
            self.led_brightness,
            id,
        );
        let _ = write_msg(dev, &th);
    }
}

fn write_msg(dev: &HidDevice, msg: &str) -> Result<(), String> {
    for report in frame_rpc(&format!("{msg}\n")) {
        dev.write(&report).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Reassembles newline-terminated messages from input reports.
#[derive(Default)]
struct Reader {
    buf: Vec<u8>,
}

impl Reader {
    /// Read for up to `ms`; Err on a read failure (pad gone).
    fn poll(&mut self, dev: &HidDevice, ms: i32) -> Result<Vec<Value>, String> {
        let mut report = [0u8; REPORT_SIZE];
        let n = dev.read_timeout(&mut report, ms).map_err(|e| e.to_string())?;
        // hidapi keeps the report id on numbered reports; tolerate either.
        let frame = if n > 0 && report[0] == REPORT_ID {
            &report[1..n]
        } else {
            &report[..n]
        };
        if frame.len() < 2 || frame[0] != CHANNEL_RPC {
            return Ok(Vec::new());
        }
        let len = (frame[1] as usize).min(frame.len() - 2);
        self.buf.extend_from_slice(&frame[2..2 + len]);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Ok(msg) = serde_json::from_slice::<Value>(&line) {
                out.push(msg);
            }
        }
        if self.buf.len() > 8192 {
            self.buf.clear(); // a lost newline must not grow forever
        }
        Ok(out)
    }
}

fn post_inputs(msg: &Value, stick: &mut Stick) {
    for input in parse_notification(msg, stick) {
        if let Some(event) = input.event {
            events::post(DeviceMsg::Event(event.to_pad_event()));
        }
        events::post(DeviceMsg::HostInput {
            slot: input.slot,
            pressed: input.pressed,
        });
    }
}

/// Ask for the firmware version, forwarding any input that arrives first.
fn query_version(dev: &HidDevice, reader: &mut Reader, stick: &mut Stick) -> String {
    let _ = write_msg(dev, r#"{"method":"sys.version","params":null,"id":901}"#);
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        let Ok(msgs) = reader.poll(dev, EVENT_READ_MS) else {
            break;
        };
        for msg in msgs {
            if msg.get("id").and_then(Value::as_u64) == Some(901) {
                if let Some(v) = msg.pointer("/result/version").and_then(Value::as_str) {
                    return v.to_string();
                }
            } else {
                post_inputs(&msg, stick);
            }
        }
    }
    "?".into()
}

/// Read-only discovery queries for `probe-rpc`. Nothing here writes to the
/// pad; the methods are fixed so the probe can never send anything else.
const PROBE_METHODS: &[&str] = &["sys.version", "device.status", "fs.list"];

/// Send one probe request and wait for its reply.
fn probe_request(dev: &HidDevice, reader: &mut Reader, method: &str, params: &str, id: u64) -> Option<Value> {
    let _ = write_msg(
        dev,
        &format!(r#"{{"method":"{method}","params":{params},"id":{id}}}"#),
    );
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        let Ok(msgs) = reader.poll(dev, EVENT_READ_MS) else {
            break;
        };
        if let Some(msg) = msgs
            .into_iter()
            .find(|msg| msg.get("id").and_then(Value::as_u64) == Some(id))
        {
            return Some(msg);
        }
    }
    None
}

/// Send the probe queries, then read the start of every listed file, and
/// write the replies to `out` (or stdout).
pub fn run_probe(out: Option<&str>) -> i32 {
    let Ok(api) = HidApi::new() else {
        return 1;
    };
    let Some(dev) = find(&api).and_then(|(path, _)| open(&api, &path)) else {
        eprintln!("probe-rpc: stock pad not found or not permitted");
        return 1;
    };
    let mut report = String::new();
    let mut reader = Reader::default();
    let mut files = Vec::new();
    for (n, method) in PROBE_METHODS.iter().enumerate() {
        let reply = probe_request(&dev, &mut reader, method, "null", 7000 + n as u64);
        if *method == "fs.list" {
            if let Some(list) = reply.as_ref().and_then(|r| r.get("result")).and_then(Value::as_array) {
                files.extend(
                    list.iter()
                        .filter_map(|f| f.get("name").and_then(Value::as_str).map(str::to_string)),
                );
            }
        }
        let shown = reply.map_or_else(|| "(no reply)".to_string(), |msg| msg.to_string());
        report.push_str(&format!("{method}: {shown}\n"));
    }
    for (n, file) in files.iter().enumerate() {
        let params = serde_json::json!({"file": file, "offset": 0, "len": 3072}).to_string();
        let reply = probe_request(&dev, &mut reader, "fs.readbin", &params, 7100 + n as u64);
        let shown = reply.map_or_else(|| "(no reply)".to_string(), |msg| msg.to_string());
        report.push_str(&format!("fs.readbin {file}: {shown}\n"));
    }
    match out {
        Some(path) => {
            let _ = std::fs::write(path, report);
        }
        None => print!("{report}"),
    }
    0
}

const NO_FLASH: &str = "firmware updates are disabled for a Codex Micro on stock firmware";

/// Serve one connected stock pad until it goes away. Posts Connected first
/// and Disconnected is left to the caller. Returns false when the command
/// channel closed (app quitting).
pub fn serve(
    api: &mut HidApi,
    rx: &mpsc::Receiver<DeviceCmd>,
    dev: &HidDevice,
    path: &CString,
    serial: String,
) -> bool {
    let mut state = State::new();
    let mut reader = Reader::default();
    let mut stick = Stick::default();

    let version = query_version(dev, &mut reader, &mut stick);
    pad_log(&format!("pad connected ({version})"));
    events::post(DeviceMsg::Connected {
        version: format!("{version}{STOCK_SUFFIX}"),
        serial,
        mode: None,
        boot: None,
    });
    // Nothing is stored on the pad; the app sees a blank keymap, decides it
    // differs from the active profile and syncs it to us.
    events::post(state.keymap_msg());

    let mut next_presence = Instant::now() + PRESENCE_PERIOD;
    let mut next_battery = Instant::now();
    let mut battery_id = None;
    let mut next_display_check = Instant::now();
    loop {
        if Instant::now() >= next_display_check {
            next_display_check = Instant::now() + DISPLAY_CHECK_PERIOD;
            let asleep = display_asleep();
            if asleep != state.paused {
                state.paused = asleep;
                pad_log(if asleep { "display asleep: pad traffic paused" } else { "display awake: pad traffic resumed" });
                if !asleep {
                    // Catch up on everything that changed while paused.
                    state.dimmed = false;
                    state.last_input = Instant::now();
                    state.apply_leds(dev);
                    next_battery = Instant::now();
                }
            }
        }
        loop {
            match rx.try_recv() {
                Ok(cmd) => handle_cmd(dev, &mut state, cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
        if state.agent_leds_dirty {
            state.apply_agent_leds(dev);
        }
        match reader.poll(dev, EVENT_READ_MS) {
            Ok(msgs) => {
                for msg in msgs {
                    let id = msg.get("id").and_then(Value::as_u64);
                    if id.is_some() && id == battery_id {
                        battery_id = None;
                        if let Some(battery) = parse_battery(&msg) {
                            events::post(battery);
                        }
                        log_pad_status(&msg, state.last_input.elapsed());
                        continue;
                    }
                    if is_input(&msg) {
                        state.note_input(dev);
                    }
                    post_inputs(&msg, &mut stick);
                }
            }
            Err(_) => return true,
        }
        if !state.paused
            && !state.dimmed
            && state.dim_after.is_some_and(|after| state.last_input.elapsed() >= after)
        {
            state.dimmed = true;
            state.apply_leds(dev);
        }
        if !state.paused && Instant::now() >= next_battery {
            next_battery = Instant::now() + BATTERY_PERIOD;
            let id = state.id();
            battery_id = Some(id as u64);
            let _ = write_msg(
                dev,
                &format!(r#"{{"method":"device.status","params":null,"id":{id}}}"#),
            );
        }
        if Instant::now() >= next_presence {
            next_presence = Instant::now() + PRESENCE_PERIOD;
            let _ = api.refresh_devices();
            if !api.device_list().any(|info| info.path() == path.as_c_str()) {
                return true;
            }
        }
    }
}

/// Append one timestamped line to ~/Library/Logs/OpenMicro/pad-status.log,
/// which records how the pad's power state changes over time.
pub fn pad_log(line: &str) {
    use std::io::Write;
    let Some(dir) = dirs::home_dir().map(|home| home.join("Library/Logs/OpenMicro")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("pad-status.log"))
    {
        let _ = writeln!(file, "{stamp}  {line}");
    }
}

/// Log the power-related fields of a `device.status` reply.
fn log_pad_status(msg: &Value, idle: Duration) {
    let Some(result) = msg.get("result") else {
        return;
    };
    let field = |key: &str| result.get(key).map_or("?".to_string(), Value::to_string);
    pad_log(&format!(
        "idle {}s  standby {}  ble {}  host_suspended {}  auto_dimmed {}  led_idle {}  battery {}%  charging {}  recovery {}",
        idle.as_secs(),
        field("battery_standby"),
        field("ble_connected"),
        field("ble_host_suspended"),
        field("auto_dimmed"),
        field("led_static_idle"),
        field("battery_percent"),
        field("battery_charging"),
        field("ble_session_recovery_reason"),
    ));
    // Bluetooth slot and bond state, to explain a pad that won't reconnect.
    pad_log(&format!(
        "  mode {} (pref {})  bonds {}/{}  slot_bound {}  adv {}  pairing {}  stale_clears {}  peer_deletes {}  store_full {}  recovery_attempts {}  exhausted {}",
        field("connection_mode"),
        field("preferred_connection_mode"),
        field("ble_bond_count"),
        field("ble_bond_capacity"),
        field("ble_slot_bound"),
        field("ble_advertising"),
        field("ble_pairing"),
        field("ble_stale_slot_clear_events"),
        field("ble_peer_delete_attempts"),
        field("ble_store_full_events"),
        field("ble_session_recovery_attempts"),
        field("ble_session_recovery_exhausted"),
    ));
}

/// True for a key, knob or joystick notification.
fn is_input(msg: &Value) -> bool {
    matches!(
        msg.get("method").and_then(Value::as_str),
        Some("v.oai.hid" | "v.oai.rad")
    )
}

/// The battery fields of a `device.status` reply.
fn parse_battery(msg: &Value) -> Option<DeviceMsg> {
    let result = msg.get("result")?;
    let percent = result
        .get("battery_percent")
        .or_else(|| result.get("battery"))
        .and_then(Value::as_u64)?;
    let charging = result
        .get("battery_charging")
        .or_else(|| result.get("is_charging"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(DeviceMsg::Battery {
        percent: percent.min(100) as u8,
        charging,
    })
}

fn handle_cmd(dev: &HidDevice, state: &mut State, cmd: DeviceCmd) {
    match cmd {
        DeviceCmd::StartUpdate { .. }
        | DeviceCmd::EnterDfuOnly
        | DeviceCmd::EnterBootOnly
        | DeviceCmd::BootRun => events::post(UpdateMsg::Failed(NO_FLASH.into())),
        DeviceCmd::SetDeviceMode { .. } => events::post(DeviceMsg::SyncDone {
            ok: false,
            detail: "device modes need OpenMicro firmware".into(),
        }),
        DeviceCmd::SyncKeymap {
            slots,
            joy_threshold,
            joy_mode,
            joy_mouse_speed,
            led_brightness,
            led_key_pattern,
            led_ambient_pattern,
        } => {
            state.slots = slots;
            state.joy_threshold = joy_threshold;
            state.joy_mode = joy_mode;
            state.joy_mouse_speed = joy_mouse_speed;
            state.led_brightness = led_brightness;
            state.led_key_pattern = led_key_pattern;
            state.led_ambient_pattern = led_ambient_pattern;
            state.shown_keys = led_key_pattern;
            state.shown_ambient = led_ambient_pattern;
            state.apply_leds(dev);
            events::post(DeviceMsg::SyncDone {
                ok: true,
                detail: "keymap applied on the Mac (stock firmware)".into(),
            });
        }
        DeviceCmd::SetLedBrightness { brightness } => {
            state.led_brightness = brightness;
            state.apply_leds(dev);
        }
        DeviceCmd::SetAmbientBrightness { brightness } => {
            if state.ambient_brightness != brightness {
                state.ambient_brightness = brightness;
                state.apply_leds(dev);
            }
        }
        DeviceCmd::SetTransientLedPattern {
            key_pattern,
            ambient_pattern,
        } => {
            if (key_pattern, ambient_pattern) != (state.shown_keys, state.shown_ambient) {
                state.shown_keys = key_pattern;
                state.shown_ambient = ambient_pattern;
                state.apply_leds(dev);
            }
        }
        DeviceCmd::SetKeyLedOverride { index, color, status, breathe } => {
            let i = index as usize;
            let next = (color, status, breathe);
            if i < AGENT_KEYS && (state.overrides[i], state.status_keys[i], state.breathe_keys[i]) != next {
                state.overrides[i] = color;
                state.status_keys[i] = status;
                state.breathe_keys[i] = breathe;
                state.agent_leds_dirty = true;
            }
        }
        DeviceCmd::SetStandby { minutes } => {
            state.dim_after = (minutes > 0).then(|| Duration::from_secs(minutes as u64 * 60));
            if state.dimmed && state.dim_after.map_or(true, |after| state.last_input.elapsed() < after) {
                state.dimmed = false;
                state.apply_leds(dev);
            }
        }
        DeviceCmd::ReadKeymap => events::post(state.keymap_msg()),
        DeviceCmd::FactoryReset => {
            *state = State::new();
            state.apply_leds(dev);
            events::post(state.keymap_msg());
            events::post(DeviceMsg::SyncDone {
                ok: true,
                detail: "host keymap reset".into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_split_at_61_bytes() {
        let reports = frame_rpc(&"x".repeat(130));
        assert_eq!(reports.len(), 3);
        assert_eq!(&reports[0][..3], &[6, 2, 61]);
        assert_eq!(reports[2][2], 8);
    }

    #[test]
    fn keys_map_one_to_one_onto_key_slots() {
        let mut stick = Stick::default();
        let ag = json!({"method":"v.oai.hid","params":{"k":"AG03","act":1,"ag":3}});
        let act = json!({"method":"v.oai.hid","params":{"k":"ACT08","act":0}});
        assert_eq!(
            parse_notification(&ag, &mut stick),
            vec![StockInput { slot: 3, pressed: true, event: Some(PadEventKind::Key(3, true)) }]
        );
        assert_eq!(
            parse_notification(&act, &mut stick),
            vec![StockInput { slot: 8, pressed: false, event: Some(PadEventKind::Key(8, false)) }]
        );
    }

    #[test]
    fn encoder_steps_are_presses() {
        let mut stick = Stick::default();
        let cw = json!({"method":"v.oai.hid","params":{"k":"ENC_CW","act":2}});
        let cc = json!({"method":"v.oai.hid","params":{"k":"ENC_CC","act":2}});
        assert_eq!(parse_notification(&cw, &mut stick)[0].slot, SLOT_ENC_CCW);
        assert!(parse_notification(&cw, &mut stick)[0].pressed);
        assert_eq!(parse_notification(&cc, &mut stick)[0].slot, SLOT_ENC_CW);
    }

    #[test]
    fn joystick_presses_once_and_releases_the_same_slot() {
        let mut stick = Stick::default();
        let up = json!({"method":"v.oai.rad","params":{"a":0.75,"d":1}});
        let right = json!({"method":"v.oai.rad","params":{"a":0.0,"d":1}});
        let center = json!({"method":"v.oai.rad","params":{"a":0.0,"d":0}});
        let first = parse_notification(&up, &mut stick);
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].slot, first[0].pressed), (SLOT_JOY_UP, true));
        assert!(parse_notification(&up, &mut stick).is_empty());
        let flick = parse_notification(&right, &mut stick);
        assert_eq!(
            flick.iter().map(|i| (i.slot, i.pressed)).collect::<Vec<_>>(),
            vec![(SLOT_JOY_UP, false), (SLOT_JOY_RIGHT, true)]
        );
        let release = parse_notification(&center, &mut stick);
        assert_eq!((release[0].slot, release[0].pressed), (SLOT_JOY_RIGHT, false));
    }

    #[test]
    fn led_requests_are_valid_json() {
        let req = rgbcfg_request(LedPattern::Solid { r: 255, g: 0, b: 0 }, LedPattern::Rainbow, 255, 64, 7);
        let v: Value = serde_json::from_str(&req).unwrap();
        assert_eq!(v["params"]["keys"]["b"], 1.0);
        assert_eq!(v["params"]["ambient"]["b"], 0.25);
        let v: Value = serde_json::from_str(&req).unwrap();
        assert_eq!(v["params"]["keys"]["c"], 0xFF0000);
        assert_eq!(v["params"]["ambient"]["e"], 3);
        let mut overrides = [None; AGENT_KEYS];
        overrides[2] = Some((0, 0, 255));
        overrides[3] = Some((0, 255, 0));
        let mut status = [false; AGENT_KEYS];
        status[2] = true;
        let mut breathe = [false; AGENT_KEYS];
        let th = thstatus_request(LedPattern::White, &overrides, &status, &breathe, 0, 255, 8);
        let v: Value = serde_json::from_str(&th).unwrap();
        assert_eq!(v["params"][2]["c"], 255);
        assert_eq!(v["params"][0]["e"], 0); // brightness 0 -> off
        assert_eq!(v["params"][2]["e"], 1); // a status override keeps its own level
        assert_eq!(v["params"][3]["e"], 0); // a profile colour dims with the rest
        breathe[2] = true;
        let th = thstatus_request(LedPattern::White, &overrides, &status, &breathe, 255, 255, 9);
        let v: Value = serde_json::from_str(&th).unwrap();
        assert_eq!(v["params"][2]["e"], EFFECT_BREATH); // Working breathes on the pad
        assert_eq!(v["params"][3]["e"], 1);
        let snake = LedPattern::Animated {
            effect: LedEffect::Snake,
            r: 0,
            g: 255,
            b: 255,
            speed: crate::config::LedSpeed::Slow,
        };
        let v: Value = serde_json::from_str(&rgbcfg_request(snake, LedPattern::Rainbow, 255, 255, 10)).unwrap();
        assert_eq!(v["params"]["keys"]["e"], EFFECT_SNAKE);
        assert_eq!(v["params"]["keys"]["c"], 0x00FFFF);
    }

    #[test]
    fn battery_comes_from_device_status() {
        let reply = json!({"id":4,"result":{"battery":82,"battery_percent":82,"battery_charging":true}});
        assert!(matches!(
            parse_battery(&reply),
            Some(DeviceMsg::Battery { percent: 82, charging: true })
        ));
        assert!(parse_battery(&json!({"id":4,"error":{"code":-32601}})).is_none());
    }

    #[test]
    fn stock_versions_are_recognised() {
        assert!(is_stock_version(&format!("0.1.43-ai-micro{STOCK_SUFFIX}")));
        assert!(!is_stock_version("0.15.0"));
    }
}
