// SPDX-License-Identifier: MIT OR Apache-2.0
//! GUI application state and egui layout (design §8.3): device and layout
//! pickers, a slot map per bank with status highlighting, write/extract/wipe
//! per slot, flat-image export, live progress, and the equivalent CLI
//! command line for every operation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use notify::{RecursiveMode, Watcher};
use sdslot_core::device::DeviceInfo;
use sdslot_core::events::{OpKind, SlotState};
use sdslot_core::layout::Layout;
use sdslot_core::units::format_bytes;

use crate::backend::{self, RunningOp};
use crate::devices::DeviceEntry;
use crate::ops::{format_elapsed, LogMsg, OpFold, SlotOutcome, SlotUpdate, SlotView, ViewState};
use crate::settings::{settings_path, Settings};
use crate::theme;

/// A running CLI operation: the subprocess handle plus the pure event fold
/// (`ops::OpFold`) that turns its JSON stream into progress/slot-map state.
struct OpState {
    running: RunningOp,
    fold: OpFold,
}

/// One row of the "write all" confirmation: slot, source image, byte range.
struct PlannedWrite {
    bank: String,
    unit: u32,
    image: PathBuf,
    offset: u64,
    slot_len: u64,
    missing: bool,
}

enum Modal {
    None,
    ConfirmWrite {
        bank: String,
        unit: u32,
        image: PathBuf,
    },
    ConfirmWriteSelected {
        planned: Vec<PlannedWrite>,
        /// Selected slots with no image to write; ignored with a note.
        no_image: usize,
    },
    ConfirmExtractSelected {
        slots: Vec<(String, u32)>,
        dir: PathBuf,
    },
    ConfirmWipeSelected {
        slots: Vec<(String, u32)>,
    },
    /// Shown when the user checks "Advanced"; enables it only on confirm.
    ConfirmAdvanced,
    /// Shown when the user checks "Select first removable disk at startup";
    /// enables it only on confirm.
    ConfirmAutoSelect,
    ConfirmWipe {
        bank: String,
        unit: u32,
    },
    Extract {
        bank: String,
        unit: u32,
        length_choice: usize,
    },
}

/// A copyable tag for which `Modal` variant is showing, so `modal_windows`
/// can pick a handler without holding a borrow of `self.modal` across the
/// `&mut self` call that handler needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    None,
    ConfirmWrite,
    ConfirmWriteSelected,
    ConfirmExtractSelected,
    ConfirmWipeSelected,
    ConfirmAdvanced,
    ConfirmAutoSelect,
    ConfirmWipe,
    Extract,
}

impl Modal {
    fn kind(&self) -> ModalKind {
        match self {
            Modal::None => ModalKind::None,
            Modal::ConfirmWrite { .. } => ModalKind::ConfirmWrite,
            Modal::ConfirmWriteSelected { .. } => ModalKind::ConfirmWriteSelected,
            Modal::ConfirmExtractSelected { .. } => ModalKind::ConfirmExtractSelected,
            Modal::ConfirmWipeSelected { .. } => ModalKind::ConfirmWipeSelected,
            Modal::ConfirmAdvanced => ModalKind::ConfirmAdvanced,
            Modal::ConfirmAutoSelect => ModalKind::ConfirmAutoSelect,
            Modal::ConfirmWipe { .. } => ModalKind::ConfirmWipe,
            Modal::Extract { .. } => ModalKind::Extract,
        }
    }
}

/// What a modal's `show` produced this frame: nothing yet, a plain dismiss,
/// one of the two settings confirmations, or a CLI operation to launch.
enum ModalOutcome {
    None,
    Close,
    EnableAdvanced,
    EnableAutoSelect,
    Run(backend::CliCommand),
}

const LENGTH_CHOICES: [&str; 3] = ["canonical", "toc", "slot"];

// Slot-map column widths (points), shared by every bank's table so the
// columns line up across the whole panel.
const COL_SEL: f32 = 24.0;
const COL_UNIT: f32 = 36.0;
const COL_NAME: f32 = 200.0;
const COL_STATUS: f32 = 130.0;
const COL_SIZE: f32 = 90.0;
const COL_ACTIONS: f32 = 240.0;

/// The smallest common SD card marketing size (decimal GB) that fits
/// `bytes`, e.g. "a 16 GB" — sizing guidance for the card map.
fn suggest_card(bytes: u64) -> String {
    const SIZES_GB: [u64; 10] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512];
    for gb in SIZES_GB {
        if gb * 1_000_000_000 >= bytes {
            return format!("a {gb} GB");
        }
    }
    "a 1 TB".to_string()
}

pub struct App {
    devices: Vec<DeviceEntry>,
    selected_device: Option<usize>,
    /// Alternative target: a regular file (full-card image), design §9.
    file_device: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
    layout: Option<Layout>,
    layout_error: Option<String>,
    slot_states: HashMap<(String, u32), SlotView>,
    /// Slots ticked for the batch Write/Extract/Wipe Selected operations.
    /// Auto-populated on manifest load with every slot whose image exists.
    selected_slots: HashSet<(String, u32)>,
    op: Option<OpState>,
    log: VecDeque<String>,
    modal: Modal,
    /// Persisted settings (~/.sdslot); every change is written back.
    settings: Settings,
    settings_open: bool,
    /// A window resize was observed; save the new size once it settles.
    window_save_at: Option<Instant>,
    /// Manifest-named image files currently absent on disk (disables their
    /// Write buttons).
    missing_images: HashSet<PathBuf>,
    images_checked_at: Option<Instant>,
    /// Watches the manifest images' directories; any filesystem event there
    /// sets `fs_dirty` so the missing-image set refreshes in (near) real
    /// time. A slow periodic re-stat remains as fallback for filesystems
    /// where watching is unreliable (network mounts).
    _watcher: Option<notify::RecommendedWatcher>,
    fs_dirty: Arc<AtomicBool>,
    /// Fresh enumeration from the hotplug poller thread; arrives only when
    /// the device set actually changed.
    devices_rx: std::sync::mpsc::Receiver<Vec<DeviceInfo>>,
    egui_ctx: egui::Context,
}

