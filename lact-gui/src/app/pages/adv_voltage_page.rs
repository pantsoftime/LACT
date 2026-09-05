//! "Advanced voltage & XBAR" page.
//!
//! An mVolt+-style dashboard for the NVIDIA clock domains and rail offsets
//! that NVML does not expose (XBAR / SYS / video frequency offsets, the MSVDD
//! rail offset), plus a read-only view of what the Overclocking page already
//! manages so the whole picture sits on one screen. The full mVolt+ option set
//! is laid out now; controls without a Linux mechanism yet are present but
//! insensitive, with a tooltip saying what is missing, so the page can be
//! wired up over time without moving anything.
//!
//! Values flow like every other LACT page: edits mark the global settings as
//! changed, `apply_clocks_config` folds them into the pending config on Apply,
//! and the daemon's confirm-or-revert timer covers the result.

use crate::app::msg::AppMsg;
use crate::app::pages::PageUpdate;
use gtk::prelude::*;
use lact_schema::request::{ClockspeedType, SetClocksCommand};
use lact_schema::{ClocksTable, DeviceStats, NvidiaClockOffset, NvidiaClocksTable, config};
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt};
use std::sync::Arc;

/// Highest XBAR offset the correctness harness has passed on the reference
/// card. Going above it needs the explicit switch on the page.
const VALIDATED_MAX_XBAR_MHZ: f64 = 300.0;

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

/// One editable offset: title, enable switch, spin + slider on a shared
/// adjustment, and a "current / range" line.
struct OffsetCard {
    frame: gtk::Frame,
    switch: gtk::Switch,
    adjustment: gtk::Adjustment,
    current_label: gtk::Label,
    clock_type: ClockspeedType,
    unit: &'static str,
}

impl OffsetCard {
    fn new(
        title: &str,
        unit: &'static str,
        step: f64,
        clock_type: ClockspeedType,
        note: &str,
        note_is_warning: bool,
        sender: &ComponentSender<AdvVoltagePage>,
    ) -> Self {
        let adjustment = gtk::Adjustment::new(0.0, -1000.0, 1000.0, step, step * 5.0, 0.0);
        let switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Enabled: the value below is applied. Off: stock (0).")
            .build();
        let current_label = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .label("Current: — ")
            .build();

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
            .tooltip_text("Set to 0")
            .build();

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(
            &gtk::Label::builder()
                .label("Target")
                .css_classes(["dim-label"])
                .build(),
        );
        row.append(&spin);
        row.append(&scale);
        row.append(&gtk::Label::new(Some(unit)));
        row.append(&default_button);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title_label = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        header.append(&title_label);
        header.append(&switch);

        let note_label = gtk::Label::builder()
            .label(note)
            .xalign(0.0)
            .wrap(true)
            .css_classes(if note_is_warning {
                ["caption", "warning"]
            } else {
                ["caption", "dim-label"]
            })
            .build();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_all(10);
        body.append(&header);
        body.append(&current_label);
        body.append(&row);
        body.append(&note_label);

        let frame = gtk::Frame::builder().child(&body).build();

        // Any edit marks the global settings as changed, like every other page.
        {
            let sender = sender.clone();
            adjustment.connect_value_changed(move |_| {
                let _ = sender.output(AppMsg::SettingsChanged);
            });
        }
        {
            let sender = sender.clone();
            let row = row.clone();
            switch.connect_active_notify(move |switch| {
                row.set_sensitive(switch.is_active());
                let _ = sender.output(AppMsg::SettingsChanged);
            });
        }
        {
            let adjustment = adjustment.clone();
            default_button.connect_clicked(move |_| adjustment.set_value(0.0));
        }
        row.set_sensitive(false);

        Self {
            frame,
            switch,
            adjustment,
            current_label,
            clock_type,
            unit,
        }
    }

    /// Load bounds and the currently applied value from the daemon's table.
    /// `None` means the daemon does not offer this control on this system.
    fn set_from(&self, offset: Option<&NvidiaClockOffset>) {
        match offset {
            Some(offset) => {
                self.adjustment.set_lower(f64::from(offset.min));
                self.adjustment.set_upper(f64::from(offset.max));
                self.adjustment.set_value(f64::from(offset.current));
                self.switch.set_active(offset.current != 0);
                self.current_label.set_label(&format!(
                    "Current {:+} {}   Driver range {}…{} {}",
                    offset.current, self.unit, offset.min, offset.max, self.unit
                ));
                self.frame.set_sensitive(true);
            }
            None => {
                self.current_label.set_label(
                    "Not available: the daemon could not enable the RM interface on this driver",
                );
                self.frame.set_sensitive(false);
            }
        }
    }

