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
use crate::APP_BROKER;
use crate::app::pages::PageUpdate;
use gtk::prelude::*;
use lact_client::DaemonClient;
use lact_schema::boot_guard::{BootGuardConfig, BootGuardStatus};
use lact_schema::config::GpuConfig;
use lact_schema::request::{ClockspeedType, SetClocksCommand};
use lact_schema::{
    ClocksTable, DeviceStats, NvidiaClockOffset, NvidiaClocksTable, NvidiaDomainVfCurve,
    NvidiaMemoryTimingRow, NvidiaThermalSensor, NvidiaVoltageRail, RailLimit,
};
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt};
use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where the tooling repo (harness, loads, probes) lives; overridable.
const TOOLS_DIR_ENV: &str = "LACT_ADV_TOOLS_DIR";
const TOOLS_DIR_DEFAULT: &str = "claude_workspace/linux_mvolt";
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
    /// Boot guard status, forwarded from the app's poll when it changes
    BootGuard(Arc<BootGuardStatus>),
    /// Profile names for the fallback picker
    Profiles(Vec<String>),
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

    /// Refresh the caption line only (live readings), leaving edits alone.
    fn set_caption(&self, text: &str) {
        self.current_label.set_label(text);
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

/// Runs the tooling repo's scripts from the page: the correctness harness,
/// the baseline rebuild, a steady load and the post-driver-update check.
/// Each run is a shell pipeline in its own process group (so Stop ends all
/// of it), writing to a log file under `<tools>/gui_tests/` that the page
/// tails once a second. The verdict is read out of the log when it ends.
struct TestRunner {
    tools_dir: PathBuf,
    venv: String,
    status: gtk::Label,
    buffer: gtk::TextBuffer,
    buttons: Rc<RefCell<Vec<gtk::Button>>>,
    stop: gtk::Button,
    child: Rc<RefCell<Option<(Child, String, Instant, PathBuf)>>>,
}

impl TestRunner {
    fn tools_dir() -> PathBuf {
        std::env::var_os(TOOLS_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(TOOLS_DIR_DEFAULT)
            })
    }

    /// A string field of the tooling's own config (`linuxvolt.json`).
    fn config_str(tools_dir: &std::path::Path, key: &str) -> Option<String> {
        fs::read_to_string(tools_dir.join("linuxvolt.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get(key)?.as_str().map(str::to_owned))
            .filter(|s| !s.trim().is_empty())
    }

    /// The torch venv the harness needs, from the tooling's own config.
    fn venv(tools_dir: &std::path::Path) -> String {
        Self::config_str(tools_dir, "venv_python").unwrap_or_else(|| "python3".to_owned())
    }

    /// The gaming-style load: `furmark_cmd` from the tooling config if set,
    /// else a 60 s FurMark 2 GL benchmark in a fixed 1920x1080 window (a 1440-tall
    /// window gets clipped by the panel on a 1440p screen) when the binary is
    /// installed. Its stdout carries the SCORE and FPS lines the status
    /// line reads out.
    fn furmark_cmd(tools_dir: &std::path::Path) -> Option<String> {
        Self::config_str(tools_dir, "furmark_cmd").or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .any(|dir| dir.join("furmark").is_file())
                    .then(|| {
                        // --vsync 0 alone is ignored: the NVIDIA GL driver syncs to
                        // the display (240 FPS cap on the 491CQP) unless told not to.
                        "__GL_SYNC_TO_VBLANK=0 furmark --demo furmark-gl --width 1920 --height 1080 --no-resize --vsync 0 --benchmark --duration-ms 60000 --no-score-box"
                            .to_owned()
                    })
            })
        })
    }

    fn new() -> (Self, gtk::Frame) {
        let tools_dir = Self::tools_dir();
        let venv = Self::venv(&tools_dir);
        let available = tools_dir.join("xbar_verify.py").is_file();

        // Only directory names on the visible line: full paths belong in the
        // tooltip, not in every screenshot of the page.
        let short = |p: &std::path::Path| {
            p.file_name()
                .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
        };
        let venv_short = std::path::Path::new(&venv)
            .ancestors()
            .nth(2)
            .map_or_else(|| venv.clone(), short);
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .label(&if available {
                format!("Idle   ·   tooling {}   ·   venv {venv_short}", short(&tools_dir))
            } else {
                format!("Tooling not found (set {TOOLS_DIR_ENV})")
            })
            .tooltip_text(&format!("Tooling directory: {}\nPython: {venv}", tools_dir.display()))
            .css_classes(["caption", "dim-label"])
            .build();
        let buffer = gtk::TextBuffer::new(None);
        let view = gtk::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .left_margin(6)
            .right_margin(6)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(150)
            .max_content_height(260)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .build();
        let stop = gtk::Button::builder()
            .label("Stop")
            .sensitive(false)
            .css_classes(["destructive-action"])
            .tooltip_text("Ends the running test (its whole process group).")
            .build();

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_all(8);
        body.append(&row);
        body.append(&status);
        body.append(&scroller);
        let frame = gtk::Frame::builder().child(&body).build();

        let runner = Self {
            tools_dir,
            venv,
            status,
            buffer,
            buttons: Rc::new(RefCell::new(Vec::new())),
            stop: stop.clone(),
            child: Rc::new(RefCell::new(None)),
        };
        let v = format!("'{}'", runner.venv);
        let furmark = Self::furmark_cmd(&runner.tools_dir);
        let tests: [(&str, &str, String, bool); 6] = [
            (
                "Correctness check",
                "Runs the harness at the setting that is applied right now and compares it bit for bit \
                 with the stock baseline (stock_a.json). ~2.5 min at ~400 W. MATCH or SILENT CORRUPTION.",
                format!("{v} xbar_verify.py run --out gui_check.json && {v} xbar_verify.py compare stock_a.json gui_check.json"),
                available,
            ),
            (
                "Rebuild baseline",
                "Two stock runs plus the probe reference (~6 min). Refuses unless every RM offset is 0 \
                 and 4 GiB of VRAM is free — turn XBAR off and apply first.",
                format!("VENV_PYTHON={v} sh rebuild_baseline.sh"),
                available,
            ),
            (
                "Steady load (2 min)",
                "Duty-cycled matmul + memory sweep that keeps the card in P0 below the power cap; \
                 what the clock probes and the ratio A/B were measured under.",
                format!("{v} steady_load.py --seconds 120"),
                available,
            ),
            (
                "Torch bench",
                "Throughput, not correctness: a 32B-class decoder run token by token against a KV cache \
                 (LLM decode — bandwidth-bound, the path XBAR sits on) then a batched prefill pass (compute). \
                 10 s warm-up + 30 s timed per phase, ~1.5 min total at ~500 W; fixed 22 GB working set so runs compare \
                 (needs that much free VRAM). Status line: decode tok/s + GB/s, prefill TFLOPS. Reference 2026-09-22 at \
                 c150/m6000/x270/660 W: 61.4 tok/s, 1427 GB/s, 205 TFLOPS. Compare runs \
                 at different settings, single runs drift a few percent with temperature.",
                format!("{v} ab_torch_kernels.py --duration 30 --warmup 10 --vram-gb 22"),
                available,
            ),
            (
                "FurMark bench",
                "Gaming-style load: FurMark 2 OpenGL in a 1920x1080 window, 60 s, vsync off. Raster, not \
                 compute, so it exercises the display/raster path the torch tests do not and it is the closest \
                 thing to the 3DMark and game-FPS numbers the forum results are quoted in. The status line reads \
                 out the SCORE and min/avg/max FPS; FurMark prints nothing until it finishes, so the output pane stays \
                 empty for the 60 s. Vsync is off (__GL_SYNC_TO_VBLANK=0), otherwise the display's 240 Hz caps it. Override the command with `furmark_cmd` in the tooling's \
                 linuxvolt.json (e.g. a Vulkan demo, another preset, or a game launcher). \
                 Reference at daily settings, 6 s check run: ~658 FPS (the score is roughly frames rendered, so ~39k for 60 s); a run pinned at 240 FPS was vsync-capped.",
                furmark.clone().unwrap_or_default(),
                furmark.is_some(),
            ),
            (
                "Driver check",
                "The read-only half of the post-driver-update checklist: snapshot the RM surface, \
                 diff it against the previous driver, measure, correlate.",
                "sh after_driver_update.sh".to_owned(),
                available,
            ),
        ];
        for (label, tip, cmd, enabled) in tests {
            let button = gtk::Button::builder()
                .label(label)
                .tooltip_text(tip)
                .sensitive(enabled)
                .build();
            let (name, cmd, r) = (label.to_owned(), cmd.clone(), runner.handles());
            button.connect_clicked(move |_| r.start(&name, &cmd));
            row.append(&button);
            runner.buttons.borrow_mut().push(button);
        }
        row.append(&stop);
        {
            let child = runner.child.clone();
            stop.connect_clicked(move |_| {
                if let Some((c, ..)) = child.borrow().as_ref() {
                    // The child is its own process-group leader; kill the group.
                    let _ = Command::new("kill")
                        .args(["-TERM", "--", &format!("-{}", c.id())])
                        .status();
                }
            });
        }
        (runner, frame)
    }

    /// Clones of what a click handler needs.
    fn handles(&self) -> Self {
        Self {
            tools_dir: self.tools_dir.clone(),
            venv: self.venv.clone(),
            status: self.status.clone(),
            buffer: self.buffer.clone(),
            buttons: self.buttons.clone(),
            stop: self.stop.clone(),
            child: self.child.clone(),
        }
    }

    fn start(&self, name: &str, cmd: &str) {
        if self.child.borrow().is_some() {
            return;
        }
        let log_dir = self.tools_dir.join("gui_tests");
        let started = Instant::now();
        let log = log_dir.join(format!(
            "{}-{}.log",
            name.to_lowercase().replace(' ', "_"),
            gtk::glib::DateTime::now_local()
                .ok()
                .and_then(|t| t.format("%Y%m%d-%H%M%S").ok())
                .map_or_else(|| "now".to_owned(), |t| t.to_string())
        ));
        let spawned = fs::create_dir_all(&log_dir)
            .and_then(|()| fs::File::create(&log))
            .and_then(|file| {
                let err = file.try_clone()?;
                Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(&self.tools_dir)
                    .stdout(Stdio::from(file))
                    .stderr(Stdio::from(err))
                    .process_group(0)
                    .spawn()
            });
        let child = match spawned {
            Ok(child) => child,
            Err(err) => {
                self.status.set_label(&format!("Could not start {name}: {err}"));
                return;
            }
        };
        self.buffer.set_text(&format!("$ {cmd}\n"));
        self.status.set_label(&format!("Running {name}…"));
        for b in self.buttons.borrow().iter() {
            b.set_sensitive(false);
        }
        self.stop.set_sensitive(true);
        *self.child.borrow_mut() = Some((child, name.to_owned(), started, log));

        let r = self.handles();
        gtk::glib::timeout_add_local(Duration::from_secs(1), move || r.poll());
    }

    fn poll(&self) -> gtk::glib::ControlFlow {
        let mut slot = self.child.borrow_mut();
        let Some((child, name, started, log)) = slot.as_mut() else {
            return gtk::glib::ControlFlow::Break;
        };
        let text = read_log(log);
        let tail: Vec<&str> = text.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
        self.buffer.set_text(&tail.join("\n"));
        let elapsed = started.elapsed().as_secs();
        match child.try_wait() {
            Ok(None) => {
                self.status.set_label(&format!("Running {name}… {elapsed} s"));
                gtk::glib::ControlFlow::Continue
            }
            Ok(Some(code)) => {
                // Programs writing to a file block-buffer stdout, so their whole
                // output can land between the read above and the exit being
                // seen (FurMark does exactly that): read the log once more.
                // Not strict UTF-8: FurMark writes its degree sign as a lone
                // Latin-1 byte, which made a strict read drop the whole log.
                let text = read_log(log);
                let tail: Vec<&str> = text.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
                self.buffer.set_text(&tail.join("\n"));
                let verdict = if text.contains("SILENT CORRUPTION") {
                    "SILENT CORRUPTION — the applied setting computes wrong results".to_owned()
                } else if text.contains("MATCH") {
                    "MATCH — bit-identical to the stock baseline".to_owned()
                } else if let Some(summary) = bench_summary(&text) {
                    summary
                } else if code.success() {
                    "finished".to_owned()
                } else {
                    "failed".to_owned()
                };
                self.status.set_label(&format!(
                    "{name}: {verdict}   ({elapsed} s, exit {}, log {})",
                    code.code().unwrap_or(-1),
                    log.display()
                ));
                self.finish(&mut slot)
            }
            Err(err) => {
                self.status.set_label(&format!("{name}: could not wait: {err}"));
                self.finish(&mut slot)
            }
        }
    }

    fn finish(&self, slot: &mut Option<(Child, String, Instant, PathBuf)>) -> gtk::glib::ControlFlow {
        *slot = None;
        for b in self.buttons.borrow().iter() {
            b.set_sensitive(true);
        }
        self.stop.set_sensitive(false);
        gtk::glib::ControlFlow::Break
    }
}