impl App {
    /// `initial_manifest` is the layout given on the command line, loaded as
    /// if it had been picked with the Open… button.
    pub fn new(
        initial_manifest: Option<PathBuf>,
        egui_ctx: egui::Context,
        settings: Settings,
    ) -> App {
        // Event-driven hotplug listener: listens for OS-level device arrival/removal
        // events (DiskArbitration on macOS, WM_DEVICECHANGE on Windows, Netlink uevents
        // on Linux) and enumerates devices only when a hotplug event occurs or on startup.
        let (devices_tx, devices_rx) = std::sync::mpsc::channel::<Vec<DeviceInfo>>();
        let (signal_tx, signal_rx) = std::sync::mpsc::channel::<()>();
        sdslot_core::device::hotplug::spawn_hotplug_listener(signal_tx);

        let poll_ctx = egui_ctx.clone();
        std::thread::spawn(move || {
            let mut last: Option<Vec<DeviceInfo>> = None;
            while signal_rx.recv().is_ok() {
                if let Ok(devices) = backend::enumerate_local() {
                    if last.as_ref() != Some(&devices) {
                        last = Some(devices.clone());
                        if devices_tx.send(devices).is_err() {
                            return; // app is gone
                        }
                        poll_ctx.request_repaint();
                    }
                }
            }
        });

        let mut app = App {
            devices: Vec::new(),
            selected_device: None,
            file_device: None,
            manifest_path: None,
            layout: None,
            layout_error: None,
            slot_states: HashMap::new(),
            selected_slots: HashSet::new(),
            op: None,
            log: VecDeque::new(),
            modal: Modal::None,
            settings,
            settings_open: false,
            window_save_at: None,
            missing_images: HashSet::new(),
            images_checked_at: None,
            _watcher: None,
            fs_dirty: Arc::new(AtomicBool::new(false)),
            devices_rx,
            egui_ctx,
        };
        app.log(format!("sdslot-gui {}", sdslot_core::VERSION_FULL));
        app.log(format!(
            "{} — MIT OR Apache-2.0 — {}",
            sdslot_core::COPYRIGHT,
            sdslot_core::REPOSITORY
        ));
        app.refresh_devices();
        if let Some(path) = initial_manifest {
            app.load_manifest(path);
        }
        app
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.log(format!("cannot save settings: {e}"));
        }
    }

    fn log(&mut self, line: impl Into<String>) {
        self.log.push_back(line.into());
        while self.log.len() > 200 {
            self.log.pop_front();
        }
    }

    fn refresh_devices(&mut self) {
        match backend::enumerate_local() {
            Ok(devices) => {
                self.apply_devices(devices.into_iter().map(DeviceEntry::from).collect(), false)
            }
            Err(e) => self.log(format!("device enumeration failed: {e}")),
        }
    }

    /// Install a new device list: selection follows the device *path* (list
    /// order can change under hotplug), and with the auto-select setting on,
    /// an idle app picks up a newly plugged removable disk.
    fn apply_devices(&mut self, new: Vec<DeviceEntry>, announce: bool) {
        let old_paths: Vec<String> = self.devices.iter().map(|d| d.path.clone()).collect();
        let selected_path = self
            .selected_device
            .and_then(|i| self.devices.get(i))
            .map(|d| d.path.clone());
        self.devices = new;
        self.selected_device =
            selected_path.and_then(|p| self.devices.iter().position(|d| d.path == p));
        self.prune_selection();

        let new_paths: Vec<String> = self.devices.iter().map(|d| d.path.clone()).collect();
        if announce && old_paths != new_paths {
            self.log("device list updated (hotplug)");
        }

        if self.settings.select_first_removable
            && self.selected_device.is_none()
            && self.file_device.is_none()
            && !self.busy()
        {
            let first = self
                .devices
                .iter()
                .position(|d| d.has_media() && d.is_removable() && !d.system);
            if let Some(i) = first {
                self.selected_device = Some(i);
                let path = self.devices[i].path.clone();
                self.log(format!("auto-selected removable disk: {path}"));
            }
        }
    }

    fn load_manifest(&mut self, path: PathBuf) {
        match Layout::load(&path) {
            Ok(layout) => {
                self.layout = Some(layout);
                self.layout_error = None;
                self.manifest_path = Some(path);
                self.slot_states.clear();
                self.rebuild_watcher();
                self.recheck_images();
                self.auto_select_existing();
            }
            Err(e) => {
                self.layout = None;
                self.layout_error = Some(e.to_string());
                self.manifest_path = Some(path);
                self._watcher = None;
                self.missing_images.clear();
                self.selected_slots.clear();
            }
        }
    }

    /// Tick every slot whose manifest image exists on disk (the manifest
    /// just loaded; `missing_images` is fresh).
    fn auto_select_existing(&mut self) {
        self.selected_slots.clear();
        if let Some(layout) = &self.layout {
            for bank in &layout.banks {
                for slot in bank.slots.values() {
                    if let Some(image) = &slot.image {
                        if !self.missing_images.contains(image) {
                            self.selected_slots.insert((bank.name.clone(), slot.unit));
                        }
                    }
                }
            }
        }
    }

    /// Selected slots in layout order (bank order, then unit).
    fn selected_sorted(&self, layout: &Layout) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        for bank in &layout.banks {
            for unit in 0..bank.units {
                if self.selected_slots.contains(&(bank.name.clone(), unit)) {
                    out.push((bank.name.clone(), unit));
                }
            }
        }
        out
    }

    /// Stat every manifest-named image and rebuild the missing set.
    fn recheck_images(&mut self) {
        self.missing_images.clear();
        if let Some(layout) = &self.layout {
            for bank in &layout.banks {
                for slot in bank.slots.values() {
                    if let Some(image) = &slot.image {
                        if !image.is_file() {
                            self.missing_images.insert(image.clone());
                        }
                    }
                }
            }
        }
        self.images_checked_at = Some(Instant::now());
    }

    /// Watch the directories containing the manifest's images; events flip
    /// `fs_dirty` and wake the UI, which then re-stats the image files.
    fn rebuild_watcher(&mut self) {
        self._watcher = None;
        let Some(layout) = &self.layout else { return };
        let dirs: HashSet<PathBuf> = layout
            .banks
            .iter()
            .flat_map(|b| b.slots.values())
            .filter_map(|s| s.image.as_ref())
            .filter_map(|p| p.parent().map(|d| d.to_path_buf()))
            .collect();
        if dirs.is_empty() {
            return;
        }
        let dirty = self.fs_dirty.clone();
        let ctx = self.egui_ctx.clone();
        let mut watcher =
            match notify::recommended_watcher(move |_res: Result<notify::Event, notify::Error>| {
                dirty.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }) {
                Ok(w) => w,
                Err(e) => {
                    self.log(format!(
                        "image directory watcher unavailable ({e}); falling back to polling"
                    ));
                    return;
                }
            };
        for dir in dirs {
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                self.log(format!("cannot watch {}: {e}", dir.display()));
            }
        }
        self._watcher = Some(watcher);
    }

    fn device_arg(&self) -> Option<String> {
        if let Some(f) = &self.file_device {
            return Some(f.display().to_string());
        }
        self.selected_device
            .and_then(|i| self.devices.get(i))
            .map(|d| d.path.clone())
    }

    /// May this device be chosen as the target right now? The system disk is
    /// never selectable (the CLI refuses it outright); non-removable disks
    /// need confirmed advanced mode; no media, nothing to write.
    fn device_selectable(&self, d: &DeviceEntry) -> bool {
        d.has_media() && !d.system && (d.is_removable() || self.settings.advanced)
    }

    /// Why a device is not selectable, for the grayed-out entry.
    fn device_block_reason(&self, d: &DeviceEntry) -> &'static str {
        if !d.has_media() {
            "no media"
        } else if d.system {
            "system disk — never writable"
        } else if !d.is_removable() {
            "non-removable — enable Advanced"
        } else {
            ""
        }
    }

    /// Drop a selection that the current filters/mode no longer allow.
    fn prune_selection(&mut self) {
        if let Some(i) = self.selected_device {
            let ok = self
                .devices
                .get(i)
                .is_some_and(|d| self.device_selectable(d));
            if !ok {
                self.selected_device = None;
            }
        }
    }

    fn busy(&self) -> bool {
        self.op.as_ref().is_some_and(|o| !o.fold.finished)
    }

    fn ready_for_ops(&self) -> bool {
        !self.busy() && self.layout.is_some() && self.device_arg().is_some()
    }

    fn target_needs_force(&self) -> bool {
        self.file_device.is_none()
            && self
                .selected_device
                .and_then(|i| self.devices.get(i))
                .is_some_and(|d| !d.is_removable())
    }

    fn start_op(&mut self, cmd: backend::CliCommand) {
        let label = cmd.label();
        let running = backend::spawn_op(cmd);
        if self.settings.developer_mode {
            self.log(format!("$ {}", running.equivalent));
        } else {
            self.log(format!("starting {label}…"));
        }
        self.op = Some(OpState {
            running,
            fold: OpFold::new(label),
        });
    }

    fn start_status(&mut self) {
        let (Some(device), Some(manifest)) = (self.device_arg(), self.manifest_path.clone()) else {
            return;
        };
        self.slot_states.clear();
        self.start_op(backend::CliCommand::Status {
            device,
            manifest: Some(manifest),
        });
    }

    /// The selected slots that name an image, with byte ranges — the preview
    /// for the "Write Selected…" confirmation — plus the count of selected
    /// slots that have no image to write (they are ignored). `None` when
    /// nothing writable is selected.
    fn plan_selected_writes(&self) -> Option<(Vec<PlannedWrite>, usize)> {
        let layout = self.layout.as_ref()?;
        let selected = self.selected_sorted(layout);
        let summary = sdslot_core::engine::summarize_plan(layout, &selected);
        if summary.items.is_empty() {
            None
        } else {
            let planned = summary
                .items
                .into_iter()
                .filter_map(|item| {
                    let image = item.image?;
                    Some(PlannedWrite {
                        bank: item.bank,
                        unit: item.unit,
                        image,
                        offset: item.offset,
                        slot_len: item.slot_len,
                        missing: item.missing,
                    })
                })
                .collect();
            Some((planned, summary.unmapped_count))
        }
    }

    fn poll_op(&mut self) {
        let Some(op) = &mut self.op else { return };
        let mut lines: Vec<LogMsg> = Vec::new();
        let mut updates: Vec<((String, u32), SlotUpdate)> = Vec::new();
        while let Ok(msg) = op.running.rx.try_recv() {
            let (msg_lines, msg_updates) = op.fold.apply(msg);
            lines.extend(msg_lines);
            updates.extend(msg_updates);
        }
        for (k, update) in updates {
            match update {
                SlotUpdate::Status(view) => {
                    self.slot_states.insert(k, view);
                }
                SlotUpdate::Outcome(SlotOutcome::Set { state, length }) => {
                    // Preserve the known name; the row falls back to the
                    // manifest slot name when none is recorded.
                    let prior = self.slot_states.get(&k);
                    let name = prior.and_then(|v| v.name.clone());
                    let length = length.or_else(|| prior.and_then(|v| v.length));
                    self.slot_states.insert(
                        k,
                        SlotView {
                            state,
                            name,
                            length,
                        },
                    );
                }
                SlotUpdate::Outcome(SlotOutcome::Cleared) => {
                    // Keep the row visible as an explicit blank pack, the
                    // same state a status re-scan would report.
                    let name = self.slot_states.get(&k).and_then(|v| v.name.clone());
                    self.slot_states.insert(
                        k,
                        SlotView {
                            state: ViewState::Core(SlotState::Wiped),
                            name,
                            length: None,
                        },
                    );
                }
            }
        }
        // If the operation ended (cancel, crash) with rows still marked
        // busy, resolve them honestly: an interrupted write/wipe leaves the
        // slot suspect; an interrupted verify leaves the data written but
        // unconfirmed.
        if self.op.as_ref().is_some_and(|o| o.fold.finished) {
            for view in self.slot_states.values_mut() {
                view.state = match view.state {
                    ViewState::Busy(OpKind::Verify) => ViewState::Written,
                    ViewState::Busy(_) => ViewState::Core(SlotState::Differs),
                    other => other,
                };
            }
        }
        for msg in lines {
            match msg {
                LogMsg::New(l) => self.log(l),
                LogMsg::Complete {
                    start,
                    suffix,
                    fallback,
                } => {
                    if self.log.back().is_some_and(|b| *b == start) {
                        if let Some(back) = self.log.back_mut() {
                            back.push_str(&suffix);
                        }
                    } else {
                        // Something else was logged in between; keep the
                        // outcome on its own line.
                        self.log(fallback);
                    }
                }
            }
        }
    }
}

