//! User-facing key behaviors and their execution mappings.
//!
//! The editor talks in terms of application shortcuts, macOS controls,
//! keystrokes, and apps. This module translates those choices into the
//! existing device slot plus optional host action without leaking that split
//! into the UI.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{
    default_codex_profile, Action, ControlBehavior, InputConfig, MacOsControl, MacroStep,
    MacroStepEntry, Profile, Slot, SlotKind,
};
use crate::keycodes::{keyboard_name, mods_label};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutPreset {
    pub id: &'static str,
    /// Command label, in the app's own menu wording.
    pub label: &'static str,
    pub mods: u8,
    pub key: u16,
    /// Non-empty for multi-step presets (open the app, press keys, type
    /// text); `mods`/`key` are then unused.
    pub steps: &'static [PresetStep],
}

impl ShortcutPreset {
    pub fn label(&self) -> &'static str {
        self.label
    }

    pub fn is_sequence(&self) -> bool {
        !self.steps.is_empty()
    }
}

/// One step of a multi-step shortcut preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresetStep {
    Open(&'static str),
    Key(u8, u16),
    Type(&'static str),
    Delay(u64),
}

/// The macro a multi-step preset runs.
pub fn preset_macro(steps: &[PresetStep]) -> Vec<MacroStepEntry> {
    let mut out = Vec::new();
    for step in steps {
        match *step {
            PresetStep::Open(target) => out.push(MacroStep::Open {
                target: target.to_string(),
            }),
            PresetStep::Key(mods, key) => out.push(MacroStep::Keystroke {
                mods: shortcut_mods_for_host(mods),
                key,
            }),
            PresetStep::Type(text) => {
                out.extend(text.chars().filter_map(crate::actions::key_for_char).map(
                    |(mods, key)| MacroStep::Keystroke { mods, key },
                ))
            }
            PresetStep::Delay(ms) => out.push(MacroStep::Delay { ms }),
        }
    }
    out.into_iter().map(MacroStepEntry::from).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutApplication {
    pub id: &'static str,
    pub label: &'static str,
    /// Picker artwork: `simple:<slug>` brand icon or a bare Lucide name.
    pub icon: &'static str,
    pub shortcuts: &'static [ShortcutPreset],
}

// --- generated shortcut catalog begin ---
const FINDER_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("new_window", "New Finder window", 0x08, 0x11),
    shortcut("new_folder", "New folder", 0x0a, 0x11),
    shortcut("go_to_folder", "Go to folder", 0x0a, 0x0a),
    shortcut("get_info", "Get info", 0x08, 0x0c),
    shortcut("quick_look", "Quick Look", 0x00, 0x2c),
    shortcut("move_to_trash", "Move to Trash", 0x08, 0x2a),
    shortcut("new_tab", "New tab", 0x08, 0x17),
    shortcut("downloads_folder", "Open Downloads folder", 0x0c, 0x0f),
    shortcut("home_folder", "Open Home folder", 0x0a, 0x0b),
    shortcut("duplicate", "Duplicate", 0x08, 0x07),
];

const SAFARI_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("new_tab", "New tab", 0x08, 0x17),
    shortcut("close_tab", "Close tab", 0x08, 0x1a),
    shortcut("reopen_tab", "Reopen last closed tab", 0x0a, 0x17),
    shortcut("address", "Focus address bar", 0x08, 0x0f),
    shortcut("downloads", "Show downloads", 0x0c, 0x0f),
    shortcut("private_window", "New Private Window", 0x0a, 0x11),
    shortcut("reload", "Reload Page", 0x08, 0x15),
    shortcut("reader", "Show Reader", 0x0a, 0x15),
    shortcut("tab_overview", "Show Tab Overview", 0x0a, 0x31),
    shortcut("add_reading_list", "Add to Reading List", 0x0a, 0x07),
];

const CHROME_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("new_tab", "New Tab", 0x08, 0x17),
    shortcut("close_tab", "Close Tab", 0x08, 0x1a),
    shortcut("reopen_tab", "Reopen Closed Tab", 0x0a, 0x17),
    shortcut("address", "Open Location", 0x08, 0x0f),
    shortcut("incognito", "New incognito window", 0x0a, 0x11),
    shortcut("new_window", "New Window", 0x08, 0x11),
    shortcut("reload", "Reload This Page", 0x08, 0x15),
    shortcut("bookmarks_bar", "Always Show Bookmarks Bar", 0x0a, 0x05),
    shortcut("downloads", "Downloads", 0x0a, 0x0d),
    shortcut("dev_tools", "Developer Tools", 0x0c, 0x0c),
];

const VSCODE_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("command_palette", "Command Palette", 0x0a, 0x13),
    shortcut("quick_open", "Quick Open", 0x08, 0x13),
    shortcut("toggle_terminal", "Toggle terminal", 0x01, 0x35),
    shortcut("toggle_sidebar", "Toggle Primary Side Bar", 0x08, 0x05),
    shortcut("toggle_panel", "Toggle Panel", 0x08, 0x0d),
    shortcut("find_in_files", "Find in files", 0x0a, 0x09),
    shortcut("start_debugging", "Start Debugging", 0x00, 0x3e),
    shortcut("split_editor", "Split Editor", 0x08, 0x31),
    shortcut("extensions", "Extensions", 0x0a, 0x1b),
    shortcut("new_window", "New window", 0x0a, 0x11),
];

const XCODE_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("build", "Build", 0x08, 0x05),
    shortcut("run", "Run", 0x08, 0x15),
    shortcut("stop", "Stop", 0x08, 0x37),
    shortcut("test", "Test", 0x08, 0x18),
    shortcut("clean_build_folder", "Clean Build Folder", 0x0a, 0x0e),
    shortcut("open_quickly", "Open Quickly", 0x0a, 0x12),
    shortcut("debug_area", "Show Debug Area", 0x0a, 0x1c),
    shortcut("navigator", "Show/Hide Navigator", 0x08, 0x27),
    shortcut("inspectors", "Show/Hide Inspectors", 0x0c, 0x27),
    shortcut("library", "Show Library", 0x0a, 0x0f),
];

const TERMINAL_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("new_window", "New window", 0x08, 0x11),
    shortcut("new_tab", "New tab", 0x08, 0x17),
    shortcut("clear", "Clear to Start", 0x08, 0x0e),
    shortcut("close", "Close Tab", 0x08, 0x1a),
    shortcut("find", "Find", 0x08, 0x09),
    shortcut("split_pane", "Split Pane", 0x08, 0x07),
    shortcut("next_tab", "Next Tab", 0x01, 0x2b),
    shortcut("show_inspector", "Show Inspector", 0x08, 0x0c),
    shortcut("previous_mark", "Jump to Previous Mark", 0x08, 0x52),
    shortcut("next_mark", "Jump to Next Mark", 0x08, 0x51),
];

const SLACK_SHORTCUTS: &[ShortcutPreset] = &[
    sequence(
        "search_you",
        "Search \u{201C}you\u{201D}",
        &[
            PresetStep::Open("/Applications/Slack.app"),
            PresetStep::Delay(500),
            PresetStep::Key(0x08, 0x0a), // ⌘G
            PresetStep::Delay(200),
            PresetStep::Type("you"),
            PresetStep::Delay(100),
            PresetStep::Key(0x08, 0x28), // ⌘Return
        ],
    ),
    shortcut("quick_switcher", "Quick switcher", 0x08, 0x0e),
    shortcut("unreads", "All unreads", 0x0a, 0x04),
    shortcut("dms", "Direct messages", 0x0a, 0x0e),
    shortcut("activity", "Activity", 0x0a, 0x10),
    shortcut("threads", "Threads", 0x0a, 0x17),
    shortcut("history_back", "Previous page", 0x08, 0x2f),
    shortcut("history_forward", "Next page", 0x08, 0x30),
    shortcut("preferences", "Preferences", 0x08, 0x36),
];

const FIGMA_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("quick_actions", "Quick actions", 0x08, 0x38),
    shortcut("move", "Move tool", 0x00, 0x19),
    shortcut("frame", "Frame tool", 0x00, 0x09),
    shortcut("pen", "Pen tool", 0x00, 0x13),
    shortcut("text", "Text tool", 0x00, 0x17),
    shortcut("rectangle", "Rectangle tool", 0x00, 0x15),
    shortcut("components", "Create component", 0x0c, 0x0e),
    shortcut("auto_layout", "Add auto layout", 0x02, 0x04),
    shortcut("toggle_ui", "Show/hide UI", 0x08, 0x31),
    shortcut("comment", "Add comment", 0x00, 0x06),
];