/// A test log as text. Valid UTF-8 is kept as is; any other byte is taken as
/// Latin-1, which is what FurMark writes its degree sign in (0xB0 → °).
fn read_log(path: &std::path::Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        text.extend(chunk.invalid().iter().map(|&b| char::from(b)));
    }
    text
}

/// One-line read-out of a benchmark log: the torch kernel bench prints a
/// single JSON line; FurMark prints `- SCORE : n` and `- FPS (min/avg/max) : a / b / c`.
fn bench_summary(text: &str) -> Option<String> {
    if let Some(json) = text
        .lines()
        .rev()
        .find(|l| l.starts_with('{'))
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
    {
        let f = |k: &str| json.get(k).and_then(serde_json::Value::as_f64);
        if let (Some(tok), Some(gbps), Some(tflops)) =
            (f("decode_tok_s"), f("decode_gbps"), f("prefill_tflops"))
        {
            return Some(format!(
                "decode {tok:.1} tok/s ({gbps:.0} GB/s), prefill {tflops:.1} TFLOPS"
            ));
        }
    }
    let field = |key: &str| {
        text.lines()
            .find(|l| l.trim_start().starts_with("- ") && l.contains(key))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_owned())
    };
    match (field("SCORE"), field("FPS (min/avg/max)")) {
        (Some(score), Some(fps)) => {
            let fps = fps
                .split('/')
                .map(|v| v.trim().parse::<f64>().map_or_else(|_| v.trim().to_owned(), |n| format!("{n:.0}")))
                .collect::<Vec<_>>()
                .join(" / ");
            Some(format!("SCORE {score}, FPS min/avg/max {fps}"))
        }
        (Some(score), None) => Some(format!("SCORE {score}")),
        _ => None,
    }
}

// ------------------------------------------------------------ widget helpers

/// Boot guard controls (this fork): the arm switch, the fallback picker, the
/// one-shot flag, and the trip notice with Resume / Keep. The daemon owns
/// the state; every change is sent at once and the row re-renders from the
/// daemon's answer, so what is shown is always what the daemon has.
struct BootGuardPanel {
    frame: gtk::Frame,
    state: Rc<BootGuardState>,
}

struct BootGuardState {
    client: DaemonClient,
    enabled: gtk::Switch,
    fallback: gtk::DropDown,
    force: gtk::CheckButton,
    status: gtk::Label,
    resume: gtk::Button,
    keep: gtk::Button,
    profiles: RefCell<Vec<String>>,
    last: RefCell<Option<Arc<BootGuardStatus>>>,
    /// Suppresses the change handlers while widgets are set from a status.
    loading: Cell<bool>,
}

const STOCK_LABEL: &str = "Stock (nothing applied)";

impl BootGuardPanel {
    fn new(client: DaemonClient) -> Self {
        let enabled = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text(
                "On: a marker is written before settings are applied and removed on a clean shutdown. \
                 If the marker is still there at the next start, the saved profile is NOT applied; \
                 the fallback below is, and automatic switching stays off until you resume.",
            )
            .build();
        let fallback = gtk::DropDown::from_strings(&[STOCK_LABEL]);
        fallback.set_tooltip_text(Some(
            "What to run after an unclean stop. Stock applies nothing at all; a profile applies that profile's settings.",
        ));
        let force = gtk::CheckButton::builder()
            .label("Fallback at next start")
            .tooltip_text(
                "One-shot: engage at the next daemon start whether or not anything crashed. \
                 Cleared automatically once used. For trying settings you already distrust.",
            )
            .build();
        let status = caption("—", false);
        status.set_hexpand(true);
        status.set_width_chars(-1);
        status.set_max_width_chars(-1);
        let resume = gtk::Button::builder()
            .label("Resume saved profile")
            .css_classes(["suggested-action"])
            .tooltip_text("Apply the saved profile again and restart automatic switching if it was on")
            .visible(false)
            .build();
        let keep = gtk::Button::builder()
            .label("Keep fallback until reboot")
            .tooltip_text("Stay as is and clear the notice; the saved profile applies again at the next boot")
            .visible(false)
            .build();

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        controls.append(&info_icon(
            "Protects against a tuned profile being re-applied at boot after it hung or crashed the system. \
             A login notice is also written to /run/motd.d while engaged.",
            false,
        ));
        controls.append(&enabled);
        controls.append(&gtk::Label::new(Some("Fallback")));
        controls.append(&fallback);
        controls.append(&force);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.append(&status);
        actions.append(&keep);
        actions.append(&resume);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column.set_margin_all(10);
        column.append(&controls);
        column.append(&actions);
        let frame = gtk::Frame::builder().child(&column).css_classes(["card"]).build();

        let state = Rc::new(BootGuardState {
            client,
            enabled,
            fallback,
            force,
            status,
            resume,
            keep,
            profiles: RefCell::new(Vec::new()),
            last: RefCell::new(None),
            loading: Cell::new(false),
        });
        {
            let s = state.clone();
            state.enabled.connect_active_notify(move |_| s.push());
        }
        {
            let s = state.clone();
            state.fallback.connect_selected_notify(move |_| s.push());
        }
        {
            let s = state.clone();
            state.force.connect_toggled(move |_| s.push());
        }
        {
            let s = state.clone();
            state.resume.connect_clicked(move |_| {
                let s = s.clone();
                relm4::spawn_local(async move {
                    let result = s.client.boot_guard_resume().await;
                    let resumed = result.is_ok();
                    s.answer(result);
                    if resumed {
                        // Same as the banner's Resume: show the re-applied profile.
                        APP_BROKER.send(AppMsg::ReloadData { full: false });
                    }
                });
            });
        }
        {
            let s = state.clone();
            state.keep.connect_clicked(move |_| {
                let s = s.clone();
                relm4::spawn_local(async move {
                    let result = s.client.boot_guard_acknowledge().await;
                    s.answer(result);
                });
            });
        }
        Self { frame, state }
    }
}

impl BootGuardState {
    /// Send the widgets' current values to the daemon.
    fn push(self: &Rc<Self>) {
        if self.loading.get() {
            return;
        }
        let selected = self.fallback.selected() as usize;
        let config = BootGuardConfig {
            enabled: self.enabled.is_active(),
            fallback: (selected > 0)
                .then(|| self.profiles.borrow().get(selected - 1).cloned())
                .flatten(),
            force_next_boot: self.force.is_active(),
        };
        let s = self.clone();
        relm4::spawn_local(async move {
            let result = s.client.set_boot_guard(config).await;
            s.answer(result);
        });
    }

    fn answer(&self, result: anyhow::Result<BootGuardStatus>) {
        match result {
            Ok(status) => self.show(&Arc::new(status)),
            Err(err) => {
                self.status.set_label(&format!("Boot guard: {err:#}"));
                self.status.set_css_classes(&["caption", "warning"]);
            }
        }
    }

    /// The profile names, in the daemon's order, for the fallback picker.
    fn set_profiles(&self, names: Vec<String>) {
        if *self.profiles.borrow() == names {
            return;
        }
        let mut items: Vec<&str> = vec![STOCK_LABEL];
        items.extend(names.iter().map(String::as_str));
        self.loading.set(true);
        self.fallback.set_model(Some(&gtk::StringList::new(&items)));
        self.loading.set(false);
        *self.profiles.borrow_mut() = names;
        if let Some(last) = self.last.borrow().clone() {
            self.show(&last);
        }
    }

    fn show(&self, status: &Arc<BootGuardStatus>) {
        *self.last.borrow_mut() = Some(status.clone());
        let cfg = &status.config;
        self.loading.set(true);
        self.enabled.set_active(cfg.enabled);
        self.force.set_active(cfg.force_next_boot);
        let position = cfg
            .fallback
            .as_ref()
            .and_then(|name| self.profiles.borrow().iter().position(|p| p == name))
            .map_or(0, |i| i as u32 + 1);
        self.fallback.set_selected(position);
        self.loading.set(false);

        let usable = status.error.is_none();
        for w in [
            self.enabled.upcast_ref::<gtk::Widget>(),
            self.fallback.upcast_ref(),
            self.force.upcast_ref(),
        ] {
            w.set_sensitive(usable);
        }

        let (text, warning) = boot_guard_text(status);
        self.status.set_label(&text);
        self.status
            .set_css_classes(if warning { &["caption", "warning"] } else { &["caption", "dim-label"] });
        let engaged = status.engaged.as_ref();
        self.resume.set_visible(engaged.is_some());
        self.keep.set_visible(engaged.is_some_and(|t| !t.acknowledged));
    }
}

/// The status line under the boot guard controls.
fn boot_guard_text(status: &BootGuardStatus) -> (String, bool) {
    if let Some(err) = &status.error {
        return (format!("Guard unavailable: {err}"), true);
    }
    if let Some(trip) = &status.engaged {
        let cause = if trip.forced {
            "the one-shot flag was set".to_owned()
        } else {
            format!(
                "the {} while profile '{}' was active",
                if trip.same_boot {
                    "daemon died"
                } else {
                    "system stopped uncleanly"
                },
                trip.profile.as_deref().unwrap_or("default")
            )
        };
        let running = trip
            .applied_fallback
            .as_deref()
            .map_or("stock settings".to_owned(), |p| format!("fallback profile '{p}'"));
        let tail = if trip.acknowledged {
            " Kept until the next boot."
        } else {
            ""
        };
        return (format!("ENGAGED: {cause}. Running {running}.{tail}"), true);
    }
    let mut text = if !status.config.enabled {
        "Off: a crashed profile is re-applied at the next boot.".to_owned()
    } else if status.armed {
        "Armed: an unclean stop from here engages the fallback at the next start.".to_owned()
    } else {
        "Enabled; the marker is written when settings are next applied.".to_owned()
    };
    if status.config.force_next_boot {
        text.push_str(" One-shot set: the fallback engages at the next daemon start.");
    }
    (text, false)
}

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
        RailLimit::Vmin => "VMIN — the rail's minimum-voltage floor. Effect: raising it holds the rail higher at idle (more idle power, no performance); lowering it lets it sag further at idle. Use: normally leave alone.",
        RailLimit::Rel => "REL — the reliability limit, normally the binding ceiling (MAX = tightest of REL / ALT-OP / OV). Effect: raising it lets boost use higher voltage points, if ALT/OP and OV are not tighter. Use: the first limit to raise for peak clock. Observed: −50 mV is the driver default on MSVDD here.",
        RailLimit::AltRel => "ALT/OP — the alternate-reliability / operating limit (Vop), the vendor's operating ceiling. Effect: same as REL; whichever is lower binds. Use: raise together with REL. Risk: above it is extreme-overclocking territory.",
        RailLimit::Ov => "OV — the overvoltage ceiling. Effect: only matters once REL and ALT/OP are both above it. Use: last. Risk: the daemon holds it under the device maximum; nothing here goes past the hardware's own limit.",
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

