//! This fork: the VBIOS page (NVIDIA).
//!
//! Shows the flashed vBIOS identity (from the PCI expansion ROM), the state of a vBIOS
//! override loaded through a patched driver, and the decoded tables of the override image
//! or of a ROM file picked here. The PCI expansion ROM only carries the legacy and EFI
//! images, so the flashed tables themselves can only be shown from a full dump file.
//!
//! A loaded GB20x image can be edited into a new file (board power limits, REL voltage
//! limits, memory offset ceiling); everything else is handed to an external editor
//! (Panchovix's VBIOS Vix Editor) when it is installed. Nothing here writes to the GPU.

use super::PageId;
use crate::app::msg::AppMsg;
use gtk::{glib, prelude::*};
use lact_client::DaemonClient;
use lact_schema::{
    DeviceInfo, DeviceStats,
    vbios::{
        ImageCompleteness, PowerPolicyRole, PowerUnit, RailLimitRole, VbiosInfo,
        VbiosOverrideScope, VbiosOverrideStatus, VbiosReport,
        edit::{EditField, EditUnit, EditableFields},
    },
};
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

/// Overrides the external editor's executable; otherwise these names are looked up in PATH.
const EXTERNAL_EDITOR_ENV: &str = "LACT_NVBIOS_EDITOR";
const EXTERNAL_EDITOR_NAMES: [&str; 2] = ["nvbios-editor", "nvbios-gui"];
const EXTERNAL_EDITOR_URL: &str = "https://github.com/Panchovix/nvidia-vbios-vix-editor";

#[derive(Debug)]
pub enum VbiosPageMsg {
    /// A GPU was selected.
    Gpu {
        id: String,
        info: Arc<DeviceInfo>,
    },
    Stats(Arc<DeviceStats>),
    Refresh,
    Info {
        id: String,
        result: Result<Box<VbiosInfo>, String>,
    },
    LoadFile,
    /// Load a known path (the override image) without a file dialog.
    LoadPath(PathBuf),
    FileLoaded(Result<Box<LoadedFile>, String>),
    ClearFile,
    SaveEdit,
    /// The saved copy (now the loaded file) and a summary, or an error.
    EditSaved(Result<Box<(LoadedFile, String)>, String>),
    RevertEdit,
    OpenExternalEditor,
}

#[derive(Debug)]
pub struct LoadedFile {
    path: PathBuf,
    data: Vec<u8>,
    report: VbiosReport,
    /// Same legacy/EFI images as the card's flash; `None` if the flash could not be read.
    matches_flash: Option<bool>,
    fields: Result<EditableFields, String>,
    /// Edited values in each field's stored unit, written by the spin buttons directly so a
    /// re-render keeps them.
    pending: Rc<RefCell<BTreeMap<EditField, i64>>>,
}

impl LoadedFile {
    fn new(path: PathBuf, data: Vec<u8>, report: VbiosReport, matches_flash: Option<bool>) -> Self {
        let fields =
            lact_schema::vbios::edit::editable_fields(&data).map_err(|err| err.to_string());
        Self {
            path,
            data,
            report,
            matches_flash,
            fields,
            pending: Rc::default(),
        }
    }
}

pub struct VbiosPage {
    client: DaemonClient,
    content: gtk::Box,
    body: gtk::Box,
    gpu_id: Option<String>,
    info: Option<VbiosInfo>,
    error: Option<String>,
    file: Option<LoadedFile>,
    stats: Option<Arc<DeviceStats>>,
    /// Result of the last save or external-editor launch; `true` marks an error.
    edit_status: Option<(String, bool)>,
}

#[relm4::component(pub)]
impl relm4::Component for VbiosPage {
    type Init = DaemonClient;
    type Input = VbiosPageMsg;
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
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_all(15);
        content.set_margin_top(20);

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_halign(gtk::Align::End);
        let load = gtk::Button::builder()
            .label("Load ROM file…")
            .tooltip_text(
                "Decode a full vBIOS dump (e.g. from nvflash --save or TechPowerUp) or an edited image. \
                 The file is only read; it is checked against the card's flashed option ROM.",
            )
            .build();
        load.connect_clicked(glib::clone!(
            #[strong]
            sender,
            move |_| sender.input(VbiosPageMsg::LoadFile)
        ));
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Read the flash and override state again"));
        refresh.connect_clicked(glib::clone!(
            #[strong]
            sender,
            move |_| sender.input(VbiosPageMsg::Refresh)
        ));
        bar.append(&load);
        bar.append(&refresh);
        content.append(&bar);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.append(&body);