const ZOOM_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("mute", "Mute or unmute", 0x0a, 0x04),
    shortcut("video", "Start or stop video", 0x0a, 0x19),
    shortcut("share", "Start or stop screen share", 0x0a, 0x16),
    shortcut("raise_hand", "Raise or lower hand", 0x04, 0x1c),
    shortcut("record", "Start/stop local recording", 0x0a, 0x15),
    shortcut("chat", "Show meeting chat", 0x0a, 0x0b),
    shortcut("participants", "Show participants", 0x08, 0x18),
    shortcut("gallery_view", "Speaker or gallery view", 0x0a, 0x1a),
    shortcut("fullscreen", "Enter or exit full screen", 0x0a, 0x09),
    shortcut("invite", "Invite participants", 0x08, 0x0c),
];

const DAVINCIRESOLVE_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("add_serial_node", "Add Serial Node", 0x04, 0x16),
    shortcut("grab_still", "Grab Still", 0x0c, 0x0a),
    shortcut("bypass_color_grades", "Bypass Color Grades", 0x02, 0x07),
    shortcut("highlight", "Highlight", 0x02, 0x0b),
    shortcut("add_layer_node", "Add Layer Node", 0x04, 0x0f),
    shortcut("add_marker", "Add Marker", 0x00, 0x10),
    shortcut("play_stop", "Play/Stop", 0x00, 0x2c),
    shortcut("open_color_page", "Color Page", 0x02, 0x23),
    shortcut("loop_playback", "Loop", 0x08, 0x38),
    shortcut("cinema_viewer", "Cinema Viewer", 0x08, 0x09),
];

const FINALCUTPRO_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("blade", "Blade", 0x08, 0x05),
    shortcut("append_to_storyline", "Append to Storyline", 0x00, 0x08),
    shortcut("connect_to_storyline", "Connect to Primary Storyline", 0x00, 0x14),
    shortcut("insert_edit", "Insert", 0x00, 0x1a),
    shortcut("overwrite_edit", "Overwrite", 0x00, 0x07),
    shortcut("add_marker", "Add Marker", 0x00, 0x10),
    shortcut("select_tool", "Select", 0x00, 0x04),
    shortcut("trim_tool", "Trim", 0x00, 0x17),
    shortcut("show_retime_editor", "Show Retime Editor", 0x08, 0x15),
    shortcut("play_pause", "Play/Pause", 0x00, 0x2c),
];

const PREMIEREPRO_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("add_edit", "Add Edit", 0x08, 0x0e),
    shortcut("add_marker", "Add Marker", 0x00, 0x10),
    shortcut("insert", "Insert", 0x00, 0x36),
    shortcut("overwrite", "Overwrite", 0x00, 0x37),
    shortcut("lift", "Lift", 0x00, 0x33),
    shortcut("extract", "Extract", 0x00, 0x34),
    shortcut("razor_tool", "Razor Tool", 0x00, 0x06),
    shortcut("selection_tool", "Selection Tool", 0x00, 0x19),
    shortcut("match_frame", "Match Frame", 0x00, 0x09),
    shortcut("export_media", "Export Media", 0x08, 0x10),
];

const OBSSTUDIO_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("edit_transform", "Edit Transform", 0x08, 0x08),
    shortcut("fit_to_screen", "Fit to Screen", 0x08, 0x09),
    shortcut("stretch_to_screen", "Stretch to Screen", 0x08, 0x16),
    shortcut("center_to_screen", "Center to Screen", 0x08, 0x07),
    shortcut("reset_transform", "Reset Transform", 0x08, 0x15),
    shortcut("move_source_up", "Move Source Up", 0x08, 0x52),
    shortcut("move_source_down", "Move Source Down", 0x08, 0x51),
];

const PHOTOSHOP_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("free_transform", "Free Transform", 0x08, 0x17),
    shortcut("layer_via_copy", "Layer Via Copy", 0x08, 0x0d),
    shortcut("new_layer", "New Layer", 0x0a, 0x11),
    shortcut("deselect", "Deselect", 0x08, 0x07),
    shortcut("select_inverse", "Inverse", 0x0a, 0x0c),
    shortcut("increase_brush_size", "Increase Brush Size", 0x00, 0x30),
    shortcut("decrease_brush_size", "Decrease Brush Size", 0x00, 0x2f),
    shortcut("merge_visible", "Merge Visible", 0x0a, 0x08),
    shortcut("fit_on_screen", "Fit on Screen", 0x08, 0x27),
    shortcut("brush_tool", "Brush Tool", 0x00, 0x05),
];

const ILLUSTRATOR_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("transform_again", "Transform Again", 0x08, 0x07),
    shortcut("group", "Group", 0x08, 0x0a),
    shortcut("ungroup", "Ungroup", 0x0a, 0x0a),
    shortcut("make_clipping_mask", "Make Clipping Mask", 0x08, 0x24),
    shortcut("lock_selection", "Lock Selection", 0x08, 0x1f),
    shortcut("toggle_outline_mode", "Outline", 0x08, 0x1c),
    shortcut("create_outlines", "Create Outlines", 0x0a, 0x12),
    shortcut("bring_to_front", "Bring to Front", 0x0a, 0x30),
    shortcut("send_to_back", "Send to Back", 0x0a, 0x2f),
    shortcut("fit_artboard_in_window", "Fit Artboard in Window", 0x08, 0x27),
];

const LOGICPRO_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("play_stop", "Play or Stop", 0x00, 0x2c),
    shortcut("record", "Record", 0x00, 0x15),
    shortcut("cycle_mode", "Cycle Mode", 0x00, 0x06),
    shortcut("metronome", "Metronome", 0x00, 0x0e),
    shortcut("go_to_beginning", "Go to Beginning", 0x00, 0x28),
    shortcut("show_mixer", "Show Mixer", 0x00, 0x1b),
    shortcut("show_editors", "Show Editors", 0x00, 0x08),
    shortcut("show_library", "Show Library", 0x00, 0x1c),
    shortcut("show_automation", "Show Automation", 0x00, 0x04),
    shortcut("bounce_project", "Bounce Project or Section", 0x08, 0x05),
];

const BLENDER_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("toggle_edit_mode", "Toggle Edit Mode", 0x00, 0x2b),
    shortcut("play_animation", "Play Animation", 0x00, 0x2c),
    shortcut("shading_pie_menu", "Shading Pie Menu", 0x00, 0x1d),
    shortcut("add_menu", "Add", 0x02, 0x04),
    shortcut("duplicate_objects", "Duplicate Objects", 0x02, 0x07),
    shortcut("frame_all", "Frame All", 0x00, 0x4a),
    shortcut("toggle_sidebar", "Toggle Sidebar", 0x00, 0x11),
    shortcut("toggle_toolbar", "Toggle Toolbar", 0x00, 0x17),
    shortcut("render_image", "Render Image", 0x00, 0x45),
    shortcut("maximize_area", "Toggle Maximize Area", 0x01, 0x2c),
];

const ITERM2_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("split_vertically", "Split Vertically", 0x08, 0x07),
    shortcut("split_horizontally", "Split Horizontally", 0x0a, 0x07),
    shortcut("maximize_pane", "Maximize Active Pane", 0x0a, 0x28),
    shortcut("clear_buffer", "Clear Buffer", 0x08, 0x0e),
    shortcut("broadcast_input_tab", "Broadcast Input to Tab", 0x0c, 0x0c),
    shortcut("instant_replay", "Start Instant Replay", 0x0c, 0x05),
    shortcut("paste_history", "Open Paste History", 0x0a, 0x0b),
    shortcut("set_mark", "Set Mark", 0x0a, 0x10),
    shortcut("open_autocomplete", "Open Autocomplete", 0x08, 0x33),
];

const INTELLIJ_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("run", "Run", 0x01, 0x15),
    shortcut("debug", "Debug", 0x01, 0x07),
    shortcut("find_action", "Find Action", 0x0a, 0x04),
    shortcut("recent_files", "Recent Files", 0x08, 0x08),
    shortcut("reformat_code", "Reformat Code", 0x0c, 0x0f),
    shortcut("rename", "Rename", 0x02, 0x3f),
    shortcut("show_context_actions", "Show Context Actions", 0x04, 0x28),
    shortcut("build_project", "Build Project", 0x08, 0x42),
    shortcut("step_over", "Step Over", 0x00, 0x41),
    shortcut("hide_all_tool_windows", "Hide All Tool Windows", 0x0a, 0x45),
];

const CURSOR_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("open_agent", "Open Agent", 0x08, 0x0c),
    shortcut("toggle_chat", "Toggle Chat", 0x08, 0x0f),
    shortcut("inline_edit", "Inline Edit", 0x08, 0x0e),
    shortcut("add_selection_to_chat", "Add Selection to Chat", 0x0a, 0x0f),
    shortcut("cursor_settings", "Cursor Settings", 0x0a, 0x0d),
    shortcut("toggle_terminal", "Toggle Terminal", 0x01, 0x35),
    shortcut("toggle_sidebar", "Toggle Primary Side Bar", 0x08, 0x05),
    shortcut("command_palette", "Command Palette", 0x0a, 0x13),
    shortcut("toggle_panel", "Toggle Panel", 0x08, 0x0d),
];