// --------------------------------------------------------- domain V/F chart

const CURVE_COLOURS: [(f64, f64, f64); 6] = [
    (0.36, 0.55, 0.96), // XBAR blue
    (0.95, 0.60, 0.20), // SYS orange
    (0.30, 0.75, 0.45), // video green
    (0.80, 0.40, 0.80), // PWRCLK purple
    (0.60, 0.60, 0.60), // GPC grey (other rail)
    (0.90, 0.30, 0.30),
];

/// Per-domain, per-point offsets, MHz: domain name → point index → offset.
type CurveOffsets = indexmap::IndexMap<String, indexmap::IndexMap<u8, i32>>;

struct CurveChart {
    frame: gtk::Frame,
    area: gtk::DrawingArea,
    legend: gtk::Box,
    readout: gtk::Label,
    curves: Rc<RefCell<Vec<NvidiaDomainVfCurve>>>,
    enabled: Rc<RefCell<Vec<bool>>>,
    /// The offsets the editor wants: what the card holds until the user
    /// stages a change, then the staged state until Apply or Discard.
    pending: Rc<RefCell<CurveOffsets>>,
    dirty: Rc<Cell<bool>>,
    editor: Rc<CurveEditor>,
    selection: Rc<Cell<Option<(u32, u32)>>>,
    undo: Rc<RefCell<Vec<CurveOffsets>>>,
    /// Each domain's global clock offset, MHz, so the summary can show the
    /// effective offset (global + per-point) and not just the per-point part.
    globals: Rc<RefCell<std::collections::HashMap<String, i32>>>,
}

/// The range editor under the chart: pick an editable curve, a voltage
/// range and an offset. Deliberately range-based rather than point-dragging:
/// the use is holding a fabric clock down (or up) over the voltage region
/// where it misbehaves, which is a range operation.
struct CurveEditor {
    row: gtk::Box,
    domain: gtk::DropDown,
    from_mv: gtk::SpinButton,
    to_mv: gtk::SpinButton,
    offset: gtk::SpinButton,
    summary: gtk::Label,
    names: RefCell<Vec<String>>,
    /// The user has picked a range (clicked the chart or edited From / To).
    /// Until then nothing can be staged: the boxes start at the curve's full
    /// span, and staging on a range nobody chose is how a whole curve gets
    /// shifted by accident.
    has_selection: Cell<bool>,
    /// Set while the boxes are filled programmatically.
    loading: Cell<bool>,
}

impl CurveEditor {
    /// From / To in mV, ordered, if a range has been chosen.
    fn range(&self) -> Option<(u32, u32)> {
        if !self.has_selection.get() {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (a, b) = (self.from_mv.value() as u32, self.to_mv.value() as u32);
        Some((a.min(b), a.max(b)))
    }

    /// Put the boxes back to "nothing chosen" for a curve.
    fn reset_boxes(&self, curve: Option<&NvidiaDomainVfCurve>) {
        self.loading.set(true);
        if let Some(c) = curve
            && let (Some(first), Some(last)) = (c.points.first(), c.points.last())
        {
            self.from_mv.set_range(f64::from(first.voltage_mv), f64::from(last.voltage_mv));
            self.to_mv.set_range(f64::from(first.voltage_mv), f64::from(last.voltage_mv));
            self.from_mv.set_value(f64::from(first.voltage_mv));
            self.to_mv.set_value(f64::from(last.voltage_mv));
            self.offset.set_range(f64::from(c.offset_min_mhz), f64::from(c.offset_max_mhz));
        }
        self.offset.set_value(0.0);
        self.has_selection.set(false);
        self.loading.set(false);
    }

    fn selected(&self) -> Option<String> {
        self.names.borrow().get(self.domain.selected() as usize).cloned()
    }
}

/// The curves as they would look with the pending offsets applied: an
/// editable curve's frequency is its reported one minus the offset the card
/// holds plus the pending offset.
fn effective_curves(curves: &[NvidiaDomainVfCurve], pending: &CurveOffsets) -> Vec<NvidiaDomainVfCurve> {
    curves
        .iter()
        .map(|c| {
            let mut c = c.clone();
            if c.editable {
                let want = pending.get(&c.domain);
                for (i, p) in c.points.iter_mut().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    let target = want.and_then(|m| m.get(&(i as u8))).copied().unwrap_or(0);
                    let freq = i64::from(p.freq_mhz) - i64::from(p.offset_mhz) + i64::from(target);
                    p.freq_mhz = u32::try_from(freq.max(0)).unwrap_or(0);
                    p.offset_mhz = target;
                }
            }
            c
        })
        .collect()
}

fn offsets_summary(
    curves: &[NvidiaDomainVfCurve],
    pending: &CurveOffsets,
    dirty: bool,
    globals: &std::collections::HashMap<String, i32>,
) -> String {
    let mut parts = Vec::new();
    for c in curves.iter().filter(|c| c.editable) {
        let Some(points) = pending.get(&c.domain).filter(|m| m.values().any(|v| *v != 0)) else {
            continue;
        };
        let edited: Vec<(u32, i32)> = points
            .iter()
            .filter(|(_, v)| **v != 0)
            .filter_map(|(i, v)| c.points.get(usize::from(*i)).map(|p| (p.voltage_mv, *v)))
            .collect();
        let (lo, hi) = (
            edited.iter().map(|e| e.0).min().unwrap_or(0),
            edited.iter().map(|e| e.0).max().unwrap_or(0),
        );
        let (omin, omax) = (
            edited.iter().map(|e| e.1).min().unwrap_or(0),
            edited.iter().map(|e| e.1).max().unwrap_or(0),
        );
        let span = |a: i32, b: i32| if a == b { format!("{a:+} MHz") } else { format!("{a:+}…{b:+} MHz") };
        // Per-point offsets stack on the domain's global offset: show both.
        let effective = globals.get(&c.domain).map_or(String::new(), |g| {
            format!(" → effective {} with the {g:+} global", span(omin + g, omax + g))
        });
        parts.push(format!(
            "{}: {} points, {lo}–{hi} mV, per-point {}{effective}",
            c.domain,
            edited.len(),
            span(omin, omax)
        ));
    }
    let state = if dirty { "Staged (Apply to write)" } else { "On the card" };
    if parts.is_empty() {
        format!("{state}: no per-point offsets")
    } else {
        format!("{state}: {}", parts.join("   ·   "))
    }
}

impl CurveChart {
    const MARGIN_L: f64 = 56.0;
    const MARGIN_R: f64 = 14.0;
    const MARGIN_T: f64 = 10.0;
    const MARGIN_B: f64 = 30.0;

