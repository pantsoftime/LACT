//! "Advanced" page: a one-stop overclocking dashboard laid out after mVolt+.
//!
//! Every tunable lives here as a card in a grid — the NVML-backed ones the
//! Overclocking page also exposes (core offset, memory offset, power limit,
//! voltage boost, locked core clock) and the RM ClockClient ones only this
//! page reaches (XBAR / SYS / video offsets, the MSVDD and NVVDD rail offsets).
//! The full mVolt+ option set is present; a control with no Linux mechanism
//! yet is shown insensitive with the reason in its tooltip rather than left
//! out, so it can be wired later without moving anything.
//!
//! Editing on this page and on the Overclocking page both feed the same
//! pending config. A card only writes its value on Apply if the user touched
//! it here (`dirty`), so leaving a card alone never overrides what the other
//! page set. The core and memory cards write pstate 0 and clear the other
//! pstate entries: on this driver the offset register is global and a stray
//! zero for another pstate silently cancels the value (a bug found in the
//! packaged config).

use crate::app::graphs_window::stat::StatType;
use crate::app::msg::AppMsg;
use crate::app::pages::PageUpdate;
use gtk::prelude::*;
use lact_schema::config::GpuConfig;
use lact_schema::request::{ClockspeedType, SetClocksCommand};
use lact_schema::{ClocksTable, DeviceStats, NvidiaClockOffset, NvidiaClocksTable};
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// Highest XBAR offset the correctness harness has passed on the reference
/// card. Going above it needs the explicit switch on the page.
const VALIDATED_MAX_XBAR_MHZ: f64 = 300.0;
/// Wrap width for card text, which is what keeps three cards to a row.
const CARD_TEXT_CHARS: i32 = 34;

#[derive(Debug)]
pub enum AdvVoltagePageMsg {
    /// `initial` is carried for parity with the other page messages; this
    /// page treats every update the same way.
    Update {
        update: PageUpdate,
        #[allow(dead_code)]
        initial: bool,
    },
    ClocksTable(Option<ClocksTable>),
}

/// What an editable card writes into the pending config on Apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    /// One `SetClocksCommand` — the RM domains and voltage boost.
    Clock(ClockspeedType),
    /// `gpu_clock_offsets = {0: v}` (and no other pstate entries).
    CoreOffset,
    /// `mem_clock_offsets = {0: v}`.
    MemOffset,
    /// `power_cap` in watts; off = driver default.
    PowerCap,
    /// Locked core clock: `min_core_clock = max_core_clock = v`.
    BoostLock,
}

/// One editable card: title, enable switch, spin + slider on a shared
/// adjustment, a "current / range" line, and a note.
struct Card {
    frame: gtk::Frame,
    switch: gtk::Switch,
    adjustment: gtk::Adjustment,
    current_label: gtk::Label,
    control: Control,
    default_value: Rc<Cell<f64>>,
    /// Set by user interaction, cleared when the value is loaded from the
    /// daemon or written on Apply. Only dirty cards write on Apply.
    dirty: Rc<Cell<bool>>,
    /// Suppresses the change handlers while a value is loaded programmatically.
    loading: Rc<Cell<bool>>,
}

impl Card {
    fn new(
        title: &str,
        unit: &'static str,
        step: f64,
        control: Control,
        note: &str,
        warning: bool,
        sender: &ComponentSender<AdvVoltagePage>,
    ) -> Self {
        let adjustment = gtk::Adjustment::new(0.0, -1000.0, 1000.0, step, step * 5.0, 0.0);
        let dirty = Rc::new(Cell::new(false));
        let loading = Rc::new(Cell::new(false));
        let default_value = Rc::new(Cell::new(0.0));

        let switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("On: the target below is applied. Off: stock / driver default.")
            .build();
        let current_label = caption("Current: —", false);
        let spin = gtk::SpinButton::builder()
            .adjustment(&adjustment)
            .digits(0)
            .width_chars(6)
            .build();
        let scale = gtk::Scale::builder()
            .adjustment(&adjustment)
            .orientation(gtk::Orientation::Horizontal)
            .hexpand(true)
            .draw_value(false)
            .build();
        scale.add_mark(0.0, gtk::PositionType::Bottom, None);
        let default_button = gtk::Button::builder()
            .label("Default")
            .css_classes(["flat"])
            .tooltip_text("Reset the target to its default")
            .build();

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&spin);
        row.append(&scale);
        row.append(&gtk::Label::new(Some(unit)));
        row.append(&default_button);
        row.set_sensitive(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.append(&heading(title));
        header.append(&switch);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 5);
        body.set_margin_all(10);
        body.append(&header);
        body.append(&current_label);
        body.append(&row);
        body.append(&caption(note, warning));