        let model = Self {
            client,
            content,
            body,
            gpu_id: None,
            info: None,
            error: None,
            file: None,
            stats: None,
            edit_status: None,
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            VbiosPageMsg::Gpu { id, info } => {
                let nvidia = info.driver.starts_with("nvidia");
                let _ = sender.output(AppMsg::SetPageVisible(PageId::Vbios, nvidia));
                self.info = None;
                self.error = None;
                self.file = None;
                self.gpu_id = nvidia.then_some(id);
                if nvidia {
                    sender.input(VbiosPageMsg::Refresh);
                }
            }
            VbiosPageMsg::Stats(stats) => {
                // Only the power limits are used; redraw only when they change.
                let limits = |stats: &DeviceStats| {
                    (
                        stats.power.cap_min,
                        stats.power.cap_default,
                        stats.power.cap_max,
                    )
                };
                let changed = self.stats.as_deref().map(limits) != Some(limits(&stats));
                self.stats = Some(stats);
                if !changed {
                    return;
                }
            }
            VbiosPageMsg::Refresh => {
                let Some(id) = self.gpu_id.clone() else {
                    return;
                };
                let client = self.client.clone();
                relm4::spawn_local(glib::clone!(
                    #[strong]
                    sender,
                    async move {
                        let result = client
                            .get_vbios_info(&id)
                            .await
                            .map(Box::new)
                            .map_err(|err| format!("{err:#}"));
                        sender.input(VbiosPageMsg::Info { id, result });
                    }
                ));
            }
            VbiosPageMsg::Info { id, result } => {
                if self.gpu_id.as_deref() != Some(id.as_str()) {
                    return;
                }
                match result {
                    Ok(info) => {
                        self.info = Some(*info);
                        self.error = None;
                    }
                    Err(err) => self.error = Some(err),
                }
            }
            VbiosPageMsg::LoadFile => {
                let Some(id) = self.gpu_id.clone() else {
                    return;
                };
                let client = self.client.clone();
                let window = root.root().and_downcast::<gtk::Window>();
                relm4::spawn_local(glib::clone!(
                    #[strong]
                    sender,
                    async move {
                        if let Some(result) = load_file(&client, &id, window.as_ref()).await {
                            sender.input(VbiosPageMsg::FileLoaded(result));
                        }
                    }
                ));
                return;
            }
            VbiosPageMsg::LoadPath(path) => {
                let Some(id) = self.gpu_id.clone() else {
                    return;
                };
                let client = self.client.clone();
                relm4::spawn_local(glib::clone!(
                    #[strong]
                    sender,
                    async move {
                        let result = read_file(&client, &id, path).await;
                        sender.input(VbiosPageMsg::FileLoaded(result));
                    }
                ));
                return;
            }
            VbiosPageMsg::FileLoaded(result) => {
                self.edit_status = None;
                match result {
                    Ok(file) => {
                        self.file = Some(*file);
                        self.error = None;
                    }
                    Err(err) => self.error = Some(err),
                }
            }
            VbiosPageMsg::ClearFile => {
                self.file = None;
                self.edit_status = None;
            }
            VbiosPageMsg::SaveEdit => {
                self.save_edit(&sender, root);
                return;
            }
            VbiosPageMsg::EditSaved(result) => match result {
                Ok(saved) => {
                    let (file, status) = *saved;
                    self.file = Some(file);
                    self.edit_status = Some((status, false));
                }
                Err(err) => self.edit_status = Some((err, true)),
            },
            VbiosPageMsg::RevertEdit => {
                if let Some(file) = &self.file {
                    file.pending.borrow_mut().clear();
                }
                self.edit_status = None;
            }
            VbiosPageMsg::OpenExternalEditor => {
                if let Some(file) = &self.file {
                    self.edit_status = Some(match open_external_editor(&file.path) {
                        Ok(program) => (format!("Opened the file in {}", program.display()), false),
                        Err(err) => (err, true),
                    });
                }
            }
        }
        self.render(&sender);
    }
}