    fn new(on_edit: impl Fn() + 'static) -> Self {
        let on_edit: Rc<dyn Fn()> = Rc::new(on_edit);
        let curves: Rc<RefCell<Vec<NvidiaDomainVfCurve>>> = Rc::new(RefCell::new(Vec::new()));
        let enabled: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let pending: Rc<RefCell<CurveOffsets>> = Rc::new(RefCell::new(CurveOffsets::new()));
        let dirty = Rc::new(Cell::new(false));
        // The editor's From..To range in mV, mirrored here for the draw function.
        let selection: Rc<Cell<Option<(u32, u32)>>> = Rc::new(Cell::new(None));
        // Snapshots of `pending` before each staged edit, for Ctrl+Z.
        let undo: Rc<RefCell<Vec<CurveOffsets>>> = Rc::new(RefCell::new(Vec::new()));
        let globals: Rc<RefCell<std::collections::HashMap<String, i32>>> = Rc::new(RefCell::new(std::collections::HashMap::new()));
        let hover: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
        let area = gtk::DrawingArea::builder()
            .content_height(260)
            .hexpand(true)
            .build();
        let readout = caption("Hover over the chart to read a point", false);
        readout.set_width_chars(-1);
        readout.set_max_width_chars(-1);
        let legend = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        {
            let (curves, enabled, hover, pending) = (curves.clone(), enabled.clone(), hover.clone(), pending.clone());
            let selection = selection.clone();
            area.set_draw_func(move |_, cr, w, h| {
                let shown = effective_curves(&curves.borrow(), &pending.borrow());
                let (w, h) = (f64::from(w), f64::from(h));
                // The editor's voltage range, as a band behind the curves.
                if let (Some((lo, hi)), Some(r)) = (selection.get(), Self::ranges(&shown, &enabled.borrow())) {
                    let (x0, _) = Self::project(f64::from(lo), 0.0, w, h, r);
                    let (x1, _) = Self::project(f64::from(hi), 0.0, w, h, r);
                    cr.set_source_rgba(0.5, 0.6, 0.9, 0.18);
                    cr.rectangle(x0.min(x1) - 2.0, Self::MARGIN_T, (x1 - x0).abs() + 4.0, h - Self::MARGIN_T - Self::MARGIN_B);
                    let _ = cr.fill();
                }
                Self::draw(cr, w, h, &shown, &enabled.borrow(), hover.get());
            });
        }
        {
            let motion = gtk::EventControllerMotion::new();
            {
                let (hover, area2, readout, curves, enabled, pending) =
                    (hover.clone(), area.clone(), readout.clone(), curves.clone(), enabled.clone(), pending.clone());
                motion.connect_motion(move |_, x, y| {
                    hover.set(Some((x, y)));
                    let w = f64::from(area2.width());
                    let h = f64::from(area2.height());
                    let shown = effective_curves(&curves.borrow(), &pending.borrow());
                    readout.set_label(&Self::nearest(x, y, w, h, &shown, &enabled.borrow()));
                    area2.queue_draw();
                });
            }
            {
                let (hover, area2) = (hover.clone(), area.clone());
                motion.connect_leave(move |_| {
                    hover.set(None);
                    area2.queue_draw();
                });
            }
            area.add_controller(motion);
        }
        // ---- the range editor
        let domain = gtk::DropDown::from_strings(&[]);
        domain.set_tooltip_text(Some("The curve to edit: GPC, XBAR, SYS and video accept per-point offsets"));
        let spin = |lo: f64, hi: f64, step: f64, tip: &str| {
            let s = gtk::SpinButton::with_range(lo, hi, step);
            s.set_width_chars(5);
            s.set_tooltip_text(Some(tip));
            s
        };
        let from_mv = spin(0.0, 2000.0, 5.0, "First voltage of the range, mV (snaps to the nearest curve point)");
        let to_mv = spin(0.0, 2000.0, 5.0, "Last voltage of the range, mV");
        let offset = spin(-1000.0, 300.0, 5.0, "Frequency offset for every point in the range, MHz; adds to the domain's global clock offset. Negative = slower at those voltages (the safe direction).");
        let set_button = gtk::Button::builder()
            .label("Set offset on range")
            .tooltip_text("Stage the offset on every curve point between From and To (replaces, does not add)")
            .build();
        let flatten_button = gtk::Button::builder()
            .label("Flatten above From")
            .tooltip_text("Stage offsets that hold every point above From at From's frequency — the fabric equivalent of the core undervolt flatten")
            .build();
        let clear_button = gtk::Button::builder()
            .label("Clear curve")
            .css_classes(["flat"])
            .tooltip_text("Stage zero offsets for the selected curve")
            .build();
        let discard_button = gtk::Button::builder()
            .label("Discard")
            .css_classes(["flat"])
            .tooltip_text("Drop the staged edits and show what the card holds")
            .build();
        let summary = caption("On the card: no per-point offsets", false);
        summary.set_width_chars(-1);
        summary.set_max_width_chars(-1);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for w in [
            gtk::Label::new(Some("Edit")).upcast::<gtk::Widget>(),
            domain.clone().upcast(),
            gtk::Label::new(Some("From")).upcast(),
            from_mv.clone().upcast(),
            gtk::Label::new(Some("to")).upcast(),
            to_mv.clone().upcast(),
            gtk::Label::new(Some("mV   offset")).upcast(),
            offset.clone().upcast(),
            gtk::Label::new(Some("MHz")).upcast(),
            set_button.clone().upcast(),
            flatten_button.clone().upcast(),
            clear_button.clone().upcast(),
            discard_button.clone().upcast(),
        ] {
            row.append(&w);
        }
        let editor = Rc::new(CurveEditor {
            row,
            domain,
            from_mv,
            to_mv,
            offset,
            summary,
            names: RefCell::new(Vec::new()),
            has_selection: Cell::new(false),
            loading: Cell::new(false),
        });

        // One closure applies an edit to the selected curve's pending map.
        let stage = {
            let (curves, pending, dirty, editor, area, on_edit, globals, undo) = (
                curves.clone(),
                pending.clone(),
                dirty.clone(),
                editor.clone(),
                area.clone(),
                on_edit.clone(),
                globals.clone(),
                undo.clone(),
            );
            Rc::new(move |edit: &dyn Fn(&NvidiaDomainVfCurve, &mut indexmap::IndexMap<u8, i32>)| {
                let Some(name) = editor.selected() else { return };
                let curves_ref = curves.borrow();
                let Some(curve) = curves_ref.iter().find(|c| c.domain == name && c.editable) else {
                    return;
                };
                {
                    let mut pending = pending.borrow_mut();
                    {
                        let mut undo = undo.borrow_mut();
                        undo.push(pending.clone());
                        if undo.len() > 50 {
                            undo.remove(0);
                        }
                    }
                    let points = pending.entry(name.clone()).or_default();
                    edit(curve, points);
                    points.retain(|_, v| *v != 0);
                    if points.is_empty() {
                        pending.shift_remove(&name);
                    }
                }
                dirty.set(true);
                editor
                    .summary
                    .set_label(&offsets_summary(&curves_ref, &pending.borrow(), true, &globals.borrow()));
                area.queue_draw();
                on_edit();
            })
        };
        {
            let (stage, editor) = (stage.clone(), editor.clone());
            set_button.connect_clicked(move |_| {
                let Some((lo, hi)) = editor.range() else {
                    editor.summary.set_label("Select a range first: click a point on the chart (Shift+click extends), or edit From / To");
                    return;
                };
                #[allow(clippy::cast_possible_truncation)]
                let mhz = editor.offset.value() as i32;
                stage(&|curve, points| {
                    let mhz = mhz.clamp(curve.offset_min_mhz, curve.offset_max_mhz);
                    for (i, p) in curve.points.iter().enumerate() {
                        if (lo..=hi).contains(&p.voltage_mv) {
                            #[allow(clippy::cast_possible_truncation)]
                            points.insert(i as u8, mhz);
                        }
                    }
                });
            });
        }
        {
            let (stage, editor) = (stage.clone(), editor.clone());
            flatten_button.connect_clicked(move |_| {
                let Some((from, _)) = editor.range() else {
                    editor.summary.set_label("Select the point to flatten from first: click it on the chart");
                    return;
                };
                stage(&|curve, points| {
                    let Some(k) = curve.points.iter().position(|p| p.voltage_mv >= from) else {
                        return;
                    };
                    // Frequencies without any per-point offset, then the target.
                    let base = |i: usize| i64::from(curve.points[i].freq_mhz) - i64::from(curve.points[i].offset_mhz);
                    #[allow(clippy::cast_possible_truncation)]
                    let target = base(k) + i64::from(points.get(&(k as u8)).copied().unwrap_or(0));
                    for i in k + 1..curve.points.len() {
                        #[allow(clippy::cast_possible_truncation)]
                        let want = (target - base(i)).clamp(i64::from(curve.offset_min_mhz), i64::from(curve.offset_max_mhz)) as i32;
                        #[allow(clippy::cast_possible_truncation)]
                        points.insert(i as u8, want);
                    }
                });
            });
        }
        {
            let stage = stage.clone();
            clear_button.connect_clicked(move |_| stage(&|_, points| points.clear()));
        }
        {
            let (curves, pending, dirty, editor, area, globals, selection) = (
                curves.clone(),
                pending.clone(),
                dirty.clone(),
                editor.clone(),
                area.clone(),
                globals.clone(),
                selection.clone(),
            );
            discard_button.connect_clicked(move |_| {
                *pending.borrow_mut() = applied_offsets(&curves.borrow());
                dirty.set(false);
                editor
                    .summary
                    .set_label(&offsets_summary(&curves.borrow(), &pending.borrow(), false, &globals.borrow()));
                // Also put the entry boxes back and drop the selection.
                let name = editor.selected();
                editor.reset_boxes(curves.borrow().iter().find(|c| c.editable && Some(&c.domain) == name.as_ref()));
                selection.set(None);
                area.queue_draw();
            });
        }

        {
            let (editor2, curves, selection, area) = (editor.clone(), curves.clone(), selection.clone(), area.clone());
            editor.domain.connect_selected_notify(move |_| {
                let name = editor2.selected();
                editor2.reset_boxes(curves.borrow().iter().find(|c| c.editable && Some(&c.domain) == name.as_ref()));
                selection.set(None);
                area.queue_draw();
            });
        }

        // ---- mouse and keyboard on the chart
        // Click sets the range to the nearest point of the selected curve,
        // Shift+click extends it; the chart takes focus so the keys work.
        area.set_focusable(true);
        {
            let sync = {
                let (editor, selection, area) = (editor.clone(), selection.clone(), area.clone());
                move || {
                    if editor.loading.get() {
                        return;
                    }
                    // Editing From / To by hand is choosing a range.
                    editor.has_selection.set(true);
                    selection.set(editor.range());
                    area.queue_draw();
                }
            };
            let s1 = sync.clone();
            editor.from_mv.connect_value_changed(move |_| s1());
            let s2 = sync.clone();
            editor.to_mv.connect_value_changed(move |_| s2());
        }
        {
            // The offset box is live: its value is staged, absolute, on the
            // chosen range, so "select, type, Apply" works and cannot double.
            let (stage, editor2) = (stage.clone(), editor.clone());
            editor.offset.connect_value_changed(move |spin| {
                if editor2.loading.get() {
                    return;
                }
                let Some((lo, hi)) = editor2.range() else { return };
                #[allow(clippy::cast_possible_truncation)]
                let mhz = spin.value() as i32;
                stage(&|curve, points| {
                    let mhz = mhz.clamp(curve.offset_min_mhz, curve.offset_max_mhz);
                    for (i, p) in curve.points.iter().enumerate() {
                        if (lo..=hi).contains(&p.voltage_mv) {
                            #[allow(clippy::cast_possible_truncation)]
                            points.insert(i as u8, mhz);
                        }
                    }
                });
            });
        }
        {
            let click = gtk::GestureClick::new();
            let (curves, enabled, pending, editor, area2, selection) = (
                curves.clone(),
                enabled.clone(),
                pending.clone(),
                editor.clone(),
                area.clone(),
                selection.clone(),
            );
            click.connect_pressed(move |gesture, _, x, _| {
                area2.grab_focus();
                let shown = effective_curves(&curves.borrow(), &pending.borrow());
                let Some((v_min, v_max, _)) = Self::ranges(&shown, &enabled.borrow()) else {
                    return;
                };
                let w = f64::from(area2.width());
                let span = w - Self::MARGIN_L - Self::MARGIN_R;
                if span <= 0.0 {
                    return;
                }
                let mv = v_min + ((x - Self::MARGIN_L) / span).clamp(0.0, 1.0) * (v_max - v_min);
                let name = editor.selected();
                let snapped = shown
                    .iter()
                    .find(|c| c.editable && Some(&c.domain) == name.as_ref())
                    .and_then(|c| {
                        c.points
                            .iter()
                            .min_by(|a, b| {
                                (f64::from(a.voltage_mv) - mv)
                                    .abs()
                                    .total_cmp(&(f64::from(b.voltage_mv) - mv).abs())
                            })
                            .map(|p| f64::from(p.voltage_mv))
                    });
                let Some(v) = snapped else { return };
                let extend = gesture.current_event_state().contains(gtk::gdk::ModifierType::SHIFT_MASK)
                    && editor.has_selection.get();
                editor.loading.set(true);
                if extend {
                    editor.to_mv.set_value(v);
                } else {
                    // A plain click (or Shift+click with nothing selected yet)
                    // selects just that point and shows what it holds.
                    editor.from_mv.set_value(v);
                    editor.to_mv.set_value(v);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let held = shown
                        .iter()
                        .find(|c| c.editable && Some(&c.domain) == name.as_ref())
                        .and_then(|c| c.points.iter().find(|p| p.voltage_mv == v as u32))
                        .map_or(0, |p| p.offset_mhz);
                    editor.offset.set_value(f64::from(held));
                }
                editor.has_selection.set(true);
                editor.loading.set(false);
                selection.set(editor.range());
                area2.queue_draw();
            });
            area.add_controller(click);
        }
        {
            let keys = gtk::EventControllerKey::new();
            let (stage, editor, undo, pending, dirty, curves, area2, globals, on_edit) = (
                stage.clone(),
                editor.clone(),
                undo.clone(),
                pending.clone(),
                dirty.clone(),
                curves.clone(),
                area.clone(),
                globals.clone(),
                on_edit.clone(),
            );
            keys.connect_key_pressed(move |_, key, _, state| {
                use gtk::gdk::{Key, ModifierType};
                let is_undo = matches!(key, Key::z | Key::Z) && state.contains(ModifierType::CONTROL_MASK);
                // Nothing selected: only undo makes sense.
                let (lo, hi) = match editor.range() {
                    Some(range) => range,
                    None if is_undo => (0, 0),
                    None => return gtk::glib::Propagation::Proceed,
                };
                let step = if state.contains(ModifierType::CONTROL_MASK) {
                    25
                } else if state.contains(ModifierType::SHIFT_MASK) {
                    1
                } else {
                    5
                };
                match key {
                    // Nudge the range: relative to what each point already has.
                    Key::Up | Key::Down => {
                        let delta = if key == Key::Up { step } else { -step };
                        stage(&|curve, points| {
                            for (i, p) in curve.points.iter().enumerate() {
                                if (lo..=hi).contains(&p.voltage_mv) {
                                    #[allow(clippy::cast_possible_truncation)]
                                    let i = i as u8;
                                    let now = points.get(&i).copied().unwrap_or(0);
                                    points.insert(i, (now + delta).clamp(curve.offset_min_mhz, curve.offset_max_mhz));
                                }
                            }
                        });
                        gtk::glib::Propagation::Stop
                    }
                    // Move the whole range along the curve.
                    Key::Left | Key::Right => {
                        let name = editor.selected();
                        let curves_ref = curves.borrow();
                        if let Some(c) = curves_ref.iter().find(|c| c.editable && Some(&c.domain) == name.as_ref()) {
                            let volts: Vec<u32> = c.points.iter().map(|p| p.voltage_mv).collect();
                            let at = |v: u32| volts.iter().position(|x| *x >= v).unwrap_or(volts.len().saturating_sub(1));
                            let (i, j) = (at(lo), at(hi));
                            let shift = |k: usize| {
                                if key == Key::Right {
                                    (k + 1).min(volts.len() - 1)
                                } else {
                                    k.saturating_sub(1)
                                }
                            };
                            if !volts.is_empty() {
                                if state.contains(ModifierType::SHIFT_MASK) {
                                    editor.to_mv.set_value(f64::from(volts[shift(j)]));
                                } else {
                                    editor.from_mv.set_value(f64::from(volts[shift(i)]));
                                    editor.to_mv.set_value(f64::from(volts[shift(j)]));
                                }
                            }
                        }
                        gtk::glib::Propagation::Stop
                    }
                    Key::Delete | Key::BackSpace => {
                        stage(&|curve, points| {
                            for (i, p) in curve.points.iter().enumerate() {
                                if (lo..=hi).contains(&p.voltage_mv) {
                                    #[allow(clippy::cast_possible_truncation)]
                                    points.shift_remove(&(i as u8));
                                }
                            }
                        });
                        gtk::glib::Propagation::Stop
                    }
                    Key::z | Key::Z if state.contains(ModifierType::CONTROL_MASK) => {
                        if let Some(previous) = undo.borrow_mut().pop() {
                            let applied = applied_offsets(&curves.borrow());
                            dirty.set(previous != applied);
                            *pending.borrow_mut() = previous;
                            editor.summary.set_label(&offsets_summary(
                                &curves.borrow(),
                                &pending.borrow(),
                                dirty.get(),
                                &globals.borrow(),
                            ));
                            area2.queue_draw();
                            on_edit();
                        }
                        gtk::glib::Propagation::Stop
                    }
                    _ => gtk::glib::Propagation::Proceed,
                }
            });
            area.add_controller(keys);
        }
        let keys_help = caption(
            "Click a point to select it, Shift+click to extend the range; the offset box sets that range (absolute) · ↑/↓ nudge it by 5 MHz (Shift 1, Ctrl 25; adds to what is there) · ←/→ move the range (Shift extends) · Delete clears it · Ctrl+Z undo · nothing is written until Apply",
            false,
        );
        keys_help.set_width_chars(-1);
        keys_help.set_max_width_chars(-1);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.set_margin_all(8);
        body.append(&legend);
        body.append(&area);
        body.append(&readout);
        body.append(&editor.row);
        body.append(&keys_help);
        body.append(&editor.summary);
        let frame = gtk::Frame::builder().child(&body).build();
        Self {
            frame,
            area,
            legend,
            readout,
            curves,
            enabled,
            pending,
            dirty,
            editor,
            selection,
            undo,
            globals,
        }
    }