        let frame = gtk::Frame::builder().child(&body).hexpand(true).build();

        {
            let (sender, dirty, loading) = (sender.clone(), dirty.clone(), loading.clone());
            adjustment.connect_value_changed(move |_| {
                if loading.get() {
                    return;
                }
                dirty.set(true);
                let _ = sender.output(AppMsg::SettingsChanged);
            });
        }
        {
            let (sender, dirty, loading, row) =
                (sender.clone(), dirty.clone(), loading.clone(), row.clone());
            switch.connect_active_notify(move |switch| {
                row.set_sensitive(switch.is_active());
                if loading.get() {
                    return;
                }
                dirty.set(true);
                let _ = sender.output(AppMsg::SettingsChanged);
            });
        }
        {
            let (adjustment, default_value) = (adjustment.clone(), default_value.clone());
            default_button.connect_clicked(move |_| adjustment.set_value(default_value.get()));
        }

        Self {
            frame,
            switch,
            adjustment,
            current_label,
            control,
            default_value,
            dirty,
            loading,
        }
    }

    /// Load bounds and the applied value from the daemon. Skipped while the
    /// user has unapplied edits on this card.
    fn load(
        &self,
        current: Option<f64>,
        min: f64,
        max: f64,
        default: f64,
        enabled: bool,
        label: &str,
    ) {
        if self.dirty.get() {
            return;
        }
        self.loading.set(true);
        self.adjustment.set_lower(min);
        self.adjustment.set_upper(max);
        self.default_value.set(default);
        self.adjustment.set_value(current.unwrap_or(default));
        self.switch.set_active(enabled);
        self.current_label.set_label(label);
        self.frame.set_sensitive(true);
        self.loading.set(false);
    }

    fn unavailable(&self, reason: &str) {
        self.loading.set(true);
        self.switch.set_active(false);
        self.current_label.set_label(reason);
        self.frame.set_sensitive(false);
        self.loading.set(false);
    }

    fn value(&self) -> Option<i32> {
        #[allow(clippy::cast_possible_truncation)]
        self.switch
            .is_active()
            .then(|| self.adjustment.value().round() as i32)
    }

    fn apply(&self, config: &mut GpuConfig) {
        if !self.dirty.get() {
            return;
        }
        let clocks = &mut config.clocks_configuration;
        match self.control {
            Control::Clock(clock_type) => clocks.apply_clocks_command(&SetClocksCommand {
                r#type: clock_type,
                value: self.value(),
            }),
            Control::CoreOffset => {
                clocks.gpu_clock_offsets.clear();
                if let Some(v) = self.value() {
                    clocks.gpu_clock_offsets.insert(0, v);
                }
            }
            Control::MemOffset => {
                clocks.mem_clock_offsets.clear();
                if let Some(v) = self.value() {
                    clocks.mem_clock_offsets.insert(0, v);
                }
            }
            Control::PowerCap => {
                config.power_cap = self
                    .switch
                    .is_active()
                    .then(|| self.adjustment.value().round());
            }
            Control::BoostLock => {
                let v = self.value();
                clocks.min_core_clock = v;
                clocks.max_core_clock = v;
            }
        }
        self.dirty.set(false);
    }
}

// ------------------------------------------------------------ widget helpers

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .max_width_chars(CARD_TEXT_CHARS)
        .css_classes(["heading"])
        .build()
}

fn caption(text: &str, warning: bool) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(CARD_TEXT_CHARS)
        .css_classes(if warning {
            ["caption", "warning"]
        } else {
            ["caption", "dim-label"]
        })
        .build()
}