async fn load_file(
    client: &DaemonClient,
    gpu_id: &str,
    window: Option<&gtk::Window>,
) -> Option<Result<Box<LoadedFile>, String>> {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("vBIOS images"));
    for pattern in ["*.rom", "*.bin", "*.ROM", "*.BIN"] {
        filter.add_pattern(pattern);
    }
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let file = match gtk::FileDialog::builder()
        .title("Open vBIOS image")
        .filters(&filters)
        .modal(true)
        .build()
        .open_future(window)
        .await
    {
        Ok(file) => file,
        Err(err)
            if err.matches(gtk::DialogError::Cancelled)
                || err.matches(gtk::DialogError::Dismissed) =>
        {
            return None;
        }
        Err(err) => return Some(Err(format!("Could not select a file: {err}"))),
    };
    let Some(path) = file.path() else {
        return Some(Err("The selected file has no local path".to_owned()));
    };

    Some(read_file(client, gpu_id, path).await)
}

async fn read_file(
    client: &DaemonClient,
    gpu_id: &str,
    path: PathBuf,
) -> Result<Box<LoadedFile>, String> {
    let data =
        std::fs::read(&path).map_err(|err| format!("Could not read {}: {err}", path.display()))?;
    let report =
        lact_schema::vbios::parse(&data).map_err(|err| format!("{}: {err}", path.display()))?;
    let matches_flash = client
        .dump_vbios(gpu_id)
        .await
        .ok()
        .and_then(|flash| lact_schema::vbios::same_pci_images(&data, &flash));
    Ok(Box::new(LoadedFile::new(path, data, report, matches_flash)))
}

impl VbiosPage {
    fn render(&self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.body.first_child() {
            self.body.remove(&child);
        }
        let body = &self.body;

        if let Some(err) = &self.error {
            body.append(&label(err, &["error"]));
        }
        let Some(info) = &self.info else {
            if self.error.is_none() {
                body.append(&label("Reading the vBIOS…", &["dim-label"]));
            }
            return;
        };

        body.append(&self.flash_section(info));
        if let Some(status) = &info.override_status {
            body.append(&override_section(status, info));
        }

        // The tables: a picked file wins over the override image.
        if let Some(file) = &self.file {
            let mut source = format!("File: {}", file.path.display());
            match file.matches_flash {
                Some(true) => source.push_str("\nSame option ROM as the card's flash."),
                Some(false) => {
                    source.push_str("\nNot the card's flashed vBIOS: its option ROM differs.")
                }
                None => {}
            }
            let clear = gtk::Button::builder()
                .label("Close file")
                .halign(gtk::Align::Start)
                .build();
            clear.connect_clicked(glib::clone!(
                #[strong]
                sender,
                move |_| sender.input(VbiosPageMsg::ClearFile)
            ));
            body.append(&self.edit_section(file, sender));
            body.append(&self.tables_section(&source, &file.report, false));
            body.append(&clear);
        } else if let Some(report) = &info.override_image {
            let source = "Override image (the tables the GPU firmware was handed at boot)";
            body.append(&self.tables_section(source, report, true));
            if let Some(path) = info
                .override_status
                .as_ref()
                .and_then(|status| status.image_path.clone())
            {
                let button = gtk::Button::builder()
                    .label("Edit a copy of this image…")
                    .halign(gtk::Align::Start)
                    .tooltip_text(
                        "Opens the override image as the loaded file; edits are saved to a new \
                         file, the override image itself is not changed.",
                    )
                    .build();
                button.connect_clicked(glib::clone!(
                    #[strong]
                    sender,
                    move |_| sender.input(VbiosPageMsg::LoadPath(path.clone()))
                ));
                body.append(&button);
            }
        } else if let Some(err) = &info.override_image_error {
            body.append(&label(&format!("Override image: {err}"), &["error"]));
        } else {
            body.append(&label(
                "The flashed tables are not readable through the PCI expansion ROM. \
                 Use \"Load ROM file…\" with a full dump of this card's vBIOS to see and \
                 edit them.",
                &["dim-label"],
            ));
        }
    }

