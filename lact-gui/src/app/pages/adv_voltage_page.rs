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
use lact_schema::{
    ClocksTable, DeviceStats, NvidiaClockOffset, NvidiaClocksTable, NvidiaVoltageRail, RailLimit,
};
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

/// Highest XBAR offset the correctness harness has passed on the reference
/// card. Going above it needs the explicit switch on the page.
const VALIDATED_MAX_XBAR_MHZ: f64 = 300.0;
/// Requested width of card text; every card asks for the same width so the
/// grid stays aligned, and it is what sets how many cards fit a row.
const CARD_TEXT_CHARS: i32 = 26;

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
    /// GPC→XBAR propagation ratio; the card edits the ratio, the config
    /// stores it × 1000.
    Ratio,
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
        digits: u32,
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
            .digits(digits)
            .width_chars(if digits > 0 { 7 } else { 6 })
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

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&heading(title));
        header.append(&info_icon(note, warning));
        header.append(&switch);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.set_margin_all(8);
        body.append(&header);
        body.append(&current_label);
        body.append(&row);

        let frame = gtk::Frame::builder()
            .child(&body)
            .hexpand(true)
            .valign(gtk::Align::Start)
            .build();

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
            Control::Ratio => {
                #[allow(clippy::cast_possible_truncation)]
                let milli = self
                    .switch
                    .is_active()
                    .then(|| (self.adjustment.value() * 1000.0).round() as i32);
                clocks.gpc_xbar_ratio_milli = milli;
            }
        }
        self.dirty.set(false);
    }
}

/// One voltage rail: live target / sensed voltage and evaluated limits on
/// two lines, then the four policy-limit deltas (VMIN, REL, ALT/OP, OV) as
/// slider rows. Off = the deltas the daemon found at start (the firmware
/// defaults, e.g. −50 mV REL on MSVDD). The driver's evaluated MAX — the
/// tightest of REL / ALT / OV — is what binds.
///
/// Taller than the other cards, so it lives in its own row: put in a
/// homogeneous grid with them it would set the height of every cell.
struct RailCard {
    frame: gtk::Frame,
    switch: gtk::Switch,
    live: gtk::Label,
    limits_label: gtk::Label,
    adjustments: [(RailLimit, gtk::Adjustment); 4],
    rail: u8,
    defaults: Rc<RefCell<[i32; 4]>>,
    dirty: Rc<Cell<bool>>,
    loading: Rc<Cell<bool>>,
}