const FIREFOX_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("undo_close_tab", "Undo Close Tab", 0x0a, 0x17),
    shortcut("reader_view", "Toggle Reader View", 0x0c, 0x15),
    shortcut("mute_tab", "Mute/Unmute Audio", 0x01, 0x10),
    shortcut("private_window", "New Private Window", 0x0a, 0x13),
    shortcut("downloads", "Downloads", 0x08, 0x0d),
    shortcut("bookmarks_sidebar", "Bookmarks Sidebar", 0x08, 0x05),
    shortcut("history_sidebar", "History Sidebar", 0x0a, 0x0b),
    shortcut("focus_address_bar", "Open Location", 0x08, 0x0f),
    shortcut("next_tab", "Next Tab", 0x01, 0x2b),
];

const ARC_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("toggle_sidebar", "Toggle Sidebar", 0x08, 0x16),
    shortcut("copy_url", "Copy URL", 0x0a, 0x06),
    shortcut("little_arc", "New Little Arc Window", 0x0c, 0x11),
    shortcut("next_space", "Next Space", 0x0c, 0x4f),
    shortcut("previous_space", "Previous Space", 0x0c, 0x50),
    shortcut("next_tab", "Next Tab", 0x0c, 0x51),
    shortcut("previous_tab", "Previous Tab", 0x0c, 0x52),
    shortcut("restore_tab", "Restore Closed Tab", 0x0a, 0x17),
    shortcut("new_tab", "New Tab", 0x08, 0x17),
];

const NOTES_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("new_note", "New Note", 0x08, 0x11),
    shortcut("checklist", "Checklist", 0x0a, 0x0f),
    shortcut("mark_checklist_item", "Mark as Checked", 0x0a, 0x18),
    shortcut("format_title", "Title", 0x0a, 0x17),
    shortcut("format_heading", "Heading", 0x0a, 0x0b),
    shortcut("format_body", "Body", 0x0a, 0x05),
    shortcut("insert_table", "Table", 0x0c, 0x17),
    shortcut("search_all_notes", "Note List Search", 0x0c, 0x09),
];

const MAIL_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("archive", "Archive", 0x09, 0x04),
    shortcut("reply", "Reply", 0x08, 0x15),
    shortcut("reply_all", "Reply All", 0x0a, 0x15),
    shortcut("forward", "Forward", 0x0a, 0x09),
    shortcut("send", "Send", 0x0a, 0x07),
    shortcut("mark_read_unread", "Mark as Read/Unread", 0x0a, 0x18),
    shortcut("flag", "Toggle Flag", 0x0a, 0x0f),
    shortcut("get_new_mail", "Get New Mail", 0x0a, 0x11),
    shortcut("move_to_junk", "Move to Junk", 0x0a, 0x0d),
    shortcut("new_message", "New Message", 0x08, 0x11),
];

const KEYNOTE_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("play_slideshow", "Play Slideshow", 0x0c, 0x13),
    shortcut("presenter_notes", "Show Presenter Notes", 0x0a, 0x13),
    shortcut("new_slide", "New Slide", 0x0a, 0x11),
    shortcut("skip_slide", "Skip Slide", 0x0a, 0x0b),
    shortcut("add_comment", "Comment", 0x0a, 0x0e),
    shortcut("group_objects", "Group", 0x0c, 0x0a),
    shortcut("ungroup_objects", "Ungroup", 0x0e, 0x0a),
    shortcut("toggle_inspector", "Show/Hide Inspector", 0x0c, 0x0c),
];

const PREVIEW_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("markup_toolbar", "Show Markup Toolbar", 0x0a, 0x04),
    shortcut("highlight_text", "Highlight Text", 0x09, 0x0b),
    shortcut("rotate_left", "Rotate Left", 0x08, 0x0f),
    shortcut("rotate_right", "Rotate Right", 0x08, 0x15),
    shortcut("thumbnails", "Thumbnails", 0x0c, 0x1f),
    shortcut("table_of_contents", "Table of Contents", 0x0c, 0x20),
    shortcut("show_inspector", "Show Inspector", 0x08, 0x0c),
    shortcut("zoom_to_fit", "Zoom to Fit", 0x08, 0x26),
];

const NOTION_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("toggle_sidebar", "Toggle Sidebar", 0x08, 0x31),
    shortcut("search", "Search", 0x08, 0x13),
    shortcut("toggle_dark_mode", "Toggle Dark Mode", 0x0a, 0x0f),
    shortcut("go_back", "Go Back", 0x08, 0x2f),
    shortcut("go_forward", "Go Forward", 0x08, 0x30),
    shortcut("new_page", "New Page", 0x08, 0x11),
    shortcut("new_tab", "New Tab", 0x08, 0x17),
    shortcut("comment", "Comment", 0x0a, 0x10),
    shortcut("new_window", "New Window", 0x0a, 0x11),
];

const OUTLOOK_SHORTCUTS: &[ShortcutPreset] = &[
    sequence(
        "open_calendar",
        "Open Calendar",
        &[
            PresetStep::Open("/Applications/Microsoft Outlook.app"),
            PresetStep::Delay(500),
            PresetStep::Key(0x08, 0x1f), // ⌘2
        ],
    ),
    sequence(
        "open_mail",
        "Open Mail",
        &[
            PresetStep::Open("/Applications/Microsoft Outlook.app"),
            PresetStep::Delay(500),
            PresetStep::Key(0x08, 0x1e), // ⌘1
        ],
    ),
    shortcut("calendar", "Calendar", 0x08, 0x1f),
    shortcut("mail", "Mail", 0x08, 0x1e),
    shortcut("new_item", "New email / event", 0x08, 0x11),
    shortcut("reply", "Reply", 0x08, 0x15),
    shortcut("reply_all", "Reply all", 0x0a, 0x15),
    shortcut("forward", "Forward", 0x08, 0x0d),
];

const OBSIDIAN_SHORTCUTS: &[ShortcutPreset] = &[
    sequence(
        "today",
        "Open \u{201C}Today\u{201D} command",
        &[
            PresetStep::Open("/Applications/Obsidian.app"),
            PresetStep::Delay(500),
            PresetStep::Key(0x08, 0x13), // ⌘P
            PresetStep::Delay(200),
            PresetStep::Type("Today"),
            PresetStep::Delay(150),
            PresetStep::Key(0x00, 0x28), // Return
        ],
    ),
    shortcut("command_palette", "Open Command Palette", 0x08, 0x13),
    shortcut("quick_switcher", "Open Quick Switcher", 0x08, 0x12),
    shortcut("toggle_reading_view", "Toggle Reading View", 0x08, 0x08),
    shortcut("graph_view", "Open Graph View", 0x08, 0x0a),
    shortcut("global_search", "Search in All Files", 0x0a, 0x09),
    shortcut("new_note", "Create New Note", 0x08, 0x11),
    shortcut("toggle_checkbox", "Toggle Checkbox Status", 0x08, 0x0f),
    shortcut("navigate_back", "Navigate Back", 0x0c, 0x50),
    shortcut("navigate_forward", "Navigate Forward", 0x0c, 0x4f),
];

const ONEPASSWORD_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("quick_access", "Show Quick Access", 0x0a, 0x2c),
    shortcut("autofill", "Autofill", 0x08, 0x31),
    shortcut("lock", "Lock 1Password", 0x0a, 0x0f),
    shortcut("copy_password", "Copy Password", 0x0a, 0x06),
    shortcut("copy_one_time_password", "Copy One-Time Password", 0x0c, 0x06),
    shortcut("open_and_fill", "Open and Fill", 0x0a, 0x09),
    shortcut("new_item", "New Item", 0x08, 0x11),
    shortcut("edit_item", "Edit Item", 0x08, 0x08),
    shortcut("toggle_sidebar", "Collapse/Expand Sidebar", 0x0a, 0x07),
];

const SPOTIFY_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("play_pause", "Play/Pause", 0x00, 0x2c),
    shortcut("next_track", "Next Track", 0x08, 0x4f),
    shortcut("previous_track", "Previous Track", 0x08, 0x50),
    shortcut("volume_up", "Volume Up", 0x08, 0x52),
    shortcut("volume_down", "Volume Down", 0x08, 0x51),
    shortcut("toggle_shuffle", "Shuffle", 0x04, 0x16),
    shortcut("toggle_repeat", "Repeat", 0x04, 0x15),
    shortcut("like_song", "Like/Dislike Song", 0x06, 0x05),
    shortcut("go_to_queue", "Go to Queue", 0x06, 0x14),
    shortcut("open_search", "Open Search", 0x08, 0x0e),
];