    fn edit_section(&self, file: &LoadedFile, sender: &ComponentSender<Self>) -> gtk::Widget {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.append(&label(
            "Changes are written to a new file; this one and the GPU are not touched. The new \
             file's signature is invalid, so it only takes effect when loaded through a vBIOS \
             override, and it must never be flashed. Raising power or voltage limits can damage \
             hardware.",
            &["warning"],
        ));

        match &file.fields {
            Ok(fields) => {
                let grid = gtk::Grid::builder()
                    .column_spacing(12)
                    .row_spacing(6)
                    .build();
                for (row, described) in fields.fields.iter().enumerate() {
                    let row = row as i32;
                    let (scale, unit, digits_note) = display_unit(described.field.unit());
                    let mut name = described.field.label().to_owned();
                    if described.field.experimental() {
                        name.push_str(" (experimental)");
                    }
                    let name = label(&name, &[]);
                    name.set_valign(gtk::Align::Center);

                    let lower = (described.min as f64 / scale).ceil();
                    let upper = (described.max as f64 / scale).floor();
                    let pending = file.pending.borrow().get(&described.field).copied();
                    let shown = pending.unwrap_or(described.current) as f64 / scale;
                    let spin = gtk::SpinButton::with_range(lower.min(shown), upper.max(shown), 1.0);
                    spin.set_value(shown);
                    spin.set_numeric(true);
                    let (field, current) = (described.field, described.current);
                    let pending = file.pending.clone();
                    spin.connect_value_changed(move |spin| {
                        #[allow(clippy::cast_possible_truncation)]
                        let stored = (spin.value() * scale).round() as i64;
                        let mut pending = pending.borrow_mut();
                        if stored == current {
                            pending.remove(&field);
                        } else {
                            pending.insert(field, stored);
                        }
                    });

                    let mut note = format!(
                        "{unit}   ·   currently {}",
                        fmt_value(described.current, scale)
                    );
                    if let Some(extra) = digits_note {
                        note.push_str(&format!("   ·   {extra}"));
                    }
                    if let Some(extra) = &described.note {
                        note.push_str(&format!("   ·   {extra}"));
                    }
                    grid.attach(&name, 0, row, 1, 1);
                    grid.attach(&spin, 1, row, 1, 1);
                    grid.attach(&label(&note, &["dim-label"]), 2, row, 1, 1);
                }
                content.append(&grid);

                for unavailable in &fields.unavailable {
                    content.append(&label(
                        &format!(
                            "{}: not editable here ({})",
                            unavailable.field.label(),
                            unavailable.reason
                        ),
                        &["dim-label"],
                    ));
                }
            }
            Err(err) => {
                content.append(&label(&format!("Not editable here: {err}"), &["dim-label"]))
            }
        }

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if file.fields.is_ok() {
            let save = gtk::Button::builder()
                .label("Save edited copy…")
                .css_classes(["suggested-action"])
                .build();
            save.connect_clicked(glib::clone!(
                #[strong]
                sender,
                move |_| sender.input(VbiosPageMsg::SaveEdit)
            ));
            let revert = gtk::Button::with_label("Revert");
            revert.connect_clicked(glib::clone!(
                #[strong]
                sender,
                move |_| sender.input(VbiosPageMsg::RevertEdit)
            ));
            buttons.append(&save);
            buttons.append(&revert);
        }
        let external = gtk::Button::with_label("Open in VBIOS Vix Editor");
        match find_external_editor() {
            Some(program) => external.set_tooltip_text(Some(&format!(
                "All other fields (current limits, input current, slot power, fans, other clock \
                 ranges, ALT/OV/VMIN voltages) and Ada/Ampere images: {}",
                program.display()
            ))),
            None => {
                external.set_sensitive(false);
                external.set_tooltip_text(Some(&format!(
                    "Not installed. Install it from {EXTERNAL_EDITOR_URL} (nvbios-editor in PATH), \
                     or set {EXTERNAL_EDITOR_ENV} to its executable."
                )));
            }
        }
        external.connect_clicked(glib::clone!(
            #[strong]
            sender,
            move |_| sender.input(VbiosPageMsg::OpenExternalEditor)
        ));
        buttons.append(&external);
        content.append(&buttons);

        if let Some((status, error)) = &self.edit_status {
            content.append(&label(
                status,
                if *error { &["error"] } else { &["success"] },
            ));
        }

        section("Edit a copy", &content)
    }