impl RailCard {
    fn new(title: &str, rail: u8, note: &str, sender: &ComponentSender<AdvVoltagePage>) -> Self {
        let dirty = Rc::new(Cell::new(false));
        let loading = Rc::new(Cell::new(false));
        let defaults = Rc::new(RefCell::new([0i32; 4]));
        let switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("On: the deltas below are applied. Off: the values found at daemon start.")
            .build();
        let live = gtk::Label::builder()
            .label("—")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();
        let limits_label = gtk::Label::builder()
            .label("—")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();

        let grid = gtk::Grid::builder().column_spacing(6).row_spacing(2).build();
        let adjustments = RailLimit::ALL.map(|limit| {
            let adjustment = gtk::Adjustment::new(0.0, -250.0, 250.0, 5.0, 25.0, 0.0);
            (limit, adjustment)
        });
        for (row, (limit, adjustment)) in adjustments.iter().enumerate() {
            let label = gtk::Label::builder()
                .label(limit.label())
                .xalign(0.0)
                .width_chars(7)
                .tooltip_text(limit_note(*limit))
                .css_classes(["caption"])
                .build();
            let spin = gtk::SpinButton::builder()
                .adjustment(adjustment)
                .digits(0)
                .width_chars(6)
                .build();
            let scale = gtk::Scale::builder()
                .adjustment(adjustment)
                .orientation(gtk::Orientation::Horizontal)
                .hexpand(true)
                .draw_value(false)
                .build();
            scale.add_mark(0.0, gtk::PositionType::Bottom, None);
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let row = row as i32;
            grid.attach(&label, 0, row, 1, 1);
            grid.attach(&spin, 1, row, 1, 1);
            grid.attach(&scale, 2, row, 1, 1);
            let (sender, dirty, loading) = (sender.clone(), dirty.clone(), loading.clone());
            adjustment.connect_value_changed(move |_| {
                if loading.get() {
                    return;
                }
                dirty.set(true);
                let _ = sender.output(AppMsg::SettingsChanged);
            });
        }
        let default_button = gtk::Button::builder()
            .label("Default")
            .css_classes(["flat"])
            .halign(gtk::Align::End)
            .tooltip_text("Reset the deltas to the values found at daemon start")
            .build();
        grid.attach(&default_button, 2, 4, 1, 1);
        grid.set_sensitive(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&heading(title));
        header.append(&info_icon(note, true));
        header.append(&switch);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.set_margin_all(8);
        body.append(&header);
        body.append(&live);
        body.append(&limits_label);
        body.append(&grid);
        let frame = gtk::Frame::builder().child(&body).hexpand(true).build();

        {
            let (sender, dirty, loading, grid) =
                (sender.clone(), dirty.clone(), loading.clone(), grid.clone());
            switch.connect_active_notify(move |switch| {
                grid.set_sensitive(switch.is_active());
                if loading.get() {
                    return;
                }
                dirty.set(true);
                let _ = sender.output(AppMsg::SettingsChanged);
            });
        }
        {
            let (adjustments, defaults) = (adjustments.clone(), defaults.clone());
            default_button.connect_clicked(move |_| {
                for (i, (_, adjustment)) in adjustments.iter().enumerate() {
                    adjustment.set_value(f64::from(defaults.borrow()[i]));
                }
            });
        }

        Self {
            frame,
            switch,
            live,
            limits_label,
            adjustments,
            rail,
            defaults,
            dirty,
            loading,
        }
    }

    fn load(&self, rail: &NvidiaVoltageRail) {
        self.live.set_label(&format!(
            "Target {} mV{}   ·   device max {}",
            rail.target_mv,
            rail.sensed_mv
                .map_or(String::new(), |s| format!(", sensed {s} mV")),
            rail.device_max_mv
                .map_or("—".to_owned(), |m| format!("{m} mV")),
        ));
        self.limits_label.set_label(&format!(
            "VMIN {} · REL {} · ALT/OP {} · OV {}   →   MAX {} mV",
            rail.vmin_limit_mv,
            rail.rel_limit_mv,
            rail.alt_rel_limit_mv,
            rail.ov_limit_mv,
            rail.max_limit_mv
        ));
        if self.dirty.get() {
            return;
        }
        self.loading.set(true);
        let mut any_off_default = false;
        let mut defaults = [0i32; 4];
        for (i, (limit, adjustment)) in self.adjustments.iter().enumerate() {
            if let Some(d) = rail.limit_deltas.iter().find(|d| d.limit == *limit) {
                adjustment.set_lower(f64::from(d.min_mv));
                adjustment.set_upper(f64::from(d.max_mv));
                adjustment.set_value(f64::from(d.current_mv));
                defaults[i] = d.default_mv;
                any_off_default |= d.current_mv != d.default_mv;
            }
        }
        *self.defaults.borrow_mut() = defaults;
        self.switch.set_active(any_off_default);
        self.frame.set_sensitive(!rail.limit_deltas.is_empty());
        self.loading.set(false);
    }

    fn unavailable(&self, reason: &str) {
        self.loading.set(true);
        self.switch.set_active(false);
        self.live.set_label(reason);
        self.limits_label.set_label("—");
        self.frame.set_sensitive(false);
        self.loading.set(false);
    }