fn state_label(state: ViewState) -> (&'static str, egui::Color32) {
    match state {
        ViewState::Busy(kind) => (
            match kind {
                OpKind::Write => "writing…",
                OpKind::Verify => "verifying…",
                OpKind::Wipe => "wiping…",
                OpKind::Read | OpKind::Status => "reading…",
            },
            theme::p().teal,
        ),
        ViewState::Written => ("written", theme::p().teal),
        ViewState::Core(SlotState::Matches) => ("✔ matches", theme::p().green),
        ViewState::Core(SlotState::Modified) => ("⚠ modified", theme::p().orange),
        ViewState::Core(SlotState::Differs) => ("≠ differs", theme::p().orange),
        ViewState::Core(SlotState::Wiped) => ("wiped", theme::p().text_dim),
        ViewState::Core(SlotState::Unknown) => ("", theme::p().text_dim),
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_op();
        // Persist the window size the user settles on (debounced so a drag
        // doesn't hammer the settings file).
        let size = ctx.input(|i| i.screen_rect().size());
        if (size.x - self.settings.window_width).abs() > 1.0
            || (size.y - self.settings.window_height).abs() > 1.0
        {
            self.settings.window_width = size.x;
            self.settings.window_height = size.y;
            self.window_save_at = Some(Instant::now());
        }
        if let Some(at) = self.window_save_at {
            if at.elapsed() > Duration::from_millis(800) {
                self.window_save_at = None;
                self.save_settings();
            } else {
                ctx.request_repaint_after(Duration::from_millis(850));
            }
        }
        // Hotplug: the poller only sends when the device set changed.
        while let Ok(devices) = self.devices_rx.try_recv() {
            self.apply_devices(devices.into_iter().map(DeviceEntry::from).collect(), true);
        }
        // Refresh the missing-image set when the watcher saw filesystem
        // activity, or periodically as a fallback.
        if self.layout.is_some() {
            let dirty = self.fs_dirty.swap(false, Ordering::Relaxed);
            let stale = self
                .images_checked_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(5));
            if dirty || stale {
                self.recheck_images();
            }
        }
        if self.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        self.top_panel(ctx);
        self.bottom_panel(ctx);
        self.central_panel(ctx);
        self.settings_window(ctx);
        self.modal_windows(ctx);
    }
}