const MUSIC_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("play_pause", "Play/Pause", 0x00, 0x2c),
    shortcut("next_song", "Next Song", 0x00, 0x4f),
    shortcut("previous_song", "Previous Song", 0x00, 0x50),
    shortcut("volume_up", "Turn Volume Up", 0x08, 0x52),
    shortcut("volume_down", "Turn Volume Down", 0x08, 0x51),
    shortcut("miniplayer", "MiniPlayer", 0x0a, 0x10),
    shortcut("playing_next", "Playing Next", 0x0c, 0x18),
    shortcut("visualizer", "Visualizer", 0x08, 0x17),
    shortcut("equalizer", "Equalizer", 0x0c, 0x08),
];

const DISCORD_SHORTCUTS: &[ShortcutPreset] = &[
    shortcut("toggle_mute", "Toggle Mute", 0x0a, 0x10),
    shortcut("toggle_deafen", "Toggle Deafen", 0x0a, 0x07),
    shortcut("quick_switcher", "Quick Switcher", 0x08, 0x0e),
    shortcut("mark_server_read", "Mark Server as Read", 0x02, 0x29),
    shortcut("toggle_pins", "Toggle Pins", 0x08, 0x13),
    shortcut("toggle_inbox", "Toggle Inbox", 0x08, 0x0c),
    shortcut("emoji_picker", "Toggle Emoji Picker", 0x08, 0x08),
    shortcut("gif_picker", "Toggle GIF Picker", 0x08, 0x0a),
    shortcut("upload_file", "Upload a File", 0x0a, 0x18),
];

pub const APPLICATION_SHORTCUTS: &[ShortcutApplication] = &[
    ShortcutApplication {
        id: "finder",
        label: "Finder",
        icon: "app-window-mac",
        shortcuts: FINDER_SHORTCUTS,
    },
    ShortcutApplication {
        id: "safari",
        label: "Safari",
        icon: "simple:safari",
        shortcuts: SAFARI_SHORTCUTS,
    },
    ShortcutApplication {
        id: "chrome",
        label: "Google Chrome",
        icon: "simple:googlechrome",
        shortcuts: CHROME_SHORTCUTS,
    },
    ShortcutApplication {
        id: "vscode",
        label: "Visual Studio Code",
        icon: "code-xml",
        shortcuts: VSCODE_SHORTCUTS,
    },
    ShortcutApplication {
        id: "xcode",
        label: "Xcode",
        icon: "simple:xcode",
        shortcuts: XCODE_SHORTCUTS,
    },
    ShortcutApplication {
        id: "terminal",
        label: "Terminal",
        icon: "square-terminal",
        shortcuts: TERMINAL_SHORTCUTS,
    },
    ShortcutApplication {
        id: "slack",
        label: "Slack",
        icon: "hash",
        shortcuts: SLACK_SHORTCUTS,
    },
    ShortcutApplication {
        id: "figma",
        label: "Figma",
        icon: "simple:figma",
        shortcuts: FIGMA_SHORTCUTS,
    },
    ShortcutApplication {
        id: "zoom",
        label: "Zoom Workplace",
        icon: "simple:zoom",
        shortcuts: ZOOM_SHORTCUTS,
    },
    ShortcutApplication {
        id: "davinciresolve",
        label: "DaVinci Resolve",
        icon: "simple:davinciresolve",
        shortcuts: DAVINCIRESOLVE_SHORTCUTS,
    },
    ShortcutApplication {
        id: "finalcutpro",
        label: "Final Cut Pro",
        icon: "clapperboard",
        shortcuts: FINALCUTPRO_SHORTCUTS,
    },
    ShortcutApplication {
        id: "premierepro",
        label: "Premiere Pro",
        icon: "film",
        shortcuts: PREMIEREPRO_SHORTCUTS,
    },
    ShortcutApplication {
        id: "obsstudio",
        label: "OBS Studio",
        icon: "simple:obsstudio",
        shortcuts: OBSSTUDIO_SHORTCUTS,
    },
    ShortcutApplication {
        id: "photoshop",
        label: "Adobe Photoshop",
        icon: "image",
        shortcuts: PHOTOSHOP_SHORTCUTS,
    },
    ShortcutApplication {
        id: "illustrator",
        label: "Adobe Illustrator",
        icon: "pen-tool",
        shortcuts: ILLUSTRATOR_SHORTCUTS,
    },
    ShortcutApplication {
        id: "logicpro",
        label: "Logic Pro",
        icon: "music",
        shortcuts: LOGICPRO_SHORTCUTS,
    },
    ShortcutApplication {
        id: "blender",
        label: "Blender",
        icon: "simple:blender",
        shortcuts: BLENDER_SHORTCUTS,
    },
    ShortcutApplication {
        id: "iterm2",
        label: "iTerm2",
        icon: "simple:iterm2",
        shortcuts: ITERM2_SHORTCUTS,
    },
    ShortcutApplication {
        id: "intellij",
        label: "IntelliJ IDEA",
        icon: "simple:intellijidea",
        shortcuts: INTELLIJ_SHORTCUTS,
    },
    ShortcutApplication {
        id: "cursor",
        label: "Cursor",
        icon: "simple:cursor",
        shortcuts: CURSOR_SHORTCUTS,
    },
    ShortcutApplication {
        id: "firefox",
        label: "Firefox",
        icon: "simple:firefox",
        shortcuts: FIREFOX_SHORTCUTS,
    },
    ShortcutApplication {
        id: "arc",
        label: "Arc",
        icon: "simple:arc",
        shortcuts: ARC_SHORTCUTS,
    },
    ShortcutApplication {
        id: "notes",
        label: "Apple Notes",
        icon: "notebook",
        shortcuts: NOTES_SHORTCUTS,
    },
    ShortcutApplication {
        id: "mail",
        label: "Apple Mail",
        icon: "mail",
        shortcuts: MAIL_SHORTCUTS,
    },
    ShortcutApplication {
        id: "keynote",
        label: "Apple Keynote",
        icon: "presentation",
        shortcuts: KEYNOTE_SHORTCUTS,
    },
    ShortcutApplication {
        id: "preview",
        label: "Apple Preview",
        icon: "file-image",
        shortcuts: PREVIEW_SHORTCUTS,
    },
    ShortcutApplication {
        id: "outlook",
        label: "Microsoft Outlook",
        icon: "calendar-days",
        shortcuts: OUTLOOK_SHORTCUTS,
    },
    ShortcutApplication {
        id: "notion",
        label: "Notion",
        icon: "simple:notion",
        shortcuts: NOTION_SHORTCUTS,
    },
    ShortcutApplication {
        id: "obsidian",
        label: "Obsidian",
        icon: "simple:obsidian",
        shortcuts: OBSIDIAN_SHORTCUTS,
    },
    ShortcutApplication {
        id: "onepassword",
        label: "1Password",
        icon: "simple:1password",
        shortcuts: ONEPASSWORD_SHORTCUTS,
    },
    ShortcutApplication {
        id: "spotify",
        label: "Spotify",
        icon: "simple:spotify",
        shortcuts: SPOTIFY_SHORTCUTS,
    },
    ShortcutApplication {
        id: "music",
        label: "Music",
        icon: "simple:applemusic",
        shortcuts: MUSIC_SHORTCUTS,
    },
    ShortcutApplication {
        id: "discord",
        label: "Discord",
        icon: "simple:discord",
        shortcuts: DISCORD_SHORTCUTS,
    },
];
// --- generated shortcut catalog end ---

/// Return the curated catalogs that have meaningful shortcuts on this host.
/// Existing profiles can still resolve every catalog ID so they remain
/// portable between operating systems.
pub fn shortcut_applications_for_host() -> Vec<&'static ShortcutApplication> {
    APPLICATION_SHORTCUTS
        .iter()
        .filter(|application| {
            if cfg!(target_os = "windows") {
                !matches!(
                    application.id,
                    "finder"
                        | "safari"
                        | "xcode"
                        | "terminal"
                        | "finalcutpro"
                        | "logicpro"
                        | "iterm2"
                        | "notes"
                        | "mail"
                        | "keynote"
                        | "preview"
                        | "music"
                        | "outlook"
                )
            } else {
                true
            }
        })
        .collect()
}

const fn shortcut(
    id: &'static str,
    label: &'static str,
    mods: u8,
    key: u16,
) -> ShortcutPreset {
    ShortcutPreset {
        id,
        label,
        mods,
        key,
        steps: &[],
    }
}

const fn sequence(
    id: &'static str,
    label: &'static str,
    steps: &'static [PresetStep],
) -> ShortcutPreset {
    ShortcutPreset {
        id,
        label,
        mods: 0,
        key: 0,
        steps,
    }
}

pub fn shortcut_application(id: &str) -> Option<&'static ShortcutApplication> {
    APPLICATION_SHORTCUTS.iter().find(|app| app.id == id)
}