    fn apply(&self, config: &mut GpuConfig) {
        if !self.dirty.get() {
            return;
        }
        let on = self.switch.is_active();
        for (limit, adjustment) in &self.adjustments {
            #[allow(clippy::cast_possible_truncation)]
            let value = on.then(|| adjustment.value().round() as i32);
            config
                .clocks_configuration
                .set_rail_limit_delta(self.rail, *limit, value);
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

/// Card text. Requests a fixed width (not just a maximum) so a value that
/// changes length — "975" to "1020" — never changes what the homogeneous
/// grid asks for; that was what made the page jump.
fn caption(text: &str, warning: bool) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .width_chars(CARD_TEXT_CHARS)
        .max_width_chars(CARD_TEXT_CHARS)
        .css_classes(if warning {
            ["caption", "warning"]
        } else {
            ["caption", "dim-label"]
        })
        .build()
}

/// The card's explanation, on hover, instead of a paragraph under every
/// card (mVolt+ 0.40 does the same with its tooltips).
fn info_icon(note: &str, warning: bool) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(if warning {
            "dialog-warning-symbolic"
        } else {
            "dialog-information-symbolic"
        })
        .tooltip_text(note)
        .valign(gtk::Align::Center)
        .css_classes(if warning { ["warning"] } else { ["dim-label"] })
        .build()
}

fn limit_note(limit: RailLimit) -> &'static str {
    match limit {
        RailLimit::Vmin => "Delta to the rail's minimum-voltage floor. Raising it holds the rail higher at idle (more idle power); lowering it lets it sag further.",
        RailLimit::Rel => "Delta to the reliability limit, normally the binding maximum (MAX = tightest of REL / ALT-OP / OV). −50 mV is the driver default on MSVDD here. Raising it alone does nothing while ALT/OP or OV is tighter.",
        RailLimit::AltRel => "Delta to the alternate-reliability / operating limit (Vop). The vendor's operating ceiling; going above it is XOC territory.",
        RailLimit::Ov => "Delta to the overvoltage ceiling. Only matters once REL and ALT/OP are above it. Held under the device maximum by the daemon.",
    }
}

fn section_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .margin_top(8)
        .css_classes(["heading", "accent"])
        .build()
}

