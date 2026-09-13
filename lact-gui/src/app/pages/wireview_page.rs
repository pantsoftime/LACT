//! "WireView II" page: the Thermal Grizzly WireView Pro II power meter that
//! sits inline on the GPU's 12V-2x6 cable, read and configured through the
//! daemon (this fork). Laid out after the `wv2gui` tool: a live panel with
//! bars scaled to the configured limits, and the whole device configuration
//! in five tabs, written with the same backup / read-back / flash-verify
//! discipline.
//!
//! The page is listed in the sidebar only while a device is detected. It
//! polls the daemon once a second while it is being looked at; the daemon
//! releases the serial port a few seconds after the last request, so the
//! command-line tooling still works when this page is not visible.

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use lact_client::DaemonClient;
use lact_schema::{
    WireViewConfig, WireViewConfigState, WireViewInfo, WireViewReadings, WireViewStatus,
    WireViewWriteResult,
    wireview::{CONFIG_SIZE, CONFIG_VERSION, FAULT_BITS, SCREENS, decode_config, unhex},
};
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Duration,
};

const POLL_MS: u64 = 1000;
/// How often the page checks for a device while none is plugged in (in polls).
const DETECT_EVERY: u64 = 5;

const CSS: &str = "
levelbar.wireview block.filled.ok { background-color: #3fa34d; }
levelbar.wireview block.filled.warn { background-color: #e0a800; }
levelbar.wireview block.filled.bad { background-color: #d64541; }
levelbar.wireview.vertical trough { min-width: 22px; min-height: 130px; }
levelbar.wireview.horizontal trough { min-height: 10px; }
.wireview-edited { color: #d97706; font-weight: 600; }
.wireview-bad { color: #d64541; font-weight: 600; }
";

// ---------------------------------------------------------------- key table

#[derive(Clone, Copy)]
enum Kind {
    Int { lo: f64, hi: f64, unit: &'static str },
    /// Stored ×10 (0.1 °C, 0.1 A).
    Tenths { lo: f64, hi: f64, unit: &'static str },
    Choice(&'static [(&'static str, i64)]),
    Flags(&'static [&'static str]),
    Color,
    Text,
}

struct Key {
    key: &'static str,
    field: &'static str,
    label: &'static str,
    help: &'static str,
    kind: Kind,
}

const FAN_MODES: &[(&str, i64)] = &[("curve", 0), ("fixed", 1)];
const FAN_SOURCES: &[(&str, i64)] = &[("ts_in", 0), ("ts_out", 1), ("ext1", 2), ("ext2", 3), ("max", 4)];
const AVG: &[(&str, i64)] = &[
    ("22ms", 0),
    ("44ms", 1),
    ("89ms", 2),
    ("177ms", 3),
    ("354ms", 4),
    ("709ms", 5),
    ("1417ms", 6),
];
const SCREEN_CHOICES: &[(&str, i64)] = &[("main", 0), ("simple", 1), ("current", 2), ("temp", 3), ("status", 4)];
const TIMEOUT_MODES: &[(&str, i64)] = &[("static", 0), ("cycle", 1), ("sleep", 2)];
const CURRENT_SCALES: &[(&str, i64)] = &[("5A", 0), ("10A", 1), ("15A", 2), ("20A", 3)];
const POWER_SCALES: &[(&str, i64)] = &[("auto", 0), ("300W", 1), ("600W", 2)];
const ROTATIONS: &[(&str, i64)] = &[("0", 0), ("180", 1)];
const ON_OFF: &[(&str, i64)] = &[("off", 0), ("on", 1)];
const BACKGROUNDS: &[(&str, i64)] = &[("orange", 1), ("dark", 2), ("none", 255)];

const KEYS: &[Key] = &[
    Key { key: "name", field: "name", label: "Device name", help: "friendly device name (up to 31 ASCII characters)", kind: Kind::Text },
    Key { key: "backlight", field: "backlight", label: "Backlight", help: "display backlight", kind: Kind::Int { lo: 0.0, hi: 100.0, unit: "%" } },
    Key { key: "fan.mode", field: "fan_mode", label: "Mode", help: "curve: ramp duty_min..duty_max over temp_min..temp_max; fixed: duty_min", kind: Kind::Choice(FAN_MODES) },
    Key { key: "fan.source", field: "fan_source", label: "Temperature source", help: "temperature driving the fan curve", kind: Kind::Choice(FAN_SOURCES) },
    Key { key: "fan.duty_min", field: "fan_duty_min", label: "Minimum duty", help: "", kind: Kind::Int { lo: 0.0, hi: 100.0, unit: "%" } },
    Key { key: "fan.duty_max", field: "fan_duty_max", label: "Maximum duty", help: "", kind: Kind::Int { lo: 0.0, hi: 100.0, unit: "%" } },
    Key { key: "fan.temp_min", field: "fan_temp_min", label: "Curve start", help: "curve start", kind: Kind::Tenths { lo: 0.0, hi: 150.0, unit: "°C" } },
    Key { key: "fan.temp_max", field: "fan_temp_max", label: "Curve end", help: "curve end", kind: Kind::Tenths { lo: 0.0, hi: 150.0, unit: "°C" } },
    Key { key: "limit.temp", field: "ts_fault", label: "Over-temperature", help: "over-temperature (otp_ts) threshold", kind: Kind::Tenths { lo: 0.0, hi: 150.0, unit: "°C" } },
    Key { key: "limit.ocp", field: "ocp", label: "Over-current (total)", help: "total over-current threshold", kind: Kind::Int { lo: 0.0, hi: 255.0, unit: "A" } },
    Key { key: "limit.wire_ocp", field: "wire_ocp", label: "Over-current (per wire)", help: "per-wire over-current threshold", kind: Kind::Tenths { lo: 0.0, hi: 25.5, unit: "A" } },
    Key { key: "limit.opp", field: "opp", label: "Over-power", help: "over-power threshold", kind: Kind::Int { lo: 0.0, hi: 65535.0, unit: "W" } },
    Key { key: "limit.imbalance", field: "imbalance", label: "Current imbalance", help: "current-imbalance threshold", kind: Kind::Int { lo: 0.0, hi: 100.0, unit: "%" } },
    Key { key: "limit.imbalance_min_load", field: "imbalance_min_load", label: "Imbalance minimum load", help: "imbalance only checked above this total current", kind: Kind::Int { lo: 0.0, hi: 255.0, unit: "A" } },
    Key { key: "fault.display", field: "fault_display", label: "Display", help: "faults shown on screen", kind: Kind::Flags(&FAULT_BITS) },
    Key { key: "fault.buzzer", field: "fault_buzzer", label: "Buzzer", help: "faults that sound the buzzer", kind: Kind::Flags(&FAULT_BITS) },
    Key { key: "fault.soft_off", field: "fault_soft_off", label: "Soft Off", help: "faults that trigger Soft Off", kind: Kind::Flags(&FAULT_BITS) },
    Key { key: "fault.hard_off", field: "fault_hard_off", label: "Hard Off", help: "faults that trigger Hard Off", kind: Kind::Flags(&FAULT_BITS) },
    Key { key: "shutdown_wait", field: "shutdown_wait", label: "Shutdown wait", help: "delay before a power-off action", kind: Kind::Int { lo: 0.0, hi: 255.0, unit: "s" } },
    Key { key: "log_interval", field: "log_interval", label: "Logging interval", help: "on-device logging interval", kind: Kind::Int { lo: 0.0, hi: 255.0, unit: "s" } },
    Key { key: "avg", field: "avg", label: "Averaging", help: "sensor averaging window", kind: Kind::Choice(AVG) },
    Key { key: "ui.default_screen", field: "default_screen", label: "Default screen", help: "", kind: Kind::Choice(SCREEN_CHOICES) },
    Key { key: "ui.cycle_screens", field: "cycle_screens", label: "Cycle screens", help: "screens shown in cycle mode", kind: Kind::Flags(&SCREENS) },
    Key { key: "ui.cycle_time", field: "cycle_time", label: "Cycle time", help: "", kind: Kind::Int { lo: 1.0, hi: 60.0, unit: "s" } },
    Key { key: "ui.timeout_mode", field: "timeout_mode", label: "After timeout", help: "what happens after the timeout", kind: Kind::Choice(TIMEOUT_MODES) },
    Key { key: "ui.timeout", field: "timeout", label: "Timeout", help: "", kind: Kind::Int { lo: 0.0, hi: 255.0, unit: "s" } },
    Key { key: "ui.current_scale", field: "current_scale", label: "Pin current scale", help: "per-pin current bar scale on the device display", kind: Kind::Choice(CURRENT_SCALES) },
    Key { key: "ui.power_scale", field: "power_scale", label: "Power scale", help: "", kind: Kind::Choice(POWER_SCALES) },
    Key { key: "ui.rotation", field: "rotation", label: "Rotation", help: "", kind: Kind::Choice(ROTATIONS) },
    Key { key: "ui.invert", field: "invert", label: "Invert colours", help: "invert display colours", kind: Kind::Choice(ON_OFF) },
    Key { key: "ui.background", field: "background", label: "Background", help: "background bitmap (sets the fan icon to match)", kind: Kind::Choice(BACKGROUNDS) },
    Key { key: "ui.color.primary", field: "color_primary", label: "Primary colour", help: "", kind: Kind::Color },
    Key { key: "ui.color.secondary", field: "color_secondary", label: "Secondary colour", help: "", kind: Kind::Color },
    Key { key: "ui.color.highlight", field: "color_highlight", label: "Highlight colour", help: "", kind: Kind::Color },
    Key { key: "ui.color.background", field: "color_background", label: "Background colour", help: "", kind: Kind::Color },
];

const FAULT_LABELS: [&str; 6] = [
    "Chip over-temperature",
    "Over-temperature",
    "Over-current (total)",
    "Over-current (per wire)",
    "Over-power",
    "Current imbalance",
];
const FAULT_SHORT: [&str; 6] = ["chip temp", "temp", "OCP", "wire OCP", "OPP", "imbalance"];
const FAULT_ACTION_KEYS: [&str; 4] = ["fault.display", "fault.buzzer", "fault.soft_off", "fault.hard_off"];

const TABS: &[(&str, &[&str])] = &[
    (
        "Protection",
        &[
            "limit.ocp",
            "limit.wire_ocp",
            "limit.opp",
            "limit.temp",
            "limit.imbalance",
            "limit.imbalance_min_load",
            "shutdown_wait",
        ],
    ),
    (
        "Fan",
        &["fan.mode", "fan.source", "fan.duty_min", "fan.duty_max", "fan.temp_min", "fan.temp_max"],
    ),
    (
        "Display",
        &[
            "backlight",
            "ui.default_screen",
            "ui.cycle_screens",
            "ui.timeout_mode",
            "ui.timeout",
            "ui.cycle_time",
            "ui.current_scale",
            "ui.power_scale",
            "ui.rotation",
        ],
    ),
    (
        "Theme",
        &[
            "ui.background",
            "ui.invert",
            "ui.color.primary",
            "ui.color.secondary",
            "ui.color.highlight",
            "ui.color.background",
        ],
    ),
    ("Device", &["name", "avg", "log_interval"]),
];

fn key(name: &str) -> &'static Key {
    KEYS.iter().find(|k| k.key == name).expect("known key")
}

/// Numeric field access by name; the name field is handled separately.
fn get(c: &WireViewConfig, field: &str) -> i64 {
    match field {
        "fan_mode" => c.fan_mode.into(),
        "fan_source" => c.fan_source.into(),
        "fan_duty_min" => c.fan_duty_min.into(),
        "fan_duty_max" => c.fan_duty_max.into(),
        "fan_temp_min" => c.fan_temp_min.into(),
        "fan_temp_max" => c.fan_temp_max.into(),
        "backlight" => c.backlight.into(),
        "fault_display" => c.fault_display.into(),
        "fault_buzzer" => c.fault_buzzer.into(),
        "fault_soft_off" => c.fault_soft_off.into(),
        "fault_hard_off" => c.fault_hard_off.into(),
        "ts_fault" => c.ts_fault.into(),
        "ocp" => c.ocp.into(),
        "wire_ocp" => c.wire_ocp.into(),
        "opp" => c.opp.into(),
        "imbalance" => c.imbalance.into(),
        "imbalance_min_load" => c.imbalance_min_load.into(),
        "shutdown_wait" => c.shutdown_wait.into(),
        "log_interval" => c.log_interval.into(),
        "avg" => c.avg.into(),
        "default_screen" => c.default_screen.into(),
        "current_scale" => c.current_scale.into(),
        "power_scale" => c.power_scale.into(),
        "rotation" => c.rotation.into(),
        "timeout_mode" => c.timeout_mode.into(),
        "cycle_screens" => c.cycle_screens.into(),
        "cycle_time" => c.cycle_time.into(),
        "timeout" => c.timeout.into(),
        "color_primary" => c.color_primary.into(),
        "color_secondary" => c.color_secondary.into(),
        "color_highlight" => c.color_highlight.into(),
        "color_background" => c.color_background.into(),
        "background" => c.background.into(),
        "fan_bitmap" => c.fan_bitmap.into(),
        "invert" => c.invert.into(),
        _ => 0,
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn set(c: &mut WireViewConfig, field: &str, v: i64) {
    match field {
        "fan_mode" => c.fan_mode = v as u8,
        "fan_source" => c.fan_source = v as u8,
        "fan_duty_min" => c.fan_duty_min = v as u8,
        "fan_duty_max" => c.fan_duty_max = v as u8,
        "fan_temp_min" => c.fan_temp_min = v as i16,
        "fan_temp_max" => c.fan_temp_max = v as i16,
        "backlight" => c.backlight = v as u8,
        "fault_display" => c.fault_display = v as u16,
        "fault_buzzer" => c.fault_buzzer = v as u16,
        "fault_soft_off" => c.fault_soft_off = v as u16,
        "fault_hard_off" => c.fault_hard_off = v as u16,
        "ts_fault" => c.ts_fault = v as i16,
        "ocp" => c.ocp = v as u8,
        "wire_ocp" => c.wire_ocp = v as u8,
        "opp" => c.opp = v as u16,
        "imbalance" => c.imbalance = v as u8,
        "imbalance_min_load" => c.imbalance_min_load = v as u8,
        "shutdown_wait" => c.shutdown_wait = v as u8,
        "log_interval" => c.log_interval = v as u8,
        "avg" => c.avg = v as u8,
        "default_screen" => c.default_screen = v as u8,
        "current_scale" => c.current_scale = v as u8,
        "power_scale" => c.power_scale = v as u8,
        "rotation" => c.rotation = v as u8,
        "timeout_mode" => c.timeout_mode = v as u8,
        "cycle_screens" => c.cycle_screens = v as u8,
        "cycle_time" => c.cycle_time = v as u8,
        "timeout" => c.timeout = v as u8,
        "color_primary" => c.color_primary = v as u32,
        "color_secondary" => c.color_secondary = v as u32,
        "color_highlight" => c.color_highlight = v as u32,
        "color_background" => c.color_background = v as u32,
        "background" => c.background = v as u8,
        "fan_bitmap" => c.fan_bitmap = v as u8,
        "invert" => c.invert = v as u8,
        _ => {}
    }
}

/// A value as the CLI prints it.
#[allow(clippy::cast_precision_loss)]
fn show(k: &Key, c: &WireViewConfig) -> String {
    let raw = get(c, k.field);
    match k.kind {
        Kind::Text => format!("{:?}", c.name),
        Kind::Int { unit, .. } => format!("{raw} {unit}"),
        Kind::Tenths { unit, .. } => format!("{:.1} {unit}", raw as f64 / 10.0),
        Kind::Choice(names) => names
            .iter()
            .find(|(_, v)| *v == raw)
            .map_or_else(|| format!("unknown ({raw})"), |(n, _)| (*n).to_owned()),
        Kind::Flags(bits) => flag_names(raw, bits),
        Kind::Color => format!("#{:06x}", raw & 0xFF_FFFF),
    }
}

fn flag_names(raw: i64, bits: &[&str]) -> String {
    let names: Vec<&str> = bits
        .iter()
        .enumerate()
        .filter(|(i, _)| raw & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(",")
    }
}

fn fault_summary(mask: u16) -> String {
    let names: Vec<&str> = FAULT_SHORT
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

fn changed(k: &Key, old: &WireViewConfig, new: &WireViewConfig) -> bool {
    if matches!(k.kind, Kind::Text) {
        old.name != new.name
    } else {
        get(old, k.field) != get(new, k.field)
    }
}

fn fan_bitmap_for(background: u8) -> u8 {
    match background {
        1 => 0x64,
        2 => 0x75,
        _ => 0x98,
    }
}

// ---------------------------------------------------------------- editors

enum Editor {
    Spin { spin: gtk::SpinButton, scale: f64 },
    Drop { drop: gtk::DropDown, values: Rc<RefCell<Vec<i64>>> },
    Flags { checks: Vec<gtk::CheckButton>, extra: Cell<i64> },
    Color(gtk::ColorDialogButton),
    Text(gtk::Entry),
}

impl Editor {
    #[allow(clippy::cast_possible_truncation)]
    fn get(&self) -> i64 {
        match self {
            Editor::Spin { spin, scale } => (spin.value() * scale).round() as i64,
            Editor::Drop { drop, values } => {
                let values = values.borrow();
                values.get(drop.selected() as usize).copied().unwrap_or(0)
            }
            Editor::Flags { checks, extra } => {
                extra.get()
                    | checks
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.is_active())
                        .map(|(i, _)| 1i64 << i)
                        .sum::<i64>()
            }
            Editor::Color(button) => {
                let c = button.rgba();
                let byte = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as i64;
                0xFF00_0000 | (byte(c.red()) << 16) | (byte(c.green()) << 8) | byte(c.blue())
            }
            Editor::Text(_) => 0,
        }
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn set(&self, raw: i64) {
        match self {
            Editor::Spin { spin, scale } => {
                let v = raw as f64 / scale;
                let adj = spin.adjustment();
                adj.set_lower(adj.lower().min(v));
                adj.set_upper(adj.upper().max(v));
                spin.set_value(v);
            }
            Editor::Drop { drop, values } => {
                let index = values.borrow().iter().position(|v| *v == raw);
                let index = match index {
                    Some(i) => i,
                    None => {
                        if let Some(list) = drop.model().and_downcast::<gtk::StringList>() {
                            list.append(&format!("unknown ({raw})"));
                        }
                        values.borrow_mut().push(raw);
                        values.borrow().len() - 1
                    }
                };
                drop.set_selected(index as u32);
            }
            Editor::Flags { checks, extra } => {
                extra.set(raw & !((1i64 << checks.len()) - 1));
                for (i, check) in checks.iter().enumerate() {
                    check.set_active(raw & (1 << i) != 0);
                }
            }
            Editor::Color(button) => {
                let f = |shift: i64| ((raw >> shift) & 0xFF) as f32 / 255.0;
                button.set_rgba(&gdk::RGBA::new(f(16), f(8), f(0), 1.0));
            }
            Editor::Text(_) => {}
        }
    }
}

// ---------------------------------------------------------------- the page

#[derive(Debug)]
pub enum WireViewPageMsg {
    Tick,
    Info(Result<Option<WireViewInfo>, String>),
    Status(Result<WireViewStatus, String>),
    Edited,
    Reload,
    Discard,
    Commit { live: bool },
    Write { config: Box<WireViewConfig>, live: bool },
    WriteDone { result: Result<WireViewWriteResult, String>, live: bool },
    ClearFaults,
    Done(Result<String, String>),
    Backup,
    Restore,
    RestoreLoaded(Result<(String, WireViewConfig), String>),
}

pub struct WireViewPage {
    root: gtk::ScrolledWindow,
    client: DaemonClient,
    content: gtk::Box,

    info_label: gtk::Label,
    status_label: gtk::Label,
    headline: gtk::Label,
    power_bar: gtk::LevelBar,
    current_bar: gtk::LevelBar,
    power_text: gtk::Label,
    current_text: gtk::Label,
    pin_caption: gtk::Label,
    pin_bars: Vec<gtk::LevelBar>,
    pin_texts: Vec<gtk::Label>,
    details: gtk::Label,
    faults: gtk::Label,
    clear_button: gtk::Button,
    tabs: gtk::Notebook,
    editors: HashMap<&'static str, Editor>,
    labels: HashMap<&'static str, gtk::Label>,
    changes_label: gtk::Label,
    backup_button: gtk::Button,
    restore_button: gtk::Button,
    reload_button: gtk::Button,
    discard_button: gtk::Button,
    apply_button: gtk::Button,
    save_button: gtk::Button,

    info: Option<WireViewInfo>,
    loaded: Option<WireViewConfigState>,
    detected: bool,
    busy: bool,
    in_flight: bool,
    accept_next_config: bool,
    filling: Rc<Cell<bool>>,
    ticks: u64,
    last_error: Option<String>,
}

fn install_css() {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.with(Cell::get) {
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        INSTALLED.with(|i| i.set(true));
    }
}

fn level_bar(vertical: bool) -> gtk::LevelBar {
    let bar = gtk::LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .value(0.0)
        .css_classes(["wireview"])
        .build();
    for name in [gtk::LEVEL_BAR_OFFSET_LOW, gtk::LEVEL_BAR_OFFSET_HIGH, gtk::LEVEL_BAR_OFFSET_FULL] {
        bar.remove_offset_value(Some(name));
    }
    // GTK applies the class of the smallest offset that is >= the value:
    // green below 80 % of the limit, amber to 95 %, red above.
    bar.add_offset_value("ok", 0.8);
    bar.add_offset_value("warn", 0.95);
    bar.add_offset_value("bad", 1.0);
    if vertical {
        bar.set_orientation(gtk::Orientation::Vertical);
        bar.set_inverted(true);
        bar.set_valign(gtk::Align::End);
    } else {
        bar.set_hexpand(true);
        bar.set_valign(gtk::Align::Center);
    }
    bar
}

fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption", "dim-label"])
        .build()
}

#[relm4::component(pub)]
impl relm4::Component for WireViewPage {
    type Init = DaemonClient;
    type Input = WireViewPageMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,
            set_vexpand: true,

            model.content.clone() {},
        }
    }

    fn init(client: Self::Init, root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        install_css();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_all(15);
        content.set_margin_top(20);

        let info_label = gtk::Label::builder()
            .label("Looking for a WireView Pro II…")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        content.append(&info_label);

        // ---- live panel
        let live_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        live_box.set_margin_all(10);
        let headline = gtk::Label::builder()
            .label("— W")
            .xalign(0.0)
            .css_classes(["title-1", "numeric"])
            .build();
        live_box.append(&headline);
        let totals = gtk::Grid::builder().column_spacing(8).row_spacing(6).build();
        let power_bar = level_bar(false);
        let current_bar = level_bar(false);
        let power_text = gtk::Label::builder().label("—").xalign(0.0).width_chars(14).css_classes(["numeric"]).build();
        let current_text = gtk::Label::builder().label("—").xalign(0.0).width_chars(14).css_classes(["numeric"]).build();
        for (row, (name, bar, text)) in [("Power", &power_bar, &power_text), ("Current", &current_bar, &current_text)]
            .into_iter()
            .enumerate()
        {
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let row = row as i32;
            totals.attach(&gtk::Label::builder().label(name).xalign(0.0).build(), 0, row, 1, 1);
            totals.attach(bar, 1, row, 1, 1);
            totals.attach(text, 2, row, 1, 1);
        }
        live_box.append(&totals);

        let pin_caption = caption("Per-pin current");
        pin_caption.set_margin_top(8);
        live_box.append(&pin_caption);
        let pins = gtk::Grid::builder().column_spacing(10).row_spacing(2).column_homogeneous(true).build();
        let mut pin_bars = Vec::new();
        let mut pin_texts = Vec::new();
        for i in 0..6i32 {
            let bar = level_bar(true);
            let text = gtk::Label::builder().label("—").justify(gtk::Justification::Center).css_classes(["numeric", "caption"]).build();
            let name = gtk::Label::builder().label(format!("Pin {}", i + 1)).css_classes(["caption", "dim-label"]).build();
            pins.attach(&bar, i, 0, 1, 1);
            pins.attach(&text, i, 1, 1, 1);
            pins.attach(&name, i, 2, 1, 1);
            bar.set_halign(gtk::Align::Center);
            pin_bars.push(bar);
            pin_texts.push(text);
        }
        live_box.append(&pins);

        let details = caption("");
        details.set_margin_top(8);
        live_box.append(&details);
        let faults_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let faults = gtk::Label::builder().label("").xalign(0.0).wrap(true).hexpand(true).css_classes(["caption"]).build();
        let clear_button = gtk::Button::builder()
            .label("Clear faults")
            .valign(gtk::Align::End)
            .tooltip_text("Clears the active fault status and the fault log on the device")
            .build();
        faults_row.append(&faults);
        faults_row.append(&clear_button);
        live_box.append(&faults_row);
        let live_frame = gtk::Frame::builder().label("Live").child(&live_box).build();
        live_frame.set_size_request(440, -1);

        // ---- settings tabs
        let tabs = gtk::Notebook::new();
        let mut editors: HashMap<&'static str, Editor> = HashMap::new();
        let mut labels: HashMap<&'static str, gtk::Label> = HashMap::new();
        let filling = Rc::new(Cell::new(false));
        let edited = {
            let (sender, filling) = (sender.clone(), filling.clone());
            move || {
                if !filling.get() {
                    sender.input(WireViewPageMsg::Edited);
                }
            }
        };
        for (title, keys) in TABS {
            let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
            page.set_margin_all(10);
            let form = gtk::Grid::builder().column_spacing(12).row_spacing(6).build();
            for (row, name) in keys.iter().enumerate() {
                let k = key(name);
                let label = gtk::Label::builder().label(k.label).xalign(0.0).tooltip_text(k.help).build();
                let (widget, editor) = make_editor(k, &edited);
                widget.set_tooltip_text(Some(k.help));
                #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                let row = row as i32;
                form.attach(&label, 0, row, 1, 1);
                form.attach(&widget, 1, row, 1, 1);
                labels.insert(k.key, label);
                editors.insert(k.key, editor);
            }
            page.append(&form);
            if *title == "Protection" {
                // Fault-action matrix: one check box per fault × action.
                let grid = gtk::Grid::builder().column_spacing(14).row_spacing(4).build();
                let mut columns: Vec<Vec<gtk::CheckButton>> = vec![Vec::new(); FAULT_ACTION_KEYS.len()];
                for (col, name) in FAULT_ACTION_KEYS.iter().enumerate() {
                    let k = key(name);
                    let header = gtk::Label::builder().label(k.label).tooltip_text(k.help).build();
                    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                    grid.attach(&header, col as i32 + 1, 0, 1, 1);
                    labels.insert(k.key, header);
                }
                for (row, fault) in FAULT_LABELS.iter().enumerate() {
                    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                    let row = row as i32 + 1;
                    grid.attach(&gtk::Label::builder().label(*fault).xalign(0.0).hexpand(true).build(), 0, row, 1, 1);
                    for (col, checks) in columns.iter_mut().enumerate() {
                        let check = gtk::CheckButton::builder().halign(gtk::Align::Center).build();
                        let edited = edited.clone();
                        check.connect_toggled(move |_| edited());
                        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                        grid.attach(&check, col as i32 + 1, row, 1, 1);
                        checks.push(check);
                    }
                }
                for (name, checks) in FAULT_ACTION_KEYS.iter().zip(columns) {
                    editors.insert(key(name).key, Editor::Flags { checks, extra: Cell::new(0) });
                }
                let frame = gtk::Frame::builder().label("Fault actions").child(&grid).build();
                grid.set_margin_all(8);
                page.append(&frame);
            }
            tabs.append_page(&page, Some(&gtk::Label::new(Some(title))));
        }
        tabs.set_sensitive(false);
        tabs.set_hexpand(true);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        body.append(&live_frame);
        body.append(&tabs);
        content.append(&body);

        // ---- buttons
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let backup_button = gtk::Button::builder().label("Backup…").tooltip_text("Write the loaded configuration to a JSON file").build();
        let restore_button = gtk::Button::builder().label("Restore…").tooltip_text("Load a configuration backup into the editor; review, then Apply or Save").build();
        let changes_label = gtk::Label::builder().label("").css_classes(["dim-label"]).build();
        let reload_button = gtk::Button::builder().label("Reload").tooltip_text("Read the configuration from the device again").build();
        let discard_button = gtk::Button::builder().label("Discard edits").build();
        let apply_button = gtk::Button::builder().label("Apply live").tooltip_text("Write to the device without saving; lost on power loss").build();
        let save_button = gtk::Button::builder()
            .label("Save to flash")
            .tooltip_text("Write to the device, save to flash, and verify both")
            .css_classes(["suggested-action"])
            .build();
        buttons.append(&backup_button);
        buttons.append(&restore_button);
        changes_label.set_hexpand(true);
        buttons.append(&changes_label);
        for b in [&reload_button, &discard_button, &apply_button, &save_button] {
            buttons.append(b);
        }
        content.append(&buttons);
        let status_label = gtk::Label::builder().label("").xalign(0.0).wrap(true).selectable(true).css_classes(["caption"]).build();
        content.append(&status_label);

        let connect = |button: &gtk::Button, make: fn() -> WireViewPageMsg| {
            let sender = sender.clone();
            button.connect_clicked(move |_| sender.input(make()));
        };
        connect(&backup_button, || WireViewPageMsg::Backup);
        connect(&restore_button, || WireViewPageMsg::Restore);
        connect(&reload_button, || WireViewPageMsg::Reload);
        connect(&discard_button, || WireViewPageMsg::Discard);
        connect(&clear_button, || WireViewPageMsg::ClearFaults);
        connect(&apply_button, || WireViewPageMsg::Commit { live: true });
        connect(&save_button, || WireViewPageMsg::Commit { live: false });

        {
            let sender = sender.clone();
            glib::timeout_add_local(Duration::from_millis(POLL_MS), move || {
                sender.input(WireViewPageMsg::Tick);
                glib::ControlFlow::Continue
            });
        }

        let model = Self {
            root: root.clone(),
            client,
            content,
            info_label,
            status_label,
            headline,
            power_bar,
            current_bar,
            power_text,
            current_text,
            pin_caption,
            pin_bars,
            pin_texts,
            details,
            faults,
            clear_button,
            tabs,
            editors,
            labels,
            changes_label,
            backup_button,
            restore_button,
            reload_button,
            discard_button,
            apply_button,
            save_button,
            info: None,
            loaded: None,
            detected: false,
            busy: false,
            in_flight: false,
            accept_next_config: false,
            filling,
            ticks: 0,
            last_error: None,
        };
        model.update_buttons();
        // The sidebar entry is hidden as soon as the first poll says there is
        // no device (not before: a saved "selected tab" of this page must
        // still be honoured when the device is present).
        sender.input(WireViewPageMsg::Tick);

        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            WireViewPageMsg::Tick => self.tick(&sender),
            WireViewPageMsg::Info(result) => {
                self.in_flight = false;
                match result {
                    Ok(Some(info)) => {
                        let first = !self.detected;
                        self.detected = true;
                        // The by-id path carries the USB serial; show the
                        // plain tty on the page and keep the full path in the tooltip.
                        let tty = std::fs::canonicalize(&info.port)
                            .map_or_else(|_| info.port.clone(), |p| p.display().to_string());
                        self.info_label.set_label(&format!(
                            "{}   ·   firmware v{} ({})   ·   {tty}",
                            info.product, info.firmware, info.build
                        ));
                        self.info_label.set_tooltip_text(Some(&info.port));
                        self.info = Some(info);
                        set_page_visible(&self.root, true);
                        if first {
                            self.status("", false);
                            self.request_status(&sender);
                        }
                    }
                    Ok(None) => self.disconnected("No WireView Pro II detected (STM32 virtual COM port under /dev/serial/by-id)"),
                    Err(err) => self.status(&err, true),
                }
            }
            WireViewPageMsg::Status(result) => {
                self.in_flight = false;
                match result {
                    Ok(status) => {
                        self.last_error = None;
                        self.on_config(status.state, &sender);
                        self.show_readings(&status.readings);
                    }
                    Err(err) => {
                        if err.contains("no WireView Pro II found") {
                            self.disconnected("WireView Pro II unplugged; waiting for it to come back");
                        } else if self.last_error.as_deref() != Some(&err) {
                            self.status(&err, true);
                        }
                        self.last_error = Some(err);
                    }
                }
            }
            WireViewPageMsg::Edited => self.on_edit(),
            WireViewPageMsg::Reload => {
                if self.pending_changes().is_empty() {
                    self.accept_next_config = true;
                    self.request_status(&sender);
                } else {
                    self.confirm(
                        "Reload",
                        "Reload from the device and discard your edits?",
                        "Reload",
                        false,
                        move |sender| {
                            sender.input(WireViewPageMsg::Reload);
                        },
                        sender.clone(),
                        true,
                    );
                }
            }
            WireViewPageMsg::Discard => {
                if let Some(state) = self.loaded.clone() {
                    self.fill(&state.config);
                    self.on_edit();
                }
            }
            WireViewPageMsg::Commit { live } => {
                let Some(loaded) = self.loaded.clone() else { return };
                let config = self.collect();
                let changes = self.diff(&loaded.config, &config);
                if changes.is_empty() {
                    return;
                }
                let lines: Vec<String> = changes
                    .iter()
                    .map(|(label, before, after)| format!("{label}:  {before}  →  {after}"))
                    .collect();
                let question = if live {
                    "Write these changes to the device without saving them to flash?"
                } else {
                    "Write these changes to the device and save them to flash?"
                };
                let body = format!("{question}\n\n{}", lines.join("\n"));
                let label = if live { "Apply live" } else { "Save to flash" };
                self.confirm(
                    "Confirm changes",
                    &body,
                    label,
                    !live,
                    move |sender| {
                        sender.input(WireViewPageMsg::Write {
                            config: Box::new(config),
                            live,
                        });
                    },
                    sender.clone(),
                    false,
                );
            }
            WireViewPageMsg::Write { config, live } => {
                let Some(loaded) = self.loaded.clone() else { return };
                self.busy = true;
                self.update_buttons();
                self.status("writing…", false);
                let client = self.client.clone();
                let sender = sender.clone();
                relm4::spawn_local(async move {
                    let result = client
                        .wireview_set_config(*config, Some(loaded.raw), live)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    sender.input(WireViewPageMsg::WriteDone { result, live });
                });
            }
            WireViewPageMsg::WriteDone { result, live } => {
                self.busy = false;
                match result {
                    Ok(res) => {
                        let backup = res
                            .backup
                            .as_deref()
                            .map_or(String::new(), |b| format!("   (previous config backed up to {b})"));
                        self.status(
                            &format!(
                                "{}{backup}",
                                if live { "applied live (not saved to flash)" } else { "saved to flash and verified" }
                            ),
                            false,
                        );
                        self.load_state(res.state);
                    }
                    Err(err) => {
                        self.status(&err, true);
                        self.alert("WireView Pro II", &err);
                    }
                }
                self.on_edit();
            }
            WireViewPageMsg::ClearFaults => {
                let client = self.client.clone();
                let sender = sender.clone();
                relm4::spawn_local(async move {
                    let result = client
                        .wireview_clear_faults()
                        .await
                        .map(|()| "fault status and fault log cleared".to_owned())
                        .map_err(|e| format!("{e:#}"));
                    sender.input(WireViewPageMsg::Done(result));
                });
            }
            WireViewPageMsg::Done(result) => match result {
                Ok(text) => self.status(&text, false),
                Err(err) => {
                    self.status(&err, true);
                    self.alert("WireView Pro II", &err);
                }
            },
            WireViewPageMsg::Backup => self.backup(&sender),
            WireViewPageMsg::Restore => self.restore(&sender),
            WireViewPageMsg::RestoreLoaded(result) => match result {
                Ok((name, config)) => {
                    self.fill(&config);
                    self.on_edit();
                    self.status(&format!("loaded {name} into the editor; review, then Apply or Save"), false);
                }
                Err(err) => self.alert("Restore", &err),
            },
        }
    }
}

fn set_page_visible(root: &gtk::ScrolledWindow, visible: bool) {
    if let Some(stack) = root.parent().and_downcast::<gtk::Stack>() {
        stack.page(root).set_visible(visible);
    }
}

fn make_editor(k: &Key, edited: &(impl Fn() + Clone + 'static)) -> (gtk::Widget, Editor) {
    match k.kind {
        Kind::Int { lo, hi, unit } | Kind::Tenths { lo, hi, unit } => {
            let tenths = matches!(k.kind, Kind::Tenths { .. });
            let step = if !tenths {
                1.0
            } else if unit.contains('C') {
                0.5
            } else {
                0.1
            };
            let adjustment = gtk::Adjustment::new(lo, lo, hi, step, step * 10.0, 0.0);
            let spin = gtk::SpinButton::builder()
                .adjustment(&adjustment)
                .digits(u32::from(tenths))
                .width_chars(7)
                .build();
            let edited = edited.clone();
            spin.connect_value_changed(move |_| edited());
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.append(&spin);
            row.append(&gtk::Label::new(Some(unit)));
            (
                row.upcast(),
                Editor::Spin {
                    spin,
                    scale: if tenths { 10.0 } else { 1.0 },
                },
            )
        }
        Kind::Choice(names) => {
            let list: Vec<&str> = names.iter().map(|(n, _)| *n).collect();
            let drop = gtk::DropDown::from_strings(&list);
            let edited = edited.clone();
            drop.connect_selected_notify(move |_| edited());
            let values = Rc::new(RefCell::new(names.iter().map(|(_, v)| *v).collect()));
            (drop.clone().upcast(), Editor::Drop { drop, values })
        }
        Kind::Flags(bits) => {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let mut checks = Vec::new();
            for bit in bits {
                let check = gtk::CheckButton::with_label(bit);
                let edited = edited.clone();
                check.connect_toggled(move |_| edited());
                row.append(&check);
                checks.push(check);
            }
            (row.upcast(), Editor::Flags { checks, extra: Cell::new(0) })
        }
        Kind::Color => {
            let button = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::new()));
            let edited = edited.clone();
            button.connect_rgba_notify(move |_| edited());
            (button.clone().upcast(), Editor::Color(button))
        }
        Kind::Text => {
            let entry = gtk::Entry::builder().max_length(31).width_chars(24).build();
            let edited = edited.clone();
            entry.connect_changed(move |_| edited());
            (entry.clone().upcast(), Editor::Text(entry))
        }
    }
}

impl WireViewPage {
    fn tick(&mut self, sender: &ComponentSender<Self>) {
        self.ticks += 1;
        if self.in_flight || self.busy {
            return;
        }
        let mapped = self.root.is_mapped();
        if !self.detected {
            if self.ticks % DETECT_EVERY == 1 {
                self.request_info(sender);
            }
        } else if mapped {
            self.request_status(sender);
        } else if self.ticks.is_multiple_of(2 * DETECT_EVERY) {
            self.request_info(sender);
        }
    }

    fn request_info(&mut self, sender: &ComponentSender<Self>) {
        self.in_flight = true;
        let client = self.client.clone();
        let sender = sender.clone();
        relm4::spawn_local(async move {
            let result = client.wireview_info().await.map_err(|e| format!("{e:#}"));
            sender.input(WireViewPageMsg::Info(result));
        });
    }

    fn request_status(&mut self, sender: &ComponentSender<Self>) {
        self.in_flight = true;
        let client = self.client.clone();
        let sender = sender.clone();
        relm4::spawn_local(async move {
            let result = client.wireview_status().await.map_err(|e| format!("{e:#}"));
            sender.input(WireViewPageMsg::Status(result));
        });
    }

    fn disconnected(&mut self, text: &str) {
        self.detected = false;
        self.info = None;
        self.loaded = None;
        self.info_label.set_label(text);
        self.tabs.set_sensitive(false);
        self.clear_live();
        self.update_buttons();
        set_page_visible(&self.root, false);
    }

    fn status(&self, text: &str, error: bool) {
        self.status_label.set_label(text);
        if error {
            self.status_label.add_css_class("wireview-bad");
        } else {
            self.status_label.remove_css_class("wireview-bad");
        }
    }

    fn alert(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("ok", "OK");
        dialog.present(Some(&self.root));
    }

    /// Ask, then run `then` on the accepting response.
    #[allow(clippy::too_many_arguments)]
    fn confirm(
        &self,
        heading: &str,
        body: &str,
        accept_label: &str,
        suggested: bool,
        then: impl FnOnce(&ComponentSender<Self>) + 'static,
        sender: ComponentSender<Self>,
        destructive: bool,
    ) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", "Cancel"), ("accept", accept_label)]);
        dialog.set_response_appearance(
            "accept",
            if destructive {
                adw::ResponseAppearance::Destructive
            } else if suggested {
                adw::ResponseAppearance::Suggested
            } else {
                adw::ResponseAppearance::Default
            },
        );
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let then = RefCell::new(Some(then));
        dialog.choose(Some(&self.root), gio::Cancellable::NONE, move |response| {
            if response == "accept"
                && let Some(then) = then.borrow_mut().take()
            {
                then(&sender);
            }
        });
    }

    // ---- readings

    fn limits(&self) -> (f64, f64, f64) {
        self.loaded.as_ref().map_or((0.0, 0.0, 0.0), |s| {
            (
                f64::from(s.config.opp),
                f64::from(s.config.ocp),
                f64::from(s.config.wire_ocp) / 10.0,
            )
        })
    }

    fn show_readings(&self, r: &WireViewReadings) {
        let (opp, ocp, wire) = self.limits();
        let fraction = |value: f64, limit: f64| if limit > 0.0 { (value / limit).min(1.0) } else { 0.0 };
        self.headline.set_label(&format!("{:.0} W", r.power));
        self.power_bar.set_value(fraction(f64::from(r.power), opp));
        self.current_bar.set_value(fraction(f64::from(r.current), ocp));
        self.power_text.set_label(&if opp > 0.0 {
            format!("{:.1} / {opp:.0} W", r.power)
        } else {
            format!("{:.1} W", r.power)
        });
        self.current_text.set_label(&if ocp > 0.0 {
            format!("{:.2} / {ocp:.0} A", r.current)
        } else {
            format!("{:.2} A", r.current)
        });
        if wire > 0.0 {
            self.pin_caption
                .set_label(&format!("Per-pin current  (bars scale to the {wire:.1} A per-wire limit)"));
        }
        for (i, (bar, text)) in self.pin_bars.iter().zip(&self.pin_texts).enumerate() {
            bar.set_value(fraction(f64::from(r.pin_a[i]), wire));
            text.set_label(&format!("{:.2} A\n{:.2} V", r.pin_a[i], r.pin_v[i]));
        }
        let temps: Vec<String> = ["In", "Out", "Ext 1", "Ext 2"]
            .iter()
            .zip(&r.temps)
            .map(|(name, t)| format!("{name} {}", t.map_or("—".to_owned(), |t| format!("{t:.1} °C"))))
            .collect();
        self.details.set_label(&format!(
            "{}\nFan {}%    Average {:.2} V    PSU {}",
            temps.join("    "),
            r.fan,
            r.avg_v,
            r.psu_cap
        ));
        self.faults.set_label(&format!(
            "Active faults: {}\nLogged faults: {}",
            fault_summary(r.fault_status),
            fault_summary(r.fault_log)
        ));
        if r.fault_status != 0 {
            self.faults.add_css_class("wireview-bad");
        } else {
            self.faults.remove_css_class("wireview-bad");
        }
    }

    fn clear_live(&self) {
        self.headline.set_label("— W");
        for bar in self.pin_bars.iter().chain([&self.power_bar, &self.current_bar]) {
            bar.set_value(0.0);
        }
        for label in self.pin_texts.iter().chain([&self.power_text, &self.current_text]) {
            label.set_label("—");
        }
        self.details.set_label("");
        self.faults.set_label("");
    }

    // ---- config editing

    fn on_config(&mut self, state: WireViewConfigState, _sender: &ComponentSender<Self>) {
        let Some(loaded) = &self.loaded else {
            self.load_state(state);
            self.on_edit();
            return;
        };
        if self.accept_next_config {
            self.accept_next_config = false;
            self.load_state(state);
            self.on_edit();
            self.status("reloaded from the device", false);
        } else if state.raw != loaded.raw {
            if self.pending_changes().is_empty() {
                self.load_state(state);
                self.on_edit();
            } else {
                self.status("the config on the device changed under your edits; Reload to pick it up", true);
            }
        }
    }

    fn load_state(&mut self, state: WireViewConfigState) {
        self.fill(&state.config);
        self.loaded = Some(state);
        self.tabs.set_sensitive(true);
        self.update_buttons();
    }

    fn fill(&self, config: &WireViewConfig) {
        self.filling.set(true);
        for k in KEYS {
            if let Some(editor) = self.editors.get(k.key) {
                match editor {
                    Editor::Text(entry) => entry.set_text(&config.name),
                    other => other.set(get(config, k.field)),
                }
            }
        }
        self.filling.set(false);
    }

    fn collect(&self) -> WireViewConfig {
        let mut config = self.loaded.as_ref().map(|s| s.config.clone()).unwrap_or_default();
        let base_background = config.background;
        for k in KEYS {
            if let Some(editor) = self.editors.get(k.key) {
                match editor {
                    Editor::Text(entry) => config.name = entry.text().to_string(),
                    other => set(&mut config, k.field, other.get()),
                }
            }
        }
        if config.background != base_background {
            config.fan_bitmap = fan_bitmap_for(config.background);
        }
        config
    }

    fn diff(&self, old: &WireViewConfig, new: &WireViewConfig) -> Vec<(&'static str, String, String)> {
        KEYS.iter()
            .filter(|k| changed(k, old, new))
            .map(|k| (k.label, show(k, old), show(k, new)))
            .collect()
    }

    fn pending_changes(&self) -> Vec<(&'static str, String, String)> {
        match &self.loaded {
            Some(loaded) => self.diff(&loaded.config, &self.collect()),
            None => Vec::new(),
        }
    }

    fn on_edit(&self) {
        let Some(loaded) = &self.loaded else {
            self.update_buttons();
            return;
        };
        let new = self.collect();
        let mut count = 0;
        for k in KEYS {
            let edited = changed(k, &loaded.config, &new);
            count += usize::from(edited);
            if let Some(label) = self.labels.get(k.key) {
                if edited {
                    label.add_css_class("wireview-edited");
                } else {
                    label.remove_css_class("wireview-edited");
                }
            }
        }
        self.changes_label.set_label(&match count {
            0 => String::new(),
            1 => "1 unsaved change".to_owned(),
            n => format!("{n} unsaved changes"),
        });
        self.update_buttons_dirty(count > 0);
    }

    fn update_buttons(&self) {
        let dirty = !self.pending_changes().is_empty();
        self.update_buttons_dirty(dirty);
    }

    fn update_buttons_dirty(&self, dirty: bool) {
        let loaded = self.loaded.is_some();
        for b in [&self.discard_button, &self.apply_button, &self.save_button] {
            b.set_sensitive(dirty && !self.busy);
        }
        for b in [&self.reload_button, &self.backup_button, &self.restore_button] {
            b.set_sensitive(loaded && !self.busy);
        }
        self.clear_button.set_sensitive(self.detected && !self.busy);
        self.tabs.set_sensitive(loaded && !self.busy);
    }

    // ---- backup / restore files (same JSON as the wv2ctl tooling)

    fn window(&self) -> Option<gtk::Window> {
        self.root.root().and_downcast::<gtk::Window>()
    }

    fn backup(&self, sender: &ComponentSender<Self>) {
        let (Some(loaded), Some(info)) = (self.loaded.clone(), self.info.clone()) else { return };
        let dialog = gtk::FileDialog::builder()
            .title("Back up WireView configuration")
            .initial_name("wireview-config.json")
            .build();
        let sender = sender.clone();
        dialog.save(self.window().as_ref(), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            let doc = serde_json::json!({
                "device": "WireView Pro II",
                "uid": info.uid,
                "firmware": info.firmware,
                "config_version": loaded.config.version,
                "saved_at": glib::DateTime::now_local().ok().and_then(|t| t.format("%Y-%m-%dT%H:%M:%S").ok()).map(|s| s.to_string()),
                "raw": loaded.raw,
                "config": loaded.config,
            });
            let result = serde_json::to_string_pretty(&doc)
                .map_err(|e| e.to_string())
                .and_then(|text| std::fs::write(&path, text + "\n").map_err(|e| e.to_string()))
                .map(|()| format!("backup written to {}", path.display()));
            sender.input(WireViewPageMsg::Done(result));
        });
    }

    fn restore(&self, sender: &ComponentSender<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Load WireView configuration backup")
            .build();
        let sender = sender.clone();
        dialog.open(self.window().as_ref(), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            let name = path
                .file_name()
                .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let loaded = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).map_err(|e| e.to_string()))
                .and_then(|doc| {
                    let raw = doc
                        .get("raw")
                        .and_then(|r| r.as_str())
                        .ok_or_else(|| "no \"raw\" field in the file".to_owned())?;
                    let raw = unhex(raw)?;
                    if raw.len() != CONFIG_SIZE || raw[2] != CONFIG_VERSION {
                        return Err(format!("not a {CONFIG_SIZE}-byte version {CONFIG_VERSION} config"));
                    }
                    decode_config(&raw)
                })
                .map(|config| (name, config))
                .map_err(|e| format!("Can't use {}:\n{e}", path.display()));
            sender.input(WireViewPageMsg::RestoreLoaded(loaded));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tab_key_and_fault_action_is_in_the_table() {
        for (_, keys) in TABS {
            for k in *keys {
                assert!(KEYS.iter().any(|key| key.key == *k), "missing key {k}");
            }
        }
        for k in FAULT_ACTION_KEYS {
            assert!(KEYS.iter().any(|key| key.key == k), "missing key {k}");
        }
        // Every table entry names a real config field (or the name).
        let probe = WireViewConfig { ocp: 7, ..Default::default() };
        for key in KEYS {
            if !matches!(key.kind, Kind::Text) {
                let mut c = WireViewConfig::default();
                set(&mut c, key.field, 1);
                assert_eq!(get(&c, key.field), 1, "field {} does not round-trip", key.field);
            }
        }
        assert_eq!(get(&probe, "ocp"), 7);
    }

    #[test]
    fn values_render_like_the_cli() {
        let mut c = WireViewConfig { wire_ocp: 105, fault_hard_off: 30, color_primary: 0xFF2141E6, ..Default::default() };
        c.name = "WV".to_owned();
        assert_eq!(show(key("limit.wire_ocp"), &c), "10.5 A");
        assert_eq!(show(key("fault.hard_off"), &c), "otp_ts,ocp,wire_ocp,opp");
        assert_eq!(show(key("ui.color.primary"), &c), "#2141e6");
        assert_eq!(show(key("name"), &c), "\"WV\"");
    }
}