fn section_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .margin_top(8)
        .css_classes(["heading", "accent"])
        .build()
}

fn card_grid() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(2)
        .max_children_per_line(3)
        .homogeneous(true)
        .row_spacing(8)
        .column_spacing(8)
        .hexpand(true)
        .build()
}

/// A control mVolt+ has that has no Linux mechanism yet: same shape as a
/// card, permanently insensitive, reason in the tooltip. Returns the "live"
/// label so real telemetry can still be shown on it.
fn placeholder_card(title: &str, live: &str, why: &str) -> (gtk::Frame, gtk::Label) {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.append(&heading(title));
    header.append(&caption("Not wired", false));
    let live_label = caption(live, false);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::SpinButton::with_range(0.0, 1.0, 1.0));
    row.append(&gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        0.0,
        1.0,
        1.0,
    ));
    row.set_sensitive(false);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 5);
    body.set_margin_all(10);
    body.append(&header);
    body.append(&live_label);
    body.append(&row);
    body.append(&caption(why, false));
    let frame = gtk::Frame::builder()
        .child(&body)
        .hexpand(true)
        .tooltip_text(why)
        .build();
    frame.add_css_class("dim-label");
    (frame, live_label)
}

/// A telemetry tile. Left-click opens the graphs window on a plot of the
/// stats it represents; right-click opens a menu with Show / Remove.
///
/// Deliberately a plain `gtk::Box` rather than a `gtk::Button`: the button's
/// own click gesture claims the pointer sequence and a second gesture never
/// receives the right-click. Two gestures on a box is the standard GTK4
/// context-menu recipe and does not have that problem.
fn tele_tile(
    name: &str,
    stats: Vec<StatType>,
    sender: &ComponentSender<AdvVoltagePage>,
) -> (gtk::Box, gtk::Label) {
    let value = gtk::Label::builder()
        .label("—")
        .xalign(0.0)
        .css_classes(["title-3", "numeric"])
        .build();
    // Explicit menu button: nothing can intercept it, unlike a right-click
    // gesture (a capture-phase gesture higher in LACT's widget tree swallows
    // secondary-button presses before they reach this widget).
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu_box.set_margin_all(4);
    let show_item = gtk::Button::builder()
        .label("Show graph")
        .css_classes(["flat"])
        .build();
    let remove_item = gtk::Button::builder()
        .label("Remove graph")
        .css_classes(["flat", "destructive-action"])
        .build();
    menu_box.append(&show_item);
    menu_box.append(&remove_item);
    let menu = gtk::Popover::builder().child(&menu_box).build();
    let menu_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .popover(&menu)
        .valign(gtk::Align::Start)
        .css_classes(["flat", "circular"])
        .tooltip_text("Graph options")
        .build();

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let name_label = caption(name, false);
    name_label.set_hexpand(true);
    header.append(&name_label);
    header.append(&menu_button);
    let inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
    inner.set_margin_start(8);
    inner.set_margin_end(4);
    inner.set_margin_top(2);
    inner.set_margin_bottom(6);
    inner.append(&header);
    inner.append(&value);

    let tile = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tile.add_css_class("card");
    tile.set_cursor_from_name(Some("pointer"));
    tile.set_tooltip_text(Some("Click to graph"));
    tile.append(&inner);

    {
        let (sender, stats, menu) = (sender.clone(), stats.clone(), menu.clone());
        show_item.connect_clicked(move |_| {
            menu.popdown();
            let _ = sender.output(AppMsg::ShowGraphsFor(stats.clone()));
        });
    }
    {
        let (sender, stats, menu) = (sender.clone(), stats.clone(), menu.clone());
        remove_item.connect_clicked(move |_| {
            menu.popdown();
            let _ = sender.output(AppMsg::HideGraphsFor(stats.clone()));
        });
    }
    {
        let (sender, stats) = (sender.clone(), stats.clone());
        let left = gtk::GestureClick::new();
        left.set_button(gtk::gdk::BUTTON_PRIMARY);
        left.connect_released(move |_, _, _, _| {
            let _ = sender.output(AppMsg::ShowGraphsFor(stats.clone()));
        });
        tile.add_controller(left);
    }
    (tile, value)
}