    fn save_edit(&mut self, sender: &ComponentSender<Self>, root: &gtk::ScrolledWindow) {
        let Some(file) = &self.file else { return };
        let changes = file
            .pending
            .borrow()
            .iter()
            .map(|(field, value)| (*field, *value))
            .collect::<Vec<_>>();
        if changes.is_empty() {
            self.edit_status = Some(("Nothing to save: no field was changed".to_owned(), true));
            self.render(sender);
            return;
        }
        let result = match lact_schema::vbios::edit::edit(&file.data, &changes) {
            Ok(result) => result,
            Err(err) => {
                self.edit_status = Some((format!("Not saved: {err}"), true));
                self.render(sender);
                return;
            }
        };

        let source = file.path.clone();
        let source_sha256 = lact_schema::vbios::sha256_hex(&file.data);
        let matches_flash = file.matches_flash;
        let window = root.root().and_downcast::<gtk::Window>();
        relm4::spawn_local(glib::clone!(
            #[strong]
            sender,
            async move {
                let stem = source.file_stem().map_or_else(
                    || "vbios".to_owned(),
                    |stem| stem.to_string_lossy().into_owned(),
                );
                let mut dialog = gtk::FileDialog::builder()
                    .title("Save edited vBIOS copy")
                    .accept_label("Save")
                    .initial_name(format!("{stem}-edited.rom"))
                    .modal(true);
                if let Some(folder) = source.parent() {
                    dialog = dialog.initial_folder(&gtk::gio::File::for_path(folder));
                }
                let target = match dialog.build().save_future(window.as_ref()).await {
                    Ok(target) => target,
                    Err(err)
                        if err.matches(gtk::DialogError::Cancelled)
                            || err.matches(gtk::DialogError::Dismissed) =>
                    {
                        return;
                    }
                    Err(err) => {
                        sender.input(VbiosPageMsg::EditSaved(Err(format!(
                            "Could not choose a file: {err}"
                        ))));
                        return;
                    }
                };
                let outcome = target
                    .path()
                    .ok_or_else(|| "The chosen file has no local path".to_owned())
                    .and_then(|path| {
                        write_edited(&path, &source, &source_sha256, &result)?;
                        let report = lact_schema::vbios::parse(&result.data).map_err(|err| err.to_string())?;
                        let summary = &result.summary;
                        let status = format!(
                            "Saved {} ({} bytes changed, objects {} rehashed{}) with a manifest next to it. \
                             It is now the loaded file.",
                            path.display(),
                            summary.changed_bytes,
                            summary
                                .rehashed_objects
                                .iter()
                                .map(|id| format!("{id:#04x}"))
                                .collect::<Vec<_>>()
                                .join(", "),
                            if summary.directory_digest_repaired {
                                ", stale directory digest repaired"
                            } else {
                                ""
                            }
                        );
                        let file = LoadedFile::new(path, result.data.clone(), report, matches_flash);
                        Ok(Box::new((file, status)))
                    });
                sender.input(VbiosPageMsg::EditSaved(outcome));
            }
        ));
    }

    fn flash_section(&self, info: &VbiosInfo) -> gtk::Widget {
        let grid = KeyValueGrid::new();
        match &info.flash {
            Some(flash) => {
                grid.row("Version", flash.version.as_deref().unwrap_or("—"));
                if !flash.strings.is_empty() {
                    grid.row("Board", &flash.strings.join("\n"));
                }
                grid.row(
                    "PCI device",
                    &format!("{:04x}:{:04x}", flash.vendor_id, flash.device_id),
                );
                if flash.completeness == ImageCompleteness::PciSlice {
                    grid.row(
                        "Readable",
                        "Option ROM only (legacy + EFI images, no data tables)",
                    );
                }
            }
            None => {
                grid.row(
                    "Error",
                    info.flash_error
                        .as_deref()
                        .unwrap_or("The flash could not be read"),
                );
            }
        }
        section("Flashed vBIOS", &grid.grid)
    }