    fn command(&self) -> SetClocksCommand {
        SetClocksCommand {
            r#type: self.clock_type,
            value: if self.switch.is_active() {
                #[allow(clippy::cast_possible_truncation)]
                Some(self.adjustment.value().round() as i32)
            } else {
                None
            },
        }
    }
}

/// A control mVolt+ has that has no Linux mechanism yet: same shape as an
/// offset card, permanently insensitive, with the reason in a tooltip.
fn placeholder_card(title: &str, live: &str, why: &str) -> gtk::Frame {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build(),
    );
    header.append(
        &gtk::Label::builder()
            .label("Not wired")
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_all(10);
    body.append(&header);
    body.append(
        &gtk::Label::builder()
            .label(live)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(
        &gtk::Label::builder()
            .label("Target")
            .css_classes(["dim-label"])
            .build(),
    );
    row.append(&gtk::SpinButton::with_range(0.0, 1.0, 1.0));
    row.append(&gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        0.0,
        1.0,
        1.0,
    ));
    row.set_sensitive(false);
    body.append(&row);
    body.append(
        &gtk::Label::builder()
            .label(why)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    let frame = gtk::Frame::builder().child(&body).tooltip_text(why).build();
    frame.add_css_class("dim-label");
    frame
}

/// A value owned by another page (or by the driver), shown read-only.
fn readonly_card(title: &str, owner: &str, value_label: &gtk::Label, note: &str) -> gtk::Frame {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build(),
    );
    header.append(
        &gtk::Label::builder()
            .label(owner)
            .css_classes(["caption", "accent"])
            .build(),
    );
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_all(10);
    body.append(&header);
    value_label.set_xalign(0.0);
    value_label.add_css_class("title-3");
    body.append(value_label);
    body.append(
        &gtk::Label::builder()
            .label(note)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    gtk::Frame::builder().child(&body).build()
}

fn section_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .margin_top(6)
        .css_classes(["heading", "accent"])
        .build()
}

fn card_grid() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(1)
        .max_children_per_line(3)
        .homogeneous(true)
        .row_spacing(8)
        .column_spacing(8)
        .build()
}