pub fn shortcut_preset(application: &str, id: &str) -> Option<&'static ShortcutPreset> {
    shortcut_application(application)?
        .shortcuts
        .iter()
        .find(|shortcut| shortcut.id == id)
}

pub fn shortcut_chord_label(shortcut: &ShortcutPreset) -> String {
    if shortcut.is_sequence() {
        return shortcut
            .steps
            .iter()
            .filter_map(|step| match *step {
                PresetStep::Open(target) => Some(format!(
                    "Open {}",
                    Path::new(target)
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .unwrap_or(target)
                )),
                PresetStep::Key(mods, key) => Some(format!(
                    "{}{}",
                    mods_label(shortcut_mods_for_host(mods)),
                    keyboard_name(key).unwrap_or("?")
                )),
                PresetStep::Type(text) => Some(format!("\u{201C}{text}\u{201D}")),
                PresetStep::Delay(_) => None,
            })
            .collect::<Vec<_>>()
            .join(" · ");
    }
    let mods = mods_label(shortcut_mods_for_host(shortcut.mods));
    let key = keyboard_name(shortcut.key).unwrap_or("Unknown key");
    if mods.is_empty() {
        key.to_string()
    } else {
        format!("{mods} + {key}")
    }
}

/// The generated shortcut catalog is authored with macOS modifier bits.
/// Most cross-platform applications use Control for the equivalent Windows
/// command, so translate Command while retaining Shift, Option/Alt and any
/// explicitly-authored Control modifier.
pub const fn shortcut_mods_for_host(mods: u8) -> u8 {
    if cfg!(target_os = "windows") && mods & 0x08 != 0 {
        (mods & !0x08) | 0x01
    } else {
        mods
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacOsPreset {
    pub command: MacOsControl,
    pub label: &'static str,
    pub detail: &'static str,
}

pub const MACOS_PRESETS: &[MacOsPreset] = &[
    macos(
        MacOsControl::BrightnessUp,
        "Brightness up",
        "Increase the built-in display brightness.",
    ),
    macos(
        MacOsControl::BrightnessDown,
        "Brightness down",
        "Decrease the built-in display brightness.",
    ),
    macos(
        MacOsControl::MissionControl,
        "Mission Control",
        "Show all open windows and spaces.",
    ),
    macos(
        MacOsControl::Applications,
        "Applications / Launchpad",
        "Show the macOS applications view.",
    ),
    macos(
        MacOsControl::Search,
        "Spotlight Search",
        "Open or close Spotlight.",
    ),
    macos(
        MacOsControl::Globe,
        "Globe / Fn",
        "Use the native Globe key for input switching and Globe shortcuts.",
    ),
    macos(
        MacOsControl::LockScreen,
        "Lock screen",
        "Lock the current macOS session.",
    ),
    macos(MacOsControl::Sleep, "Sleep", "Put this Mac to sleep."),
    macos(
        MacOsControl::VolumeUp,
        "Volume up",
        "Increase the system output volume.",
    ),
    macos(
        MacOsControl::VolumeDown,
        "Volume down",
        "Decrease the system output volume.",
    ),
    macos(MacOsControl::Mute, "Mute", "Toggle system audio mute."),
    macos(
        MacOsControl::PlayPause,
        "Play / pause",
        "Toggle media playback.",
    ),
    macos(
        MacOsControl::NextTrack,
        "Next track",
        "Skip to the next media item.",
    ),
    macos(
        MacOsControl::PreviousTrack,
        "Previous track",
        "Return to the previous media item.",
    ),
    macos(
        MacOsControl::EmojiPicker,
        "Emoji & symbols",
        "Open or close the character picker.",
    ),
];

/// Windows equivalents use the existing serialized control identifiers so a
/// profile keeps its intent (search, lock, media, and so on) when it moves
/// between hosts. The execution mapping is regenerated for the current OS.
pub const WINDOWS_PRESETS: &[MacOsPreset] = &[
    macos(
        MacOsControl::BrightnessUp,
        "Brightness up",
        "Increase display brightness when the keyboard/display driver supports the standard HID control.",
    ),
    macos(
        MacOsControl::BrightnessDown,
        "Brightness down",
        "Decrease display brightness when the keyboard/display driver supports the standard HID control.",
    ),
    macos(
        MacOsControl::MissionControl,
        "Task view",
        "Show open windows and virtual desktops (Win+Tab).",
    ),
    macos(
        MacOsControl::Applications,
        "All apps",
        "Open the Windows Applications folder.",
    ),
    macos(
        MacOsControl::Search,
        "Windows Search",
        "Open Windows Search (Win+S).",
    ),
    macos(
        MacOsControl::Dictation,
        "Voice typing",
        "Open Windows voice typing (Win+H).",
    ),
    macos(
        MacOsControl::Globe,
        "Input language",
        "Switch the active keyboard/input language (Win+Space).",
    ),
    macos(
        MacOsControl::LockScreen,
        "Lock screen",
        "Lock the current Windows session (Win+L).",
    ),
    macos(MacOsControl::Sleep, "Sleep", "Suspend this Windows PC."),
    macos(
        MacOsControl::VolumeUp,
        "Volume up",
        "Increase the system output volume.",
    ),
    macos(
        MacOsControl::VolumeDown,
        "Volume down",
        "Decrease the system output volume.",
    ),
    macos(MacOsControl::Mute, "Mute", "Toggle system audio mute."),
    macos(
        MacOsControl::PlayPause,
        "Play / pause",
        "Toggle media playback.",
    ),
    macos(
        MacOsControl::NextTrack,
        "Next track",
        "Skip to the next media item.",
    ),
    macos(
        MacOsControl::PreviousTrack,
        "Previous track",
        "Return to the previous media item.",
    ),
    macos(
        MacOsControl::EmojiPicker,
        "Emoji & symbols",
        "Open the Windows emoji and symbols picker (Win+Period).",
    ),
];

const fn macos(command: MacOsControl, label: &'static str, detail: &'static str) -> MacOsPreset {
    MacOsPreset {
        command,
        label,
        detail,
    }
}

pub fn macos_preset(command: MacOsControl) -> &'static MacOsPreset {
    system_presets()
        .iter()
        .find(|preset| preset.command == command)
        .unwrap_or(&system_presets()[0])
}

pub fn system_presets() -> &'static [MacOsPreset] {
    if cfg!(target_os = "windows") {
        WINDOWS_PRESETS
    } else {
        MACOS_PRESETS
    }
}

/// Apply a curated application chord. Returns false only for stale/unknown
/// catalog IDs, leaving the existing mapping untouched.
pub fn apply_application_shortcut(
    input: &mut InputConfig,
    slot_index: usize,
    application: &str,
    shortcut_id: &str,
) -> bool {
    let Some(shortcut) = shortcut_preset(application, shortcut_id) else {
        return false;
    };
    input.behavior = Some(ControlBehavior::ApplicationShortcut {
        application: application.to_string(),
        shortcut: shortcut_id.to_string(),
    });
    if shortcut.is_sequence() {
        input.emitted = host_trigger(slot_index);
        input.action = Action::Macro {
            steps: preset_macro(shortcut.steps),
        };
    } else {
        input.emitted = keyboard_slot(shortcut_mods_for_host(shortcut.mods), shortcut.key);
        input.action = Action::None;
    }
    true
}

pub fn apply_keystroke(input: &mut InputConfig, mods: u8, key: u16) {
    input.behavior = Some(ControlBehavior::Keystroke);
    input.emitted = keyboard_slot(mods & 0x0f, key);
    input.action = Action::None;
}