fn mhz(v: Option<u64>) -> String {
    v.map_or("—".to_owned(), |v| format!("{v} MHz"))
}

// ------------------------------------------------------------------ the page

pub struct AdvVoltagePage {
    content: gtk::Box,
    status_label: gtk::Label,

    tele_gpc: gtk::Label,
    tele_xbar: gtk::Label,
    tele_sys: gtk::Label,
    tele_video: gtk::Label,
    tele_mem: gtk::Label,
    tele_ratio: gtk::Label,
    tele_volt: gtk::Label,
    tele_power: gtk::Label,

    core: Card,
    boost_lock: Card,
    vboost: Card,
    nvvdd: Card,
    xbar: Card,
    msvdd: Card,
    sys: Card,
    video: Card,
    mem: Card,
    power: Card,

    nvvdd_range_live: gtk::Label,
    ratio_live: gtk::Label,
    guard_switch: gtk::Switch,
    table: Option<NvidiaClocksTable>,
}

#[relm4::component(pub)]
impl relm4::Component for AdvVoltagePage {
    type Init = ();
    type Input = AdvVoltagePageMsg;
    type Output = AppMsg;
    type CommandOutput = ();

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,
            set_vexpand: true,

            model.content.clone() {},
        }
    }

    fn init(
        _: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_all(15);
        content.set_margin_top(20);

        // Only shown when the daemon could not enable the RM interface.
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["warning"])
            .build();
        content.append(&status_label);

        // ---- live telemetry row
        let tele = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        tele.set_homogeneous(true);
        let mut tiles = Vec::new();
        let specs: [(&str, Vec<StatType>); 8] = [
            ("GPC", vec![StatType::GpuClock]),
            ("XBAR", vec![StatType::Clockspeed("XBAR".into())]),
            ("SYS", vec![StatType::Clockspeed("SYS".into())]),
            ("Video", vec![StatType::Clockspeed("Video".into())]),
            ("Memory", vec![StatType::VramClock]),
            (
                "XBAR / GPC",
                vec![StatType::GpuClock, StatType::Clockspeed("XBAR".into())],
            ),
            ("Core V", vec![StatType::GpuVoltage]),
            (
                "Power",
                vec![
                    StatType::PowerAverage,
                    StatType::PowerCurrent,
                    StatType::PowerCap,
                ],
            ),
        ];
        for (name, stats) in specs {
            let (tile, label) = tele_tile(name, stats, &sender);
            tele.append(&tile);
            tiles.push(label);
        }
        let mut tiles = tiles.into_iter();
        let (tele_gpc, tele_xbar, tele_sys, tele_video) = (
            tiles.next().unwrap(),
            tiles.next().unwrap(),
            tiles.next().unwrap(),
            tiles.next().unwrap(),
        );
        let (tele_mem, tele_ratio, tele_volt, tele_power) = (
            tiles.next().unwrap(),
            tiles.next().unwrap(),
            tiles.next().unwrap(),
            tiles.next().unwrap(),
        );
        content.append(&tele);

        // ---- Core / NVVDD
        content.append(&section_label("Core / NVVDD"));
        let grid = card_grid();
        let core = Card::new(
            "Core clock offset",
            "MHz",
            5.0,
            Control::CoreOffset,
            "NVML VF offset — the register nvidia-smi and the Overclocking page use. Written for pstate 0 only; other pstate entries are cleared because a stray 0 cancels the value on this driver.",
            false,
            &sender,
        );
        let boost_lock = Card::new(
            "Boost lock",
            "MHz",
            15.0,
            Control::BoostLock,
            "Locks the core clock (NVML locked clocks, min = max = target; the same as nvidia-smi -lgc). Off restores boost.",
            false,
            &sender,
        );
        let vboost = Card::new(
            "Voltage boost",
            "%",
            5.0,
            Control::Clock(ClockspeedType::VoltageBoost),
            "LACT's bounded V/F limit shift via NVAPI (PR #1133). Same control as the Overclocking page.",
            false,
            &sender,
        );
        let nvvdd = Card::new(
            "Core voltage offset (NVVDD)",
            "mV",
            5.0,
            Control::Clock(ClockspeedType::NvvddOffset),
            "Experimental. Rail 0 on the XBAR domain accepts writes but produced no measurable clock change in testing. Bounded ±50 mV.",
            true,
            &sender,
        );
        let (nvvdd_range, nvvdd_range_live) = placeholder_card(
            "NVVDD voltage range",
            "Live core voltage: —",
            "mVolt+ sets a min/max window through NVAPI. No Linux path found yet; the live voltage comes from LACT's NvAPI readout.",
        );
        for f in [
            &core.frame,
            &boost_lock.frame,
            &vboost.frame,
            &nvvdd.frame,
            &nvvdd_range,
        ] {
            grid.append(f);
        }
        content.append(&grid);

        // ---- Fabric / MSVDD
        content.append(&section_label("Fabric / MSVDD"));
        let grid = card_grid();
        let xbar = Card::new(
            "XBAR clock offset",
            "MHz",
            10.0,
            Control::Clock(ClockspeedType::XbarClockOffset),
            "Verified by readback and CLK_MEASURE_FREQ. Harness-validated to +300; above that needs the guard switch below.",
            false,
            &sender,
        );
        let msvdd = Card::new(
            "XBAR voltage offset (MSVDD)",
            "mV",
            5.0,
            Control::Clock(ClockspeedType::MsvddOffset),
            "+20 mV lowered XBAR ~31 MHz on its own and did not extend the ceiling. Not free headroom.",
            true,
            &sender,
        );
        let sys = Card::new(
            "SYS clock offset",
            "MHz",
            10.0,
            Control::Clock(ClockspeedType::SysClockOffset),
            "Domain verified. Not harness-validated at any positive value.",
            false,
            &sender,
        );
        let video = Card::new(
            "Video clock offset",
            "MHz",
            10.0,
            Control::Clock(ClockspeedType::VideoClockOffset),
            "NVENC / NVDEC only. Verified domain; not harness-validated.",
            false,
            &sender,
        );
        let (ratio_card, ratio_live) = placeholder_card(
            "MSVDD clock ratio",
            "Measured XBAR / GPC: —",
            "mVolt+ sets the ceiling of the MSVDD-domain clocks as a ratio of core. That control has not been found on Linux; the measured ratio is real.",
        );
        let (msvdd_range, _) = placeholder_card(
            "MSVDD voltage range",
            "No MSVDD voltage readback on this driver",
            "NVAPI range control on Windows; no Linux path found yet.",
        );
        for f in [
            &xbar.frame,
            &msvdd.frame,
            &sys.frame,
            &video.frame,
            &ratio_card,
            &msvdd_range,
        ] {
            grid.append(f);
        }
        content.append(&grid);

        // ---- Memory / Power
        content.append(&section_label("Memory / Power"));
        let grid = card_grid();
        let mem = Card::new(
            "Memory clock offset",
            "MHz",
            50.0,
            Control::MemOffset,
            "NVML VF offset, pstate 0. GDDR7 errors can show as lower throughput rather than corruption — validate with throughput too.",
            false,
            &sender,
        );
        let power = Card::new(
            "Power limit",
            "W",
            5.0,
            Control::PowerCap,
            "Off = driver default. At the cap, extra voltage lowers clocks instead of raising power.",
            false,
            &sender,
        );
        let (ext_volt, _) = placeholder_card(
            "Extended voltage",
            "—",
            "mVolt+ extended voltage demand offsets per rail. No Linux path found.",
        );
        for f in [&mem.frame, &power.frame, &ext_volt] {
            grid.append(f);
        }
        content.append(&grid);

        // ---- Validation / guard rails
        content.append(&section_label("Validation"));
        let guard_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        guard_box.set_margin_all(10);
        let guard_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        guard_box.append(&guard_switch);
        guard_box.append(
            &gtk::Label::builder()
                .label(&format!(
                    "Allow XBAR above the validated maximum of +{VALIDATED_MAX_XBAR_MHZ:.0} MHz. \
                     +380 MHz produced wrong compute results with no crash and no Xid; +450 MHz hard-locked the machine."
                ))
                .xalign(0.0)
                .wrap(true)
                .hexpand(true)
                .build(),
        );
        guard_box.append(
            &gtk::Button::builder()
                .label("Run correctness check")
                .sensitive(false)
                .tooltip_text("Launches xbar_verify.py against the applied setting. Not wired into the GUI yet — run it from a terminal.")
                .build(),
        );
        content.append(&gtk::Frame::builder().child(&guard_box).build());
        {
            let adjustment = xbar.adjustment.clone();
            guard_switch.connect_active_notify(move |switch| {
                if switch.is_active() {
                    adjustment.set_upper(1000.0);
                } else {
                    adjustment.set_upper(VALIDATED_MAX_XBAR_MHZ);
                    if adjustment.value() > VALIDATED_MAX_XBAR_MHZ {
                        adjustment.set_value(VALIDATED_MAX_XBAR_MHZ);
                    }
                }
            });
        }

        let model = Self {
            content,
            status_label,
            tele_gpc,
            tele_xbar,
            tele_sys,
            tele_video,
            tele_mem,
            tele_ratio,
            tele_volt,
            tele_power,
            core,
            boost_lock,
            vboost,
            nvvdd,
            xbar,
            msvdd,
            sys,
            video,
            mem,
            power,
            nvvdd_range_live,
            ratio_live,
            guard_switch,
            table: None,
        };

        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            AdvVoltagePageMsg::Update { update, .. } => match update {
                PageUpdate::Stats(stats) => self.show_stats(&stats),
                PageUpdate::Info(_) => {}
            },
            AdvVoltagePageMsg::ClocksTable(table) => {
                let table = match table {
                    Some(ClocksTable::Nvidia(table)) => Some(table),
                    _ => None,
                };
                self.show_table(table.as_ref());
                self.table = table;
            }
        }
    }
}