    /// The domains' global clock offsets, MHz, for the summary line.
    fn set_globals(&self, globals: std::collections::HashMap<String, i32>) {
        *self.globals.borrow_mut() = globals;
        self.editor.summary.set_label(&offsets_summary(
            &self.curves.borrow(),
            &self.pending.borrow(),
            self.dirty.get(),
            &self.globals.borrow(),
        ));
    }

    /// Drop staged curve edits, the undo history and the selection; the next
    /// `set` reloads what the card holds.
    fn discard_staged(&self) {
        self.dirty.set(false);
        self.undo.borrow_mut().clear();
        self.selection.set(None);
        let name = self.editor.selected();
        self.editor.reset_boxes(
            self.curves
                .borrow()
                .iter()
                .find(|c| c.editable && Some(&c.domain) == name.as_ref()),
        );
    }

    /// Write the staged offsets into the pending config on Apply.
    fn apply(&self, config: &mut GpuConfig) {
        if !self.dirty.get() {
            return;
        }
        config.clocks_configuration.domain_vf_offsets = self.pending.borrow().clone();
        self.dirty.set(false);
    }

    fn set(&self, curves: &[NvidiaDomainVfCurve]) {
        let names_changed = {
            let current = self.curves.borrow();
            current.len() != curves.len() || current.iter().zip(curves).any(|(a, b)| a.domain != b.domain)
        };
        *self.curves.borrow_mut() = curves.to_vec();
        // Until the user stages something the editor mirrors the card.
        if !self.dirty.get() {
            *self.pending.borrow_mut() = applied_offsets(curves);
        }
        self.editor.summary.set_label(&offsets_summary(
            curves,
            &self.pending.borrow(),
            self.dirty.get(),
            &self.globals.borrow(),
        ));
        let editable: Vec<String> = curves.iter().filter(|c| c.editable).map(|c| c.domain.clone()).collect();
        if *self.editor.names.borrow() != editable {
            let items: Vec<&str> = editable.iter().map(String::as_str).collect();
            self.editor.domain.set_model(Some(&gtk::StringList::new(&items)));
            *self.editor.names.borrow_mut() = editable.clone();
            self.editor.reset_boxes(curves.iter().find(|c| c.editable));
            self.selection.set(None);
        }
        self.editor.row.set_sensitive(!editable.is_empty());
        if names_changed {
            while let Some(child) = self.legend.first_child() {
                self.legend.remove(&child);
            }
            // Every curve on by default, GPC included: the scale difference is
            // small and the whole set reads better together (user's call).
            *self.enabled.borrow_mut() = vec![true; curves.len()];
            for (i, curve) in curves.iter().enumerate() {
                let check = gtk::CheckButton::builder()
                    .label(format!("{} ({})", curve.domain, curve.rail))
                    .active(self.enabled.borrow()[i])
                    .build();
                let (r, g, b) = CURVE_COLOURS[i % CURVE_COLOURS.len()];
                check.set_tooltip_text(Some(&format!("{} points", curve.points.len())));
                let swatch = gtk::DrawingArea::builder().content_width(14).content_height(14).valign(gtk::Align::Center).build();
                swatch.set_draw_func(move |_, cr, w, h| {
                    cr.set_source_rgb(r, g, b);
                    cr.rectangle(0.0, 0.0, f64::from(w), f64::from(h));
                    let _ = cr.fill();
                });
                let item = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                item.append(&swatch);
                item.append(&check);
                self.legend.append(&item);
                let (enabled, area) = (self.enabled.clone(), self.area.clone());
                check.connect_toggled(move |c| {
                    if let Some(slot) = enabled.borrow_mut().get_mut(i) {
                        *slot = c.is_active();
                    }
                    area.queue_draw();
                });
            }
        }
        self.frame.set_sensitive(!curves.is_empty());
        if curves.is_empty() {
            self.readout.set_label("No domain V/F curves reported by the daemon");
        }
        self.area.queue_draw();
    }

    /// Axis ranges over the enabled curves: (mV min, mV max, MHz max).
    fn ranges(curves: &[NvidiaDomainVfCurve], enabled: &[bool]) -> Option<(f64, f64, f64)> {
        let mut v_min = f64::MAX;
        let mut v_max = f64::MIN;
        let mut f_max: f64 = 0.0;
        for (c, on) in curves.iter().zip(enabled) {
            if !on {
                continue;
            }
            for p in &c.points {
                v_min = v_min.min(f64::from(p.voltage_mv));
                v_max = v_max.max(f64::from(p.voltage_mv));
                f_max = f_max.max(f64::from(p.freq_mhz));
            }
        }
        (v_max > v_min && f_max > 0.0).then_some((v_min, v_max, f_max * 1.05))
    }

    fn project(x: f64, y: f64, w: f64, h: f64, r: (f64, f64, f64)) -> (f64, f64) {
        let (v_min, v_max, f_max) = r;
        let px = Self::MARGIN_L + (x - v_min) / (v_max - v_min) * (w - Self::MARGIN_L - Self::MARGIN_R);
        let py = h - Self::MARGIN_B - y / f_max * (h - Self::MARGIN_T - Self::MARGIN_B);
        (px, py)
    }

    fn draw(cr: &gtk::cairo::Context, w: f64, h: f64, curves: &[NvidiaDomainVfCurve], enabled: &[bool], hover: Option<(f64, f64)>) {
        let Some(r) = Self::ranges(curves, enabled) else {
            return;
        };
        let (v_min, v_max, f_max) = r;
        // axes and grid
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.35);
        cr.set_line_width(1.0);
        let f_step = if f_max > 3000.0 { 500.0 } else { 250.0 };
        let mut f = 0.0;
        while f <= f_max {
            let (_, py) = Self::project(v_min, f, w, h, r);
            cr.move_to(Self::MARGIN_L, py);
            cr.line_to(w - Self::MARGIN_R, py);
            let _ = cr.stroke();
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.9);
            cr.set_font_size(10.0);
            cr.move_to(4.0, py + 3.0);
            let _ = cr.show_text(&format!("{f:.0}"));
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.35);
            f += f_step;
        }
        let mut v = (v_min / 100.0).ceil() * 100.0;
        while v <= v_max {
            let (px, _) = Self::project(v, 0.0, w, h, r);
            cr.move_to(px, Self::MARGIN_T);
            cr.line_to(px, h - Self::MARGIN_B);
            let _ = cr.stroke();
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.9);
            cr.move_to(px - 12.0, h - Self::MARGIN_B + 14.0);
            let _ = cr.show_text(&format!("{v:.0}"));
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.35);
            v += 100.0;
        }
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.9);
        cr.move_to(w - Self::MARGIN_R - 22.0, h - 4.0);
        let _ = cr.show_text("mV");
        cr.move_to(4.0, Self::MARGIN_T + 2.0);
        let _ = cr.show_text("MHz");
        // curves
        for (i, (c, on)) in curves.iter().zip(enabled).enumerate() {
            if !on || c.points.is_empty() {
                continue;
            }
            let (cr_r, cr_g, cr_b) = CURVE_COLOURS[i % CURVE_COLOURS.len()];
            cr.set_source_rgb(cr_r, cr_g, cr_b);
            cr.set_line_width(1.6);
            for (k, p) in c.points.iter().enumerate() {
                let (px, py) = Self::project(f64::from(p.voltage_mv), f64::from(p.freq_mhz), w, h, r);
                if k == 0 {
                    cr.move_to(px, py);
                } else {
                    cr.line_to(px, py);
                }
            }
            let _ = cr.stroke();
            // hollow points every few entries: these are not draggable handles
            for p in c.points.iter().step_by(6) {
                let (px, py) = Self::project(f64::from(p.voltage_mv), f64::from(p.freq_mhz), w, h, r);
                cr.arc(px, py, 2.2, 0.0, std::f64::consts::TAU);
                let _ = cr.stroke();
            }
        }
        if let Some((hx, hy)) = hover
            && hx >= Self::MARGIN_L
            && hx <= w - Self::MARGIN_R
        {
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.6);
            cr.set_line_width(1.0);
            cr.move_to(hx, Self::MARGIN_T);
            cr.line_to(hx, h - Self::MARGIN_B);
            let _ = cr.stroke();
            let _ = hy;
        }
    }

    /// The point nearest the pointer, ranked on both axes at once (curves
    /// run within a few MHz of each other, so voltage alone misleads).
    fn nearest(x: f64, y: f64, w: f64, h: f64, curves: &[NvidiaDomainVfCurve], enabled: &[bool]) -> String {
        let Some(r) = Self::ranges(curves, enabled) else {
            return String::new();
        };
        let mut best: Option<(f64, String)> = None;
        for (c, on) in curves.iter().zip(enabled) {
            if !on {
                continue;
            }
            for p in &c.points {
                let (px, py) = Self::project(f64::from(p.voltage_mv), f64::from(p.freq_mhz), w, h, r);
                let d = (px - x).powi(2) + (py - y).powi(2);
                if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                    best = Some((d, format!("{}: {} mV → {} MHz", c.domain, p.voltage_mv, p.freq_mhz)));
                }
            }
        }
        best.map(|(_, s)| s).unwrap_or_default()
    }
}

// ------------------------------------------------------------------ the page

pub struct AdvVoltagePage {
    content: gtk::Box,
    status_label: gtk::Label,
    boot_guard: BootGuardPanel,

    tele_gpc: gtk::Label,
    tele_xbar: gtk::Label,
    tele_sys: gtk::Label,
    tele_video: gtk::Label,
    tele_mem: gtk::Label,
    tele_ratio: gtk::Label,
    tele_volt: gtk::Label,
    tele_power: gtk::Label,
    tele_nvvdd_rail: gtk::Label,
    tele_msvdd_rail: gtk::Label,
    tele_pwrclk: gtk::Label,
    tele_hub: gtk::Label,

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
    nvvdd_ocp: Card,
    msvdd_ocp: Card,