    fn tables_section(&self, source: &str, report: &VbiosReport, running: bool) -> gtk::Widget {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.append(&label(source, &["dim-label"]));

        let identity = KeyValueGrid::new();
        identity.row("Version", report.version.as_deref().unwrap_or("—"));
        if !report.strings.is_empty() {
            identity.row("Board", &report.strings.join("\n"));
        }
        if let Some(integrity) = &report.integrity {
            let mut text = if integrity.consistent() {
                "Object hashes and checksums consistent".to_owned()
            } else {
                let mut problems = vec![];
                if !integrity.bad_object_ids.is_empty() {
                    let ids = integrity
                        .bad_object_ids
                        .iter()
                        .map(|id| format!("{id:#04x}"))
                        .collect::<Vec<_>>();
                    problems.push(format!("stale hash on object {}", ids.join(", ")));
                }
                if !integrity.directory_digest_valid {
                    problems.push("stale directory digest".to_owned());
                }
                if !integrity.image_checksums_valid {
                    problems.push("image checksum mismatch".to_owned());
                }
                format!("Edited image: {}", problems.join(", "))
            };
            text.push_str(
                "\nThe cryptographic signature is not checked. Never flash an edited image.",
            );
            identity.row("Integrity", &text);
        }
        content.append(&identity.grid);

        // Power
        let power = KeyValueGrid::new();
        if let Some(board) = report.power_policy(PowerPolicyRole::BoardPower) {
            power.row(
                "Board power",
                &format!(
                    "min {} · default {} · max {}",
                    watts(board.min),
                    watts(board.default),
                    watts(board.max)
                ),
            );
            // The minimum is left out: this fork reports its own lower floor there
            // (caps below the vBIOS minimum), not the table value.
            if running
                && let Some(stats) = &self.stats
                && let (Some(default), Some(max)) = (stats.power.cap_default, stats.power.cap_max)
            {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let matches = [(default, board.default), (max, board.max)]
                    .iter()
                    .all(|(live, table)| (live * 1000.0).round() as u32 == *table);
                power.row(
                    "Driver reports",
                    &format!(
                        "default {default:.0} W · max {max:.0} W{}",
                        if matches {
                            "  (matches)"
                        } else {
                            "  (differs; the image may not be in effect)"
                        }
                    ),
                );
            }
        }
        for (role, name) in [
            (PowerPolicyRole::InputCurrent12V, "12 V input current"),
            (
                PowerPolicyRole::NvvddOutputCurrent,
                "NVVDD current limit (OCP)",
            ),
            (
                PowerPolicyRole::MsvddOutputCurrent,
                "MSVDD current limit (OCP)",
            ),
        ] {
            if let Some(row) = report.power_policy(role) {
                power.row(
                    name,
                    &format!("default {} · max {}", amps(row.default), amps(row.max)),
                );
            }
        }
        if !power.is_empty() {
            content.append(&subsection("Power", &power.grid));
        }

        // Clock offset ranges
        if !report.clock_ranges.is_empty() {
            let clocks = KeyValueGrid::new();
            for range in report
                .clock_ranges
                .iter()
                .filter(|range| range.domain.is_some())
            {
                let domain = range.domain.as_deref().unwrap_or_default();
                let mut text = format!("{:+} … {:+} MHz", range.min_mhz, range.max_mhz);
                if domain == "DRAM" {
                    text.push_str(&format!(
                        "  ({:+} … {:+} MHz as an NVML/LACT offset)",
                        i32::from(range.min_mhz) * 2,
                        i32::from(range.max_mhz) * 2
                    ));
                }
                clocks.row(domain, &text);
            }
            content.append(&subsection("Clock offset ranges", &clocks.grid));
        }

        // Voltage rails
        for rail in &report.rails {
            let grid = KeyValueGrid::new();
            grid.row("Boot voltage", &volts(rail.boot_voltage_uv as f32));
            for limit in &rail.limits {
                let name = match limit.role {
                    RailLimitRole::Rel => "Reliability limit (REL)",
                    RailLimitRole::AltRel => "Alternate REL",
                    RailLimitRole::Ov => "Overvoltage limit (OV)",
                    RailLimitRole::Vmin => "Minimum (VMIN)",
                };
                let value = match (limit.selector, limit.min_uv, limit.max_uv) {
                    (None, ..) => "—".to_owned(),
                    (Some(_), Some(min), Some(max)) if (max - min).abs() < 1.0 => volts(max),
                    (Some(_), Some(min), Some(max)) => format!("{} … {}", volts(min), volts(max)),
                    (Some(selector), ..) => format!("equation {selector} (not a fixed clamp)"),
                };
                grid.row(name, &value);
            }
            content.append(&subsection(&format!("{} voltage", rail.rail), &grid.grid));
        }

        // Every power policy row, for reference
        if !report.power_policies.is_empty() {
            let grid = gtk::Grid::builder()
                .column_spacing(16)
                .row_spacing(2)
                .build();
            for (column, title) in ["Row", "Type", "Min", "Default", "Max"]
                .into_iter()
                .enumerate()
            {
                grid.attach(&label(title, &["heading"]), column as i32, 0, 1, 1);
            }
            for (i, row) in report
                .power_policies
                .iter()
                .filter(|row| row.kind != 0)
                .enumerate()
            {
                let fmt = |value: u32| match row.unit {
                    PowerUnit::Milliwatt => watts(value),
                    PowerUnit::Milliamp => amps(value),
                };
                let cells = [
                    row.index.to_string(),
                    format!("{:#04x}", row.kind),
                    fmt(row.min),
                    fmt(row.default),
                    fmt(row.max),
                ];
                for (column, cell) in cells.iter().enumerate() {
                    grid.attach(
                        &label(cell, &["numeric"]),
                        column as i32,
                        i as i32 + 1,
                        1,
                        1,
                    );
                }
            }
            let expander = gtk::Expander::builder()
                .label("All power policy rows")
                .child(&grid)
                .build();
            content.append(&expander);
        }

        for warning in &report.warnings {
            content.append(&label(warning, &["warning"]));
        }

        section("vBIOS tables", &content)
    }
}