fn tele_tile(name: &str) -> (gtk::Box, gtk::Label) {
    let value = gtk::Label::builder()
        .label("—")
        .xalign(0.0)
        .css_classes(["title-2", "numeric"])
        .build();
    let tile = gtk::Box::new(gtk::Orientation::Vertical, 2);
    tile.set_margin_all(8);
    tile.append(
        &gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    tile.append(&value);
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("card");
    frame.append(&tile);
    (frame, value)
}

pub struct AdvVoltagePage {
    content: gtk::Box,
    status_label: gtk::Label,
    tele_gpc: gtk::Label,
    tele_xbar: gtk::Label,
    tele_sys: gtk::Label,
    tele_video: gtk::Label,
    tele_mem: gtk::Label,
    tele_ratio: gtk::Label,
    tele_power: gtk::Label,
    xbar: OffsetCard,
    msvdd: OffsetCard,
    sys: OffsetCard,
    video: OffsetCard,
    core_value: gtk::Label,
    mem_value: gtk::Label,
    power_value: gtk::Label,
    vboost_value: gtk::Label,
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
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_margin_all(15);
        content.set_margin_top(20);

        // ---- status strip
        let status_label = gtk::Label::builder()
            .label("Waiting for the daemon…")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        content.append(&status_label);

        // ---- live telemetry row
        let tele = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        tele.set_homogeneous(true);
        let (t, tele_gpc) = tele_tile("GPC");
        tele.append(&t);
        let (t, tele_xbar) = tele_tile("XBAR");
        tele.append(&t);
        let (t, tele_sys) = tele_tile("SYS");
        tele.append(&t);
        let (t, tele_video) = tele_tile("Video");
        tele.append(&t);
        let (t, tele_mem) = tele_tile("Memory");
        tele.append(&t);
        let (t, tele_ratio) = tele_tile("XBAR / GPC");
        tele.append(&t);
        let (t, tele_power) = tele_tile("Power");
        tele.append(&t);
        content.append(&tele);

        // ---- MSVDD / fabric
        content.append(&section_label("MSVDD / Fabric"));
        let fabric = card_grid();
        let xbar = OffsetCard::new(
            "XBAR clock offset",
            "MHz",
            10.0,
            ClockspeedType::XbarClockOffset,
            "Verified by readback and CLK_MEASURE_FREQ. Above the validated max only with the guard switch below.",
            false,
            &sender,
        );
        let msvdd = OffsetCard::new(
            "XBAR voltage offset (MSVDD)",
            "mV",
            5.0,
            ClockspeedType::MsvddOffset,
            "+20 mV lowered XBAR ~31 MHz on its own and did not extend the ceiling. Not free headroom.",
            true,
            &sender,
        );
        let sys = OffsetCard::new(
            "SYS clock offset",
            "MHz",
            10.0,
            ClockspeedType::SysClockOffset,
            "Domain verified. Not validated by the correctness harness at any positive value.",
            false,
            &sender,
        );
        let video = OffsetCard::new(
            "Video clock offset",
            "MHz",
            10.0,
            ClockspeedType::VideoClockOffset,
            "NVENC / NVDEC only. Verified domain; not harness-validated.",
            false,
            &sender,
        );
        fabric.append(&xbar.frame);
        fabric.append(&msvdd.frame);
        fabric.append(&sys.frame);
        fabric.append(&video.frame);
        fabric.append(&placeholder_card(
            "MSVDD clock ratio",
            "Measured XBAR / GPC ratio is shown in the telemetry row",
            "mVolt+ sets the ceiling of the MSVDD-domain clocks as a ratio of core. The control has not been found on Linux.",
        ));
        fabric.append(&placeholder_card(
            "MSVDD voltage range",
            "No voltage readback on this driver (NVML: not supported)",
            "NVAPI range control on Windows; no Linux path found yet.",
        ));
        content.append(&fabric);

        // ---- NVVDD / core
        content.append(&section_label("NVVDD / Core"));
        let core = card_grid();
        let core_value = gtk::Label::new(Some("—"));
        core.append(&readonly_card(
            "Core clock offset",
            "Overclocking page",
            &core_value,
            "Applied through NVML. Set it on the Overclocking page; shown here so the picture is complete.",
        ));
        core.append(&placeholder_card(
            "Core voltage offset (NVVDD)",
            "Rail 0 accepts a write; no measured effect on any clock",
            "Left unwired until its effect is understood.",
        ));
        core.append(&placeholder_card(
            "NVVDD voltage range",
            "No voltage readback on this driver",
            "Same NVAPI dependency as the MSVDD range.",
        ));
        content.append(&core);

        // ---- memory / misc
        content.append(&section_label("Memory / VRAM  ·  Miscellaneous"));
        let misc = card_grid();
        let mem_value = gtk::Label::new(Some("—"));
        misc.append(&readonly_card(
            "Memory clock offset",
            "Overclocking page",
            &mem_value,
            "The RM block has a second, independent memory register. Deliberately not exposed: one owner.",
        ));
        let power_value = gtk::Label::new(Some("—"));
        misc.append(&readonly_card(
            "Power limit",
            "Overclocking page",
            &power_value,
            "At the cap, extra voltage lowers clocks instead of raising power.",
        ));
        let vboost_value = gtk::Label::new(Some("—"));
        misc.append(&readonly_card(
            "Voltage boost",
            "Overclocking page",
            &vboost_value,
            "Bounded V/F limit shift via NVAPI. Already in LACT; linked, not duplicated.",
        ));
        misc.append(&placeholder_card(
            "Boost lock",
            "—",
            "Lock the core clock (NVML locked clocks). Easy to wire; deliberately not yet.",
        ));
        misc.append(&placeholder_card(
            "Extended voltage",
            "—",
            "mVolt+ extended voltage demand offsets per rail. No Linux path found.",
        ));
        content.append(&misc);

        // ---- validation / guard rails
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
        let run_button = gtk::Button::builder()
            .label("Run correctness check")
            .sensitive(false)
            .tooltip_text("Launches xbar_verify.py against the applied setting. Not wired into the GUI yet — run it from a terminal.")
            .build();
        guard_box.append(&run_button);
        content.append(&gtk::Frame::builder().child(&guard_box).build());
        {
            let adjustment = xbar.adjustment.clone();
            guard_switch.connect_active_notify(move |switch| {
                let cap = if switch.is_active() {
                    1000.0
                } else {
                    VALIDATED_MAX_XBAR_MHZ
                };
                adjustment.set_upper(cap.min(adjustment.upper().max(cap)));
                if !switch.is_active() && adjustment.value() > VALIDATED_MAX_XBAR_MHZ {
                    adjustment.set_value(VALIDATED_MAX_XBAR_MHZ);
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
            tele_power,
            xbar,
            msvdd,
            sys,
            video,
            core_value,
            mem_value,
            power_value,
            vboost_value,
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
                PageUpdate::Info(info) => {
                    self.status_label.set_label(&format!(
                        "Driver {}   ·   VBIOS {}   ·   waiting for clock table",
                        info.driver,
                        info.vbios_version.as_deref().unwrap_or("unknown")
                    ));
                }
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
        let mhz = |v: Option<u64>| v.map_or("—".to_owned(), |v| format!("{v} MHz"));
        let c = &stats.clockspeed;
        self.tele_gpc.set_label(&mhz(c.gpu_clockspeed));
        self.tele_xbar.set_label(&mhz(c.xbar_clockspeed));
        self.tele_sys.set_label(&mhz(c.sys_clockspeed));
        self.tele_video.set_label(&mhz(c.video_clockspeed));
        self.tele_mem.set_label(&mhz(c.vram_clockspeed));
        self.tele_ratio
            .set_label(&match (c.xbar_clockspeed, c.gpu_clockspeed) {
                (Some(x), Some(g)) if g > 0 => format!("{:.3}", x as f64 / g as f64),
                _ => "—".to_owned(),
            });
        self.tele_power.set_label(&match (
            stats.power.average.or(stats.power.current),
            stats.power.cap_current,
        ) {
            (Some(p), Some(cap)) => format!("{p:.0} / {cap:.0} W"),
            (Some(p), None) => format!("{p:.0} W"),
            _ => "—".to_owned(),
        });
        self.power_value.set_label(&match stats.power.cap_current {
            Some(cap) => format!("{cap:.0} W"),
            None => "—".to_owned(),
        });
    }

    fn show_table(&self, table: Option<&NvidiaClocksTable>) {
        let available = table.is_some_and(|t| t.xbar_offset.is_some());
        self.xbar
            .set_from(table.and_then(|t| t.xbar_offset.as_ref()));
        self.msvdd
            .set_from(table.and_then(|t| t.msvdd_offset.as_ref()));
        self.sys.set_from(table.and_then(|t| t.sys_offset.as_ref()));
        self.video
            .set_from(table.and_then(|t| t.video_offset.as_ref()));

        // Keep the XBAR guard in force after the table refreshes the bounds.
        if !self.guard_switch.is_active() && self.xbar.adjustment.upper() > VALIDATED_MAX_XBAR_MHZ {
            self.xbar.adjustment.set_upper(VALIDATED_MAX_XBAR_MHZ);
        }

        if let Some(t) = table {
            let first_current = |m: &indexmap::IndexMap<u32, NvidiaClockOffset>| {
                m.values().map(|o| o.current).max()
            };
            self.core_value
                .set_label(&match first_current(&t.gpu_offsets) {
                    Some(v) => format!("{v:+} MHz"),
                    None => "—".to_owned(),
                });
            self.mem_value
                .set_label(&match first_current(&t.mem_offsets) {
                    Some(v) => format!("{v:+} MHz"),
                    None => "—".to_owned(),
                });
            self.vboost_value.set_label(&match t.voltage_boost {
                Some(b) => format!("{} %", b.current),
                None => "—".to_owned(),
            });
            let domains = t
                .rm_clock_domains
                .iter()
                .map(|d| {
                    format!(
                        "{}{}",
                        d.name,
                        if d.controllable && d.offset_range_mhz > 0 {
                            format!(" ±{}", d.offset_range_mhz)
                        } else {
                            String::new()
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            if available {
                self.status_label.set_label(&format!(
                    "RM ClockClient interface: layout verified.   Domains: {domains}"
                ));
            } else {
                self.status_label.set_label(
                    "RM ClockClient interface unavailable on this driver — the daemon refused to enable it. \
                     Frequency controls are disabled; the rest of the page is informational.",
                );
            }
        }
    }

    /// Fold this page's values into the pending config. Called from the app's
    /// Apply path alongside the Overclocking page.
    pub fn apply_clocks_config(&self, config: &mut config::ClocksConfiguration) {
        if self.table.as_ref().is_none_or(|t| t.xbar_offset.is_none()) {
            return;
        }
        for card in [&self.xbar, &self.sys, &self.video, &self.msvdd] {
            config.apply_clocks_command(&card.command());
        }
    }
}