/// Uniform cells: every card is the same shape (title, current line, one
/// slider row), so homogeneous costs nothing and keeps the grid aligned.
fn card_grid() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(2)
        .max_children_per_line(4)
        .homogeneous(true)
        .row_spacing(6)
        .column_spacing(6)
        .hexpand(true)
        .build()
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
    // Fixed width: the tiles are homogeneous, so a value growing from
    // "895 MHz" to "3135 MHz" used to widen every tile and shift the page.
    let value = gtk::Label::builder()
        .label("—")
        .xalign(0.0)
        .width_chars(13)
        .max_width_chars(13)
        .ellipsize(gtk::pango::EllipsizeMode::End)
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
    let name_label = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["caption", "dim-label"])
        .build();
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
    sys_volt: Card,
    video_volt: Card,
    ratio: Card,
    mem: Card,
    power: Card,

    nvvdd_rail: RailCard,
    msvdd_rail: RailCard,
    tele_msvdd: gtk::Label,
    limits_summary: gtk::Label,
    limits_list: gtk::Label,
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
        let specs: [(&str, Vec<StatType>); 9] = [
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
                "MSVDD V",
                vec![
                    StatType::Voltage("MSVDD".into()),
                    StatType::Voltage("MSVDD target".into()),
                ],
            ),
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
        let (tele_mem, tele_ratio, tele_volt, tele_msvdd, tele_power) = (
            tiles.next().unwrap(),
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
            0,
            Control::CoreOffset,
            "NVML VF offset — the register nvidia-smi and the Overclocking page use. Written for pstate 0 only; other pstate entries are cleared because a stray 0 cancels the value on this driver.",
            false,
            &sender,
        );
        let boost_lock = Card::new(
            "Boost lock",
            "MHz",
            15.0,
            0,
            Control::BoostLock,
            "Locks the core clock (NVML locked clocks, min = max = target; the same as nvidia-smi -lgc). Off restores boost.",
            false,
            &sender,
        );
        let vboost = Card::new(
            "Voltage boost",
            "%",
            5.0,
            0,
            Control::Clock(ClockspeedType::VoltageBoost),
            "LACT's bounded V/F limit shift via NVAPI (PR #1133). Same control as the Overclocking page.",
            false,
            &sender,
        );
        let nvvdd = Card::new(
            "Core voltage offset (NVVDD demand)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::NvvddOffset),
            "The GPC domain's voltage demand on its own rail (rail 0, from the driver's rail mask). Another domain or a limit can still win the rail. Bounded ±50 mV; untested at any value.",
            true,
            &sender,
        );
        for f in [&core.frame, &boost_lock.frame, &vboost.frame, &nvvdd.frame] {
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
            0,
            Control::Clock(ClockspeedType::XbarClockOffset),
            "Verified by readback and CLK_MEASURE_FREQ. Harness-validated to +300; above that needs the guard switch below.",
            false,
            &sender,
        );
        let msvdd = Card::new(
            "XBAR voltage offset (MSVDD)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::MsvddOffset),
            "+20 mV lowered XBAR ~31 MHz on its own and did not extend the ceiling. Not free headroom.",
            true,
            &sender,
        );
        let sys = Card::new(
            "SYS clock offset",
            "MHz",
            10.0,
            0,
            Control::Clock(ClockspeedType::SysClockOffset),
            "Domain verified. Not harness-validated at any positive value.",
            false,
            &sender,
        );
        let video = Card::new(
            "Video clock offset",
            "MHz",
            10.0,
            0,
            Control::Clock(ClockspeedType::VideoClockOffset),
            "NVENC / NVDEC only. Verified domain; not harness-validated.",
            false,
            &sender,
        );
        let sys_volt = Card::new(
            "SYS voltage offset (MSVDD demand)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::SysVoltageOffset),
            "The SYS domain's voltage demand on the fabric rail. Same caveats as the XBAR demand. Untested at any value.",
            true,
            &sender,
        );
        let video_volt = Card::new(
            "Video voltage offset (MSVDD demand)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::VideoVoltageOffset),
            "The video domain's voltage demand on the fabric rail. NVENC / NVDEC only. Untested at any value.",
            true,
            &sender,
        );
        let ratio = Card::new(
            "MSVDD clock ratio (GPC→XBAR propagation)",
            "×",
            0.005,
            3,
            Control::Ratio,
            "The clock arbiter's GPC→XBAR propagation ratio (factory 0.900 on GB202). A constraint, not XBAR = GPC × ratio: XBAR and SYS follow the core higher when it binds. Off = factory. Not harness-validated at any value; 0.90–0.95 was another tester's adoption envelope, 1.20 raised XBAR 174 MHz in their A/B.",
            true,
            &sender,
        );
        for f in [
            &xbar.frame,
            &msvdd.frame,
            &sys.frame,
            &sys_volt.frame,
            &video.frame,
            &video_volt.frame,
            &ratio.frame,
        ] {
            grid.append(f);
        }
        content.append(&grid);

        // ---- Voltage limits: the two tall cards share a row of their own
        content.append(&section_label("Voltage limits"));
        let rails_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        rails_row.set_homogeneous(true);
        let nvvdd_rail = RailCard::new(
            "NVVDD voltage limits",
            0,
            "Deltas (mV) to the core rail's voltage-policy limits. MAX = the tightest of REL / ALT-OP / OV is what binds; a raised limit is held under the device maximum. Not harness-validated at any value.",
            &sender,
        );
        let msvdd_rail = RailCard::new(
            "MSVDD voltage limits",
            1,
            "Deltas (mV) to the fabric rail's policy limits; −50 on REL is the driver default here. Finding 21: +30 mV here did not make XBAR +340 compute correctly, so this is not XBAR headroom on this card.",
            &sender,
        );
        rails_row.append(&nvvdd_rail.frame);
        rails_row.append(&msvdd_rail.frame);
        content.append(&rails_row);

        // ---- Memory / Power
        content.append(&section_label("Memory / Power"));
        let grid = card_grid();
        let mem = Card::new(
            "Memory clock offset",
            "MHz",
            50.0,
            0,
            Control::MemOffset,
            "NVML VF offset, pstate 0. GDDR7 errors can show as lower throughput rather than corruption — validate with throughput too.",
            false,
            &sender,
        );
        let power = Card::new(
            "Power limit",
            "W",
            5.0,
            0,
            Control::PowerCap,
            "Off = driver default. At the cap, extra voltage lowers clocks instead of raising power.",
            false,
            &sender,
        );
        // mVolt+'s "extended voltage" is the same per-domain demand offsets
        // above with a ±500 mV window; the ±50 mV bound here is deliberate.
        for f in [&mem.frame, &power.frame] {
            grid.append(f);
        }
        content.append(&grid);

        // ---- Boost limits: the arbiter's populated limit clients
        content.append(&section_label("Boost limits"));
        let limits_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        limits_box.set_margin_all(8);
        let limits_summary = gtk::Label::builder()
            .label("—")
            .xalign(0.0)
            .wrap(true)
            .build();
        let limits_list = gtk::Label::builder()
            .label("—")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label", "monospace"])
            .build();
        limits_box.append(&limits_summary);
        limits_box.append(&limits_list);
        let limits_frame = gtk::Frame::builder()
            .child(&limits_box)
            .tooltip_text(
                "Every populated limit client of the driver's clock arbiter (RM PERF_LIMITS status), \
                 read live. Voltage limits are named from the rail policy; the core is bounded by the \
                 tightest maximum among the clients that produce a core clock. Floors (controller \
                 minimums) are listed but never reported as the bound.",
            )
            .build();
        content.append(&limits_frame);

        // ---- Validation / guard rails
        content.append(&section_label("Validation"));
        let guard_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        guard_box.set_margin_all(10);
        let guard_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        guard_box.append(&guard_switch);
        guard_box.append(
            &gtk::Label::builder()
                .label(&format!(
                    "Allow XBAR above the validated +{VALIDATED_MAX_XBAR_MHZ:.0} MHz"
                ))
                .xalign(0.0)
                .hexpand(true)
                .build(),
        );
        guard_box.append(&info_icon(
            "+380 MHz produced wrong compute results with no crash and no Xid; +450 MHz hard-locked the machine. \
             Anything above the validated maximum needs the correctness harness before daily use.",
            true,
        ));
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
            sys_volt,
            video_volt,
            ratio,
            mem,
            power,
            nvvdd_rail,
            msvdd_rail,
            tele_msvdd,
            limits_summary,
            limits_list,
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

        let volt = stats
            .voltage
            .gpu
            .map_or("—".to_owned(), |v| format!("{v} mV"));
        self.tele_volt.set_label(&volt);
        let sensors = &stats.voltage.sensors;
        self.tele_msvdd
            .set_label(&match (sensors.get("MSVDD"), sensors.get("MSVDD target")) {
                (Some(s), Some(t)) => format!("{s} / {t} mV"),
                (Some(s), None) => format!("{s} mV"),
                (None, Some(t)) => format!("→ {t} mV"),
                _ => "—".to_owned(),
            });

        let p = &stats.power;
        let draw = p.average.or(p.current);
        self.tele_power.set_label(&match (draw, p.cap_current) {
            (Some(d), Some(cap)) => format!("{d:.0} / {cap:.0} W"),
            (Some(d), None) => format!("{d:.0} W"),
            _ => "—".to_owned(),
        });

        self.show_perf_limits(stats);

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

    fn show_perf_limits(&self, stats: &Arc<DeviceStats>) {
        let limits = &stats.perf_limits;
        if limits.is_empty() {
            self.limits_summary
                .set_label("Boost-limit telemetry not available on this driver");
            self.limits_list.set_label("—");
            return;
        }
        // The power cap, thermal and power-brake slowdowns are not limit
        // clients in this object (verified: a 400 W cap took the core from
        // 3067 to 2752 MHz and no client changed), so NVML's throttle reasons
        // take precedence as the reason when one is active.
        let throttled: Vec<&str> = stats
            .throttle_info
            .as_ref()
            .map(|info| {
                info.keys()
                    .filter(|k| !k.contains("GPU_IDLE"))
                    .map(|k| match k.as_str() {
                        "SW_POWER_CAP" => "the power cap",
                        "HW_SLOWDOWN" => "a hardware slowdown",
                        "SW_THERMAL_SLOWDOWN" => "thermal slowdown",
                        "HW_THERMAL_SLOWDOWN" => "hardware thermal slowdown",
                        "HW_POWER_BRAKE_SLOWDOWN" => "power brake",
                        "SYNC_BOOST" => "sync boost",
                        "APPLICATIONS_CLOCKS_SETTING" => "the applications clock setting",
                        "DISPLAY_CLOCK_SETTING" => "the display clock setting",
                        other => other,
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Otherwise the core is bounded by the tightest maximum among clients
        // that yield a core clock. Floors are excluded.
        let bound = limits
            .iter()
            .filter(|l| !l.is_minimum && l.domain.as_deref() == Some("GPCCLK"))
            .filter_map(|l| l.result_mhz.map(|mhz| (mhz, l)))
            .min_by_key(|(mhz, _)| *mhz);
        let gpc = stats.clockspeed.gpu_clockspeed;
        self.limits_summary.set_label(&match (throttled.is_empty(), bound) {
            (false, _) => format!(
                "Core held by {} (driver throttle reason){}   ·   {} populated limit clients",
                throttled.join(", "),
                gpc.map_or(String::new(), |c| format!(" at {c} MHz")),
                limits.len()
            ),
            (true, Some((mhz, l))) => format!(
                "Core bounded by {}{} at {mhz} MHz   ·   {} populated clients",
                l.name,
                l.limit_mv.map_or(String::new(), |mv| format!(" ({mv} mV)")),
                limits.len()
            ),
            (true, None) => format!("{} populated clients, none yields a core clock", limits.len()),
        });
        let mut rows: Vec<String> = limits
            .iter()
            .map(|l| {
                format!(
                    "{:<26} {:>9} {:>9}{}",
                    l.name,
                    l.limit_mhz
                        .map(|m| format!("{m} MHz"))
                        .or_else(|| l.limit_mv.map(|v| format!("{v} mV")))
                        .unwrap_or_default(),
                    l.result_mhz.map_or(String::new(), |m| format!("→ {m}")),
                    if l.is_minimum { "  (floor)" } else { "" }
                )
            })
            .collect();
        rows.sort();
        self.limits_list.set_label(&rows.join("\n"));
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
                &self.sys_volt,
                &self.video_volt,
                &self.ratio,
                &self.mem,
            ] {
                card.unavailable("No NVIDIA clocks table from the daemon");
            }
            self.nvvdd_rail.unavailable("No NVIDIA clocks table from the daemon");
            self.msvdd_rail.unavailable("No NVIDIA clocks table from the daemon");
            return;
        };

        match t.gpc_xbar_ratio {
            Some(r) => self.ratio.load(
                Some(r.current),
                r.min,
                r.max,
                r.factory,
                (r.current - r.factory).abs() > 0.0005,
                &format!(
                    "Current {:.4}   Factory {:.4}   Range {:.2}…{:.2}",
                    r.current, r.factory, r.min, r.max
                ),
            ),
            None => self
                .ratio
                .unavailable("No GPC→XBAR ratio relation found by the daemon"),
        }
        for (card, offset, what) in [
            (&self.sys_volt, t.sys_voltage_offset.as_ref(), "SYS"),
            (&self.video_volt, t.video_voltage_offset.as_ref(), "video"),
        ] {
            load_offset(card, offset, "mV", &format!("No {what} domain with a known rail"));
        }
        for (card, index) in [(&self.nvvdd_rail, 0u8), (&self.msvdd_rail, 1u8)] {
            match t.voltage_rails.iter().find(|r| r.index == index) {
                Some(r) => card.load(r),
                None => card.unavailable("Rail objects not available on this driver"),
            }
        }

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
        self.nvvdd_rail.apply(config);
        self.msvdd_rail.apply(config);
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