pub fn apply_macos(input: &mut InputConfig, slot_index: usize, command: MacOsControl) {
    input.behavior = Some(ControlBehavior::MacOs { command });
    #[cfg(target_os = "windows")]
    let (emitted, action) = match command {
        MacOsControl::BrightnessUp => (consumer_slot(0x006F), Action::None),
        MacOsControl::BrightnessDown => (consumer_slot(0x0070), Action::None),
        MacOsControl::MissionControl => (keyboard_slot(0x08, 0x2B), Action::None),
        MacOsControl::Applications => (
            host_trigger(slot_index),
            Action::Run {
                command: "explorer.exe shell:AppsFolder".to_string(),
            },
        ),
        MacOsControl::Search => (keyboard_slot(0x08, 0x16), Action::None),
        MacOsControl::Dictation => (keyboard_slot(0x08, 0x0B), Action::None),
        MacOsControl::Globe => (keyboard_slot(0x08, 0x2C), Action::None),
        MacOsControl::LockScreen => (keyboard_slot(0x08, 0x0F), Action::None),
        MacOsControl::Sleep => (
            host_trigger(slot_index),
            Action::Run {
                command: "powershell.exe -NoProfile -WindowStyle Hidden -Command \"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Application]::SetSuspendState('Suspend',$false,$false)\"".to_string(),
            },
        ),
        MacOsControl::VolumeUp => (consumer_slot(0x00E9), Action::None),
        MacOsControl::VolumeDown => (consumer_slot(0x00EA), Action::None),
        MacOsControl::Mute => (consumer_slot(0x00E2), Action::None),
        MacOsControl::PlayPause => (consumer_slot(0x00CD), Action::None),
        MacOsControl::NextTrack => (consumer_slot(0x00B5), Action::None),
        MacOsControl::PreviousTrack => (consumer_slot(0x00B6), Action::None),
        MacOsControl::EmojiPicker => (keyboard_slot(0x08, 0x37), Action::None),
    };
    #[cfg(not(target_os = "windows"))]
    let (emitted, action) = match command {
        MacOsControl::BrightnessUp => (consumer_slot(0x006F), Action::None),
        MacOsControl::BrightnessDown => (consumer_slot(0x0070), Action::None),
        MacOsControl::MissionControl => (keyboard_slot(0x01, 0x52), Action::None),
        MacOsControl::Applications => {
            let target = if Path::new("/System/Applications/Apps.app").exists() {
                "/System/Applications/Apps.app"
            } else {
                "/System/Applications/Launchpad.app"
            };
            (
                host_trigger(slot_index),
                Action::Open {
                    target: target.to_string(),
                },
            )
        }
        // The documented macOS default. Users can remap Spotlight in System
        // Settings, just like any other keyboard shortcut.
        MacOsControl::Search => (keyboard_slot(0x08, 0x2C), Action::None),
        // Kept only so profiles saved before Dictation was withdrawn still
        // load. macOS never dispatches this usage, so the slot is inert.
        MacOsControl::Dictation => (consumer_slot(0x00D8), Action::None),
        // Apple's accessory keyboard specification assigns the native Globe
        // key to Consumer-page AC Keyboard Layout Select (0x029D).
        MacOsControl::Globe => (consumer_slot(0x029D), Action::None),
        MacOsControl::LockScreen => (keyboard_slot(0x09, 0x14), Action::None),
        MacOsControl::Sleep => (
            host_trigger(slot_index),
            Action::Run {
                command: "pmset sleepnow".to_string(),
            },
        ),
        MacOsControl::VolumeUp => (consumer_slot(0x00E9), Action::None),
        MacOsControl::VolumeDown => (consumer_slot(0x00EA), Action::None),
        MacOsControl::Mute => (consumer_slot(0x00E2), Action::None),
        MacOsControl::PlayPause => (consumer_slot(0x00CD), Action::None),
        MacOsControl::NextTrack => (consumer_slot(0x00B5), Action::None),
        MacOsControl::PreviousTrack => (consumer_slot(0x00B6), Action::None),
        MacOsControl::EmojiPicker => (keyboard_slot(0x09, 0x2C), Action::None),
    };
    input.emitted = emitted;
    input.action = action;
}

pub fn apply_app(input: &mut InputConfig, slot_index: usize, target: String) {
    input.behavior = Some(ControlBehavior::App {
        target: target.clone(),
    });
    input.emitted = host_trigger(slot_index);
    input.action = Action::Open { target };
}

pub fn behavior_is_consistent(input: &InputConfig, slot_index: usize) -> bool {
    let Some(behavior) = input.behavior.clone() else {
        return false;
    };
    let mut expected = input.clone();
    let valid = match behavior {
        ControlBehavior::ApplicationShortcut {
            application,
            shortcut,
        } => apply_application_shortcut(&mut expected, slot_index, &application, &shortcut),
        ControlBehavior::MacOs { command }
            if matches!(command, MacOsControl::Applications | MacOsControl::Sleep) =>
        {
            apply_macos(&mut expected, slot_index, command);
            return hidden_trigger(input.emitted) && expected.action == input.action;
        }
        ControlBehavior::MacOs { command } => {
            apply_macos(&mut expected, slot_index, command);
            true
        }
        ControlBehavior::Keystroke => {
            return input.emitted.kind == SlotKind::Keyboard && input.action == Action::None;
        }
        ControlBehavior::App { target } => {
            apply_app(&mut expected, slot_index, target);
            return hidden_trigger(input.emitted) && expected.action == input.action;
        }
    };
    valid && expected.emitted == input.emitted && expected.action == input.action
}

/// Keep every host-assisted semantic behavior on a distinct, non-printing
/// chord. A user may freely assign an old hidden chord as a normal keystroke;
/// this allocator moves the hidden trigger instead of letting one press run
/// two behaviors.
pub fn normalize_hidden_triggers(profile: &mut Profile) {
    for slot_index in 0..profile.inputs.len() {
        if !is_host_assisted(&profile.inputs[slot_index]) {
            continue;
        }
        let current = profile.inputs[slot_index].emitted;
        let collision = profile
            .inputs
            .iter()
            .enumerate()
            .any(|(other, input)| other != slot_index && input.emitted == current);
        if hidden_trigger(current) && !collision {
            continue;
        }

        if let Some(candidate) = hidden_trigger_candidates().find(|candidate| {
            profile
                .inputs
                .iter()
                .enumerate()
                .all(|(other, input)| other == slot_index || input.emitted != *candidate)
        }) {
            profile.inputs[slot_index].emitted = candidate;
        }
    }
}

/// Build the profile actually used while the pad has the Codex USB identity.
/// Slots that still match the stock Codex profile remain the firmware's native
/// Codex controls. Explicitly customized slots keep their emitted HID value;
/// a host-side action that still occupies its stock sentinel is moved to a
/// unique hidden trigger so the firmware can recognize it as an override.
pub fn with_codex_overrides(profile: &Profile) -> Profile {
    let defaults = default_codex_profile();
    let mut effective = profile.clone();

    for slot_index in 0..effective.inputs.len().min(defaults.inputs.len()) {
        if !profile.codex_overrides.contains(&slot_index) {
            effective.inputs[slot_index].emitted = defaults.inputs[slot_index].emitted;
            effective.inputs[slot_index].action = Action::None;
            effective.inputs[slot_index].behavior = None;
            continue;
        }

        let input = &effective.inputs[slot_index];
        let default = &defaults.inputs[slot_index];
        if input.emitted != default.emitted {
            continue;
        }

        if let Some(candidate) = hidden_trigger_candidates().find(|candidate| {
            *candidate != default.emitted
                && effective
                    .inputs
                    .iter()
                    .enumerate()
                    .all(|(other, input)| other == slot_index || input.emitted != *candidate)
        }) {
            effective.inputs[slot_index].emitted = candidate;
        }
    }

    normalize_hidden_triggers(&mut effective);
    effective
}

fn is_host_assisted(input: &InputConfig) -> bool {
    matches!(
        input.behavior.as_ref(),
        Some(ControlBehavior::App { .. })
            | Some(ControlBehavior::MacOs {
                command: MacOsControl::Applications | MacOsControl::Sleep
            })
    )
}

/// Keyboard usages a host-assisted trigger may use: F13 and F16-F20.
///
/// F14/F15 (0x69/0x6A) are deliberately absent. macOS acts on those two as
/// the display-brightness keys down in the HID layer, *below* the hotkey
/// registry, so grabbing one in intercept.rs does not stop the screen from
/// dimming: the bound action fires and the brightness moves too. Six codes
/// across the sixteen modifier banks still cover every slot several times.
const TRIGGER_CODES: [u16; 6] = [0x68, 0x6B, 0x6C, 0x6D, 0x6E, 0x6F];

/// Modifier banks for host-assisted triggers, in allocation order: bare
/// first, then the combinations a user is least likely to type by hand.
const MOD_BANKS: [u8; 16] = [
    0x00, 0x02, 0x03, 0x01, 0x04, 0x08, 0x06, 0x0A, 0x0C, 0x05, 0x09, 0x07, 0x0B, 0x0D, 0x0E, 0x0F,
];

fn hidden_trigger(slot: Slot) -> bool {
    slot.kind == SlotKind::Keyboard
        && TRIGGER_CODES.contains(&slot.code)
        && slot.mods & 0xF0 == 0
}

fn hidden_trigger_candidates() -> impl Iterator<Item = Slot> {
    MOD_BANKS.into_iter().flat_map(|mods| {
        TRIGGER_CODES
            .into_iter()
            .map(move |code| keyboard_slot(mods, code))
    })
}

fn keyboard_slot(mods: u8, key: u16) -> Slot {
    Slot {
        kind: SlotKind::Keyboard,
        mods,
        code: key,
    }
}

fn consumer_slot(code: u16) -> Slot {
    Slot {
        kind: SlotKind::Consumer,
        mods: 0,
        code,
    }
}