impl AdvVoltagePage {
    fn show_stats(&self, stats: &Arc<DeviceStats>) {
        let c = &stats.clockspeed;
        self.tele_gpc.set_label(&mhz(c.gpu_clockspeed));
        self.tele_xbar.set_label(&mhz(c.xbar_clockspeed));
        self.tele_sys.set_label(&mhz(c.sys_clockspeed));
        self.tele_video.set_label(&mhz(c.video_clockspeed));
        self.tele_mem.set_label(&mhz(c.vram_clockspeed));

        let ratio = match (c.xbar_clockspeed, c.gpu_clockspeed) {
            (Some(x), Some(g)) if g > 0 => format!("{:.3}", x as f64 / g as f64),
            _ => "—".to_owned(),
        };
        self.tele_ratio.set_label(&ratio);
        self.ratio_live
            .set_label(&format!("Measured XBAR / GPC: {ratio}"));

        let volt = stats
            .voltage
            .gpu
            .map_or("—".to_owned(), |v| format!("{v} mV"));
        self.tele_volt.set_label(&volt);
        self.nvvdd_range_live
            .set_label(&format!("Live core voltage: {volt}"));

        let p = &stats.power;
        let draw = p.average.or(p.current);
        self.tele_power.set_label(&match (draw, p.cap_current) {
            (Some(d), Some(cap)) => format!("{d:.0} / {cap:.0} W"),
            (Some(d), None) => format!("{d:.0} W"),
            _ => "—".to_owned(),
        });

        // The power cap card is fed by stats, not by the clocks table.
        match (p.cap_current, p.cap_min, p.cap_max) {
            (Some(cur), Some(min), Some(max)) => {
                let default = p.cap_default.unwrap_or(max);
                let pct = if default > 0.0 {
                    cur / default * 100.0
                } else {
                    0.0
                };
                self.power.load(
                    Some(cur),
                    min,
                    max,
                    default,
                    (cur - default).abs() > 0.5,
                    &format!("Current {cur:.0} W ({pct:.0} % of {default:.0} W default)   Range {min:.0}…{max:.0} W"),
                );
            }
            _ => self
                .power
                .unavailable("Power limit not reported by the driver"),
        }
    }