    nvvdd_rail: RailCard,
    msvdd_rail: RailCard,
    tele_msvdd: gtk::Label,
    limits_summary: gtk::Label,
    limits_list: gtk::Label,
    /// Every limit client seen since the page loaded, id → (name, tag).
    /// Clients come and go between polls; keeping their rows (with a dash
    /// when absent) stops the block from changing height and shifting the
    /// page below it.
    limits_seen: RefCell<std::collections::BTreeMap<u32, (String, String)>>,
    curves: CurveChart,
    /// The rail current-limit records from the last clocks table, so the OCP
    /// cards' caption can follow the live reading between table fetches.
    ocp_limits: [Option<lact_schema::NvidiaRailCurrentLimit>; 2],
    table: Option<NvidiaClocksTable>,
    /// Thermal-input cards, one per simulatable sensor, built from the table
    thermal_grid: gtk::FlowBox,
    thermal_cards: RefCell<Vec<(u8, Card)>>,
    thermal_note: gtk::Label,
    sender: ComponentSender<AdvVoltagePage>,
    /// Memory timings, one monospace block
    timings_label: gtk::Label,
}

#[relm4::component(pub)]
impl relm4::Component for AdvVoltagePage {
    type Init = DaemonClient;
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
        client: Self::Init,
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

        // ---- live telemetry row: a flow box, so extra tiles wrap on a
        // narrow window instead of squeezing the row; homogeneous keeps the
        // tiles the same width.
        let tele = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .min_children_per_line(4)
            .max_children_per_line(13)
            .homogeneous(true)
            .row_spacing(6)
            .column_spacing(8)
            .hexpand(true)
            .build();
        let mut tiles = Vec::new();
        let specs: [(&str, Vec<StatType>); 13] = [
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
            // The power policies' own rail readings; the implied rail power
            // ("Power (NVVDD rail)") is graphable from the graphs window.
            ("NVVDD rail", vec![StatType::Current("NVVDD".into())]),
            ("MSVDD rail", vec![StatType::Current("MSVDD".into())]),
            // RM-measured; the driver permits no offset on either (range 0,
            // writes are stored and ignored — probed 2026-09-21). PWRCLK is
            // the PMU clock and tracks XBAR through the propagation ratio.
            ("PWRCLK", vec![StatType::Clockspeed("PWRCLK".into())]),
            ("HUB", vec![StatType::Clockspeed("HUBCLK".into())]),
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
        let (tele_nvvdd_rail, tele_msvdd_rail) = (tiles.next().unwrap(), tiles.next().unwrap());
        let (tele_pwrclk, tele_hub) = (tiles.next().unwrap(), tiles.next().unwrap());
        content.append(&tele);

        // ---- Core / NVVDD
        {
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&section_label("Core / NVVDD"));
            header.append(&info_icon(
                "The core (GPC) clock domain and its rail. Every card here is a knob on how fast the shader cores run and at what voltage; power and temperature limits win over all of them.\n\nEach card's tooltip is in four parts: what the control is, what raising or lowering it does, when to use it, and — separately — what was observed on the reference card, which is a data point, not a recommendation.",
                false,
            ));
            content.append(&header);
        }
        let grid = card_grid();
        let core = Card::new(
            "Core clock offset",
            "MHz",
            5.0,
            0,
            Control::CoreOffset,
            "What it is: the NVML V/F offset for the core (the same register nvidia-smi and the Overclocking page write), MHz added to every point of the boost curve.\n\
Effect: + raises the clock the core reaches at each voltage step; − lowers it. Power draw rises with clock at a fixed voltage.\n\
Use: the ordinary core overclock. Raise in 15 MHz steps until a workload errors or the power cap binds, then back off.\n\
Observed on this card (RTX 5090, XOC vBIOS — not a rule): +150 to +175 daily; written for pstate 0 only because a stray 0 in another pstate cancels the value on this driver.\n\
Risk: instability shows as application crashes or silent errors; nothing is written to firmware.",
            false,
            &sender,
        );
        let boost_lock = Card::new(
            "Boost lock",
            "MHz",
            15.0,
            0,
            Control::BoostLock,
            "What it is: NVML locked clocks with minimum = maximum = the target (nvidia-smi -lgc).\n\
Effect: the core sits at the target regardless of load or temperature, subject only to the power and thermal limits.\n\
Use: repeatable benchmarking, or pinning a clock while testing another setting. Off restores normal boost.\n\
Observed: an idle card at a locked clock burns more power than it needs to; not a daily setting.",
            false,
            &sender,
        );
        let vboost = Card::new(
            "Voltage boost",
            "%",
            5.0,
            0,
            Control::Clock(ClockspeedType::VoltageBoost),
            "What it is: LACT's bounded V/F limit shift through NvAPI, in percent of the driver's allowed range (the same control as the Overclocking page).\n\
Effect: lets the boost algorithm use higher voltage points at the top of the curve, so peak clocks rise when power and temperature allow.\n\
Use: the first thing to raise for more peak clock on an air- or water-cooled card with headroom. 100 % is the driver's own ceiling, not an unlock.\n\
Observed on this card: 50 % daily; the last few percent add power faster than clock.",
            false,
            &sender,
        );
        let nvvdd = Card::new(
            "Core voltage offset (NVVDD demand)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::NvvddOffset),
            "What it is: the GPC (core) domain's voltage demand offset on its own rail (NVVDD), mV, from the RM clock-domain object.\n\
Effect: + asks the rail for more voltage at every core clock; − asks for less (an undervolt). Another domain or a rail limit can still decide the rail.\n\
Use: undervolting the core for efficiency, or a small positive nudge to stabilise a high offset.\n\
Observed: bounded to ±50 mV by the daemon; not harness-validated at any value on this card. mVolt+'s \"extended voltage\" is this same control with a ±500 mV window.\n\
Risk: a positive value raises core rail power immediately; large values are outside anything tested here.",
            true,
            &sender,
        );
        for f in [&core.frame, &boost_lock.frame, &vboost.frame, &nvvdd.frame] {
            grid.append(f);
        }
        content.append(&grid);