/// (stored units per shown unit, shown unit, extra note) of a field.
fn display_unit(unit: EditUnit) -> (f64, &'static str, Option<&'static str>) {
    match unit {
        EditUnit::Milliwatt => (1000.0, "W", None),
        EditUnit::Microvolt => (1000.0, "mV", None),
        EditUnit::TableMhz => (1.0, "table MHz", Some("×2 as an NVML/LACT memory offset")),
    }
}

fn fmt_value(stored: i64, scale: f64) -> String {
    let shown = stored as f64 / scale;
    if shown.fract() == 0.0 {
        format!("{shown:.0}")
    } else {
        format!("{shown:.3}")
    }
}

/// Writes the edited image and a JSON manifest (`<file>.json`) describing the edit.
fn write_edited(
    path: &Path,
    source: &Path,
    source_sha256: &str,
    result: &lact_schema::vbios::edit::EditResult,
) -> Result<(), String> {
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(path, source) {
        return Err("Not saved: the edited copy cannot replace the source image".to_owned());
    }
    std::fs::write(path, &result.data)
        .map_err(|err| format!("Could not write {}: {err}", path.display()))?;

    let manifest = serde_json::json!({
        "tool": "LACT vBIOS editor",
        "source": source,
        "source_sha256": source_sha256,
        "output_sha256": lact_schema::vbios::sha256_hex(&result.data),
        "edit": result.summary,
        "cryptographic_signature": "not regenerated; never flash this image",
    });
    let mut manifest_path = path.as_os_str().to_owned();
    manifest_path.push(".json");
    let text = serde_json::to_string_pretty(&manifest).map_err(|err| err.to_string())?;
    std::fs::write(&manifest_path, text)
        .map_err(|err| format!("Saved the image, but could not write its manifest: {err}"))
}