impl App {
    fn top_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Device:");
                let selected_label = if let Some(f) = &self.file_device {
                    format!("file: {}", f.display())
                } else {
                    self.selected_device
                        .and_then(|i| self.devices.get(i))
                        .map(|d| d.label())
                        .unwrap_or_else(|| "(select a device)".into())
                };
                egui::ComboBox::from_id_salt("device")
                    .width(360.0)
                    .selected_text(selected_label)
                    .show_ui(ui, |ui| {
                        let mut shown = 0;
                        for i in 0..self.devices.len() {
                            let d = &self.devices[i];
                            // Media-less devices (empty card readers) are
                            // hidden unless "Show all".
                            if !d.has_media() && !self.settings.show_all {
                                continue;
                            }
                            shown += 1;
                            let selectable = self.device_selectable(d);
                            let mut label = d.label();
                            if !selectable {
                                let reason = self.device_block_reason(d);
                                if !reason.is_empty() {
                                    label = format!("{label}  ({reason})");
                                }
                            }
                            let checked =
                                self.selected_device == Some(i) && self.file_device.is_none();
                            if ui
                                .add_enabled(selectable, egui::SelectableLabel::new(checked, label))
                                .clicked()
                            {
                                self.selected_device = Some(i);
                                self.file_device = None;
                            }
                        }
                        if shown == 0 {
                            ui.label("(no candidate devices — insert a card and rescan)");
                        }
                    });
                if ui.button("⟳").on_hover_text("Rescan devices").clicked() {
                    self.refresh_devices();
                }
                if ui.button("Settings…").clicked() {
                    self.settings_open = !self.settings_open;
                }
                if ui
                    .button("File image…")
                    .on_hover_text("Target a regular file instead of a raw device")
                    .clicked()
                {
                    if let Some(f) = rfd::FileDialog::new()
                        .set_title("Choose or create a card image file")
                        .save_file()
                    {
                        self.file_device = Some(f);
                        self.selected_device = None;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Layout:");
                // Open/Reload come before the path text: a horizontal
                // layout doesn't wrap, so a long manifest path (deep
                // temp/scratch directories, etc.) rendered first would push
                // these buttons off the edge of the window and make them
                // unreachable. Rendering the (truncated, tooltip-on-hover)
                // path last means it can only ever shrink to fit, never
                // push anything else out of reach.
                if ui.button("Open…").clicked() {
                    if let Some(f) = rfd::FileDialog::new()
                        .add_filter("TOML layout", &["toml"])
                        .pick_file()
                    {
                        self.load_manifest(f);
                    }
                }
                if self.manifest_path.is_some() && ui.button("Reload").clicked() {
                    if let Some(p) = self.manifest_path.clone() {
                        self.load_manifest(p);
                    }
                }
                let label = self
                    .manifest_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(open a layout manifest)".into());
                ui.add(egui::Label::new(egui::RichText::new(&label).monospace()).truncate())
                    .on_hover_text(label);
            });
            ui.horizontal(|ui| {
                let ready = self.ready_for_ops();
                if ui
                    .add_enabled(ready, egui::Button::new("Refresh status"))
                    .on_hover_text("Read the card's TOC / hash-probe the slots")
                    .clicked()
                {
                    self.start_status();
                }
                let can_image = !self.busy() && self.layout.is_some();
                if ui
                    .add_enabled(can_image, egui::Button::new("Export flat image…"))
                    .on_hover_text(
                        "Assemble a full card image file writable with dd or balenaEtcher",
                    )
                    .clicked()
                {
                    if let (Some(manifest), Some(out)) = (
                        self.manifest_path.clone(),
                        rfd::FileDialog::new()
                            .set_title("Save flat card image")
                            .set_file_name("card.img")
                            .save_file(),
                    ) {
                        self.start_op(backend::CliCommand::Image {
                            manifest,
                            out,
                            slots: vec![],
                            verify: self.settings.verify,
                            yes: true,
                        });
                    }
                }
            });
            // Batch operations over the ticked slots.
            ui.horizontal(|ui| {
                let ready = self.ready_for_ops();
                let have_layout = self.layout.is_some();
                let count = self.selected_slots.len();
                ui.label(
                    egui::RichText::new(format!("Selection ({count}):")).color(theme::p().text_dim),
                );
                if ui
                    .add_enabled(have_layout, egui::Button::new("All"))
                    .on_hover_text(if self.settings.hide_empty_slots {
                        "Tick every visible slot (hidden empty slots stay unticked)"
                    } else {
                        "Tick every slot"
                    })
                    .clicked()
                {
                    if let Some(layout) = self.layout.clone() {
                        self.selected_slots = layout
                            .banks
                            .iter()
                            .flat_map(|b| {
                                (0..b.units)
                                    .filter(|&u| self.slot_visible(b, u))
                                    .map(|u| (b.name.clone(), u))
                                    .collect::<Vec<_>>()
                            })
                            .collect();
                    }
                }
                if ui
                    .add_enabled(have_layout, egui::Button::new("None"))
                    .on_hover_text("Untick every slot")
                    .clicked()
                {
                    self.selected_slots.clear();
                }
                ui.separator();
                let can_batch = ready && count > 0;
                if ui
                    .add_enabled(can_batch, egui::Button::new("Write Selected…"))
                    .on_hover_text("Write the ticked slots' images in one pass")
                    .clicked()
                {
                    match self.plan_selected_writes() {
                        Some((planned, no_image)) => {
                            self.modal = Modal::ConfirmWriteSelected { planned, no_image }
                        }
                        None => self.log("no selected slot has an image; nothing to write"),
                    }
                }
                if ui
                    .add_enabled(can_batch, egui::Button::new("Extract Selected…"))
                    .on_hover_text("Extract the ticked slots into a folder")
                    .clicked()
                {
                    if let Some(layout) = self.layout.clone() {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("Choose a folder for the extracted images")
                            .pick_folder()
                        {
                            self.modal = Modal::ConfirmExtractSelected {
                                slots: self.selected_sorted(&layout),
                                dir,
                            };
                        }
                    }
                }
                if ui
                    .add_enabled(can_batch, egui::Button::new("Wipe Selected…"))
                    .on_hover_text("Zero the ticked slots (blank packs)")
                    .clicked()
                {
                    if let Some(layout) = self.layout.clone() {
                        self.modal = Modal::ConfirmWipeSelected {
                            slots: self.selected_sorted(&layout),
                        };
                    }
                }
            });
            if let Some(err) = &self.layout_error {
                ui.colored_label(theme::p().red, format!("layout error: {err}"));
            }
            ui.add_space(4.0);
        });
    }

    /// Does this slot hold (or is it about to hold) meaningful content?
    /// Wiped and unknown slots count as free space in the card map.
    fn slot_occupied(&self, bank: &sdslot_core::layout::Bank, unit: u32) -> bool {
        let has_content = self
            .slot_states
            .get(&(bank.name.clone(), unit))
            .is_some_and(|v| {
                matches!(
                    v.state,
                    ViewState::Written
                        | ViewState::Busy(_)
                        | ViewState::Core(
                            SlotState::Matches | SlotState::Modified | SlotState::Differs
                        )
                )
            });
        has_content || bank.slots.get(&unit).is_some_and(|s| s.image.is_some())
    }

    /// The card map: the whole card to scale, banks at their offsets, slots
    /// filled dark when occupied and light when free, the TOC as a tick, and
    /// anything past the device capacity flagged in red. With no card
    /// selected, a generic 8 GB card is the reference so the user can judge
    /// what size card the layout needs. Capacity tracks the enumerated
    /// device each frame, so inserting or swapping a card refreshes the map
    /// via the hotplug poller.
    fn card_map(&self, ui: &mut egui::Ui, layout: &Layout) {
        /// A marketed "8 GB" card in decimal bytes.
        const GENERIC_CARD: u64 = 8_000_000_000;
        let device_size = if self.file_device.is_some() {
            None // file targets grow on demand
        } else {
            self.selected_device
                .and_then(|i| self.devices.get(i))
                .and_then(|d| d.size_bytes)
        };
        let generic = device_size.is_none();
        let capacity = device_size.unwrap_or(GENERIC_CARD);
        let layout_end = layout.max_extent_end();
        let scale_end = capacity.max(layout_end);
        if scale_end == 0 {
            return;
        }
        let overflow = layout_end > capacity;
        // A real card overflowing is an error (red); outgrowing the generic
        // reference card is sizing guidance (orange).
        let overflow_color = if generic {
            theme::p().orange
        } else {
            theme::p().red
        };

        const STRIP_H: f32 = 34.0;
        const LABEL_H: f32 = 14.0;
        let width = ui.available_width();
        let (area, response) =
            ui.allocate_exact_size(egui::vec2(width, STRIP_H + LABEL_H), egui::Sense::hover());
        let painter = ui.painter_at(area);
        let strip = egui::Rect::from_min_max(
            area.left_top(),
            egui::pos2(area.right(), area.top() + STRIP_H),
        );
        let px = |bytes: u64| strip.left() + (bytes as f32 / scale_end as f32) * strip.width();

        painter.rect_filled(strip, 6.0, egui::Color32::from_rgb(0x14, 0x16, 0x1c));
        if overflow {
            // The region past the end of the (real or generic) card.
            let danger = egui::Rect::from_min_max(
                egui::pos2(px(capacity), strip.top()),
                strip.right_bottom(),
            );
            painter.rect_filled(
                danger,
                0.0,
                egui::Color32::from_rgba_unmultiplied(
                    overflow_color.r(),
                    overflow_color.g(),
                    overflow_color.b(),
                    56,
                ),
            );
            painter.line_segment(
                [
                    egui::pos2(px(capacity), strip.top()),
                    egui::pos2(px(capacity), strip.bottom()),
                ],
                egui::Stroke::new(2.0_f32, overflow_color),
            );
        }

        let unused_fill = egui::Color32::from_white_alpha(22);
        for bank in &layout.banks {
            let x0 = px(bank.base);
            let x1 = px(bank.base + bank.span());
            let brect = egui::Rect::from_min_max(
                egui::pos2(x0, strip.top() + 3.0),
                egui::pos2(x1, strip.bottom() - 3.0),
            );
            let past_cap = bank.base + bank.span() > capacity;
            let used_color = if past_cap {
                overflow_color
            } else {
                theme::p().accent
            };
            let slot_w = brect.width() / bank.units as f32;
            if slot_w >= 3.0 {
                // Individual slot cells with a hairline gap.
                for unit in 0..bank.units {
                    let sx0 = brect.left() + unit as f32 * slot_w;
                    let cell = egui::Rect::from_min_max(
                        egui::pos2(sx0, brect.top()),
                        egui::pos2((sx0 + slot_w - 1.0).max(sx0 + 1.0), brect.bottom()),
                    );
                    let fill = if self.slot_occupied(bank, unit) {
                        used_color
                    } else {
                        unused_fill
                    };
                    painter.rect_filled(cell, 1.0, fill);
                }
            } else {
                // Slots are subpixel: show the used fraction as a fill level.
                let used = (0..bank.units)
                    .filter(|&u| self.slot_occupied(bank, u))
                    .count() as f32;
                painter.rect_filled(brect, 2.0, unused_fill);
                let used_w = brect.width() * used / bank.units as f32;
                if used_w >= 1.0 {
                    let urect = egui::Rect::from_min_size(
                        brect.left_top(),
                        egui::vec2(used_w, brect.height()),
                    );
                    painter.rect_filled(urect, 2.0, used_color);
                }
            }
            // Bank name centered under its region when it fits.
            if x1 - x0 >= 24.0 {
                painter.text(
                    egui::pos2((x0 + x1) * 0.5, area.bottom() - 1.0),
                    egui::Align2::CENTER_BOTTOM,
                    &bank.name,
                    egui::FontId::proportional(10.0),
                    theme::p().text_dim,
                );
            }
        }

        // The TOC region: a small orange tick (it is tiny at card scale).
        if let Some(t) = layout.toc_extent() {
            let x0 = px(t.offset);
            let x1 = px(t.end()).max(x0 + 2.0);
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, strip.top() + 3.0),
                    egui::pos2(x1, strip.bottom() - 3.0),
                ),
                1.0,
                theme::p().orange,
            );
        }

        // Live operation highlight: the slot currently being transferred
        // gets an op-colored progress fill and a pulsing outline. The busy
        // repaint loop (100 ms) animates the pulse.
        if let Some(op) = &self.op {
            if !op.fold.finished {
                if let Some((bank_name, unit, done, total)) = &op.fold.progress {
                    if let Some(bank) = layout.banks.iter().find(|b| &b.name == bank_name) {
                        if *unit < bank.units {
                            let s0 = bank.base + u64::from(*unit) * bank.slot_size;
                            let x0 = px(s0);
                            let x1 = px(s0 + bank.slot_size).max(x0 + 2.0);
                            let cell = egui::Rect::from_min_max(
                                egui::pos2(x0, strip.top() + 3.0),
                                egui::pos2(x1, strip.bottom() - 3.0),
                            );
                            let color = match op.fold.verb.as_str() {
                                "write" => theme::p().teal,
                                "verify" => theme::p().green,
                                "wipe" => theme::p().orange,
                                _ => theme::p().accent, // reading / status
                            };
                            let frac = if *total > 0 {
                                (*done as f32 / *total as f32).clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            // Blank the cell first: an occupied slot's base
                            // fill is ACCENT, which would render a same-color
                            // (read/status) progress fill invisible.
                            painter.rect_filled(
                                cell,
                                1.0,
                                egui::Color32::from_rgb(0x14, 0x16, 0x1c),
                            );
                            let fill_w = cell.width() * frac;
                            if fill_w >= 1.0 {
                                painter.rect_filled(
                                    egui::Rect::from_min_size(
                                        cell.min,
                                        egui::vec2(fill_w, cell.height()),
                                    ),
                                    1.0,
                                    color,
                                );
                            }
                            let pulse = ((ui.input(|i| i.time) * 4.0).sin() * 0.5 + 0.5) as f32;
                            let alpha = (110.0 + 145.0 * pulse) as u8;
                            painter.rect_stroke(
                                cell.expand(1.0),
                                2.0,
                                egui::Stroke::new(
                                    2.0_f32,
                                    egui::Color32::from_rgba_unmultiplied(
                                        color.r(),
                                        color.g(),
                                        color.b(),
                                        alpha,
                                    ),
                                ),
                                egui::StrokeKind::Outside,
                            );
                        }
                    }
                }
            }
        }

        // Hover: name the bank (or region) under the cursor.
        if let Some(pos) = response.hover_pos() {
            if strip.contains(pos) {
                let byte = ((pos.x - strip.left()) / strip.width() * scale_end as f32) as u64;
                let text = layout
                    .banks
                    .iter()
                    .find(|b| byte >= b.base && byte < b.base + b.span())
                    .map(|b| {
                        let occupied = (0..b.units).filter(|&u| self.slot_occupied(b, u)).count();
                        format!(
                            "bank {} — {}/{} slots occupied, {} each",
                            b.name,
                            occupied,
                            b.units,
                            format_bytes(b.slot_size)
                        )
                    })
                    .unwrap_or_else(|| {
                        if layout
                            .toc_extent()
                            .is_some_and(|t| byte >= t.offset && byte < t.end())
                        {
                            "on-card TOC".to_string()
                        } else {
                            "unallocated".to_string()
                        }
                    });
                response.clone().on_hover_text(text);
            }
        }

        // Caption: allocation vs the card, and the overflow warning.
        let occupied_bytes: u64 = layout
            .banks
            .iter()
            .map(|b| {
                (0..b.units).filter(|&u| self.slot_occupied(b, u)).count() as u64 * b.slot_size
            })
            .sum();
        let caption = if generic {
            format!(
                "no card selected — shown against a generic 8 GB card · layout {} · occupied slots {}",
                format_bytes(layout_end),
                format_bytes(occupied_bytes)
            )
        } else {
            format!(
                "layout {} of {} card · occupied slots {}",
                format_bytes(layout_end),
                format_bytes(capacity),
                format_bytes(occupied_bytes)
            )
        };
        ui.label(
            egui::RichText::new(caption)
                .color(theme::p().text_dim)
                .size(11.0),
        );
        if overflow {
            let msg = if generic {
                format!(
                    "layout needs {} — use {} card or larger",
                    format_bytes(layout_end),
                    suggest_card(layout_end)
                )
            } else {
                format!(
                    "⚠ layout overflows the card by {} — writes past the end will fail",
                    format_bytes(layout_end - capacity)
                )
            };
            ui.label(egui::RichText::new(msg).color(overflow_color).strong());
        }
    }

    /// Is this slot visible in the slot map under the current settings?
    /// Select-all controls only operate on visible slots.
    fn slot_visible(&self, bank: &sdslot_core::layout::Bank, unit: u32) -> bool {
        !self.settings.hide_empty_slots || !self.slot_is_empty(bank, unit)
    }

    /// Drop selected slots that are hidden as empty (invisible selections
    /// would make batch operations surprising).
    fn prune_hidden_selection(&mut self) {
        let Some(layout) = self.layout.clone() else {
            return;
        };
        if !self.settings.hide_empty_slots {
            return;
        }
        for bank in &layout.banks {
            for unit in 0..bank.units {
                if self.slot_is_empty(bank, unit) {
                    self.selected_slots.remove(&(bank.name.clone(), unit));
                }
            }
        }
    }

    /// An "empty" row: nothing known on card, no name, no manifest image.
    fn slot_is_empty(&self, bank: &sdslot_core::layout::Bank, unit: u32) -> bool {
        let view = self.slot_states.get(&(bank.name.clone(), unit));
        let manifest_slot = bank.slots.get(&unit);
        let has_name = view.is_some_and(|v| v.name.is_some())
            || manifest_slot.is_some_and(|s| s.name.is_some());
        let has_image = manifest_slot.is_some_and(|s| s.image.is_some());
        let state_empty = view.is_none_or(|v| v.state == ViewState::Core(SlotState::Unknown));
        !has_name && !has_image && state_empty
    }

    fn central_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(layout) = self.layout.clone() else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a layout manifest (TOML) to see the slot map.");
                });
                return;
            };
            let ready = self.ready_for_ops();
            let scanning = self
                .op
                .as_ref()
                .is_some_and(|o| !o.fold.finished && o.running.label == "status");
            let hide_empty = self.settings.hide_empty_slots;
            let hidden_total: usize = if hide_empty {
                layout
                    .banks
                    .iter()
                    .map(|b| (0..b.units).filter(|&u| self.slot_is_empty(b, u)).count())
                    .sum()
            } else {
                0
            };
            // The card map applies to the whole card: keep it fixed above
            // the scrolling bank tables.
            theme::card().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Card");
                    ui.label(
                        egui::RichText::new("used slots dark · free light · TOC orange")
                            .color(theme::p().text_dim)
                            .size(11.0),
                    );
                });
                ui.add_space(2.0);
                self.card_map(ui, &layout);
            });
            ui.add_space(8.0);
            if hidden_total > 0 {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.weak(format!(
                        "{hidden_total} empty slot(s) are hidden — un-hide in "
                    ));
                    if ui.link("Settings").on_hover_text("Open Settings").clicked() {
                        self.settings_open = true;
                    }
                    ui.weak(".");
                });
                ui.add_space(4.0);
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for bank in &layout.banks {
                    let type_note = bank
                        .drive_type
                        .as_ref()
                        .map(|t| format!("{} · ", t.name))
                        .unwrap_or_default();
                    theme::card().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.heading(format!("Bank {}", bank.name));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}{} slots × {}",
                                    type_note,
                                    bank.units,
                                    format_bytes(bank.slot_size)
                                ))
                                .color(theme::p().text_dim)
                                .size(12.0),
                            );
                        });
                        ui.add_space(4.0);
                        // Fixed column widths, identical in every bank, so the
                        // slot map reads like one spreadsheet.
                        ui.push_id(format!("bank_{}", bank.name), |ui| {
                            TableBuilder::new(ui)
                                .striped(true)
                                .vscroll(false)
                                .column(Column::exact(COL_SEL))
                                .column(Column::exact(COL_UNIT))
                                .column(Column::exact(COL_NAME))
                                .column(Column::exact(COL_STATUS))
                                .column(Column::exact(COL_SIZE))
                                .column(Column::remainder().at_least(COL_ACTIONS))
                                .header(20.0, |mut header| {
                                    header.col(|ui| {
                                        // Bank master checkbox over the
                                        // VISIBLE slots: hidden empty slots
                                        // are never selected.
                                        let visible: Vec<u32> = (0..bank.units)
                                            .filter(|&u| self.slot_visible(bank, u))
                                            .collect();
                                        let mut all = !visible.is_empty()
                                            && visible.iter().all(|&u| {
                                                self.selected_slots
                                                    .contains(&(bank.name.clone(), u))
                                            });
                                        if ui
                                            .add_enabled(
                                                !visible.is_empty(),
                                                egui::Checkbox::new(&mut all, ""),
                                            )
                                            .on_hover_text("Tick/untick this bank's visible slots")
                                            .changed()
                                        {
                                            for &u in &visible {
                                                let key = (bank.name.clone(), u);
                                                if all {
                                                    self.selected_slots.insert(key);
                                                } else {
                                                    self.selected_slots.remove(&key);
                                                }
                                            }
                                        }
                                    });
                                    for title in ["Unit", "Name", "Status", "Size", "Actions"] {
                                        header.col(|ui| {
                                            ui.strong(title);
                                        });
                                    }
                                })
                                .body(|mut body| {
                                    for unit in 0..bank.units {
                                        if hide_empty && self.slot_is_empty(bank, unit) {
                                            continue;
                                        }
                                        let key = (bank.name.clone(), unit);
                                        let view = self.slot_states.get(&key).cloned();
                                        let manifest_slot = bank.slots.get(&unit);
                                        let name = view
                                            .as_ref()
                                            .and_then(|v| v.name.clone())
                                            .or_else(|| manifest_slot.and_then(|s| s.name.clone()));
                                        body.row(24.0, |mut row| {
                                            row.col(|ui| {
                                                let key = (bank.name.clone(), unit);
                                                let mut sel = self.selected_slots.contains(&key);
                                                if ui.checkbox(&mut sel, "").changed() {
                                                    if sel {
                                                        self.selected_slots.insert(key);
                                                    } else {
                                                        self.selected_slots.remove(&key);
                                                    }
                                                }
                                            });
                                            row.col(|ui| {
                                                ui.monospace(format!("{unit:>3}"));
                                            });
                                            row.col(|ui| {
                                                match &name {
                                                    Some(n) => ui.label(n),
                                                    None => ui.weak("(empty)"),
                                                };
                                            });
                                            row.col(|ui| {
                                                match &view {
                                                    Some(v) => {
                                                        let (text, color) = state_label(v.state);
                                                        if !text.is_empty() {
                                                            theme::pill(ui, text, color);
                                                        }
                                                    }
                                                    None if scanning => {
                                                        // States were reset for
                                                        // the running scan; this
                                                        // row is not in yet.
                                                        theme::pill(ui, "…", theme::p().text_dim);
                                                    }
                                                    None => {
                                                        ui.label("");
                                                    }
                                                };
                                            });
                                            row.col(|ui| {
                                                match view.as_ref().and_then(|v| v.length) {
                                                    Some(l) => ui.monospace(format_bytes(l)),
                                                    None => ui.label(""),
                                                };
                                            });
                                            row.col(|ui| {
                                                ui.horizontal(|ui| {
                                                    self.slot_actions(
                                                        ui,
                                                        ready,
                                                        &bank.name,
                                                        unit,
                                                        manifest_slot.and_then(|s| s.image.clone()),
                                                    );
                                                });
                                            });
                                        });
                                    }
                                });
                        });
                    }); // card
                    ui.add_space(12.0);
                }
            });
        });
    }

    /// The Write/Extract/Wipe buttons for one slot row. Write is disabled
    /// when the manifest names an image that is missing on disk; a slot
    /// with no image still offers the file picker.
    fn slot_actions(
        &mut self,
        ui: &mut egui::Ui,
        ready: bool,
        bank: &str,
        unit: u32,
        manifest_image: Option<PathBuf>,
    ) {
        let missing_image = manifest_image
            .as_ref()
            .filter(|p| self.missing_images.contains(*p))
            .cloned();
        let response = ui.add_enabled(
            ready && missing_image.is_none(),
            egui::Button::new("Write…"),
        );
        let response = match &missing_image {
            Some(p) => response.on_disabled_hover_text(format!("image not found: {}", p.display())),
            None => response,
        };
        if response.clicked() {
            let image = manifest_image.or_else(|| {
                rfd::FileDialog::new()
                    .set_title("Choose disk image to write")
                    .pick_file()
            });
            if let Some(image) = image {
                self.modal = Modal::ConfirmWrite {
                    bank: bank.to_string(),
                    unit,
                    image,
                };
            }
        }
        if ui
            .add_enabled(ready, egui::Button::new("Extract…"))
            .clicked()
        {
            self.modal = Modal::Extract {
                bank: bank.to_string(),
                unit,
                length_choice: 0,
            };
        }
        if ui.add_enabled(ready, egui::Button::new("Wipe…")).clicked() {
            self.modal = Modal::ConfirmWipe {
                bank: bank.to_string(),
                unit,
            };
        }
    }

    fn bottom_panel(&mut self, ctx: &egui::Context) {
        let busy = self.busy();
        let mut cancel_clicked = false;
        let mut log_height_changed = false;
        // With the log hidden, give its space back to the slot map.
        let min_height = if self.settings.hide_log { 40.0 } else { 110.0 };
        egui::TopBottomPanel::bottom("bottom")
            .min_height(min_height)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                if let Some(op) = &self.op {
                    // With a scan-wide total (status), the bar advances over
                    // the whole scan instead of restarting per slot. With no
                    // per-slot progress: before any event the CLI is still
                    // behind the elevation prompt; after, it is between slots.
                    let pct = |frac: f32| (frac * 100.0).clamp(0.0, 100.0).round() as u32;
                    // " · 12s · 4.1 MiB/s" once the current pass's PhaseStart
                    // has arrived, else empty (nothing to time yet).
                    let rate_suffix = |bytes_so_far: u64| -> String {
                        op.fold
                            .phase_elapsed_and_rate(bytes_so_far)
                            .map(|(elapsed, rate)| {
                                format!(
                                    " · {} · {}/s",
                                    format_elapsed(elapsed),
                                    format_bytes(rate as u64)
                                )
                            })
                            .unwrap_or_default()
                    };
                    let bar = if let Some((bank, unit, done, total)) = &op.fold.progress {
                        Some(match op.fold.agg_total {
                            Some(agg_total) if agg_total > 0 => {
                                let cum = op.fold.agg_done + done;
                                let frac = cum as f32 / agg_total as f32;
                                (
                                    frac,
                                    format!(
                                        "{} {bank}:{unit} — {}% · {} / {} total{}",
                                        op.fold.verb,
                                        pct(frac),
                                        format_bytes(cum),
                                        format_bytes(agg_total),
                                        rate_suffix(cum),
                                    ),
                                )
                            }
                            _ => {
                                let frac = if *total > 0 {
                                    *done as f32 / *total as f32
                                } else {
                                    0.0
                                };
                                (
                                    frac,
                                    format!(
                                        "{} {bank}:{unit} — {}% · {} / {}{}",
                                        op.fold.verb,
                                        pct(frac),
                                        format_bytes(*done),
                                        format_bytes(*total),
                                        rate_suffix(*done),
                                    ),
                                )
                            }
                        })
                    } else if !op.fold.finished {
                        Some(match (op.fold.saw_event, op.fold.agg_total) {
                            (true, Some(agg_total)) if agg_total > 0 => {
                                let frac = op.fold.agg_done as f32 / agg_total as f32;
                                (
                                    frac,
                                    format!(
                                        "{} — {}% · {} / {} total{}",
                                        op.fold.verb,
                                        pct(frac),
                                        format_bytes(op.fold.agg_done),
                                        format_bytes(agg_total),
                                        rate_suffix(op.fold.agg_done),
                                    ),
                                )
                            }
                            (true, _) => (0.0, format!("{} — working…", op.running.label)),
                            (false, _) => (
                                0.0,
                                format!("{} — waiting for elevation approval…", op.running.label),
                            ),
                        })
                    } else {
                        None
                    };
                    if let Some((frac, text)) = bar {
                        // A plain bounded-height row: a right-to-left
                        // with_layout here claims the panel's whole remaining
                        // height, ballooning the panel and squeezing out the
                        // central slot map.
                        ui.horizontal(|ui| {
                            let bar_width = (ui.available_width() - 76.0).max(60.0);
                            ui.add_sized(
                                [bar_width, 18.0],
                                egui::ProgressBar::new(frac).text(text),
                            );
                            if ui
                                .add_enabled(busy, egui::Button::new("Cancel"))
                                .on_hover_text("Terminate the running operation")
                                .clicked()
                            {
                                cancel_clicked = true;
                            }
                        });
                    }
                    if self.settings.developer_mode {
                        ui.horizontal(|ui| {
                            ui.label("Equivalent:");
                            ui.monospace(&op.running.equivalent);
                        });
                    }
                }
                if !self.settings.hide_log {
                    // Drag handle: we adjust the log height ourselves — an
                    // egui-resizable panel re-expands to fill-height content
                    // every frame, so the height must not derive from the
                    // available space.
                    let (grip_rect, grip) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 8.0),
                        egui::Sense::drag(),
                    );
                    let grip = grip.on_hover_cursor(egui::CursorIcon::ResizeVertical);
                    if grip.dragged() {
                        self.settings.log_height =
                            (self.settings.log_height - grip.drag_delta().y).clamp(52.0, 500.0);
                    }
                    if grip.drag_stopped() {
                        log_height_changed = true;
                    }
                    let grip_color = if grip.hovered() || grip.dragged() {
                        theme::p().text_dim
                    } else {
                        egui::Color32::from_white_alpha(28)
                    };
                    ui.painter().rect_filled(
                        egui::Rect::from_center_size(grip_rect.center(), egui::vec2(36.0, 3.0)),
                        1.5,
                        grip_color,
                    );
                    // The log as an 80's phosphor CRT: VT323 in P1 green on a
                    // near-black tube of exactly the chosen height — stable
                    // whether it holds one line or a hundred.
                    let tube_h = self.settings.log_height.clamp(52.0, 500.0);
                    egui::Frame::new()
                        .fill(theme::PHOSPHOR_BG)
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.set_height(tube_h);
                            ui.spacing_mut().item_spacing.y = 1.0;
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    for line in &self.log {
                                        ui.label(
                                            egui::RichText::new(line)
                                                .font(theme::phosphor_font())
                                                .color(theme::PHOSPHOR_GREEN),
                                        );
                                    }
                                });
                        });
                }
                ui.add_space(4.0);
            });
        if cancel_clicked {
            self.cancel_current_op();
        }
        if log_height_changed {
            self.save_settings();
        }
    }

    fn cancel_current_op(&mut self) {
        let canceller = match &mut self.op {
            Some(op) if !op.fold.finished => {
                op.fold.request_cancel();
                op.running.canceller.clone()
            }
            _ => return,
        };
        match canceller.cancel() {
            Ok(()) => self.log("cancel requested — terminating the operation"),
            Err(e) => self.log(format!("cancel failed: {e}")),
        }
    }

    /// The Settings window: every toggle persists to ~/.sdslot immediately.
    /// The dangerous ones (Advanced, startup auto-select) route through
    /// their warning dialogs before taking effect.
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut show_all_changed = false;
        let mut verify_changed = false;
        let mut hide_empty_changed = false;
        let mut hide_log_changed = false;
        let mut developer_changed = false;
        let mut disable_advanced = false;
        let mut request_advanced = false;
        let mut disable_auto_select = false;
        let mut request_auto_select = false;
        let mut reset = false;

        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                show_all_changed = theme::setting_row(
                    ui,
                    "Show all devices",
                    "Also list devices without media (empty card readers)",
                    &mut self.settings.show_all,
                );

                let mut advanced = self.settings.advanced;
                if theme::setting_row(
                    ui,
                    "Advanced",
                    "Allow non-removable disks — an internal disk write can destroy this machine",
                    &mut advanced,
                ) {
                    if advanced {
                        request_advanced = true;
                    } else {
                        disable_advanced = true;
                    }
                }

                let mut auto = self.settings.select_first_removable;
                if theme::setting_row(
                    ui,
                    "Select first removable disk at startup",
                    "The first removable disk may not be the intended card",
                    &mut auto,
                ) {
                    if auto {
                        request_auto_select = true;
                    } else {
                        disable_auto_select = true;
                    }
                }

                verify_changed = theme::setting_row(
                    ui,
                    "Verify after write",
                    "Re-read and hash-compare every slot after writing it (recommended)",
                    &mut self.settings.verify,
                );

                hide_empty_changed = theme::setting_row(
                    ui,
                    "Hide empty slots",
                    "Hide slots with no content, name, or image",
                    &mut self.settings.hide_empty_slots,
                );

                hide_log_changed = theme::setting_row(
                    ui,
                    "Hide log",
                    "Hide the log pane at the bottom of the window",
                    &mut self.settings.hide_log,
                );

                developer_changed = theme::setting_row(
                    ui,
                    "Developer mode",
                    "Show the equivalent sdslot command line for every operation",
                    &mut self.settings.developer_mode,
                );

                ui.separator();
                if ui.button("Reset all settings").clicked() {
                    reset = true;
                }
                if let Some(p) = settings_path() {
                    ui.label(
                        egui::RichText::new(format!("Stored in {}", p.display()))
                            .color(theme::p().text_dim)
                            .size(11.0),
                    );
                }
            });

        self.settings_open = open;
        if show_all_changed {
            self.prune_selection();
            self.save_settings();
        }
        if verify_changed || hide_empty_changed || hide_log_changed || developer_changed {
            self.save_settings();
        }
        if hide_empty_changed && self.settings.hide_empty_slots {
            // Newly hidden empty slots must not stay invisibly selected.
            self.prune_hidden_selection();
        }
        if disable_advanced {
            self.settings.advanced = false;
            self.prune_selection();
            self.save_settings();
        }
        if request_advanced {
            self.modal = Modal::ConfirmAdvanced;
        }
        if disable_auto_select {
            self.settings.select_first_removable = false;
            self.save_settings();
        }
        if request_auto_select {
            self.modal = Modal::ConfirmAutoSelect;
        }
        if reset {
            self.settings = Settings::default();
            self.prune_selection();
            self.save_settings();
            self.log("settings reset to safe defaults");
        }
    }

    /// Dispatch to the showing modal's own handler; each handler is a
    /// self-contained function that reports what happened as a
    /// `ModalOutcome`, which `apply_modal_outcome` then applies uniformly.
    fn modal_windows(&mut self, ctx: &egui::Context) {
        let device = self.device_arg();
        let manifest = self.manifest_path.clone();
        let verify = self.settings.verify;
        let outcome = match self.modal.kind() {
            ModalKind::None => return,
            ModalKind::ConfirmAdvanced => self.modal_confirm_advanced(ctx),
            ModalKind::ConfirmAutoSelect => self.modal_confirm_auto_select(ctx),
            ModalKind::ConfirmWrite => self.modal_confirm_write(ctx, device, manifest, verify),
            ModalKind::ConfirmWriteSelected => {
                self.modal_confirm_write_selected(ctx, device, manifest, verify)
            }
            ModalKind::ConfirmExtractSelected => {
                self.modal_confirm_extract_selected(ctx, device, manifest)
            }
            ModalKind::ConfirmWipeSelected => {
                self.modal_confirm_wipe_selected(ctx, device, manifest)
            }
            ModalKind::ConfirmWipe => self.modal_confirm_wipe(ctx, device, manifest),
            ModalKind::Extract => self.modal_extract(ctx, device, manifest),
        };
        self.apply_modal_outcome(outcome);
    }

    fn apply_modal_outcome(&mut self, outcome: ModalOutcome) {
        match outcome {
            ModalOutcome::None => {}
            ModalOutcome::Close => self.modal = Modal::None,
            ModalOutcome::EnableAdvanced => {
                self.modal = Modal::None;
                self.settings.advanced = true;
                self.save_settings();
                self.log("advanced mode enabled: non-removable disks are selectable");
            }
            ModalOutcome::EnableAutoSelect => {
                self.modal = Modal::None;
                self.settings.select_first_removable = true;
                self.save_settings();
                self.log("first removable disk will be auto-selected at startup");
            }
            ModalOutcome::Run(cmd) => {
                self.modal = Modal::None;
                self.start_op(cmd);
            }
        }
    }

    fn modal_confirm_advanced(&mut self, ctx: &egui::Context) -> ModalOutcome {
        let mut outcome = ModalOutcome::None;
        egui::Window::new("Enable advanced mode?")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.colored_label(
                    theme::p().red,
                    "Advanced mode allows writing to NON-REMOVABLE disks.",
                );
                ui.label(
                    "Writing to an internal disk can destroy the operating system \
                     and all data on this machine. Only continue if you are certain \
                     the disk you intend to write is not in use by this system.",
                );
                ui.label("The system/boot disk itself remains blocked regardless.");
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("I understand the risk — enable").clicked() {
                        outcome = ModalOutcome::EnableAdvanced;
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }

    fn modal_confirm_auto_select(&mut self, ctx: &egui::Context) -> ModalOutcome {
        let mut outcome = ModalOutcome::None;
        egui::Window::new("Auto-select a disk at startup?")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.colored_label(
                    theme::p().red,
                    "The GUI would select the FIRST removable disk it finds, \
                     every time it starts.",
                );
                ui.label(
                    "The first removable disk may not be the card you intend to \
                     write — with several readers or sticks plugged in, the wrong \
                     device could be pre-selected. Always check the device selector \
                     before writing.",
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("I understand — enable").clicked() {
                        outcome = ModalOutcome::EnableAutoSelect;
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }

    fn modal_confirm_write(
        &mut self,
        ctx: &egui::Context,
        device: Option<String>,
        manifest: Option<PathBuf>,
        verify: bool,
    ) -> ModalOutcome {
        let (bank, unit, image) = match &self.modal {
            Modal::ConfirmWrite { bank, unit, image } => (bank.clone(), *unit, image.clone()),
            _ => return ModalOutcome::None,
        };
        let mut outcome = ModalOutcome::None;
        egui::Window::new("Confirm write")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Write to slot {bank}:{unit} on:"));
                ui.monospace(device.clone().unwrap_or_default());
                if let Some(l) = &self.layout {
                    if let Some((idx, b)) = l.banks.iter().enumerate().find(|(_, b)| b.name == bank)
                    {
                        let e = l.slot_extent(idx, unit);
                        ui.monospace(format!(
                            "bytes 0x{:010x}..0x{:010x} ({} slot)",
                            e.offset,
                            e.end(),
                            format_bytes(b.slot_size)
                        ));
                    }
                }
                ui.label("Image:");
                ui.monospace(image.display().to_string());
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Write").clicked() {
                        if let (Some(d), Some(m)) = (device.clone(), manifest.clone()) {
                            outcome = ModalOutcome::Run(backend::CliCommand::Write {
                                device: d,
                                manifest: m,
                                slots: vec![format!("{bank}:{unit}={}", image.display())],
                                verify,
                                yes: true,
                                force: self.target_needs_force(),
                            });
                        } else {
                            outcome = ModalOutcome::Close;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }

    fn modal_confirm_write_selected(
        &mut self,
        ctx: &egui::Context,
        device: Option<String>,
        manifest: Option<PathBuf>,
        verify: bool,
    ) -> ModalOutcome {
        let Modal::ConfirmWriteSelected { planned, no_image } = &self.modal else {
            return ModalOutcome::None;
        };
        let no_image = *no_image;
        let present: Vec<(String, u32)> = planned
            .iter()
            .filter(|p| !p.missing)
            .map(|p| (p.bank.clone(), p.unit))
            .collect();
        let planned_len = planned.len();
        let missing_count = planned_len - present.len();
        // Borrow `self.modal` ends here; everything below is owned data.
        let planned_lines: Vec<(String, bool)> = planned
            .iter()
            .map(|p| {
                let name = p
                    .image
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.image.display().to_string());
                (
                    format!(
                        "{}:{}  bytes 0x{:010x}..0x{:010x}  <- {name}",
                        p.bank,
                        p.unit,
                        p.offset,
                        p.offset + p.slot_len,
                    ),
                    p.missing,
                )
            })
            .collect();

        let mut outcome = ModalOutcome::None;
        egui::Window::new("Confirm write selected")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Write {planned_len} selected image(s) to:"));
                ui.monospace(device.clone().unwrap_or_default());
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for (line, missing) in &planned_lines {
                            if *missing {
                                ui.colored_label(
                                    theme::p().red,
                                    format!("{line}  (file not found — will be skipped)"),
                                );
                            } else {
                                ui.monospace(line);
                            }
                        }
                    });
                if no_image > 0 {
                    ui.label(
                        egui::RichText::new(format!(
                            "{no_image} selected slot(s) have no image and are ignored."
                        ))
                        .color(theme::p().text_dim),
                    );
                }
                if missing_count > 0 {
                    ui.colored_label(
                        theme::p().red,
                        format!(
                            "{missing_count} image file(s) are missing on this PC and \
                             will be SKIPPED; their slots are left untouched."
                        ),
                    );
                    if present.is_empty() {
                        ui.label("No image files are present; there is nothing to write.");
                    } else {
                        ui.label(format!(
                            "Continue and write only the {} present image(s)?",
                            present.len()
                        ));
                    }
                }
                ui.label(if verify {
                    "Only the listed slots are touched; each write is verified."
                } else {
                    "Only the listed slots are touched; verify after write is \
                     disabled in Settings."
                });
                ui.separator();
                ui.horizontal(|ui| {
                    let go_label = format!("Write {}", present.len());
                    if ui
                        .add_enabled(!present.is_empty(), egui::Button::new(go_label))
                        .clicked()
                    {
                        if let (Some(d), Some(m)) = (device.clone(), manifest.clone()) {
                            outcome = ModalOutcome::Run(backend::CliCommand::Write {
                                device: d,
                                manifest: m,
                                slots: present.iter().map(|(b, u)| format!("{b}:{u}")).collect(),
                                verify,
                                yes: true,
                                force: self.target_needs_force(),
                            });
                        } else {
                            outcome = ModalOutcome::Close;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }

    fn modal_confirm_extract_selected(
        &mut self,
        ctx: &egui::Context,
        device: Option<String>,
        manifest: Option<PathBuf>,
    ) -> ModalOutcome {
        let (slots, dir) = match &self.modal {
            Modal::ConfirmExtractSelected { slots, dir } => (slots.clone(), dir.clone()),
            _ => return ModalOutcome::None,
        };
        let layout = self.layout.clone();
        let mut outcome = ModalOutcome::None;
        egui::Window::new("Extract selected")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Extract {} slot(s) into:", slots.len()));
                ui.monospace(dir.display().to_string());
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for (bank, unit) in &slots {
                            let name = layout
                                .as_ref()
                                .and_then(|l| l.bank(bank))
                                .and_then(|b| b.slots.get(unit))
                                .and_then(|s| s.name.as_deref())
                                .map(str::to_string);
                            ui.monospace(format!(
                                "{bank}:{unit} → {}",
                                sdslot_core::engine::extract_filename(bank, *unit, name.as_deref())
                            ));
                        }
                    });
                ui.label(
                    egui::RichText::new("Existing files with these names are overwritten.")
                        .color(theme::p().text_dim),
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(format!("Extract {}", slots.len())).clicked() {
                        if let (Some(d), Some(m)) = (device.clone(), manifest.clone()) {
                            outcome = ModalOutcome::Run(backend::CliCommand::Read {
                                device: d,
                                manifest: m,
                                slots: slots.iter().map(|(b, u)| format!("{b}:{u}")).collect(),
                                out: None,
                                out_dir: Some(dir),
                                length: None,
                            });
                        } else {
                            outcome = ModalOutcome::Close;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }

    fn modal_confirm_wipe_selected(
        &mut self,
        ctx: &egui::Context,
        device: Option<String>,
        manifest: Option<PathBuf>,
    ) -> ModalOutcome {
        let Modal::ConfirmWipeSelected { slots } = &self.modal else {
            return ModalOutcome::None;
        };
        let slots = slots.clone();
        let layout = self.layout.clone();
        let mut outcome = ModalOutcome::None;
        egui::Window::new("Confirm wipe selected")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.colored_label(
                    theme::p().red,
                    format!(
                        "Zero the full extent of {} slot(s) (blank packs) on:",
                        slots.len()
                    ),
                );
                ui.monospace(device.clone().unwrap_or_default());
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for (bank, unit) in &slots {
                            if let Some(l) = &layout {
                                if let Some((idx, b)) =
                                    l.banks.iter().enumerate().find(|(_, b)| &b.name == bank)
                                {
                                    let e = l.slot_extent(idx, *unit);
                                    ui.monospace(format!(
                                        "{bank}:{unit}  bytes 0x{:010x}..0x{:010x} ({})",
                                        e.offset,
                                        e.end(),
                                        format_bytes(b.slot_size)
                                    ));
                                }
                            }
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(format!("Wipe {}", slots.len())).clicked() {
                        if let (Some(d), Some(m)) = (device.clone(), manifest.clone()) {
                            outcome = ModalOutcome::Run(backend::CliCommand::Wipe {
                                device: d,
                                manifest: m,
                                slots: slots.iter().map(|(b, u)| format!("{b}:{u}")).collect(),
                                yes: true,
                                force: self.target_needs_force(),
                            });
                        } else {
                            outcome = ModalOutcome::Close;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }

    fn modal_confirm_wipe(
        &mut self,
        ctx: &egui::Context,
        device: Option<String>,
        manifest: Option<PathBuf>,
    ) -> ModalOutcome {
        let (bank, unit) = match &self.modal {
            Modal::ConfirmWipe { bank, unit } => (bank.clone(), *unit),
            _ => return ModalOutcome::None,
        };
        let mut outcome = ModalOutcome::None;
        egui::Window::new("Confirm wipe")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Zero the full extent of slot {bank}:{unit} (blank pack) on:"
                ));
                ui.monospace(device.clone().unwrap_or_default());
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Wipe").clicked() {
                        if let (Some(d), Some(m)) = (device.clone(), manifest.clone()) {
                            outcome = ModalOutcome::Run(backend::CliCommand::Wipe {
                                device: d,
                                manifest: m,
                                slots: vec![format!("{bank}:{unit}")],
                                yes: true,
                                force: self.target_needs_force(),
                            });
                        } else {
                            outcome = ModalOutcome::Close;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }

    /// The only modal that mutates its own state live (`length_choice`) as
    /// the user interacts, so — unlike the others — it keeps a live
    /// `&mut self.modal` borrow for the whole render instead of cloning out
    /// up front.
    fn modal_extract(
        &mut self,
        ctx: &egui::Context,
        device: Option<String>,
        manifest: Option<PathBuf>,
    ) -> ModalOutcome {
        let Modal::Extract {
            bank,
            unit,
            length_choice,
        } = &mut self.modal
        else {
            return ModalOutcome::None;
        };
        let (bank, unit) = (bank.clone(), *unit);
        let mut outcome = ModalOutcome::None;
        egui::Window::new("Extract slot")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Extract slot {bank}:{unit} into an image file."));
                ui.horizontal(|ui| {
                    ui.label("Length:");
                    egui::ComboBox::from_id_salt("length")
                        .selected_text(LENGTH_CHOICES[*length_choice])
                        .show_ui(ui, |ui| {
                            for (i, c) in LENGTH_CHOICES.iter().enumerate() {
                                ui.selectable_value(length_choice, i, *c);
                            }
                        });
                });
                ui.label(
                    "canonical = drive type's canonical size · toc = byte length recorded \
                     at write time · slot = full slot extent",
                );
                ui.separator();
                let choice = *length_choice;
                ui.horizontal(|ui| {
                    if ui.button("Extract…").clicked() {
                        if let Some(out) = rfd::FileDialog::new()
                            .set_title("Save extracted image")
                            .set_file_name(format!("{bank}_{unit}.dsk"))
                            .save_file()
                        {
                            if let (Some(d), Some(m)) = (device.clone(), manifest.clone()) {
                                outcome = ModalOutcome::Run(backend::CliCommand::Read {
                                    device: d,
                                    manifest: m,
                                    slots: vec![format!("{bank}:{unit}")],
                                    out: Some(out),
                                    out_dir: None,
                                    length: Some(LENGTH_CHOICES[choice].to_string()),
                                });
                            } else {
                                outcome = ModalOutcome::Close;
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = ModalOutcome::Close;
                    }
                });
            });
        outcome
    }
}