    fn show_table(&self, table: Option<&NvidiaClocksTable>) {
        let Some(t) = table else {
            for card in [
                &self.core,
                &self.boost_lock,
                &self.vboost,
                &self.nvvdd,
                &self.xbar,
                &self.msvdd,
                &self.sys,
                &self.video,
                &self.mem,
            ] {
                card.unavailable("No NVIDIA clocks table from the daemon");
            }
            return;
        };

        // NVML-backed cards. On this driver the per-pstate offset is one global
        // register, so pstate 0 stands for all of them.
        let by_pstate = |m: &indexmap::IndexMap<u32, NvidiaClockOffset>| {
            m.get(&0).or_else(|| m.values().next()).cloned()
        };
        load_offset(
            &self.core,
            by_pstate(&t.gpu_offsets).as_ref(),
            "MHz",
            "No core offset reported",
        );
        load_offset(
            &self.mem,
            by_pstate(&t.mem_offsets).as_ref(),
            "MHz",
            "No memory offset reported",
        );

        match t.gpu_clock_range {
            Some((min, max)) => {
                let locked = t.gpu_locked_clocks.map(|(_, hi)| f64::from(hi));
                self.boost_lock.load(
                    locked,
                    f64::from(min),
                    f64::from(max),
                    f64::from(max),
                    locked.is_some(),
                    &match t.gpu_locked_clocks {
                        Some((lo, hi)) => format!("Locked {lo}…{hi} MHz   Range {min}…{max} MHz"),
                        None => format!("Not locked (boosting)   Range {min}…{max} MHz"),
                    },
                );
            }
            None => self.boost_lock.unavailable("Core clock range not reported"),
        }

        match t.voltage_boost {
            Some(b) => self.vboost.load(
                Some(f64::from(b.current)),
                f64::from(b.min),
                f64::from(b.max),
                0.0,
                b.current != 0,
                &format!("Current {} %   Range {}…{} %", b.current, b.min, b.max),
            ),
            None => self
                .vboost
                .unavailable("Voltage boost not available (needs NvAPI)"),
        }

        // RM ClockClient cards.
        let rm = "Not available: the daemon could not enable the RM interface on this driver";
        load_offset(&self.xbar, t.xbar_offset.as_ref(), "MHz", rm);
        load_offset(&self.sys, t.sys_offset.as_ref(), "MHz", rm);
        load_offset(&self.video, t.video_offset.as_ref(), "MHz", rm);
        load_offset(&self.msvdd, t.msvdd_offset.as_ref(), "mV", rm);
        load_offset(&self.nvvdd, t.nvvdd_offset.as_ref(), "mV", rm);

        // Keep the XBAR guard in force after the table refreshes the bounds.
        if !self.guard_switch.is_active() && self.xbar.adjustment.upper() > VALIDATED_MAX_XBAR_MHZ {
            self.xbar.adjustment.set_upper(VALIDATED_MAX_XBAR_MHZ);
        }

        if t.xbar_offset.is_some() {
            self.status_label.set_visible(false);
        } else {
            self.status_label.set_label(
                "RM ClockClient interface unavailable on this driver — the daemon refused to enable it. \
                 XBAR / SYS / video / rail controls are disabled; the NVML controls still work.",
            );
            self.status_label.set_visible(true);
        }
    }

    /// Fold this page's edited cards into the pending config. Called from the
    /// app's Apply path after the Overclocking page; untouched cards write
    /// nothing, so that page's values stand.
    pub fn apply_gpu_config(&self, config: &mut GpuConfig) {
        for card in [
            &self.core,
            &self.boost_lock,
            &self.vboost,
            &self.nvvdd,
            &self.xbar,
            &self.msvdd,
            &self.sys,
            &self.video,
            &self.mem,
            &self.power,
        ] {
            card.apply(config);
        }
    }
}

fn load_offset(card: &Card, offset: Option<&NvidiaClockOffset>, unit: &str, unavailable: &str) {
    match offset {
        Some(o) => card.load(
            Some(f64::from(o.current)),
            f64::from(o.min),
            f64::from(o.max),
            0.0,
            o.current != 0,
            &format!(
                "Current {:+} {unit}   Range {}…{} {unit}",
                o.current, o.min, o.max
            ),
        ),
        None => card.unavailable(unavailable),
    }
}