fn find_external_editor() -> Option<PathBuf> {
    if let Some(program) = std::env::var_os(EXTERNAL_EDITOR_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| EXTERNAL_EDITOR_NAMES.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// Starts the external editor on `file`; it runs as a separate program.
fn open_external_editor(file: &Path) -> Result<PathBuf, String> {
    let program = find_external_editor()
        .ok_or_else(|| format!("VBIOS Vix Editor is not installed ({EXTERNAL_EDITOR_URL})"))?;
    let mut child = std::process::Command::new(&program)
        .arg(file)
        .spawn()
        .map_err(|err| format!("Could not start {}: {err}", program.display()))?;
    // Reap it when it exits.
    std::thread::spawn(move || child.wait());
    Ok(program)
}

fn override_section(status: &VbiosOverrideStatus, info: &VbiosInfo) -> gtk::Widget {
    let grid = KeyValueGrid::new();
    let state = if status.active() {
        "Active: the GPU firmware is handed a host-supplied image at boot"
    } else if status.requested && status.disabled {
        "Requested but disabled (RmLinuxVbiosOverrideDisable)"
    } else if status.requested {
        "Requested, but the running nvidia module has no override support"
    } else {
        "Not configured (the GPU firmware reads the flash)"
    };
    grid.row("State", state);
    if let Some(scope) = status.scope {
        let scope = match scope {
            VbiosOverrideScope::Global => "All GPUs (NVreg_RegistryDwords)",
            VbiosOverrideScope::PerDevice => "This GPU (per-device registry keys)",
        };
        let mode = status
            .mode
            .map(|mode| format!(", mode {mode}"))
            .unwrap_or_default();
        grid.row("Configured for", &format!("{scope}{mode}"));
    }
    match status.module_supports_override {
        Some(true) => grid.row("Driver module", "Carries the vBIOS override patch"),
        Some(false) => grid.row("Driver module", "Stock (no override support)"),
        None => {}
    }
    if let Some(path) = &status.image_path {
        grid.row("Image", &path.display().to_string());
        if status.image_modified_since_boot == Some(true) {
            grid.row(
                "Note",
                "The image file changed after boot, so it may not be the one that was loaded.",
            );
        }
        if let (Some(image), Some(flash)) = (&info.override_image, &info.flash)
            && image.version != flash.version
        {
            grid.row(
                "Versions",
                &format!(
                    "image {} · flash {}",
                    image.version.as_deref().unwrap_or("—"),
                    flash.version.as_deref().unwrap_or("—")
                ),
            );
        }
    }
    section("vBIOS override", &grid.grid)
}

struct KeyValueGrid {
    grid: gtk::Grid,
    rows: std::cell::Cell<i32>,
}

impl KeyValueGrid {
    fn new() -> Self {
        Self {
            grid: gtk::Grid::builder()
                .column_spacing(16)
                .row_spacing(4)
                .build(),
            rows: std::cell::Cell::new(0),
        }
    }

    fn row(&self, key: &str, value: &str) {
        let row = self.rows.get();
        let key = label(key, &["dim-label"]);
        key.set_valign(gtk::Align::Start);
        let value = label(value, &[]);
        value.set_selectable(true);
        value.set_hexpand(true);
        self.grid.attach(&key, 0, row, 1, 1);
        self.grid.attach(&value, 1, row, 1, 1);
        self.rows.set(row + 1);
    }

    fn is_empty(&self) -> bool {
        self.rows.get() == 0
    }
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(
            classes
                .iter()
                .map(|class| (*class).to_owned())
                .collect::<Vec<_>>(),
        )
        .build()
}

fn section(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 8);
    frame.append(&label(title, &["title-4"]));
    let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    card.add_css_class("card");
    child.set_margin_all(12);
    card.append(child);
    frame.append(&card);
    frame.upcast()
}

fn subsection(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 4);
    frame.append(&label(title, &["heading"]));
    frame.append(child);
    frame.upcast()
}

#[allow(clippy::cast_precision_loss)]
fn watts(milliwatts: u32) -> String {
    format!("{:.0} W", milliwatts as f64 / 1000.0)
}

#[allow(clippy::cast_precision_loss)]
fn amps(milliamps: u32) -> String {
    format!("{:.0} A", milliamps as f64 / 1000.0)
}

fn volts(microvolts: f32) -> String {
    format!("{:.3} V", microvolts / 1_000_000.0)
}