        // ---- Fabric / MSVDD
        {
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&section_label("Fabric / MSVDD"));
            header.append(&info_icon(
                "The crossbar (XBAR), SYS and video clock domains and the fabric rail (MSVDD) they share. These are the domains the public tools cannot reach; they matter for fabric-bound work and are where the silent-corruption risk lives, so validate with the harness rather than by eye.",
                false,
            ));
            content.append(&header);
        }
        let grid = card_grid();
        let xbar = Card::new(
            "XBAR clock offset",
            "MHz",
            10.0,
            0,
            Control::Clock(ClockspeedType::XbarClockOffset),
            "What it is: an offset, MHz, on the XBAR (crossbar / fabric) clock domain through the RM clock-domain interface — a domain the public tools cannot reach.\n\
Effect: + raises the fabric clock; the SYS and PWR domains follow it through the propagation ratio. The core clock is unchanged.\n\
Use: the extra performance mVolt+ advertises on Blackwell. Worth trying on fabric-bound workloads; validate with the harness, because errors are silent.\n\
Observed on this card: +250 daily, +300 passed; silent corruption at +340 and +380 with no crash and no driver error; +270 on an LLM decode gained 0.4 %.\n\
Risk: silent data corruption above the stable point — it will not tell you. Driver range ±1000 MHz, no guard.",
            true,
            &sender,
        );
        let msvdd = Card::new(
            "XBAR voltage offset (MSVDD)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::MsvddOffset),
            "What it is: the XBAR domain's voltage demand offset on the fabric rail (MSVDD), mV.\n\
Effect: + asks the fabric rail for more voltage at every XBAR clock; − less. The rail voltage is the maximum of all demands on it, so another domain can hold it up.\n\
Use: in theory, voltage to hold a higher XBAR offset. Try it only after the clock offset alone fails the harness.\n\
Observed on this card: +20 mV lowered XBAR by ~31 MHz on its own and did not extend the corruption ceiling — not free headroom.",
            true,
            &sender,
        );
        let sys = Card::new(
            "SYS clock offset",
            "MHz",
            10.0,
            0,
            Control::Clock(ClockspeedType::SysClockOffset),
            "What it is: an offset, MHz, on the SYS clock domain (the system / host-interface clock) through the RM interface.\n\
Effect: + raises the SYS clock. It normally follows XBAR through the ratio; this moves it independently.\n\
Use: rarely useful on its own; leave at 0 unless a workload is known to be SYS-bound.\n\
Observed: domain verified; never harness-validated at a positive value on this card.",
            false,
            &sender,
        );
        let video = Card::new(
            "Video clock offset",
            "MHz",
            10.0,
            0,
            Control::Clock(ClockspeedType::VideoClockOffset),
            "What it is: an offset, MHz, on the video clock domain (NVENC / NVDEC engines) through the RM interface.\n\
Effect: faster encode / decode engines only; no effect on graphics or compute.\n\
Use: streaming or transcoding workloads. Verify with an encode job, not a game.\n\
Observed: domain verified; not harness-validated at any value.",
            false,
            &sender,
        );
        let sys_volt = Card::new(
            "SYS voltage offset (MSVDD demand)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::SysVoltageOffset),
            "What it is: the SYS domain's voltage demand offset on the fabric rail, mV.\n\
Effect: the same as the XBAR demand — a request to the shared MSVDD rail, of which the highest demand wins.\n\
Use: only with a SYS clock offset that needs it. Untested here.\n\
Risk: raises fabric rail power for every domain on the rail.",
            true,
            &sender,
        );
        let video_volt = Card::new(
            "Video voltage offset (MSVDD demand)",
            "mV",
            5.0,
            0,
            Control::Clock(ClockspeedType::VideoVoltageOffset),
            "What it is: the video domain's voltage demand offset on the fabric rail, mV.\n\
Effect: a request to the MSVDD rail for the NVENC / NVDEC clock.\n\
Use: only with a video clock offset that needs it. Untested here.",
            true,
            &sender,
        );
        let ratio = Card::new(
            "MSVDD clock ratio (GPC→XBAR propagation)",
            "×",
            0.005,
            3,
            Control::Ratio,
            "What it is: the clock arbiter's GPC→XBAR propagation ratio (factory 0.900 on GB202), the constraint that keeps the fabric clock at least this fraction of the core clock.\n\
Effect: it is a floor, not XBAR = GPC × ratio: XBAR and SYS follow the core upward when the constraint binds. Raising it pulls the fabric up with the core; lowering it lets the fabric lag.\n\
Use: an alternative to a fixed XBAR offset that scales with the core clock. Both together compound.\n\
Observed on this card: the same silent-corruption ceiling applies to the resulting XBAR clock, whichever control gets there.",
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
        {
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&section_label("Voltage limits"));
            header.append(&info_icon(
                "The voltage-policy limits of each rail and their live target / sensed voltage. The limits are the ceilings the boost algorithm is allowed to use; the deltas shift them. This is where peak clocks come from once the ordinary offsets are exhausted, and where the reliability margin lives.",
                false,
            ));
            content.append(&header);
        }
        let rails_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        rails_row.set_homogeneous(true);
        let nvvdd_rail = RailCard::new(
            "NVVDD voltage limits",
            0,
            "What it is: deltas, mV, to the core rail's voltage-policy limits. MAX (the tightest of REL, ALT/OP and OV) is the ceiling the boost algorithm may use; VMIN is the floor.\n\
Effect: raising the binding limit lets the V/F curve run to higher voltage points, so peak clock can rise; the daemon holds any raised limit under the device maximum.\n\
Use: the last step for peak clocks once voltage boost is at 100 % and power allows. Raise REL and ALT/OP together; OV only once both exceed it.\n\
Observed on this card: shown as it is; no limit delta is part of the daily profile.\n\
Risk: this is the reliability margin — voltage above the vendor's operating limit ages the silicon.",
            &sender,
        );
        let msvdd_rail = RailCard::new(
            "MSVDD voltage limits",
            1,
            "What it is: deltas, mV, to the fabric rail's policy limits (−50 mV on REL is the driver's own default here).\n\
Effect: as for NVVDD, on the MSVDD rail that the XBAR / SYS / video domains share.\n\
Use: only if a fabric offset is voltage-limited rather than corruption-limited.\n\
Observed on this card: +30 mV did not make XBAR +340 compute correctly — the fabric ceiling is not a voltage limit.",
            &sender,
        );
        rails_row.append(&nvvdd_rail.frame);
        rails_row.append(&msvdd_rail.frame);
        content.append(&rails_row);

        // ---- Memory / Power
        {
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&section_label("Memory / Power"));
            header.append(&info_icon(
                "The memory clock, the board power cap and the two rail current limits. Power is the practical ceiling on this card: at the cap, everything above trades clock for it.",
                false,
            ));
            content.append(&header);
        }
        let grid = card_grid();
        let mem = Card::new(
            "Memory clock offset",
            "MHz",
            50.0,
            0,
            Control::MemOffset,
            "What it is: the NVML V/F offset for the memory clock (GDDR7), MHz, pstate 0.\n\
Effect: + raises the memory data rate. Bandwidth-bound workloads gain directly.\n\
Use: the ordinary memory overclock. Raise in 250–500 MHz steps and check throughput, not just stability.\n\
Observed on this card: +6000 daily. GDDR7 error correction can hide errors as lower throughput rather than crashes — a run that got slower is a failed run.",
            false,
            &sender,
        );
        let power = Card::new(
            "Power limit",
            "W",
            5.0,
            0,
            Control::PowerCap,
            "What it is: the board power cap, watts (NVML inside the vBIOS range; below its minimum through the RM client power policy, from 30 W). Off = driver default.\n\
Effect: at the cap the boost algorithm lowers clocks rather than exceeding the power; extra voltage then costs clock.\n\
Use: the main efficiency knob. Lower it to trade a little peak clock for a lot of power and heat; raise it only if the card is actually power-bound.\n\
Observed on this card: 660 W daily of an 800 W XOC range; sustained draw is better cut with the V/F curve than with the cap.",
            false,
            &sender,
        );
        let nvvdd_ocp = Card::new(
            "NVVDD current limit (OCP)",
            "A",
            10.0,
            0,
            Control::Clock(ClockspeedType::RailCurrentLimit(0)),
            "What it is: the core rail's current limit in the driver's power policies (mVolt+ calls it OCP), amps.\n\
Effect: lower it and the rail is capped on current alone — the card throttles as if power-capped; raise it and nothing changes until the power limit is also raised past the point where the rail's current binds.\n\
Use: an unlock only matters on cards whose power limit is above what the rail limit allows. Lowering it is a second, current-based cap.\n\
Observed on this card: rated 480 A; the rail drew 307–373 A at 558–613 W, so it never binds at this TGP. At 100 A the card throttled to 150 W within a second. Off = the value found at daemon start; range 50 A to 2× rated.\n\
Risk: raising it removes a protection; the wire and connector limits are the WireView's job.",
            true,
            &sender,
        );
        let msvdd_ocp = Card::new(
            "MSVDD current limit (OCP)",
            "A",
            10.0,
            0,
            Control::Clock(ClockspeedType::RailCurrentLimit(1)),
            "What it is: the fabric rail's current limit in the power policies, amps.\n\
Effect: as for NVVDD, on the MSVDD rail.\n\
Use: almost never binding; a low value is a fabric-side throttle.\n\
Observed on this card: rated 180 A; ~72 A drawn at the 620 W limit. 50 A throttled the card to 250 W within two seconds; 100 A did nothing. Off = daemon-start value; range 50 A to 2× rated.",
            true,
            &sender,
        );
        // mVolt+'s "extended voltage" is the same per-domain demand offsets
        // above with a ±500 mV window; the ±50 mV bound here is deliberate.
        for f in [&mem.frame, &power.frame, &nvvdd_ocp.frame, &msvdd_ocp.frame] {
            grid.append(f);
        }
        content.append(&grid);

        // ---- Thermal inputs: simulated sensor readings (mVolt+ v0.44)
        let thermal_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        thermal_header.append(&section_label("Thermal inputs (VFE)"));
        thermal_header.append(&info_icon(
            "Tell a thermal sensor to report a fixed temperature instead of measuring (the driver's \
             sensor simulation; mVolt+ calls it a thermal input). The voltage/frequency equations take \
             the die temperature as an input, so a lower value removes the voltage margin the firmware \
             adds as the card warms — the same lever as on Windows — and NVML, the fan policies and the \
             thermal-limit policies all see the simulated value.\n\n\
             That last part is the hazard: a low value can blind the card's own fan curve and thermal \
             protection. The daemon floors the value at 20 °C, refuses it while LACT's fan control is on, \
             clears every simulation when the daemon stops or the profile changes, and watches any sensor \
             that did not follow the simulation, clearing it at 95 °C. Verified on the reference card: \
             pinning the GPU sensor to 60 °C dropped the idle boost clock from 3277 to about 1600 MHz.",
            true,
        ));
        content.append(&thermal_header);
        let thermal_note = caption("Sensor group not available on this driver", false);
        thermal_note.set_width_chars(-1);
        thermal_note.set_max_width_chars(-1);
        content.append(&thermal_note);
        let thermal_grid = card_grid();
        content.append(&thermal_grid);

        // ---- Memory timings: FBPA CONFIG0/1 per partition, read-only
        let timings_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        timings_header.append(&section_label("Memory timings"));
        timings_header.append(&info_icon(
            "The GDDR7 timings the driver programmed into each frame-buffer partition (FBPA CONFIG0 / \
             CONFIG1, decoded with NVIDIA's public Memory Tweak Table layout) and the broadcast window, \
             in memory-controller clocks. Read-only: they change with the memory P-state, so the idle \
             table differs from the loaded one. The decode checks itself: tRC = tRAS + tRP holds on every \
             row or the daemon shows nothing.",
            false,
        ));
        content.append(&timings_header);
        let timings_label = gtk::Label::builder()
            .label("—")
            .xalign(0.0)
            .css_classes(["caption", "dim-label", "monospace"])
            .build();
        let timings_frame = gtk::Frame::builder().child(&timings_label).css_classes(["card"]).build();
        timings_label.set_margin_all(8);
        content.append(&timings_frame);

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
        let limits_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        limits_summary.set_hexpand(true);
        limits_header.append(&limits_summary);
        limits_header.append(&info_icon(
            "Every populated limit client of the driver's clock arbiter (RM PERF_LIMITS status), read \
             live, named with NVIDIA's own names from NVML's table (shown in brackets). A client that was \
             populated earlier keeps its row with a dash while it is not, so the block does not change \
             height as clients come and go.\n\n\
             The two numbers on a row are the client's own value and what the driver makes of it:\n\
             • Frequency clients ask for a clock, so the first number is MHz and the arrow normally \
             echoes it (\"3375 MHz → 3375\").\n\
             • Voltage-policy clients ask for a voltage (mV); the arrow is the core clock the V/F \
             curve reaches at that voltage (\"1055 mV → 3217\" = at the reliability voltage the core \
             tops out at 3217 MHz). That is how a voltage limit becomes a clock ceiling.\n\
             • MSVDD voltage rows have no arrow: this object only reports a core result, and those \
             limits act on the fabric rail. P-state style clients carry no frequency at all.\n\
             • Floor rows (named \"… floor\" / _MIN) are minimums — the lowest clock the boost \
             controller allows, not a cap — and are excluded from the summary. Low floors mean the \
             card is idle.\n\
             • \"P-state limit (level N)\" rows carry a P-state level instead of a clock; the level's \
             meaning is not decoded.\n\
             • The Blackwell clients NVML has no name for come in two triples (P-state, GPC, XBAR): \
             \"Power cap controller\" is the board power-limit policy's set, which tracked the power \
             cap in testing; \"Power policy 2\" is a second policy's set whose XBAR member followed \
             the MSVDD rail current limit and nothing else. That is all the driver lets us say about \
             them.\n\n\
             The summary line is the smallest core result among the non-floor clients: a ceiling, so \
             it is meaningful at idle too. While NVML reports an active throttle reason (power cap, \
             thermal…), that reason is shown instead, because the power-cap controller rows are \
             sampled controller output that swings well below the average clock.",
            false,
        ));
        limits_box.append(&limits_header);
        limits_box.append(&limits_list);
        let limits_frame = gtk::Frame::builder().child(&limits_box).build();
        content.append(&limits_frame);

        // ---- Clock domain V/F curves, read-only
        let curves_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let curves_title = section_label("Clock domain V/F curves");
        curves_title.set_hexpand(true);
        curves_header.append(&curves_title);
        curves_header.append(&info_icon(
            "What it is: the 127-point voltage/frequency curve of every clock domain that keeps one, as the driver \
             reports it (RM CLK_VF_POINTS). XBAR, SYS, video and PWRCLK sit on the MSVDD rail; GPC is on NVVDD. \
             The row under the chart stages per-point frequency offsets on the GPC, XBAR, SYS and video curves; \
             PWRCLK follows XBAR through the ratio. For GPC these offsets are a second layer: the core clock \
             offset and the separate GPC editor (Edit GPC curve…) write absolute per-point values underneath, and \
             what is staged here stacks on top of them.\n\
             Effect: a per-point offset adds to the domain's global clock offset at that voltage only. Negative \
             holds the clock down in a voltage region (the safe direction); positive raises it there. Flatten \
             above pins every higher-voltage point to one frequency, the fabric version of the core undervolt.\n\
             Use: the fabric fails silently above its stable clock, so shape the curve instead of lowering the \
             whole offset — keep the global offset that passes at low voltage and pull down only the region \
             where the harness errors. Validate with the Tests section after every change.\n\
             Observed on this card (driver 615): single-point writes of −15 MHz on GPC, XBAR, SYS and video were kept \
             by the driver on every later read, moved exactly that point by 15 MHz and none of its neighbours, and \
             restored cleanly. On driver 610 another tester saw a domain drop a written point, so the daemon \
             verifies both the stored offset and the resulting curve and rolls back if the driver kept something else.\n\
             Risk: positive offsets on a fabric curve are where silent corruption lives; the daemon caps them at +300 MHz. \
             Untick a curve to rescale the rest; hover reads the nearest point.",
            false,
        ));
        let open_editor = gtk::Button::builder()
            .label("Edit GPC curve…")
            .css_classes(["flat"])
            .tooltip_text(
                "Opens the GPC (core) V/F curve editor — the same window as on the Overclocking page. \
                 Points dragged there are written into the pending config and applied with the Apply button \
                 like every other setting on this page.",
            )
            .build();
        {
            let sender = sender.clone();
            open_editor.connect_clicked(move |_| {
                let _ = sender.output(AppMsg::ShowVfCurveEditor);
            });
        }
        curves_header.append(&open_editor);
        content.append(&curves_header);
        let curves = CurveChart::new({
            let sender = sender.clone();
            move || {
                let _ = sender.output(AppMsg::SettingsChanged);
            }
        });
        content.append(&curves.frame);

        // ---- Boot guard: the fallback switch for everything above
        content.append(&section_label("Boot guard"));
        let boot_guard = BootGuardPanel::new(client);
        content.append(&boot_guard.frame);

        // ---- Tests: the tooling repo's scripts, run from here
        {
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&section_label("Tests"));
            header.append(&info_icon(
                "The private tooling's validation harness and two throughput benchmarks, run from here with the \
                 currently applied settings. Use: after any change to a fabric offset or ratio run the correctness \
                 check, because fabric errors are silent — a different result digest is a failed setting even if \
                 nothing crashed; then the torch and FurMark benches say what the setting is worth (compute/bandwidth \
                 vs raster). The output is the script's own; the tooling directory and Python come from the tooling's config.",
                false,
            ));
            content.append(&header);
        }
        let (_test_runner, tests_frame) = TestRunner::new();
        content.append(&tests_frame);

        let model = Self {
            content,
            status_label,
            boot_guard,
            tele_gpc,
            tele_xbar,
            tele_sys,
            tele_video,
            tele_mem,
            tele_ratio,
            tele_volt,
            tele_power,
            tele_nvvdd_rail,
            tele_msvdd_rail,
            tele_pwrclk,
            tele_hub,
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
            nvvdd_ocp,
            msvdd_ocp,
            nvvdd_rail,
            msvdd_rail,
            tele_msvdd,
            limits_summary,
            limits_list,
            limits_seen: RefCell::new(std::collections::BTreeMap::new()),
            curves,
            ocp_limits: [None, None],
            table: None,
            thermal_grid,
            thermal_cards: RefCell::new(Vec::new()),
            thermal_note,
            sender: sender.clone(),
            timings_label,
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
                // A table only arrives on a reload — startup, Apply, Revert, a
                // profile switch — and the app drops its pending-changes flag at
                // the same moment. Staged edits on this page must go with it,
                // or a touched card keeps its old target and hides what the
                // newly loaded profile holds.
                self.discard_staged();
                self.show_table(table.as_ref());
                self.table = table;
            }
            AdvVoltagePageMsg::BootGuard(status) => self.boot_guard.state.show(&status),
            AdvVoltagePageMsg::Profiles(names) => self.boot_guard.state.set_profiles(names),
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
        self.tele_pwrclk.set_label(&mhz(c.sensors.get("PWRCLK").copied()));
        self.tele_hub.set_label(&mhz(c.sensors.get("HUBCLK").copied()));

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

        for (index, card) in self.thermal_cards.borrow().iter() {
            if let Some(s) = stats.thermal_sensors.iter().find(|s| s.index == *index) {
                card.set_caption(&thermal_caption(s));
            }
        }
        self.show_memory_timings(&stats.memory_timings);

        let currents = &stats.power.current_sensors;
        let powers = &stats.power.sensors;
        for (card, rail, slot) in [(&self.nvvdd_ocp, "NVVDD", 0usize), (&self.msvdd_ocp, "MSVDD", 1usize)] {
            if let (Some(l), Some(a)) = (self.ocp_limits[slot].as_ref(), currents.get(rail)) {
                card.set_caption(&ocp_caption(l, Some(*a)));
            }
        }
        for (label, rail) in [(&self.tele_nvvdd_rail, "NVVDD"), (&self.tele_msvdd_rail, "MSVDD")] {
            label.set_label(&match (currents.get(rail), powers.get(&format!("{rail} rail"))) {
                (Some(a), Some(w)) => format!("{a:.0} A · {w:.0} W"),
                (Some(a), None) => format!("{a:.0} A"),
                _ => "—".to_owned(),
            });
        }

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
        let mut seen = self.limits_seen.borrow_mut();
        for l in limits {
            let tag = l
                .nvml_name
                .as_deref()
                .map_or_else(|| format!("id {:#04x}", l.id), |n| format!("[{n}]"));
            seen.insert(l.id, (l.name.clone(), tag));
        }
        let mut rows: Vec<String> = seen
            .iter()
            .map(|(id, (name, tag))| {
                let (value, result) = match limits.iter().find(|l| l.id == *id) {
                    Some(l) => (
                        l.limit_mhz
                            .map(|m| format!("{m} MHz"))
                            .or_else(|| l.limit_mv.map(|v| format!("{v} mV")))
                            .unwrap_or_default(),
                        l.result_mhz.map_or(String::new(), |m| format!("→ {m}")),
                    ),
                    // Not populated right now: keep the row, blank the numbers.
                    None => ("—".to_owned(), String::new()),
                };
                format!("{name:<44} {value:>9} {result:>9}   {tag}")
            })
            .collect();
        rows.sort();
        self.limits_list.set_label(&rows.join("\n"));
    }

    fn show_table(&mut self, table: Option<&NvidiaClocksTable>) {
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
                &self.nvvdd_ocp,
                &self.msvdd_ocp,
            ] {
                card.unavailable("No NVIDIA clocks table from the daemon");
            }
            self.nvvdd_rail.unavailable("No NVIDIA clocks table from the daemon");
            self.msvdd_rail.unavailable("No NVIDIA clocks table from the daemon");
            self.curves.set(&[]);
            return;
        };
        self.curves.set(&t.domain_vf_curves);
        let mut globals = std::collections::HashMap::new();
        for (name, offset) in [
            ("XBARCLK", t.xbar_offset.as_ref()),
            ("SYSCLK", t.sys_offset.as_ref()),
            ("VIDCLK", t.video_offset.as_ref()),
        ] {
            if let Some(o) = offset {
                globals.insert(name.to_owned(), o.current);
            }
        }
        if let Some(o) = t.gpu_offsets.get(&0).or_else(|| t.gpu_offsets.values().next()) {
            globals.insert("GPCCLK".to_owned(), o.current);
        }
        self.curves.set_globals(globals);

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
        let mut ocp_limits = [None, None];
        for (card, index) in [(&self.nvvdd_ocp, 0u8), (&self.msvdd_ocp, 1u8)] {
            let limit = t
                .voltage_rails
                .iter()
                .find(|r| r.index == index)
                .and_then(|r| r.current_limit.as_ref());
            match limit {
                Some(l) => card.load(
                    Some(f64::from(l.current_a)),
                    f64::from(l.min_a),
                    f64::from(l.max_a),
                    f64::from(l.default_a),
                    l.current_a != l.default_a,
                    &ocp_caption(l, None),
                ),
                None => card.unavailable("Power-policy objects not available on this driver"),
            }
            ocp_limits[usize::from(index)] = limit.copied();
        }

        self.ocp_limits = ocp_limits;
        self.show_thermal_sensors(&t.thermal_sensors);

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
            &self.sys_volt,
            &self.video_volt,
            &self.ratio,
            &self.mem,
            &self.power,
            &self.nvvdd_ocp,
            &self.msvdd_ocp,
        ] {
            card.apply(config);
        }
        self.nvvdd_rail.apply(config);
        self.msvdd_rail.apply(config);
        for (_, card) in self.thermal_cards.borrow().iter() {
            card.apply(config);
        }
        self.curves.apply(config);
    }

    /// Forget every staged edit on the page (see the `ClocksTable` handler).
    fn discard_staged(&self) {
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
            &self.power,
            &self.nvvdd_ocp,
            &self.msvdd_ocp,
        ] {
            card.dirty.set(false);
        }
        self.nvvdd_rail.dirty.set(false);
        self.msvdd_rail.dirty.set(false);
        for (_, card) in self.thermal_cards.borrow().iter() {
            card.dirty.set(false);
        }
        self.curves.discard_staged();
    }

    /// Rebuild the thermal-input cards when the sensor set changes, then
    /// load each from the table.
    fn show_thermal_sensors(&self, sensors: &[NvidiaThermalSensor]) {
        let simulatable: Vec<&NvidiaThermalSensor> = sensors.iter().filter(|s| s.can_simulate).collect();
        let wanted: Vec<u8> = simulatable.iter().map(|s| s.index).collect();
        let have: Vec<u8> = self.thermal_cards.borrow().iter().map(|(i, _)| *i).collect();
        if wanted != have {
            while let Some(child) = self.thermal_grid.first_child() {
                self.thermal_grid.remove(&child);
            }
            let mut cards = Vec::new();
            for s in &simulatable {
                let card = Card::new(
                    &format!("{} thermal input", s.name),
                    "°C",
                    1.0,
                    0,
                    Control::Clock(ClockspeedType::ThermalInput(s.index)),
                    &format!(
                        "What it is: a fixed value this sensor reports instead of measuring (sensor {} of the RM group). Off = measuring.\n\
                         Effect: the voltage/frequency equations take the die temperature as an input; a value below the real \
                         temperature removes the voltage margin the firmware adds as the card warms (higher clock at the same \
                         voltage), a value above adds margin (lower clock). NVML and the thermal-limit policy see the fixed value too.\n\
                         Use: on a water-cooled card that stays well under its limits, a low input recovers the clock the firmware \
                         gives away for heat that is not there. Test under a light load and watch the computed sensor.\n\
                         Observed on this card: 60 °C on the GPU sensor at idle dropped the boost clock from ~3277 to ~1600 MHz. \
                         The Memory junction sensor is the channel NvAPI/NVML report as the VRAM temperature (it equals the \
                         hottest GDDR7 chip); simulating it at 95 and 100 °C under load changed nothing on this card — memory \
                         clock, timings and boost clock all held — so on this driver it only changes what is reported. \
                         The GDDR7 refresh/timing compensation reads the chips' own registers, which this does not touch.\n\
                         Risk: the card's own thermal protection reads this channel; keep the value near reality and never \
                         below the daemon's {} °C floor. Range {}…{} °C.",
                        s.index, s.sim_min_c, s.sim_min_c, s.sim_max_c
                    ),
                    true,
                    &self.sender,
                );
                self.thermal_grid.append(&card.frame);
                cards.push((s.index, card));
            }
            *self.thermal_cards.borrow_mut() = cards;
        }
        for (index, card) in self.thermal_cards.borrow().iter() {
            if let Some(s) = simulatable.iter().find(|s| s.index == *index) {
                card.load(
                    s.sim_c.map(f64::from),
                    f64::from(s.sim_min_c),
                    f64::from(s.sim_max_c),
                    50.0,
                    s.sim_enabled,
                    &thermal_caption(s),
                );
            }
        }
        self.thermal_note.set_visible(simulatable.is_empty());
        if sensors.is_empty() {
            self.thermal_note.set_label("Sensor group not available on this driver");
        } else if simulatable.is_empty() {
            self.thermal_note.set_label("No sensor in the group reports a temperature");
        }
    }

    fn show_memory_timings(&self, rows: &[NvidiaMemoryTimingRow]) {
        if rows.is_empty() {
            self.timings_label.set_label("Not available (FBPA registers need the system daemon)");
            return;
        }
        let mut text = format!(
            "{:<10} {:>4} {:>4} {:>4} {:>5} {:>4} {:>4} {:>7} {:>7}\n",
            "", "CL", "WL", "RC", "RFC", "RAS", "RP", "RD_RCD", "WR_RCD"
        );
        for r in rows {
            text.push_str(&format!(
                "{:<10} {:>4} {:>4} {:>4} {:>5} {:>4} {:>4} {:>7} {:>7}\n",
                r.name, r.cl, r.wl, r.rc, r.rfc, r.ras, r.rp, r.rd_rcd, r.wr_rcd
            ));
        }
        self.timings_label.set_label(text.trim_end());
    }
}