/// A unique, non-printing chord for host-assisted behavior, drawn from
/// `TRIGGER_CODES` x `MOD_BANKS` so every one of the 24 input slots gets
/// a distinct chord the OS will not act on by itself.
fn host_trigger(slot_index: usize) -> Slot {
    let bank = (slot_index / TRIGGER_CODES.len()).min(MOD_BANKS.len() - 1);
    keyboard_slot(
        MOD_BANKS[bank],
        TRIGGER_CODES[slot_index % TRIGGER_CODES.len()],
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledApp {
    pub name: String,
    pub path: String,
}

/// Discover launchable applications from the operating system's normal
/// user-facing roots. Exact paths are persisted because two applications can
/// share a display name. On Windows, Start Menu shortcuts are preferred over
/// raw executables so packaged and argument-bearing applications launch with
/// the same semantics as they do from the Start menu.
pub fn installed_apps() -> Vec<InstalledApp> {
    let mut paths = BTreeSet::new();
    for root in application_roots() {
        collect_applications(&root, 0, application_scan_depth(&root), &mut paths);
    }

    let mut by_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        by_name.entry(stem.to_string()).or_default().push(path);
    }

    let mut apps = Vec::new();
    for (name, mut paths) in by_name {
        paths.sort();
        let duplicated = paths.len() > 1;
        for path in paths {
            let display_name = if duplicated {
                let parent = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|value| value.to_str())
                    .unwrap_or("Applications");
                format!("{name} — {parent}")
            } else {
                name.clone()
            };
            apps.push(InstalledApp {
                name: display_name,
                path: path.to_string_lossy().into_owned(),
            });
        }
    }
    apps
}

fn application_roots() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut roots = Vec::new();
        if let Some(app_data) = std::env::var_os("APPDATA").map(PathBuf::from) {
            roots.push(app_data.join("Microsoft/Windows/Start Menu/Programs"));
        }
        if let Some(program_data) = std::env::var_os("PROGRAMDATA").map(PathBuf::from) {
            roots.push(program_data.join("Microsoft/Windows/Start Menu/Programs"));
        }
        // Some per-user installers omit a Start Menu entry. Keep this root
        // last so a normal shortcut sorts before its raw executable.
        if let Some(local_data) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            roots.push(local_data.join("Programs"));
        }
        return roots;
    }

    #[cfg(target_os = "macos")]
    {
        let mut roots = vec![
            PathBuf::from("/Applications"),
            PathBuf::from("/System/Applications"),
        ];
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join("Applications"));
        }
        return roots;
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let mut roots = vec![PathBuf::from("/usr/share/applications")];
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".local/share/applications"));
        }
        roots
    }
}

fn application_scan_depth(root: &Path) -> usize {
    #[cfg(target_os = "windows")]
    {
        // Start Menu groups are commonly nested; LocalAppData/Programs only
        // needs enough depth to reach an app's main executable.
        return if root
            .to_string_lossy()
            .contains("Start Menu")
        {
            6
        } else {
            3
        };
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = root;
        3
    }
}

fn is_launchable_application(path: &Path, file_type: &fs::FileType) -> bool {
    #[cfg(target_os = "macos")]
    {
        return file_type.is_dir()
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"));
    }
    #[cfg(target_os = "windows")]
    {
        return file_type.is_file()
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("lnk")
                        || extension.eq_ignore_ascii_case("url")
                        || extension.eq_ignore_ascii_case("exe")
                });
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        file_type.is_file()
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("desktop"))
    }
}

fn collect_applications(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    found: &mut BTreeSet<PathBuf>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if is_launchable_application(&path, &file_type) {
            found.insert(path);
        } else if file_type.is_dir() && depth < max_depth {
            collect_applications(&path, depth + 1, max_depth, found);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_codex_profile;
    use std::collections::HashSet;

    #[test]
    fn shortcut_catalog_is_well_formed() {
        let mut app_ids = HashSet::new();
        for app in APPLICATION_SHORTCUTS {
            assert!(app_ids.insert(app.id), "duplicate app id {}", app.id);
            assert!(!app.shortcuts.is_empty(), "{} has no shortcuts", app.id);
            let mut ids = HashSet::new();
            let mut chords = HashSet::new();
            for preset in app.shortcuts {
                if preset.is_sequence() {
                    assert!(ids.insert(preset.id), "{}: duplicate id {}", app.id, preset.id);
                    continue;
                }
                assert!(
                    ids.insert(preset.id),
                    "{}: duplicate shortcut id {}",
                    app.id,
                    preset.id
                );
                assert!(
                    chords.insert((preset.mods, preset.key)),
                    "{}: duplicate chord on {}",
                    app.id,
                    preset.id
                );
                assert!(
                    keyboard_name(preset.key).is_some(),
                    "{}.{}: unknown key usage {:#04x}",
                    app.id,
                    preset.id,
                    preset.key
                );
                assert_eq!(
                    preset.mods & !0x0f,
                    0,
                    "{}.{}: chords use left-hand modifier bits only",
                    app.id,
                    preset.id
                );
                for label in [preset.label] {
                    assert!(
                        !label.trim().is_empty(),
                        "{}.{}: empty label",
                        app.id,
                        preset.id
                    );
                    assert!(
                        label.chars().count() <= 34,
                        "{}.{}: label too long: {label}",
                        app.id,
                        preset.id
                    );
                }
            }
        }
    }

    #[test]
    fn shortcut_catalog_icons_resolve() {
        for app in APPLICATION_SHORTCUTS {
            if let Some(slug) = app.icon.strip_prefix("simple:") {
                assert!(
                    crate::simple_icons::find(slug).is_some(),
                    "{}: unknown Simple Icons slug {slug}",
                    app.id
                );
            } else {
                assert!(
                    crate::lucide::icon_char(app.icon).is_some(),
                    "{}: unknown Lucide icon {}",
                    app.id,
                    app.icon
                );
            }
        }
    }

    #[test]
    fn host_shortcut_catalog_and_modifiers_match_windows_conventions() {
        let applications = shortcut_applications_for_host();
        assert!(!applications.is_empty());
        if cfg!(target_os = "windows") {
            assert_eq!(applications[0].id, "chrome");
            assert!(!applications.iter().any(|app| app.id == "finder"));
            assert_eq!(shortcut_mods_for_host(0x0A), 0x03);
        } else {
            assert_eq!(applications[0].id, "finder");
            assert_eq!(shortcut_mods_for_host(0x0A), 0x0A);
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_application_scan_finds_shortcuts_and_executables() {
        let unique = format!(
            "openmicro-app-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let nested = root.join("Utilities");
        fs::create_dir_all(&nested).expect("temporary app directory");
        fs::write(root.join("Editor.lnk"), b"shortcut").expect("shortcut fixture");
        fs::write(nested.join("Tool.exe"), b"executable").expect("executable fixture");
        fs::write(nested.join("Ignore.txt"), b"text").expect("non-app fixture");

        let mut found = BTreeSet::new();
        collect_applications(&root, 0, 3, &mut found);
        assert!(found.contains(&root.join("Editor.lnk")));
        assert!(found.contains(&nested.join("Tool.exe")));
        assert!(!found.contains(&nested.join("Ignore.txt")));

        fs::remove_dir_all(&root).expect("remove temporary app directory");
    }

    /// Saved profiles reference (application, shortcut) ids and re-derive the
    /// emitted chord from this catalog — these historical pairs must keep
    /// producing identical bytes forever.
    #[test]
    fn legacy_shortcut_ids_keep_their_chords() {
        #[rustfmt::skip]
        const FROZEN: &[(&str, &str, u8, u16)] = &[
            ("finder", "new_window", 0x08, 0x11), ("finder", "new_folder", 0x0A, 0x11),
            ("finder", "go_to_folder", 0x0A, 0x0A), ("finder", "get_info", 0x08, 0x0C),
            ("finder", "quick_look", 0x00, 0x2C), ("finder", "move_to_trash", 0x08, 0x2A),
            ("safari", "new_tab", 0x08, 0x17), ("safari", "close_tab", 0x08, 0x1A),
            ("safari", "reopen_tab", 0x0A, 0x17), ("safari", "address", 0x08, 0x0F),
            ("safari", "downloads", 0x0C, 0x0F),
            ("chrome", "new_tab", 0x08, 0x17), ("chrome", "close_tab", 0x08, 0x1A),
            ("chrome", "reopen_tab", 0x0A, 0x17), ("chrome", "address", 0x08, 0x0F),
            ("chrome", "incognito", 0x0A, 0x11),
            ("vscode", "command_palette", 0x0A, 0x13), ("vscode", "quick_open", 0x08, 0x13),
            ("vscode", "new_window", 0x0A, 0x11), ("vscode", "toggle_terminal", 0x01, 0x35),
            ("vscode", "find_in_files", 0x0A, 0x09),
            ("xcode", "build", 0x08, 0x05), ("xcode", "run", 0x08, 0x15),
            ("xcode", "test", 0x08, 0x18), ("xcode", "stop", 0x08, 0x37),
            ("xcode", "open_quickly", 0x0A, 0x12),
            ("terminal", "new_window", 0x08, 0x11), ("terminal", "new_tab", 0x08, 0x17),
            ("terminal", "clear", 0x08, 0x0E), ("terminal", "close", 0x08, 0x1A),
            ("terminal", "find", 0x08, 0x09),
            ("slack", "quick_switcher", 0x08, 0x0E), ("slack", "preferences", 0x08, 0x36),
            ("slack", "threads", 0x0A, 0x17), ("slack", "history_back", 0x08, 0x2F),
            ("slack", "history_forward", 0x08, 0x30),
            ("figma", "quick_actions", 0x08, 0x38), ("figma", "frame", 0x00, 0x09),
            ("figma", "pen", 0x00, 0x13), ("figma", "text", 0x00, 0x17),
            ("figma", "components", 0x0C, 0x0E),
            ("zoom", "mute", 0x0A, 0x04), ("zoom", "video", 0x0A, 0x19),
            ("zoom", "share", 0x0A, 0x16), ("zoom", "participants", 0x08, 0x18),
            ("zoom", "chat", 0x0A, 0x0B),
        ];
        for (app, id, mods, key) in FROZEN {
            let preset = shortcut_preset(app, id)
                .unwrap_or_else(|| panic!("{app}.{id} vanished from the catalog"));
            assert_eq!(
                (preset.mods, preset.key),
                (*mods, *key),
                "{app}.{id} chord changed"
            );
        }
    }

    fn sample_input() -> InputConfig {
        InputConfig {
            label: "Key".to_string(),
            icon: "star".to_string(),
            behavior: None,
            emitted: Slot::default(),
            action: Action::None,
        }
    }

    #[test]
    fn sequence_preset_compiles_to_a_macro() {
        let mut input = sample_input();
        assert!(apply_application_shortcut(&mut input, 3, "slack", "search_you"));
        assert!(behavior_is_consistent(&input, 3));
        let Action::Macro { steps } = &input.action else {
            panic!("expected a macro, got {:?}", input.action);
        };
        assert_eq!(
            crate::actions::macro_preview(steps),
            "Open Slack.app ⌘G you ⌘Enter"
        );
        assert_eq!(
            shortcut_chord_label(shortcut_preset("slack", "search_you").unwrap()),
            "Open Slack · ⌘G · \u{201C}you\u{201D} · ⌘Enter"
        );
    }

    #[test]
    fn application_shortcut_compiles_to_one_saved_chord() {
        let mut input = sample_input();
        assert!(apply_application_shortcut(
            &mut input,
            0,
            "vscode",
            "command_palette"
        ));
        assert_eq!(
            input.emitted,
            Slot {
                kind: SlotKind::Keyboard,
                mods: if cfg!(target_os = "windows") {
                    0x03
                } else {
                    0x0A
                },
                code: 0x13,
            }
        );
        assert_eq!(input.action, Action::None);
        assert_eq!(
            input.behavior,
            Some(ControlBehavior::ApplicationShortcut {
                application: "vscode".to_string(),
                shortcut: "command_palette".to_string(),
            })
        );
    }

    #[test]
    fn host_app_behavior_gets_a_unique_non_printing_trigger() {
        let mut first = sample_input();
        let mut touch = sample_input();
        apply_app(&mut first, 0, "/Applications/Finder.app".to_string());
        apply_app(&mut touch, 21, "/Applications/Music.app".to_string());
        assert_ne!(first.emitted, touch.emitted);
        assert_eq!(first.emitted.kind, SlotKind::Keyboard);
        assert_eq!(touch.emitted.kind, SlotKind::Keyboard);
    }

    #[test]
    fn macos_sleep_keeps_semantics_above_its_execution_mapping() {
        let mut input = sample_input();
        apply_macos(&mut input, 4, MacOsControl::Sleep);
        assert_eq!(
            input.behavior,
            Some(ControlBehavior::MacOs {
                command: MacOsControl::Sleep
            })
        );
        assert!(matches!(input.action, Action::Run { .. }));
    }

    #[test]
    fn globe_or_input_language_uses_the_platform_mapping() {
        let mut input = sample_input();
        apply_macos(&mut input, 4, MacOsControl::Globe);
        let expected = if cfg!(target_os = "windows") {
            Slot {
                kind: SlotKind::Keyboard,
                mods: 0x08,
                code: 0x2C,
            }
        } else {
            Slot {
                kind: SlotKind::Consumer,
                mods: 0,
                code: 0x029D,
            }
        };
        assert_eq!(input.emitted, expected);
        assert_eq!(input.action, Action::None);
        assert_eq!(
            input.behavior,
            Some(ControlBehavior::MacOs {
                command: MacOsControl::Globe
            })
        );
        assert!(behavior_is_consistent(&input, 4));
    }

    #[test]
    fn host_triggers_avoid_the_macos_brightness_keys() {
        // F14/F15 reach the OS as brightness down/up on macOS even when the
        // hotkey layer grabs them, so no host-assisted binding may land there.
        for slot_index in 0..crate::config::SLOT_COUNT {
            let slot = host_trigger(slot_index);
            assert!(
                slot.code != 0x69 && slot.code != 0x6A,
                "slot {slot_index} allocated brightness key 0x{:02X}",
                slot.code
            );
            assert!(
                hidden_trigger(slot),
                "slot {slot_index} is not a valid trigger"
            );
        }
        assert!(hidden_trigger_candidates().all(|s| s.code != 0x69 && s.code != 0x6A));
        // A profile already sitting on F14/F15 must no longer count as valid,
        // so normalize_hidden_triggers re-homes it.
        assert!(!hidden_trigger(keyboard_slot(0x00, 0x69)));
        assert!(!hidden_trigger(keyboard_slot(0x02, 0x6A)));
    }

    #[test]
    fn host_triggers_are_distinct_across_every_slot() {
        let mut seen = HashSet::new();
        for slot_index in 0..crate::config::SLOT_COUNT {
            let slot = host_trigger(slot_index);
            assert!(
                seen.insert((slot.mods, slot.code)),
                "slot {slot_index} reuses another slot's trigger"
            );
        }
    }

    #[test]
    fn codex_mode_only_overrides_explicitly_customized_slots() {
        let mut profile = default_codex_profile();
        let defaults = profile.clone();
        profile.inputs[3].action = Action::Open {
            target: "https://example.com".into(),
        };
        profile.codex_overrides.push(3);

        let effective = with_codex_overrides(&profile);

        for slot in 0..effective.inputs.len() {
            if slot == 3 {
                assert_ne!(
                    effective.inputs[slot].emitted,
                    defaults.inputs[slot].emitted
                );
                assert_eq!(effective.inputs[slot].action, profile.inputs[slot].action);
            } else {
                assert_eq!(
                    effective.inputs[slot].emitted,
                    defaults.inputs[slot].emitted
                );
                assert_eq!(effective.inputs[slot].action, Action::None);
                assert_eq!(effective.inputs[slot].behavior, None);
            }
        }
    }

    #[test]
    fn an_f14_app_binding_is_rehomed_off_the_brightness_key() {
        let mut profile = default_codex_profile();
        apply_app(
            &mut profile.inputs[1],
            1,
            "/Applications/Conol.app".to_string(),
        );
        // A profile saved by an older build put slot 1 on plain F14.
        profile.inputs[1].emitted = keyboard_slot(0x00, 0x69);

        normalize_hidden_triggers(&mut profile);

        let moved = profile.inputs[1].emitted;
        assert_ne!(moved, keyboard_slot(0x00, 0x69));
        assert!(hidden_trigger(moved));
        assert_eq!(
            profile.inputs[1].action,
            Action::Open {
                target: "/Applications/Conol.app".to_string()
            }
        );
        assert!(behavior_is_consistent(&profile.inputs[1], 1));
    }

    #[test]
    fn direct_chord_collision_reallocates_only_the_hidden_trigger() {
        let mut profile = default_codex_profile();
        apply_app(
            &mut profile.inputs[0],
            0,
            "/Applications/Finder.app".to_string(),
        );
        let hidden_before = profile.inputs[0].emitted;
        apply_keystroke(
            &mut profile.inputs[1],
            hidden_before.mods,
            hidden_before.code,
        );
        let direct = profile.inputs[1].emitted;

        normalize_hidden_triggers(&mut profile);

        assert_eq!(profile.inputs[1].emitted, direct);
        assert_ne!(profile.inputs[0].emitted, direct);
        assert!(behavior_is_consistent(&profile.inputs[0], 0));
        assert!(behavior_is_consistent(&profile.inputs[1], 1));
    }
}