/// The thermal-input card caption: the live reading and the simulation state.
fn thermal_caption(s: &NvidiaThermalSensor) -> String {
    let reading = s.temp_c.map_or("—".to_owned(), |t| format!("{t:.1} °C"));
    match (s.sim_enabled, s.sim_c) {
        (true, Some(c)) => format!("Reading {reading}   SIMULATED at {c:.0} °C"),
        (true, None) => format!("Reading {reading}   simulated"),
        _ => format!("Reading {reading}   measuring"),
    }
}

/// The per-point offsets the card holds, from the table's curves.
fn applied_offsets(curves: &[NvidiaDomainVfCurve]) -> CurveOffsets {
    let mut out = CurveOffsets::new();
    for c in curves.iter().filter(|c| c.editable) {
        let points: indexmap::IndexMap<u8, i32> = c
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.offset_mhz != 0)
            .map(|(i, p)| {
                #[allow(clippy::cast_possible_truncation)]
                let i = i as u8;
                (i, p.offset_mhz)
            })
            .collect();
        if !points.is_empty() {
            out.insert(c.domain.clone(), points);
        }
    }
    out
}

/// The OCP card caption; `live_a` overrides the table's snapshot reading.
fn ocp_caption(l: &lact_schema::NvidiaRailCurrentLimit, live_a: Option<f64>) -> String {
    let drawing = live_a
        .map(|a| format!("   Drawing {a:.0} A"))
        .or_else(|| l.measured_a.map(|a| format!("   Drawing {a} A")))
        .unwrap_or_default();
    format!(
        "Current {} A (rated {} A){}{drawing}   Range {}…{} A",
        l.current_a,
        l.rated_a,
        l.arbitrated_a
            .filter(|a| *a != l.current_a)
            .map_or(String::new(), |a| format!(", driver holds {a} A")),
        l.min_a,
        l.max_a
    )
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
