//! egui front end: printer bar, live preview, block editor, print panel.

use crate::data::{self, DataSource, Record, Table};
use crate::dev_ui::{DevUi, UiConfig};
use crate::history::{self, History};
use crate::job::{Pager, PrintJob, PrintSettings};
use crate::label::render::{PREVIEW_SCALE, Renderer, canvas_length_mm, preview_smooth, save_png};
use crate::icons::{self, Icon};
use crate::menu::MenuCmd;
use crate::label::{
    Align, BarKind, BarcodeBlock, Block, DataMatrixBlock, DmShape, Ec, ImageBlock, Label, QrBlock, SequenceBlock,
    TextBlock, Vars, expand, sequence_placeholder,
};
use crate::printer::{Event, PrinterHandle};
use crate::proto::media::{self, TZE_WIDTHS};
use crate::proto::{Caps, Status, Tape};
use crate::spell::{Miss, Speller, replace_chars};
use crate::transport::{Link, PrinterId, discovery};
use eframe::egui::{
    self, Color32, ComboBox, DragValue, FontId, Key, KeyboardShortcut, Modifiers, PopupCloseBehavior,
    RichText, TextEdit, TextureOptions, Vec2,
};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KEY: &str = "ptlabel";

/// Dock/window icon when running outside the .app bundle (the bundle uses PTLabel.icns / PTLabel-Dev.icns).
fn window_icon() -> egui::IconData {
    #[cfg(feature = "dev")]
    let bytes: &[u8] = include_bytes!("../assets/icon-dev.png");
    #[cfg(not(feature = "dev"))]
    let bytes: &[u8] = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(bytes).expect("icon.png").to_rgba8();
    egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() }
}

/// PTLABEL_PROFILE=x runs with its own saved state ("PTLabel-x"), e.g. for testing.
fn app_name() -> String {
    match std::env::var("PTLABEL_PROFILE") {
        Ok(p) if !p.is_empty() => format!("PTLabel-{p}"),
        _ if cfg!(feature = "dev") => "PTLabel Dev".to_string(),
        _ => "PTLabel".to_string(),
    }
}

pub fn run() -> anyhow::Result<()> {
    let name = app_name();
    let shot_mode = std::env::var_os("PTLABEL_SCREENSHOT").is_some();
    let inner_size = std::env::var("PTLABEL_WINDOW_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some([w.parse::<f32>().ok()?, h.parse::<f32>().ok()?])
        })
        .unwrap_or([1240.0, 800.0]);
    let mut vb = egui::ViewportBuilder::default()
        .with_title(&name)
        .with_inner_size(inner_size)
        .with_min_inner_size([960.0, 600.0])
        .with_icon(window_icon());
    if shot_mode {
        vb = vb.with_decorations(false);
    }
    let options = eframe::NativeOptions {
        viewport: vb,
        // Metal (wgpu) instead of OpenGL: no extra AppKit backing surfaces. Frame latency 1 =
        // two drawables instead of three, ~15 MB less at Retina size (egui still uses 2 while resizing).
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            surface: eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(&name, options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Persist {
    label: Label,
    settings: PrintSettings,
    printer: Option<PrinterId>,
    auto_connect: bool,
    hosts: Vec<String>,
    offline_tape: u8,
    doc_path: Option<String>,
    /// File › Open Recent, most recent first.
    recent: Vec<String>,
    /// Selected spreadsheet rows (indices into the loaded table).
    sel_rows: Vec<usize>,
    /// User-resizable height of the live label preview box (logical pixels).
    preview_height: f32,
}

impl Default for Persist {
    fn default() -> Self {
        Self {
            label: Label::default(),
            settings: PrintSettings::default(),
            printer: None,
            auto_connect: true,
            hosts: vec![],
            offline_tape: 12,
            doc_path: None,
            recent: vec![],
            sel_rows: vec![],
            preview_height: 220.0,
        }
    }
}

enum Conn {
    None,
    Connecting,
    Online(Status),
    Offline(String),
}

struct Running {
    id: u64,
    title: String,
    done: u32,
    total: Option<u32>,
}

/// Short name for a queued job (its first label): first text line, else sequence or code data.
fn job_title(label: &Label, v: &Vars) -> String {
    let cut = |s: &str, n: usize| {
        if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
    };
    let mut seq_idx = 0usize;
    for b in &label.blocks {
        match b {
            Block::Text(t) => {
                if let Some(l) = expand(&t.text, v).lines().map(str::trim).find(|l| !l.is_empty()) {
                    return cut(l, 26);
                }
            }
            Block::Sequence(q) => {
                let idx = seq_idx;
                seq_idx += 1;
                if label.sequence_in_text(idx) {
                    continue;
                }
                let text = v.seqs.get(idx).cloned().unwrap_or_else(|| q.format(v.index, v));
                return cut(&text, 26);
            }
            Block::Qr(q) if !q.data.is_empty() => return format!("QR {}", cut(&expand(&q.data, v), 22)),
            Block::DataMatrix(d) if !d.data.is_empty() => return format!("DM {}", cut(&expand(&d.data, v), 22)),
            Block::Barcode(b) if !b.data.is_empty() => return format!("Barcode {}", cut(&expand(&b.data, v), 18)),
            _ => {}
        }
    }
    "Label".into()
}

fn count_text(total: Option<u32>) -> String {
    match total {
        Some(1) => "1 label".into(),
        Some(n) => format!("{n} labels"),
        None => "∞".into(),
    }
}

struct Preview {
    key: u64,
    tex: egui::TextureHandle,
    /// Size in 180-dpi dots (physical), independent of the texture resolution.
    phys: [f32; 2],
    len_mm: f32,
    overflow: bool,
    warnings: Vec<String>,
}

/// Render the preview texture. Runs on a worker thread: at 720 dpi it takes ~10-20 ms for a
/// typical label and several times that for long ones, which used to stall every keystroke.
#[allow(clippy::too_many_arguments)]
fn render_preview(
    ctx: &egui::Context,
    renderer: &Mutex<Renderer>,
    key: u64,
    label: &Label,
    tape: &Tape,
    vars: &Vars,
    opts: &crate::proto::PageOpts,
    max_side: usize,
) -> Preview {
    let mut r = renderer.lock().unwrap_or_else(|e| e.into_inner());
    // Smooth high-res render; drop the scale for very long labels so the texture fits.
    let mut s = PREVIEW_SCALE;
    let out = loop {
        let o = r.render_canvas(label, tape, vars, opts, s);
        let w = o.canvas.w + 2 * crate::proto::raster::margin_dots(opts.margin_mm, false) as usize * s;
        if w <= max_side || s == 1 {
            break o;
        }
        s /= 2;
    };
    drop(r);
    let (full_w, hh, rgba) = preview_smooth(&out.canvas, s, tape, opts);
    // Still wider than a GPU texture at 1× (wgpu panics, egui debug-asserts): show the start.
    let (w, rgba) = crop_width(rgba, full_w, hh, max_side);
    let mut warnings = out.warnings;
    if w < full_w {
        warnings.push(format!("Preview shows the first {:.0} mm only", w as f32 / s as f32 * 25.4 / 180.0));
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([w, hh], &rgba);
    let len_mm = canvas_length_mm(&out.canvas, s, opts);
    let phys = [w as f32 / s as f32, hh as f32 / s as f32];
    let filter = TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
    let tex = ctx.load_texture("preview", img, filter);
    Preview { key, tex, phys, len_mm, overflow: out.overflow, warnings }
}

pub struct App {
    p: Persist,
    renderer: Arc<Mutex<Renderer>>,
    fonts: Vec<String>,
    font_filter: String,
    discovered: Arc<Mutex<Vec<PrinterId>>>,
    handle: Option<PrinterHandle>,
    conn: Conn,
    live_tape: Option<Tape>,
    job: Option<Running>,
    stopping: bool,
    /// True when the last completed job was printed in chain mode and still sits inside the printer uncut.
    chain_pending: bool,
    /// True while a manual feed-and-cut command is running.
    cutting: bool,
    msg: Option<(String, bool, Instant)>,
    preview: Option<Preview>,
    /// Preview render running on a worker thread.
    preview_job: Option<crossbeam_channel::Receiver<Preview>>,
    new_host: String,
    /// Scratch buffer & open-state tracker for the Fixed-length typing + presets dropdown.
    length_input: String,
    length_popup_was_open: bool,
    notify: Arc<dyn Fn() + Send + Sync>,
    speller: Speller,
    menu_rx: Option<crossbeam_channel::Receiver<MenuCmd>>,
    /// Undo/redo of `p.label`.
    history: History<Label>,
    /// Focused widget last frame: focus moving ends an undo step.
    last_focus: Option<egui::Id>,
    /// Key events for egui's next frame (Edit › Undo while a text field has focus).
    forward: Vec<egui::Event>,
    /// Window title last set.
    title: String,
    /// Right-clicked misspelling: (block index, word, suggestions).
    spell_target: Option<(usize, Miss, Vec<String>)>,
    /// Loaded spreadsheet for `p.label.data`.
    table: Option<Table>,
    table_err: Option<String>,
    table_filter: String,
    /// Staged field names for multi-field insertion (`{col1} {col2}…`).
    picked_fields: Vec<String>,
    /// True while `p.sel_rows` was auto-populated from column-header selection (cleared on manual row edits).
    sel_rows_auto: bool,
    /// Last text block focused/edited (target for "Add to label").
    active_text_block: Option<usize>,
    /// Active drag-paint selection state in the spreadsheet table (`Some(true)` = selecting).
    table_drag: Option<bool>,
    /// Row shown in the preview (last clicked).
    preview_row: Option<usize>,
    /// Last clicked sheet row (index into `table.rows`), for ⇧-click ranges and drag painting.
    last_click: Option<usize>,
    data_checked: Instant,
    /// Minimized state for the spreadsheet box and each block box.
    data_collapsed: bool,
    collapsed: Vec<bool>,
    /// Native file dialogs run off the UI frame (avoids macOS winit re-entrancy crashes).
    dialog_rx: crossbeam_channel::Receiver<DialogRes>,
    dialog_tx: crossbeam_channel::Sender<DialogRes>,
    dialog_busy: Arc<std::sync::atomic::AtomicBool>,
    /// The label as last opened/saved (or new); differs from `p.label` = unsaved changes.
    saved: Label,
    /// A newer release on GitHub ("Update to …" in the status bar), its modal, and install state.
    update: Option<crate::update::Release>,
    update_modal: bool,
    updating: bool,
    update_error: Option<String>,
    /// The last look for a newer release; update checks and installs answer on `upd`.
    checked: Option<Instant>,
    upd: (crossbeam_channel::Sender<Upd>, crossbeam_channel::Receiver<Upd>),
    /// Live UI config (`dev-ui.json` watcher + Dev Inspector in `--features dev` builds).
    dev: DevUi,
    /// Optional automated screenshot capture path (`PTLABEL_SCREENSHOT` env var, used by CI/scripts).
    screenshot_path: Option<String>,
    screenshot_frames: u8,
    screenshot_taken: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InsertMode {
    Append,
    NewBlock,
}

enum TableAction {
    None,
    ToggleHeader(String),
    AddToLabel,
    UseHeaderRow(usize),
}

enum DialogRes {
    PickData { path: std::path::PathBuf, add_field: bool },
    OpenLabel(std::path::PathBuf),
    SaveLabel(std::path::PathBuf),
    ExportPng(std::path::PathBuf),
    PickImage { block: usize, path: std::path::PathBuf },
}

enum Upd {
    Found(crate::update::Release),
    /// The new app is in place (its path), or why not.
    Installed(Result<std::path::PathBuf, String>),
}

/// How often a running app looks for a new release.
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let p: Persist = cc.storage.and_then(|s| eframe::get_value(s, KEY)).unwrap_or_default();
        let ctx = cc.egui_ctx.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let renderer = Renderer::new();
        let fonts = renderer.families();
        if let Some((bytes, index)) = renderer.bold_ui_font() {
            let mut fd = egui::FontData::from_owned(bytes);
            fd.index = index;
            cc.egui_ctx.add_font(egui::epaint::text::FontInsert::new(
                "ui_bold",
                fd,
                vec![egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Name("bold".into()),
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
        }
        let discovered = Arc::new(Mutex::new(vec![]));
        {
            let (d, n) = (discovered.clone(), notify.clone());
            discovery::spawn(move |v| {
                *d.lock().unwrap() = v;
                n();
            });
        }
        cc.egui_ctx.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(10.0, 6.0);
            s.spacing.interact_size.y = 28.0;
            let mut scroll = egui::style::ScrollStyle::solid();
            scroll.bar_width = 8.0;
            scroll.bar_inner_margin = 6.0;
            scroll.bar_outer_margin = 2.0;
            scroll.handle_min_length = 28.0;
            s.spacing.scroll = scroll;
        });
        let history = History::new(p.label.clone());
        let mut app = Self {
            p,
            renderer: Arc::new(Mutex::new(renderer)),
            fonts,
            font_filter: String::new(),
            discovered,
            handle: None,
            conn: Conn::None,
            live_tape: None,
            job: None,
            stopping: false,
            chain_pending: false,
            cutting: false,
            msg: None,
            preview: None,
            preview_job: None,
            new_host: String::new(),
            length_input: String::new(),
            length_popup_was_open: false,
            notify,
            speller: Speller::new(),
            menu_rx: crate::menu::install(&cc.egui_ctx),
            history,
            last_focus: None,
            forward: vec![],
            title: String::new(),
            spell_target: None,
            table: None,
            table_err: None,
            table_filter: String::new(),
            picked_fields: vec![],
            sel_rows_auto: true,
            active_text_block: None,
            table_drag: None,
            preview_row: None,
            last_click: None,
            data_checked: Instant::now(),
            data_collapsed: false,
            collapsed: vec![],
            dialog_rx: {
                // replaced right below with matched pair
                let (_, rx) = crossbeam_channel::unbounded();
                rx
            },
            dialog_tx: {
                let (tx, _) = crossbeam_channel::unbounded();
                tx
            },
            dialog_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saved: Label::default(),
            update: None,
            update_modal: false,
            updating: false,
            update_error: None,
            checked: None,
            upd: crossbeam_channel::unbounded(),
            dev: DevUi::new(),
            screenshot_path: std::env::var("PTLABEL_SCREENSHOT").ok().filter(|s| !s.is_empty()),
            screenshot_frames: 0,
            screenshot_taken: false,
        };
        let (dialog_tx, dialog_rx) = crossbeam_channel::unbounded();
        app.dialog_tx = dialog_tx;
        app.dialog_rx = dialog_rx;
        // Restored label vs. its file on disk (missing/unreadable file = unsaved).
        if let Some(l) = app.p.doc_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()) {
            app.saved = l;
        }
        app.reload_table();
        app.refresh_recent();
        if let Some(id) = app.p.printer.clone() {
            app.connect(id);
        }
        app
    }

    // ---------------------------------------------------------------- printer

    fn connect(&mut self, id: PrinterId) {
        if self.handle.as_ref().is_some_and(|h| h.id.link == id.link) {
            return; // already connected: keep the queue
        }
        self.warn_dropped_jobs();
        self.handle = Some(PrinterHandle::spawn(id.clone(), self.renderer.clone(), self.notify.clone()));
        self.conn = Conn::Connecting;
        self.live_tape = None;
        self.job = None;
        self.chain_pending = false;
        self.cutting = false;
        self.p.printer = Some(id);
        self.p.auto_connect = true;
    }

    fn warn_dropped_jobs(&mut self) {
        let n = self.handle.as_ref().map_or(0, |h| h.queued().len());
        if let Some(h) = &self.handle {
            // The old worker only exits after its current job (an ∞ one never ends) and may still
            // pop a queued job if it is mid-poll: empty the queue and stop the job, which the UI
            // can't reach any more once the handle is gone.
            h.clear();
            if let Some(r) = &self.job {
                h.stop();
                let t = r.title.clone();
                self.info(format!("Stopping {t} after the current label (printer changed)"));
            }
        }
        if n > 0 {
            self.error(format!("{n} queued job{} discarded (printer changed)", if n == 1 { "" } else { "s" }));
        }
    }

    fn disconnect(&mut self) {
        self.warn_dropped_jobs();
        self.handle = None;
        self.conn = Conn::None;
        self.live_tape = None;
        self.job = None;
        self.chain_pending = false;
        self.cutting = false;
        self.p.printer = None;
        self.p.auto_connect = false;
    }

    fn printer_list(&self) -> Vec<PrinterId> {
        let mut v = self.discovered.lock().unwrap().clone();
        for h in &self.p.hosts {
            let id = PrinterId { model: "Wi-Fi P-touch".into(), link: Link::Network(h.clone()) };
            if !v.iter().any(|p| p.link == id.link) {
                v.push(id);
            }
        }
        if let Some(cur) = &self.p.printer {
            if !v.iter().any(|p| p.link == cur.link) {
                v.push(cur.clone());
            }
        }
        v
    }

    fn status(&self) -> Option<&Status> {
        match &self.conn {
            Conn::Online(s) => Some(s),
            _ => None,
        }
    }

    /// Capabilities of the connected model (status model code wins over the name).
    fn caps(&self) -> Caps {
        let Some(h) = &self.handle else { return Caps { half_cut: true, cut_every: true } };
        match self.status().and_then(|s| s.model_name()) {
            Some(m) => PrinterId { model: m.into(), link: h.id.link.clone() }.caps(),
            None => h.id.caps(),
        }
    }

    fn model(&self) -> String {
        self.status()
            .and_then(|s| s.model_name())
            .map(String::from)
            .or_else(|| self.handle.as_ref().map(|h| h.id.model.clone()))
            .unwrap_or_default()
    }

    fn tape(&self) -> Tape {
        self.live_tape
            .or_else(|| Tape::tze(self.p.offline_tape))
            .unwrap_or_else(|| Tape::tze(12).unwrap())
    }

    fn pump(&mut self) {
        let Some(h) = &self.handle else { return };
        let events: Vec<Event> = h.rx.try_iter().collect();
        for ev in events {
            match ev {
                Event::Status(s) => {
                    let t = s.tape();
                    if self.live_tape.is_some() && t.is_none() {
                        self.chain_pending = false;
                        self.info("Tape removed".into());
                    }
                    self.live_tape = t;
                    if let Some(m) = s.model_name() {
                        if let Some(p) = &mut self.p.printer {
                            if p.model != m {
                                p.model = m.into();
                            }
                        }
                        // caps() falls back to the handle's name while offline ("Wi-Fi P-touch" for an IP).
                        if let Some(h) = &mut self.handle {
                            if h.id.model != m {
                                h.id.model = m.into();
                            }
                        }
                    }
                    self.conn = Conn::Online(s);
                }
                Event::Offline(e) => {
                    self.conn = Conn::Offline(e);
                }
                Event::Started { id, title, total } => {
                    self.job = Some(Running { id, title, done: 0, total });
                    self.stopping = false;
                }
                Event::Progress { id, done, total } => {
                    if let Some(r) = self.job.as_mut().filter(|r| r.id == id) {
                        r.done = done;
                        r.total = total;
                    }
                }
                Event::Done { id, printed, chained } => {
                    let title = self.job.take().filter(|r| r.id == id).map(|r| r.title).unwrap_or_default();
                    self.stopping = false;
                    self.chain_pending = chained;
                    self.info(format!("Printed {} · {title}", count_text(Some(printed))));
                }
                Event::Failed { id, error } => {
                    let title = self.job.take().filter(|r| r.id == id).map(|r| r.title).unwrap_or_default();
                    self.stopping = false;
                    self.error(format!("Print failed ({title}): {error}"));
                }
                Event::CutDone => {
                    self.chain_pending = false;
                    self.cutting = false;
                    self.info("Fed & cut last chained label".into());
                }
                Event::CutFailed(error) => {
                    self.cutting = false;
                    self.error(format!("Feed & cut failed: {error}"));
                }
            }
        }
    }

    // ---------------------------------------------------------------- updates

    /// Look for a newer release now and then (only an app bundle can update itself; disabled in dev).
    fn check_update(&mut self, ctx: &egui::Context) {
        if cfg!(feature = "dev") || self.checked.is_some_and(|t| t.elapsed() < UPDATE_EVERY) || crate::update::bundle().is_none() {
            return;
        }
        self.checked = Some(Instant::now());
        // Idle frames don't come by themselves: wake up for the next check.
        ctx.request_repaint_after(UPDATE_EVERY);
        let (tx, ctx) = (self.upd.0.clone(), ctx.clone());
        std::thread::spawn(move || {
            match crate::update::latest() {
                Ok(Some(r)) => {
                    let _ = tx.send(Upd::Found(r));
                    ctx.request_repaint();
                }
                Ok(None) => {}
                Err(e) => eprintln!("ptlabel: update check: {e:#}"),
            }
        });
    }

    fn start_update(&mut self, ctx: &egui::Context) {
        let (Some(r), Some(app)) = (self.update.clone(), crate::update::bundle()) else { return };
        self.updating = true;
        let (tx, ctx) = (self.upd.0.clone(), ctx.clone());
        std::thread::spawn(move || {
            let res = crate::update::install(&r, &app).map(|()| app).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Upd::Installed(res));
            ctx.request_repaint();
        });
    }

    fn updates(&mut self, ctx: &egui::Context) {
        let msgs: Vec<_> = self.upd.1.try_iter().collect();
        for m in msgs {
            match m {
                Upd::Found(r) => {
                    if self.update.as_ref().is_none_or(|u| u.version != r.version) {
                        self.update_modal = true;
                        self.update_error = None;
                    }
                    self.update = Some(r);
                }
                // In place: open the new one and quit this one (the label is kept in the saved state).
                Upd::Installed(Ok(app)) => {
                    crate::update::relaunch(&app);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Upd::Installed(Err(e)) => {
                    self.updating = false;
                    let msg = format!("Update failed: {e}");
                    self.update_error = Some(msg.clone());
                    self.error(msg);
                }
            }
        }
    }

    /// A label printing or queued: quitting now would drop it.
    fn printing(&self) -> bool {
        self.job.is_some() || self.handle.as_ref().is_some_and(|h| !h.queued().is_empty())
    }

    fn update_dialog(&mut self, ctx: &egui::Context) {
        let Some(r) = self.update.clone() else {
            self.update_modal = false;
            return;
        };
        let is_dev_preview = cfg!(feature = "dev") && self.dev.cfg.preview_update_modal;
        let accent = self.dev.cfg.accent();
        let printing = self.printing();
        let (mut close, mut install) = (false, false);
        let style = ctx.global_style();
        let frame = egui::Frame::new()
            .fill(style.visuals.panel_fill)
            .stroke(style.visuals.window_stroke)
            .corner_radius(10)
            .inner_margin(egui::Margin::same(24));
        let resp = egui::Modal::new(egui::Id::unique("update")).frame(frame).show(ctx, |ui| {
            let body_sz = self.dev.cfg.changelog_font_size;
            ui.set_width(self.dev.cfg.update_modal_width);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Update available").strong().size(22.0));
                if is_dev_preview {
                    egui::Frame::new()
                        .fill(Color32::from_rgb(248, 204, 56))
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::symmetric(6, 2))
                        .show(ui, |ui| {
                            ui.label(RichText::new("DEV PREVIEW").strong().size(11.0).color(Color32::from_rgb(28, 37, 51)));
                        });
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("PTLabel {} is ready to install.", r.version)).size(15.0));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Installed").size(13.5).weak());
                ui.label(RichText::new(env!("CARGO_PKG_VERSION")).size(13.5).monospace().weak());
                ui.label(RichText::new("→").size(13.5).weak());
                ui.label(RichText::new("New").size(13.5).weak());
                ui.label(RichText::new(&r.version).size(13.5).monospace().strong().color(accent));
            });

            if !r.notes.is_empty() {
                ui.add_space(14.0);
                ui.label(RichText::new("What's new").strong().size((body_sz + 1.0).max(15.0)));
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt("update_changelog")
                            .max_height(320.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                changelog_ui(ui, &r.notes, accent, body_sz);
                            });
                    });
            }

            if self.updating {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Downloading and installing…").size(14.0).weak());
                });
            } else if printing {
                ui.add_space(14.0);
                ui.label(RichText::new("Printing: update once the queue is done.").size(13.5).color(Color32::from_rgb(230, 150, 20)));
            }
            if let Some(e) = &self.update_error {
                ui.add_space(8.0);
                ui.label(RichText::new(e).size(13.5).color(Color32::from_rgb(220, 60, 50)));
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if !r.html_url.is_empty() {
                    ui.hyperlink_to(RichText::new("Release notes on GitHub ↗").size(13.5), &r.html_url);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if self.updating { "Updating…" } else { "Update now" };
                    let primary = egui::Button::new(RichText::new(label).size(14.5).color(Color32::WHITE).strong()).fill(accent);
                    if ui.add_enabled(!self.updating && !printing, primary).clicked() {
                        install = true;
                    }
                    if ui.add_enabled(!self.updating, egui::Button::new(RichText::new("Later").size(14.5))).clicked() {
                        close = true;
                    }
                });
            });
        });
        if install {
            if is_dev_preview {
                self.info("Dev preview: update install skipped".into());
                self.update_modal = false;
                #[cfg(feature = "dev")]
                self.dev.set_update_preview(false);
            } else {
                self.update_error = None;
                self.start_update(ctx);
            }
        } else if !self.updating && (close || resp.should_close()) {
            self.update_modal = false;
            #[cfg(feature = "dev")]
            self.dev.set_update_preview(false);
        }
    }

    fn info(&mut self, s: String) {
        self.msg = Some((s, false, Instant::now()));
    }
    fn error(&mut self, s: String) {
        self.msg = Some((s, true, Instant::now()));
    }

    /// Why queued jobs can't start right now (they wait and start automatically).
    fn waiting_reason(&self) -> Option<String> {
        match &self.conn {
            Conn::Online(s) => {
                let e = s.errors();
                if !e.is_empty() {
                    Some(e.join(", "))
                } else if s.tape().is_none() {
                    Some("No tape loaded".into())
                } else {
                    None
                }
            }
            Conn::Connecting => Some("Connecting…".into()),
            Conn::Offline(_) => Some("Printer not reachable".into()),
            Conn::None => Some("No printer selected".into()),
        }
    }

    /// Snapshot the current label + settings and add it to the printer's queue.
    fn print(&mut self) {
        let rows = if let Some(src) = &self.p.label.data {
            let rows = self.selected_records();
            if rows.is_empty() {
                let why = if self.table.is_some() {
                    if src.by_columns { "Select the spreadsheet columns to print" } else { "Select the spreadsheet rows to print" }
                } else {
                    "Spreadsheet not loaded"
                };
                self.error(why.into());
                return;
            }
            rows
        } else {
            vec![]
        };
        let job = PrintJob { label: self.p.label.clone(), settings: self.p.settings.clone(), caps: self.caps(), rows };
        let pager = Pager::new(&job.label, &job.settings, job.rows.clone());
        let total = pager.total;
        let title = job_title(&job.label, &pager.vars(0));
        let waiting = self.waiting_reason();
        let busy = self.job.is_some();
        let Some(h) = &mut self.handle else {
            self.error("Select a printer first".into());
            return;
        };
        let ahead = h.queued().len() + busy as usize;
        h.enqueue(job, title.clone(), total);
        match (ahead, waiting) {
            (0, None) => self.msg = None,
            (0, Some(w)) => self.info(format!("Queued {title}: waiting ({w})")),
            (n, _) => self.info(format!("Queued {title} ({n} ahead)")),
        }
    }

    // ---------------------------------------------------------------- dialogs & drag-drop

    /// Run a native file dialog off the UI frame. On macOS, calling `rfd::FileDialog`
    /// synchronously inside `App::ui` runs a nested `NSApplication` modal loop inside winit's
    /// event handler, which panics if a drag or repaint event re-enters winit.
    fn spawn_dialog(&self, ctx: &egui::Context, f: impl FnOnce() -> Option<DialogRes> + Send + 'static) {
        use std::sync::atomic::Ordering;
        if self.dialog_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let tx = self.dialog_tx.clone();
        let busy = self.dialog_busy.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = f();
            busy.store(false, Ordering::Release);
            if let Some(r) = res {
                let _ = tx.send(r);
                ctx.request_repaint();
            }
        });
    }

    fn pump_dialogs(&mut self) {
        let msgs: Vec<DialogRes> = self.dialog_rx.try_iter().collect();
        for res in msgs {
            match res {
                DialogRes::PickData { path, add_field } => self.attach_data_path(path, add_field),
                DialogRes::OpenLabel(path) => self.open_path(path),
                DialogRes::SaveLabel(path) => self.save_to(path),
                DialogRes::ExportPng(path) => self.export_png_to(path),
                DialogRes::PickImage { block, path } => {
                    if let Some(Block::Image(im)) = self.p.label.blocks.get_mut(block) {
                        im.path = path.display().to_string();
                    }
                }
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect()
        });
        for path in dropped {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if data::EXTENSIONS.contains(&ext.as_str()) {
                self.attach_data_path(path, false);
                break;
            } else if matches!(ext.as_str(), "ptlabel" | "json") {
                self.open_path(path);
                break;
            } else if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
                self.p.label.blocks.push(Block::Image(ImageBlock { path: path.display().to_string(), ..Default::default() }));
                self.collapsed.push(false);
                break;
            }
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop_overlay")));
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(150));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop spreadsheet (.xlsx / .csv), .ptlabel, or image to open",
                FontId::proportional(20.0),
                Color32::WHITE,
            );
        }
    }

    // ---------------------------------------------------------------- spreadsheet

    fn reload_table(&mut self) {
        let Some(src) = &self.p.label.data else {
            self.table = None;
            self.table_err = None;
            self.picked_fields.clear();
            return;
        };
        match data::load(src) {
            Ok(t) => {
                let n = t.rows.len();
                if let Some(old) = &self.table {
                    self.p.sel_rows = remap_rows(&old.rows, &t.rows, &self.p.sel_rows);
                    self.preview_row = self.preview_row.and_then(|r| remap_rows(&old.rows, &t.rows, &[r]).first().copied());
                }
                self.p.sel_rows.retain(|&r| r < n);
                if self.preview_row.is_some_and(|r| r >= n) {
                    self.preview_row = None;
                }
                self.picked_fields.retain(|f| t.headers.iter().any(|h| h == f));
                self.table = Some(t);
                self.table_err = None;
            }
            Err(e) => {
                self.table = None;
                self.table_err = Some(format!("Can't read {}: {e:#}", src.file_name()));
            }
        }
    }

    fn attach_data_path(&mut self, path: std::path::PathBuf, add_field: bool) {
        self.p.label.data = Some(DataSource { path: path.display().to_string(), ..Default::default() });
        self.p.sel_rows.clear();
        self.preview_row = None;
        self.last_click = None;
        self.table_drag = None;
        self.table_filter.clear();
        self.picked_fields.clear();
        self.sel_rows_auto = true;
        self.data_collapsed = false;
        self.reload_table();
        let summary = self
            .table
            .as_ref()
            .map(|t| (t.rows.len(), t.headers.len(), t.headers.first().cloned()));
        if let Some((nrows, ncols, first_col)) = summary {
            self.info(format!(
                "Loaded {} ({nrows} rows, {ncols} fields)",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            // Only auto-add a field if the user explicitly clicked Excel when there are no text blocks yet.
            if add_field && self.p.label.blocks.is_empty() && let Some(c) = first_col {
                self.p.label.blocks.push(Block::Text(TextBlock { text: format!("{{{c}}}"), ..Default::default() }));
                self.collapsed.push(false);
            }
        }
    }

    fn pick_data_file(&mut self, ctx: &egui::Context, add_field: bool) {
        self.spawn_dialog(ctx, move || {
            rfd::FileDialog::new()
                .add_filter("Spreadsheet", data::EXTENSIONS)
                .pick_file()
                .map(|path| DialogRes::PickData { path, add_field })
        });
    }

    /// "Excel" in the Add row: attach a spreadsheet (first time), or add the picked/next column(s).
    fn add_excel(&mut self, ctx: &egui::Context) {
        if self.p.label.data.is_none() {
            self.pick_data_file(ctx, true);
            return;
        }
        if !self.picked_fields.is_empty() {
            self.insert_picked_fields(InsertMode::NewBlock);
            return;
        }
        let Some(t) = &self.table else { return };
        let col = t.headers.iter().find(|h| !self.p.label.uses(h)).or(t.headers.first()).cloned();
        if let Some(c) = col {
            self.p.label.blocks.push(Block::Text(TextBlock { text: format!("{{{c}}}"), ..Default::default() }));
            self.collapsed.push(false);
        }
    }

    fn insert_picked_fields(&mut self, mode: InsertMode) {
        if self.picked_fields.is_empty() {
            return;
        }
        let snippet: String = self
            .picked_fields
            .iter()
            .map(|h| format!("{{{h}}}"))
            .collect::<Vec<_>>()
            .join(" ");
        let target_idx = self
            .active_text_block
            .filter(|&i| matches!(self.p.label.blocks.get(i), Some(Block::Text(_))))
            .or_else(|| self.p.label.blocks.iter().position(|b| matches!(b, Block::Text(_))));
        match mode {
            InsertMode::NewBlock => {
                self.p.label.blocks.push(Block::Text(TextBlock { text: snippet, ..Default::default() }));
                self.collapsed.push(false);
                self.active_text_block = Some(self.p.label.blocks.len() - 1);
            }
            InsertMode::Append => {
                if let Some(i) = target_idx {
                    if let Block::Text(t) = &mut self.p.label.blocks[i] {
                        if t.text.trim().is_empty() || t.text.trim() == "Label" {
                            t.text = snippet;
                        } else {
                            if !t.text.ends_with([' ', '\n']) {
                                t.text.push(' ');
                            }
                            t.text.push_str(&snippet);
                        }
                    }
                    self.active_text_block = Some(i);
                } else {
                    self.p.label.blocks.push(Block::Text(TextBlock { text: snippet, ..Default::default() }));
                    self.collapsed.push(false);
                    self.active_text_block = Some(self.p.label.blocks.len() - 1);
                }
            }
        }
    }

    fn toggle_picked_field(&mut self, name: &str) {
        let added = if let Some(pos) = self.picked_fields.iter().position(|h| h == name) {
            self.picked_fields.remove(pos);
            false
        } else {
            self.picked_fields.push(name.to_string());
            true
        };
        let Some(t) = &self.table else { return };
        if self.p.sel_rows.is_empty() {
            self.sel_rows_auto = true;
        }
        if self.sel_rows_auto {
            self.p.sel_rows = non_empty_rows_for_fields(t, &self.picked_fields, &self.table_filter);
        } else if added {
            let extra = non_empty_rows_for_fields(t, &[name.to_string()], &self.table_filter);
            for r in extra {
                if !self.p.sel_rows.contains(&r) {
                    self.p.sel_rows.push(r);
                }
            }
            self.p.sel_rows.sort_unstable();
        }
        if !self.p.sel_rows.is_empty()
            && self.preview_row.is_none_or(|r| !self.p.sel_rows.contains(&r))
        {
            self.preview_row = self.p.sel_rows.first().copied();
        }
    }

    /// Selected rows in sheet order.
    fn selected_records(&self) -> Vec<Arc<Record>> {
        let Some(t) = &self.table else { return vec![] };
        let mut sel = self.p.sel_rows.clone();
        sel.sort_unstable();
        sel.dedup();
        sel.into_iter().filter_map(|i| t.record(i)).collect()
    }

    /// Values for the preview: the clicked selected row (or first selected); with
    /// nothing selected, the clicked or first row as a sample.
    fn preview_vars(&self) -> Vars {
        let mut sel = self.p.sel_rows.clone();
        sel.sort_unstable();
        let (row, k) = match &self.table {
            Some(t) if self.p.label.data.is_some() => {
                if sel.is_empty() {
                    (t.record(self.preview_row.unwrap_or(0)), 0)
                } else {
                    let k = self.preview_row.and_then(|r| sel.iter().position(|&s| s == r)).unwrap_or(0);
                    (t.record(sel[k]), k)
                }
            }
            _ => (None, 0),
        };
        Pager::new(&self.p.label, &self.p.settings, row.into_iter().collect()).record_vars(k as u64)
    }

    fn data_panel(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, editor_avail_h: f32) {
        let Some(src) = self.p.label.data.clone() else { return };
        enum DAct {
            Change,
            Reload,
            Remove,
            Sheet(String),
            HeaderRow(usize),
            FillMerged(bool),
            ToggleField(String),
            Insert(InsertMode),
        }
        let mut act: Option<DAct> = None;
        let units = if src.by_columns { "columns" } else { "rows" };
        egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                icons::collapse_button(ui, &mut self.data_collapsed);
                icons::show(ui, Icon::Sheet, 18.0);
                ui.label(RichText::new(src.file_name()).strong()).on_hover_text(&src.path);
                if self.data_collapsed {
                    if let Some(t) = &self.table {
                        ui.weak(format!("·  {} of {} {units} selected", self.p.sel_rows.len(), t.rows.len()));
                    } else if self.table_err.is_some() {
                        ui.colored_label(Color32::from_rgb(220, 60, 50), "·  load error");
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Remove").on_hover_text("Stop using this spreadsheet").clicked() {
                        act = Some(DAct::Remove);
                    }
                    if ui.button("Change…").clicked() {
                        act = Some(DAct::Change);
                    }
                    if ui.button("Reload").on_hover_text("Re-read the file (automatic when it is saved)").clicked() {
                        act = Some(DAct::Reload);
                    }
                });
            });
            if !self.data_collapsed {
                if let Some(t) = &self.table
                    && (t.sheets.len() > 1 || t.merged_count > 0)
                {
                    ui.horizontal_wrapped(|ui| {
                        if t.sheets.len() > 1 {
                            ComboBox::from_id_salt("sheet").selected_text(&t.sheet).show_ui(ui, |ui| {
                                for sh in &t.sheets {
                                    if ui.selectable_label(*sh == t.sheet, sh).clicked() {
                                        act = Some(DAct::Sheet(sh.clone()));
                                    }
                                }
                            });
                        }
                        if t.merged_count > 0 {
                            let mut fm = src.fill_merged;
                            if ui
                                .checkbox(&mut fm, format!("Fill merged ({})", t.merged_count))
                                .on_hover_text("Repeat merged-cell values on every row in the group so every label gets them")
                                .changed()
                            {
                                act = Some(DAct::FillMerged(fm));
                            }
                        }
                    });
                }

                if let Some(e) = &self.table_err {
                    ui.colored_label(Color32::from_rgb(220, 60, 50), e);
                }

                if let Some(t) = &self.table {
                    let mut tbl_act = TableAction::None;
                    let sel_before = self.p.sel_rows.clone();
                    let reserve_for_blocks = if self.p.label.blocks.is_empty()
                        || self.collapsed.iter().take(self.p.label.blocks.len()).all(|&c| c)
                    {
                        90.0
                    } else {
                        150.0
                    };
                    let table_max_h = (editor_avail_h - reserve_for_blocks).max(self.dev.cfg.table_max_height);
                    table_ui(
                        ui,
                        t,
                        src.by_columns,
                        &mut self.p.sel_rows,
                        &mut self.table_filter,
                        &mut self.preview_row,
                        &mut self.last_click,
                        &mut self.table_drag,
                        &self.picked_fields,
                        &self.dev.cfg,
                        table_max_h,
                        &mut tbl_act,
                    );
                    if self.p.sel_rows != sel_before {
                        self.sel_rows_auto = self.p.sel_rows.is_empty();
                    }
                    match tbl_act {
                        TableAction::ToggleHeader(h) => act = Some(DAct::ToggleField(h)),
                        TableAction::AddToLabel => act = Some(DAct::Insert(InsertMode::Append)),
                        TableAction::UseHeaderRow(r) => act = Some(DAct::HeaderRow(r)),
                        TableAction::None => {}
                    }
                }
            }
        });
        ui.add_space(6.0);
        let reset = |app: &mut Self| {
            app.p.sel_rows.clear();
            app.sel_rows_auto = true;
            app.preview_row = None;
            app.last_click = None;
            app.table_drag = None;
        };
        match act {
            Some(DAct::Change) => {
                self.pick_data_file(ctx, false);
            }
            Some(DAct::Reload) => {
                self.reload_table();
                if self.table.is_some() {
                    self.info(format!("Reloaded {}", src.file_name()));
                }
            }
            Some(DAct::Remove) => {
                self.p.label.data = None;
                self.data_collapsed = false;
                self.picked_fields.clear();
                reset(self);
                self.reload_table();
            }
            Some(DAct::Sheet(s)) => {
                if let Some(d) = &mut self.p.label.data {
                    d.sheet = s;
                    d.header_row = None;
                }
                self.picked_fields.clear();
                reset(self);
                self.reload_table();
            }
            Some(DAct::HeaderRow(r)) => {
                if let Some(d) = &mut self.p.label.data {
                    d.header = true;
                    d.header_row = Some(r);
                }
                self.picked_fields.clear();
                reset(self);
                self.reload_table();
            }
            Some(DAct::FillMerged(fm)) => {
                if let Some(d) = &mut self.p.label.data {
                    d.fill_merged = fm;
                }
                self.reload_table();
            }
            Some(DAct::ToggleField(h)) => self.toggle_picked_field(&h),
            Some(DAct::Insert(m)) => self.insert_picked_fields(m),
            None => {}
        }
    }

    /// Re-read the spreadsheet when it is saved (checked every 2 s).
    fn watch_data(&mut self, ctx: &egui::Context) {
        let Some(src) = &self.p.label.data else { return };
        ctx.request_repaint_after(Duration::from_secs(2));
        if self.data_checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.data_checked = Instant::now();
        let current = data::mtime(&src.path);
        let loaded = self.table.as_ref().and_then(|t| t.mtime);
        if current.is_some() && current != loaded {
            let name = src.file_name();
            self.reload_table();
            if self.table.is_some() {
                self.info(format!("Reloaded {name} (file changed)"));
            }
        }
    }

    // ---------------------------------------------------------------- files

    fn save(&mut self, ctx: &egui::Context, as_new: bool) {
        if let (Some(p), false) = (&self.p.doc_path, as_new) {
            self.save_to(std::path::PathBuf::from(p));
            return;
        }
        self.spawn_dialog(ctx, || {
            rfd::FileDialog::new()
                .add_filter("Label", &["ptlabel", "json"])
                .set_file_name("label.ptlabel")
                .save_file()
                .map(DialogRes::SaveLabel)
        });
    }

    fn save_to(&mut self, path: std::path::PathBuf) {
        match serde_json::to_string_pretty(&self.p.label)
            .map_err(anyhow::Error::from)
            .and_then(|s| std::fs::write(&path, s).map_err(Into::into))
        {
            Ok(()) => {
                self.info(format!("Saved {}", path.display()));
                self.p.doc_path = Some(path.display().to_string());
                self.saved = self.p.label.clone();
                self.history.commit(&self.p.label);
                self.add_recent(&path.display().to_string());
            }
            Err(e) => self.error(format!("Save failed: {e}")),
        }
    }

    /// Before the label is replaced (New/Open): offer to save unsaved changes. False = cancel.
    fn confirm_discard(&mut self) -> bool {
        if self.p.label == self.saved {
            return true;
        }
        let save = "Save".to_string();
        let r = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Unsaved changes")
            .set_description("Save the current label before replacing it?")
            .set_buttons(rfd::MessageButtons::YesNoCancelCustom(save.clone(), "Don't Save".into(), "Cancel".into()))
            .show();
        match r {
            rfd::MessageDialogResult::Custom(c) if c == save => {
                let path = match &self.p.doc_path {
                    Some(p) => Some(std::path::PathBuf::from(p)),
                    None => rfd::FileDialog::new()
                        .add_filter("Label", &["ptlabel", "json"])
                        .set_file_name("label.ptlabel")
                        .save_file(),
                };
                if let Some(p) = path {
                    self.save_to(p);
                }
                self.p.label == self.saved // cancelled or failed save keeps the label
            }
            rfd::MessageDialogResult::Custom(c) => c == "Don't Save",
            _ => false,
        }
    }

    fn open(&mut self, ctx: &egui::Context) {
        self.spawn_dialog(ctx, || {
            rfd::FileDialog::new()
                .add_filter("Label", &["ptlabel", "json"])
                .pick_file()
                .map(DialogRes::OpenLabel)
        });
    }

    fn open_path(&mut self, path: std::path::PathBuf) {
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|s| serde_json::from_str::<Label>(&s).map_err(Into::into))
        {
            Ok(l) => {
                if !self.confirm_discard() {
                    return;
                }
                self.saved = l.clone();
                self.p.label = l;
                self.p.sel_rows.clear();
                self.preview_row = None;
                self.last_click = None;
                self.collapsed.clear();
                self.data_collapsed = false;
                self.reload_table();
                self.p.doc_path = Some(path.display().to_string());
                self.history = History::new(self.p.label.clone());
                self.add_recent(&path.display().to_string());
                self.info(format!("Opened {}", path.display()));
            }
            Err(e) => self.error(format!("Open failed: {e}")),
        }
    }

    /// The label as previewed (current values / first row) as a pixel-exact PNG, like `ptlabel render`.
    fn export_png(&mut self, ctx: &egui::Context) {
        let stem = self.p.doc_path.as_deref().and_then(|p| std::path::Path::new(p).file_stem());
        let name = format!("{}.png", stem.map_or("label".into(), |s| s.to_string_lossy()));
        self.spawn_dialog(ctx, move || {
            rfd::FileDialog::new()
                .add_filter("PNG image", &["png"])
                .set_file_name(name)
                .save_file()
                .map(DialogRes::ExportPng)
        });
    }

    fn export_png_to(&mut self, path: std::path::PathBuf) {
        let (tape, vars, opts) = (self.tape(), self.preview_vars(), &self.p.settings.opts);
        let out = self.renderer.lock().unwrap_or_else(|e| e.into_inner()).render(&self.p.label, &tape, &vars, opts);
        match save_png(&out.bitmap, &tape, opts, &path) {
            Ok(()) if out.warnings.is_empty() => self.info(format!("Exported {}", path.display())),
            Ok(()) => self.error(format!("Exported {} · {}", path.display(), out.warnings.join("; "))),
            Err(e) => self.error(format!("Export failed: {e}")),
        }
    }

    /// Put `path` first in File › Open Recent.
    fn add_recent(&mut self, path: &str) {
        history::push_recent(&mut self.p.recent, path);
        self.refresh_recent();
    }

    /// Drop recent files that are gone and update the menu.
    fn refresh_recent(&mut self) {
        history::prune_recent(&mut self.p.recent);
        crate::menu::set_recent(&self.p.recent);
    }

    // ---------------------------------------------------------------- undo

    /// Feed the label to the undo history once per frame, before the UI edits it. A pointer press
    /// or a focus change ends the current step; a drag ends once the pointer is released.
    fn track_edits(&mut self, ctx: &egui::Context) {
        let focus = ctx.memory(|m| m.focused());
        let (pressed, down) = ctx.input(|i| (i.pointer.any_pressed(), i.pointer.any_down()));
        let boundary = pressed || focus != self.last_focus;
        self.last_focus = focus;
        self.history.observe(&self.p.label, Instant::now(), down, boundary);
    }

    /// Undo (`back`) or redo a label edit.
    fn undo(&mut self, back: bool) {
        let step = if back { self.history.undo(&self.p.label) } else { self.history.redo(&self.p.label) };
        let Some(l) = step else { return };
        let data_changed = l.data != self.p.label.data;
        self.p.label = l;
        self.spell_target = None;
        if data_changed {
            self.p.sel_rows.clear();
            self.preview_row = None;
            self.last_click = None;
            self.reload_table();
        }
    }

    /// Window title: document name, plus "Edited" while the label differs from the saved file.
    fn update_title(&mut self, ctx: &egui::Context) {
        let mut t = app_name();
        if let Some(n) = self.p.doc_path.as_deref().and_then(|p| std::path::Path::new(p).file_name()) {
            t += &format!(" — {}", n.to_string_lossy());
        }
        if self.p.label != self.saved {
            t += " — Edited";
        }
        if t != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t.clone()));
            self.title = t;
        }
    }

    // ---------------------------------------------------------------- preview

    fn update_preview(&mut self, ctx: &egui::Context) {
        // One render at a time, off the UI thread; when it lands (and wakes the UI), the
        // newest state is rendered next if it changed meanwhile.
        if let Some(rx) = &self.preview_job {
            match rx.try_recv() {
                Ok(p) => self.preview = Some(p),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {}
            }
            self.preview_job = None;
        }
        let tape = self.tape();
        let vars = self.preview_vars();
        let opts = &self.p.settings.opts;
        let mut h = std::hash::DefaultHasher::new();
        serde_json::to_string(&self.p.label).unwrap_or_default().hash(&mut h);
        serde_json::to_string(opts).unwrap_or_default().hash(&mut h);
        format!("{tape:?}").hash(&mut h);
        vars.hash(&mut h);
        if self.p.label.uses("date") || self.p.label.uses("time") {
            chrono::Local::now().format("%Y%m%d%H%M").to_string().hash(&mut h);
        }
        let key = h.finish();
        if self.preview.as_ref().is_some_and(|p| p.key == key) {
            return;
        }
        let max_side = ctx.input(|i| i.max_texture_side);
        let (renderer, label, opts) = (self.renderer.clone(), self.p.label.clone(), opts.clone());
        if self.preview.is_none() {
            // First frame: show the preview right away.
            self.preview = Some(render_preview(ctx, &renderer, key, &label, &tape, &vars, &opts, max_side));
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                let _ = tx.send(render_preview(&ctx, &renderer, key, &label, &tape, &vars, &opts, max_side));
                ctx.request_repaint();
            })
            .expect("spawn preview render");
        self.preview_job = Some(rx);
    }

    // ---------------------------------------------------------------- UI parts

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            icons::logo(ui, 22.0);
            ui.label(RichText::new("PTLabel").strong().size(16.0));
            #[cfg(feature = "dev")]
            {
                let dev_fill = if self.dev.open {
                    Color32::from_rgb(230, 165, 16)
                } else {
                    Color32::from_rgb(248, 204, 56)
                };
                let pill = egui::Button::new(RichText::new("DEV").strong().size(11.0).color(Color32::from_rgb(28, 37, 51)))
                    .fill(dev_fill)
                    .corner_radius(4.0)
                    .min_size(egui::vec2(0.0, 20.0));
                if ui
                    .add(pill)
                    .on_hover_text("Toggle Dev UI Inspector (⌘⇧D) · live-reloads dev-ui.json without recompiling")
                    .clicked()
                {
                    self.dev.open = !self.dev.open;
                }
            }
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.separator();

            let list = self.printer_list();
            let current = self
                .handle
                .as_ref()
                .map(|h| h.id.display())
                .unwrap_or_else(|| "No printer (design only)".into());
            let mut choice: Option<Option<PrinterId>> = None;
            let selected_link = self.handle.as_ref().map(|h| h.id.link.clone());
            let mut forget: Option<String> = None;
            let mut add_host = false;
            let hosts = &self.p.hosts;
            let new_host = &mut self.new_host;
            let design_only = self.handle.is_none();
            ComboBox::from_id_salt("printer")
                .width(300.0)
                .selected_text(current)
                .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                .show_ui(ui, |ui| {
                    for id in &list {
                        ui.horizontal(|ui| {
                            if ui.selectable_label(selected_link.as_ref() == Some(&id.link), id.display()).clicked() {
                                choice = Some(Some(id.clone()));
                                ui.close();
                            }
                            if let Link::Network(h) = &id.link {
                                if hosts.contains(h)
                                    && ui.button("Forget").on_hover_text("Remove this IP from the list").clicked()
                                {
                                    forget = Some(h.clone());
                                }
                            }
                        });
                    }
                    if list.is_empty() {
                        #[cfg(target_os = "windows")]
                        ui.weak("Searching… plug in via USB (Editor Lite off) or pair Bluetooth in Settings › Bluetooth & devices");
                        #[cfg(not(target_os = "windows"))]
                        ui.weak("Searching… pair the PT-P710BT in System Settings › Bluetooth");
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        let r = ui.add(TextEdit::singleline(new_host).hint_text("IP or hostname").desired_width(150.0));
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        let ok = !new_host.trim().is_empty();
                        if (ui.add_enabled(ok, egui::Button::new("Add Wi-Fi printer")).clicked() || enter) && ok {
                            add_host = true;
                            ui.close();
                        }
                    });
                    ui.separator();
                    if ui.selectable_label(design_only, "No printer (design only)").clicked() {
                        choice = Some(None);
                        ui.close();
                    }
                });
            if let Some(h) = forget {
                self.p.hosts.retain(|x| *x != h);
            }
            if add_host {
                let host = self.new_host.trim().to_string();
                if !self.p.hosts.contains(&host) {
                    self.p.hosts.push(host.clone());
                }
                self.new_host.clear();
                choice = Some(Some(PrinterId { model: "Wi-Fi P-touch".into(), link: Link::Network(host) }));
            }
            match choice {
                Some(Some(id)) => self.connect(id),
                Some(None) => self.disconnect(),
                None => {}
            }

            ui.separator();
            let (dot, text, hover) = match &self.conn {
                Conn::None => (Color32::GRAY, "Design only".to_string(), String::new()),
                Conn::Connecting => (Color32::from_rgb(230, 170, 30), "Connecting…".into(), String::new()),
                Conn::Offline(e) => (Color32::from_rgb(220, 60, 50), "Not reachable".into(), e.clone()),
                Conn::Online(s) => {
                    if let Some(r) = &self.job {
                        let n = r.total.map_or("∞".to_string(), |n| n.to_string());
                        (Color32::from_rgb(60, 130, 240), format!("Printing {}/{n}", r.done), String::new())
                    } else if !s.errors().is_empty() {
                        (Color32::from_rgb(220, 60, 50), s.summary(), String::new())
                    } else {
                        (Color32::from_rgb(40, 180, 90), s.summary(), String::new())
                    }
                }
            };
            if ui.add(egui::Button::new("↻").min_size(egui::vec2(28.0, 26.0))).on_hover_text("Re-read printer status").clicked() {
                if let Some(h) = &self.handle {
                    h.refresh();
                }
            }
            ui.label(RichText::new("●").color(dot).size(14.0));
            let r = ui.label(text);
            if !hover.is_empty() {
                r.on_hover_text(hover);
            }

            ui.separator();
            let tape = self.tape();
            let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 18.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 3.0, rgb(media::tape_color(tape.tape_color).1));
            ui.painter().rect_stroke(rect, 3.0, (1.0, Color32::from_gray(120)), egui::StrokeKind::Inside);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Aa",
                FontId::proportional(12.0),
                rgb(media::text_color(tape.text_color).1),
            );
            if self.live_tape.is_some() {
                ui.label(tape.describe()).on_hover_text("Detected from the loaded cassette");
            } else {
                ui.label("Tape");
                ComboBox::from_id_salt("offline_tape")
                    .width(80.0)
                    .selected_text(format!("{} mm", if self.p.offline_tape == 4 { 3.5 } else { self.p.offline_tape as f32 }))
                    .show_ui(ui, |ui| {
                        for w in TZE_WIDTHS {
                            let t = if w == 4 { "3.5 mm".to_string() } else { format!("{w} mm") };
                            ui.selectable_value(&mut self.p.offline_tape, w, t);
                        }
                    })
                    .response
                    .on_hover_text("Used until a printer reports the loaded tape");
            }

        });
    }

    fn print_panel(&mut self, ui: &mut egui::Ui) {
        let caps = self.caps();
        let model = self.model();
        let n_sel = self.p.sel_rows.len();
        let has_data = self.p.label.data.is_some();
        let by_columns = self.p.label.data.as_ref().is_some_and(|d| d.by_columns);
        let (unit, units) = if by_columns { ("column", "columns") } else { ("row", "rows") };
        let (records, total) = Pager::counts(&self.p.label, &self.p.settings, n_sel);
        let multi = has_data || records != Some(1);
        let queued = self.handle.as_ref().map(|h| h.queued()).unwrap_or_default();
        let busy = self.job.is_some() || !queued.is_empty();
        let label = match total {
            Some(1) => "Print".to_string(),
            Some(n) => format!("Print {n} labels"),
            None => "Print until stopped".to_string(),
        };
        let label = if busy { format!("Add to queue · {}", count_text(total)) } else { label };
        let enabled = self.handle.is_some();
        let show_cut = self.p.settings.opts.chain && self.chain_pending;
        let icon_id = egui::IdSalt::new("print-icon");
        let text_rt = if enabled {
            RichText::new(label).size(20.0).strong().color(Color32::WHITE)
        } else {
            RichText::new(label).size(20.0).strong()
        };
        ui.add_space(6.0);
        let mut do_print = false;
        let mut do_cut = false;
        ui.horizontal(|ui| {
            let cut_w = 58.0;
            let gap = ui.spacing().item_spacing.x;
            let print_w = if show_cut { (ui.available_width() - cut_w - gap).max(120.0) } else { ui.available_width() };
            let mut btn = egui::Button::new((egui::Atom::custom(icon_id, egui::Vec2::splat(28.0)), text_rt))
                .min_size(egui::vec2(print_w, 58.0));
            if enabled {
                btn = btn.fill(self.dev.cfg.accent());
            }
            let out = ui.add_enabled_ui(enabled, |ui| btn.atom_ui(ui)).inner;
            if let Some(r) = out.rect(icon_id) {
                let col = if enabled { Color32::WHITE } else { ui.style().interact(&out.response).text_color() };
                icons::paint(ui.painter(), r, Icon::Printer, col);
            }
            #[cfg(target_os = "macos")]
            const PRINT_HINT: &str = "⌘P · jobs queue up while printing";
            #[cfg(not(target_os = "macos"))]
            const PRINT_HINT: &str = "Ctrl+P · jobs queue up while printing";
            if out.response.on_hover_text(PRINT_HINT).clicked() {
                do_print = true;
            }
            if show_cut {
                let cut_id = egui::IdSalt::new("cut-chain-icon");
                let wait_reason = self.waiting_reason();
                let can_cut = enabled && !busy && !self.cutting && wait_reason.is_none();
                let cut_btn = egui::Button::new(egui::Atom::custom(cut_id, egui::Vec2::splat(26.0)))
                    .min_size(egui::vec2(cut_w, 58.0));
                let cut_out = ui.add_enabled_ui(can_cut, |ui| cut_btn.atom_ui(ui)).inner;
                if let Some(r) = cut_out.rect(cut_id) {
                    let col = ui.style().interact(&cut_out.response).text_color();
                    icons::paint(ui.painter(), r, Icon::Scissors, col);
                }
                let tip = if self.cutting {
                    "Feeding & cutting the last chained label…".to_string()
                } else if let Some(w) = wait_reason {
                    format!("Feed & cut unavailable: {w}")
                } else {
                    "Feed & cut the last chained label waiting in the printer".to_string()
                };
                if cut_out.response.on_hover_text(&tip).on_disabled_hover_text(&tip).clicked() {
                    do_cut = true;
                }
            }
        });
        if do_print {
            self.print();
        }
        if do_cut {
            if let Some(h) = &self.handle {
                h.feed_and_cut(caps);
                self.cutting = true;
            }
        }
        if self.handle.is_none() {
            ui.weak("Select a printer");
        } else if let Some(w) = self.waiting_reason() {
            ui.weak(format!("{w}. Jobs wait and print automatically."));
        } else if show_cut {
            ui.weak(if self.cutting {
                "Feeding & cutting last label…"
            } else {
                "Last chained label waiting — print next or cut"
            });
        } else if has_data && n_sel == 0 {
            ui.weak(format!("Select {units} in the spreadsheet table"));
        }
        ui.add_space(8.0);

        let s = &mut self.p.settings;
        egui::Grid::new("copies").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            ui.label(if multi { "Copies each" } else { "Labels" });
            ui.horizontal(|ui| {
                ui.add_enabled(!s.infinite, DragValue::new(&mut s.copies).range(1..=99_999).speed(0.2));
                ui.toggle_value(&mut s.infinite, " ∞ ")
                    .on_hover_text("Keep printing until you press Stop or the tape runs out");
            });
            ui.end_row();
            if self.p.label.uses("n") {
                ui.label("Number from");
                ui.add(DragValue::new(&mut s.num_start).speed(0.2));
                ui.end_row();
                ui.label("Step");
                ui.add(DragValue::new(&mut s.num_step).speed(0.1));
                ui.end_row();
            }
        });

        let copies = s.copies.max(1);
        let times = if copies > 1 { format!(" × {copies} copies") } else { String::new() };
        if has_data {
            match n_sel {
                0 => ui.colored_label(Color32::from_rgb(230, 170, 30), format!("Select spreadsheet {units} to print")),
                1 => ui.weak(format!("1 {unit}{times}")),
                n => ui.weak(format!("{n} {units}{times}")),
            };
        } else if self.p.label.sequence().is_some() {
            let v = Vars::default();
            let seqs: Vec<&SequenceBlock> = self.p.label.sequences().collect();
            let multi_seq = seqs.len() > 1;
            for (k, q) in seqs.iter().enumerate() {
                let span = match records {
                    Some(1) => q.format(0, &v),
                    Some(n) => format!("{} → {}", q.format(0, &v), q.format(n as u64 - 1, &v)),
                    None => format!("{} → …", q.format(0, &v)),
                };
                let name = if multi_seq { format!("Sequence {}", k + 1) } else { "Sequence".into() };
                let suffix = if k == 0 { times.as_str() } else { "" };
                ui.weak(format!("{name} {span}{suffix}"));
            }
        }

        ui.separator();
        ui.label(RichText::new("Cutting").strong());
        let o = &mut s.opts;
        ui.checkbox(&mut o.auto_cut, "Auto cut");
        ui.add_enabled(caps.half_cut, egui::Checkbox::new(&mut o.half_cut, "Half cut (keep backing joined)"))
            .on_disabled_hover_text(format!("{model} has no half cutter"));
        ui.horizontal(|ui| {
            let en = caps.cut_every && o.auto_cut;
            ui.add_enabled(en, egui::Label::new("Cut every"));
            ui.add_enabled(en, DragValue::new(&mut o.cut_every).range(1..=99))
                .on_disabled_hover_text(format!("{model} cuts after every label"));
            ui.add_enabled(en, egui::Label::new("labels"));
        });
        ui.checkbox(&mut o.chain, "Chain printing")
            .on_hover_text("Don't feed and cut after the last label (saves ~25 mm of tape). Cut when the next job starts, or press the Cut button next to Print to feed & cut the last label.");
        ui.checkbox(&mut o.special_tape, "Never cut (fabric / special tape)");

        ui.separator();
        ui.label(RichText::new("Output").strong());
        ui.checkbox(&mut o.high_res, "High resolution (180×360 dpi)");
        ui.checkbox(&mut o.mirror, "Mirror");
        ui.horizontal(|ui| {
            ui.label("Margins");
            ui.add(DragValue::new(&mut o.margin_mm).range(2.0..=127.0).speed(0.1).fixed_decimals(1).suffix(" mm"));
        });

        if let Some(r) = &self.job {
            ui.add_space(10.0);
            ui.label(RichText::new(format!("Printing: {}", r.title)).strong());
            let frac = r.total.map_or(0.0, |n| r.done as f32 / n.max(1) as f32);
            let txt = match r.total {
                Some(n) => format!("{} / {n}", r.done),
                None => format!("{} printed", r.done),
            };
            ui.add(egui::ProgressBar::new(frac).text(txt).animate(r.total.is_none()));
            let stop = egui::Button::new(if self.stopping { "Stopping after this label…" } else { "Stop this job" })
                .min_size(egui::vec2(ui.available_width(), 34.0));
            if ui.add_enabled(!self.stopping, stop).clicked() {
                if let Some(h) = &self.handle {
                    h.stop();
                    self.stopping = true;
                }
            }
        }

        if !queued.is_empty() {
            ui.add_space(10.0);
            ui.label(RichText::new(format!("Queue ({})", queued.len())).strong());
            let mut cancel = None;
            for (k, (id, title, total)) in queued.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{}. {title}", k + 1));
                    ui.weak(count_text(*total));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            cancel = Some(*id);
                        }
                    });
                });
            }
            if let Some(h) = &self.handle {
                if let Some(id) = cancel {
                    h.cancel(id);
                }
                if queued.len() > 1 && ui.button("Clear queue").clicked() {
                    h.clear();
                }
            }
        }
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        let tape = self.tape();
        let Some(pv) = &self.preview else { return };
        let tex_id = pv.tex.id();
        let [w, h] = pv.phys;
        let len_mm = pv.len_mm;
        let overflow = pv.overflow;
        let warnings = pv.warnings.clone();
        ui.horizontal(|ui| {
            let has_stepper = self.dev.cfg.show_preview_stepper
                && self.p.label.data.is_some()
                && self.table.is_some();
            let avail_w = ui.available_width();
            ui.label(RichText::new(format!("{len_mm:.1} mm")).strong().size(15.0));
            ui.label(format!("on {}", tape.describe()));
            if !has_stepper || avail_w > 640.0 {
                ui.weak(format!("· print area {:.1} mm", tape.print_pins as f32 * 25.4 / 180.0));
            }
            if self.live_tape.is_none() && (!has_stepper || avail_w > 740.0) {
                ui.weak("· design only");
            }
            if self.dev.cfg.show_preview_stepper
                && self.p.label.data.is_some()
                && let Some(t) = &self.table
            {
                let by_columns = self.p.label.data.as_ref().is_some_and(|d| d.by_columns);
                let f = self.table_filter.to_lowercase();
                let mut nav_rows: Vec<usize> = if self.p.sel_rows.is_empty() {
                    (0..t.rows.len())
                        .filter(|&i| f.is_empty() || t.rows[i].iter().any(|c| c.to_lowercase().contains(&f)))
                        .collect()
                } else {
                    self.p.sel_rows.clone()
                };
                nav_rows.sort_unstable();
                nav_rows.dedup();
                if !nav_rows.is_empty() {
                    let cur_nav_pos = self
                        .preview_row
                        .and_then(|r| nav_rows.iter().position(|&x| x == r))
                        .unwrap_or(0);
                    let active_preview_row = nav_rows[cur_nav_pos];
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_step = nav_rows.len() > 1;
                        if ui
                            .add_enabled(can_step, egui::Button::new("›").min_size(egui::vec2(24.0, 22.0)))
                            .on_hover_text("Preview next label")
                            .clicked()
                        {
                            let next = (cur_nav_pos + 1) % nav_rows.len();
                            self.preview_row = Some(nav_rows[next]);
                        }
                        let row_lbl = t.row_labels.get(active_preview_row).map(String::as_str).unwrap_or("?");
                        let tag = if by_columns { format!("Col {row_lbl}") } else { format!("Row {row_lbl}") };
                        ui.weak(format!("{tag} ({}/{})", cur_nav_pos + 1, nav_rows.len()));
                        if ui
                            .add_enabled(can_step, egui::Button::new("‹").min_size(egui::vec2(24.0, 22.0)))
                            .on_hover_text("Preview previous label")
                            .clicked()
                        {
                            let prev = if cur_nav_pos == 0 { nav_rows.len() - 1 } else { cur_nav_pos - 1 };
                            self.preview_row = Some(nav_rows[prev]);
                        }
                        ui.weak("Preview");
                    });
                }
            }
        });
        for warn in &warnings {
            ui.colored_label(Color32::from_rgb(220, 60, 50), warn);
        }
        if overflow {
            ui.colored_label(Color32::from_rgb(230, 150, 20), "Content is longer than the fixed length; the label will be longer.");
        }
        let min_h = 48.0_f32;
        let max_h = (ui.ctx().content_rect().height() * 0.55).clamp(160.0, 480.0);
        let box_h = self.p.preview_height.clamp(min_h, max_h);
        let pad_y: f32 = if box_h < 76.0 { 4.0 } else { 10.0 };
        let inner_h = (box_h - 2.0 * pad_y).max(20.0);
        let avail_w = (ui.available_width() - 24.0).max(40.0);
        let scale = (avail_w / w).min(inner_h / h).clamp(0.12, 4.0);
        let size = Vec2::new(w, h) * scale;
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(12, pad_y as i8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.set_min_height(inner_h);
                ui.set_max_height(inner_h);
                egui::ScrollArea::horizontal().id_salt("pv").show(ui, |ui| {
                    let top_pad = ((inner_h - size.y) * 0.5).max(0.0);
                    ui.vertical_centered(|ui| {
                        if top_pad > 0.5 {
                            ui.add_space(top_pad);
                        }
                        let img = egui::Image::from_texture((tex_id, size)).corner_radius(2.0);
                        ui.add(img).on_hover_text("Edges are the cut lines");
                    });
                });
            });

        // Draggable splitter handle to resize the preview height (double-click toggles compact).
        let (handle_rect, handle_resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 12.0),
            egui::Sense::click_and_drag(),
        );
        let active = handle_resp.hovered() || handle_resp.dragged();
        if active {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        if handle_resp.dragged() {
            self.p.preview_height = (box_h + handle_resp.drag_delta().y).clamp(min_h, max_h);
        }
        if handle_resp.double_clicked() {
            self.p.preview_height = if box_h > 110.0 {
                72.0
            } else {
                self.dev.cfg.preview_max_height.clamp(min_h, max_h)
            };
        }
        let p = ui.painter();
        let line_y = handle_rect.center().y;
        let line_col = if active {
            self.dev.cfg.accent().gamma_multiply(0.55)
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color.gamma_multiply(0.45)
        };
        p.line_segment(
            [egui::pos2(handle_rect.min.x, line_y), egui::pos2(handle_rect.max.x, line_y)],
            (1.0, line_col),
        );
        let pill = egui::Rect::from_center_size(handle_rect.center(), egui::vec2(38.0, 4.0));
        let pill_col = if active {
            self.dev.cfg.accent()
        } else {
            ui.visuals().weak_text_color().gamma_multiply(0.55)
        };
        p.rect_filled(pill, 2.0, pill_col);
        handle_resp.on_hover_text("Drag to resize preview · Double-click to toggle compact size");
    }

    fn label_options(&mut self, ui: &mut egui::Ui) {
        let l = &mut self.p.label;
        let mut popup_open = false;
        toolbar_section(ui, "LENGTH", |ui| {
            let h = ui.spacing().interact_size.y;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if ui
                    .add(egui::Button::new("Auto").selected(!l.fixed_length).min_size(egui::vec2(52.0, h)))
                    .clicked()
                {
                    l.fixed_length = false;
                }
                if ui
                    .add(egui::Button::new("Fixed").selected(l.fixed_length).min_size(egui::vec2(54.0, h)))
                    .clicked()
                {
                    l.fixed_length = true;
                }
            });
            if l.fixed_length {
                let was_open = self.length_popup_was_open;
                let input = &mut self.length_input;
                ComboBox::from_id_salt("fixed_length")
                    .width(96.0)
                    .selected_text(format!("{:.1} mm", l.length_mm))
                    .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                    .show_ui(ui, |ui| {
                        popup_open = true;
                        if !was_open {
                            *input = format!("{:.1}", l.length_mm);
                        }
                        ui.horizontal(|ui| {
                            let id = ui.make_persistent_id("fixed_len_edit");
                            let out = TextEdit::singleline(input)
                                .id(id)
                                .desired_width(72.0)
                                .hint_text("5–1000")
                                .show(ui);
                            let resp = out.response;
                            if !was_open {
                                resp.request_focus();
                                let mut state = out.state;
                                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                                    egui::text::CCursor::new(0),
                                    egui::text::CCursor::new(input.chars().count()),
                                )));
                                state.store(ui.ctx(), id);
                            }
                            ui.label("mm");
                            if resp.changed()
                                && let Ok(v) = input.trim().replace(',', ".").parse::<f32>()
                                && (5.0..=1000.0).contains(&v)
                            {
                                l.length_mm = v;
                            }
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                if let Ok(v) = input.trim().replace(',', ".").parse::<f32>() {
                                    l.length_mm = v.clamp(5.0, 1000.0);
                                }
                                ui.close();
                            }
                        });
                        ui.separator();
                        ui.label(RichText::new("Presets").small().weak());
                        for (name, mm) in LENGTH_PRESETS {
                            let active = (l.length_mm - mm).abs() < 0.05;
                            if ui.selectable_label(active, format!("{name} · {mm:.0} mm")).clicked() {
                                l.length_mm = mm;
                                *input = format!("{mm:.1}");
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text("Click to enter a custom length or pick a preset");
                align_buttons(ui, &mut l.align, "Align label");
            }
            ui.separator();
            ui.label(RichText::new("Gap").weak());
            ui.add(DragValue::new(&mut l.gap_mm).range(0.0..=50.0).speed(0.1).fixed_decimals(1).suffix(" mm"));
            ui.separator();
            ui.checkbox(&mut l.frame, "Frame");
            ui.checkbox(&mut l.invert, "Invert color");
        });
        self.length_popup_was_open = popup_open;
    }

    fn add_block_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut add_excel = false;
        let blocks = &mut self.p.label.blocks;
        let collapsed = &mut self.collapsed;
        toolbar_section(ui, "ADD", |ui| {
            if icons::button(ui, Icon::Text, "Text").clicked() {
                blocks.push(Block::Text(TextBlock { text: "Text".into(), ..Default::default() }));
                collapsed.push(false);
            }
            if icons::button(ui, Icon::Sequence, "Sequence")
                .on_hover_text("Numbered labels: prefix, start, step, count, digits")
                .clicked()
            {
                blocks.push(Block::Sequence(SequenceBlock::default()));
                collapsed.push(false);
            }
            if icons::button(ui, Icon::Sheet, "Excel")
                .on_hover_text("Print from spreadsheet rows or columns (xlsx, xls, ods, csv) — or drag a file onto the window")
                .clicked()
            {
                add_excel = true;
            }
            if icons::button(ui, Icon::Qr, "QR code").clicked() {
                blocks.push(Block::Qr(QrBlock { data: "https://".into(), ..Default::default() }));
                collapsed.push(false);
            }
            if icons::button(ui, Icon::DataMatrix, "Data Matrix").clicked() {
                blocks.push(Block::DataMatrix(DataMatrixBlock { data: "PTL-{n:04}".into(), ..Default::default() }));
                collapsed.push(false);
            }
            if icons::button(ui, Icon::Barcode, "Barcode").clicked() {
                blocks.push(Block::Barcode(BarcodeBlock { data: "12345678".into(), ..Default::default() }));
                collapsed.push(false);
            }
            if icons::button(ui, Icon::Image, "Image").clicked() {
                blocks.push(Block::Image(ImageBlock::default()));
                collapsed.push(false);
            }
            if icons::button(ui, Icon::Spacer, "Spacer").clicked() {
                blocks.push(Block::Spacer { mm: 3.0 });
                collapsed.push(false);
            }
        });
        if add_excel {
            self.add_excel(ctx);
        }
    }

    fn blocks_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        enum Act {
            Up,
            Down,
            Del,
            Dup,
        }
        let mut act: Option<(usize, Act)> = None;
        let n = self.p.label.blocks.len();
        self.collapsed.resize(n, false);
        let fonts = &self.fonts;
        let filter = &mut self.font_filter;
        let speller = &mut self.speller;
        let target = &mut self.spell_target;
        let headers: Vec<String> = self.table.as_ref().map(|t| t.headers.to_vec()).unwrap_or_default();
        let rows_drive = self.p.label.data.is_some();
        let by_columns = self.p.label.data.as_ref().is_some_and(|d| d.by_columns);
        let seq_count = self.p.label.sequences().count();
        let seq_in_text: Vec<bool> = (0..seq_count).map(|k| self.p.label.sequence_in_text(k)).collect();
        let mut seq_idx = 0usize;
        let mut picked_image: Option<usize> = None;
        for i in 0..n {
            let b = &mut self.p.label.blocks[i];
            let seq_info = if matches!(b, Block::Sequence(_)) {
                let k = seq_idx;
                seq_idx += 1;
                Some((k, sequence_placeholder(k), seq_in_text[k]))
            } else {
                None
            };
            let minimized = &mut self.collapsed[i];
            egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    icons::collapse_button(ui, minimized);
                    icons::show(ui, block_icon(b), 18.0);
                    if let Some((k, ph, _)) = &seq_info {
                        let title = if seq_count > 1 { format!("Sequence {}", k + 1) } else { "Sequence".into() };
                        ui.label(RichText::new(title).strong());
                        ui.label(RichText::new(ph).monospace().weak());
                    } else {
                        ui.label(RichText::new(b.kind_name()).strong());
                    }
                    if *minimized {
                        let summary = block_summary(b);
                        if !summary.is_empty() {
                            ui.weak(format!("·  {summary}"));
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Remove").clicked() {
                            act = Some((i, Act::Del));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("↓").min_size(egui::vec2(28.0, 26.0))).clicked() {
                            act = Some((i, Act::Down));
                        }
                        if ui.add_enabled(i > 0, egui::Button::new("↑").min_size(egui::vec2(28.0, 26.0))).clicked() {
                            act = Some((i, Act::Up));
                        }
                        if ui.button("Duplicate").clicked() {
                            act = Some((i, Act::Dup));
                        }
                    });
                });
                if !*minimized {
                    match b {
                        Block::Text(t) => {
                            if text_editor(ui, i, t, fonts, filter, speller, target, &headers) {
                                self.active_text_block = Some(i);
                            }
                        }
                        Block::Sequence(q) => {
                            let (_, ph, in_text) = seq_info.as_ref().unwrap();
                            sequence_editor(ui, i, q, fonts, filter, rows_drive, by_columns, *in_text, ph);
                        }
                        Block::Qr(q) => qr_editor(ui, i, q, &headers),
                        Block::DataMatrix(d) => dm_editor(ui, i, d, &headers),
                        Block::Barcode(bc) => barcode_editor(ui, i, bc, &headers),
                        Block::Image(im) => {
                            if image_editor(ui, im) {
                                picked_image = Some(i);
                            }
                        }
                        Block::Spacer { mm } => {
                            ui.horizontal(|ui| {
                                ui.label("Width");
                                ui.add(DragValue::new(mm).range(0.5..=300.0).speed(0.1).fixed_decimals(1).suffix(" mm"));
                            });
                        }
                    }
                }
            });
        }
        if let Some(i) = picked_image {
            self.spawn_dialog(ctx, move || {
                rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg"])
                    .pick_file()
                    .map(|path| DialogRes::PickImage { block: i, path })
            });
        }
        if act.is_some() {
            self.spell_target = None; // its block index is stale once blocks move
        }
        let blocks = &mut self.p.label.blocks;
        match act {
            Some((i, Act::Up)) => {
                blocks.swap(i, i - 1);
                self.collapsed.swap(i, i - 1);
            }
            Some((i, Act::Down)) => {
                blocks.swap(i, i + 1);
                self.collapsed.swap(i, i + 1);
            }
            Some((i, Act::Del)) => {
                blocks.remove(i);
                self.collapsed.remove(i);
            }
            Some((i, Act::Dup)) => {
                let c = blocks[i].clone();
                blocks.insert(i + 1, c);
                let col = self.collapsed[i];
                self.collapsed.insert(i + 1, col);
            }
            None => {}
        }
    }

    fn active_msg(&self) -> Option<(&str, bool, Duration)> {
        let (m, err, t) = self.msg.as_ref()?;
        let elapsed = t.elapsed();
        if *err || elapsed.as_secs() < 12 {
            Some((m.as_str(), *err, Duration::from_secs(12).saturating_sub(elapsed)))
        } else {
            None
        }
    }

    fn placeholders_ui(&self, ui: &mut egui::Ui) {
        ui.spacing_mut().interact_size.y = 0.0;
        ui.spacing_mut().item_spacing.y = 2.0;
        let seq_count = self.p.label.sequences().count();
        let seq_hints = (0..seq_count.max(1)).map(sequence_placeholder).collect::<Vec<_>>().join("  ");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
            ui.label(RichText::new("Sequence:").size(11.5).weak());
            ui.label(RichText::new(format!("{{n}}  {{n:03}}  {seq_hints}")).size(11.5).monospace().weak());
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
            ui.label(RichText::new("Time:").size(11.5).weak());
            ui.label(RichText::new("{date}  {time}").size(11.5).monospace().weak());
        });
        if self.p.label.data.is_some()
            && let Some(t) = &self.table
            && !t.headers.is_empty()
        {
            let cols: Vec<String> = t.headers.iter().take(8).map(|h| format!("{{{h}}}")).collect();
            let mut text = cols.join("  ");
            if t.headers.len() > 8 {
                text += &format!("  +{} more", t.headers.len() - 8);
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                ui.label(RichText::new("Columns:").size(11.5).weak());
                ui.label(RichText::new(text).size(11.5).monospace().weak());
            });
        }
    }

    fn status_right(&mut self, ui: &mut egui::Ui) {
        #[cfg(target_os = "macos")]
        const SHORTCUTS: &str = "⌘P print  ⌘S save  ⌘O open  ⌘N new";
        #[cfg(not(target_os = "macos"))]
        const SHORTCUTS: &str = "Ctrl+P print  Ctrl+S save  Ctrl+O open  Ctrl+N new";

        ui.spacing_mut().interact_size.y = 0.0;
        let avail_w = (ui.available_width() - 2.0).max(40.0);
        let ref_sz = 10.0;
        let ref_w = ui
            .painter()
            .layout_no_wrap(SHORTCUTS.to_string(), FontId::proportional(ref_sz), Color32::WHITE)
            .size()
            .x;
        let sz = if ref_w > 0.0 { (ref_sz * (avail_w * 0.88) / ref_w).clamp(11.0, 14.0) } else { 13.0 };

        ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            if let Some(r) = &self.update {
                let t = if self.updating { "Updating…".to_string() } else { format!("Update to {}…", r.version) };
                let resp = ui
                    .add(egui::Label::new(RichText::new(format!("● {t}")).size(13.0).color(self.dev.cfg.accent())).sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Installs it and opens PTLabel again");
                if resp.clicked() {
                    self.update_modal = true;
                }
            }
            ui.add(egui::Label::new(RichText::new(SHORTCUTS).size(sz).weak()).extend());
        });
    }

    /// File commands from the native menu bar and keyboard shortcuts. The menu
    /// normally takes the key equivalents; a command runs at most once per frame.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let sc = |m, k| KeyboardShortcut::new(m, k);
        let shift_cmd = Modifiers::COMMAND | Modifiers::SHIFT;
        #[cfg(feature = "dev")]
        if ctx.input_mut(|i| i.consume_shortcut(&sc(shift_cmd, Key::D))) {
            self.dev.open = !self.dev.open;
        }
        let keys = [
            (sc(shift_cmd, Key::S), MenuCmd::SaveAs),
            (sc(shift_cmd, Key::E), MenuCmd::ExportPng),
            (sc(Modifiers::COMMAND, Key::S), MenuCmd::Save),
            (sc(Modifiers::COMMAND, Key::O), MenuCmd::Open),
            (sc(Modifiers::COMMAND, Key::N), MenuCmd::New),
            (sc(Modifiers::COMMAND, Key::P), MenuCmd::Print),
        ];
        let mut cmds: Vec<MenuCmd> = self.menu_rx.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for (k, c) in keys {
            if ctx.input_mut(|i| i.consume_shortcut(&k)) {
                cmds.push(c);
            }
        }
        // ⌘Z / ⇧⌘Z (or Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y) in a text field are its own undo.
        if !ctx.text_edit_focused() {
            for (k, c) in [(sc(shift_cmd, Key::Z), MenuCmd::Redo), (sc(Modifiers::COMMAND, Key::Z), MenuCmd::Undo)] {
                if ctx.input_mut(|i| i.consume_shortcut(&k)) {
                    cmds.push(c);
                }
            }
            #[cfg(not(target_os = "macos"))]
            if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND, Key::Y))) {
                cmds.push(MenuCmd::Redo);
            }
        }
        let mut seen = Vec::new();
        for c in cmds {
            if seen.contains(&c) {
                continue;
            }
            seen.push(c);
            self.run_menu_cmd(ctx, c);
        }
    }

    fn run_menu_cmd(&mut self, ctx: &egui::Context, c: MenuCmd) {
        match c {
            MenuCmd::Undo | MenuCmd::Redo if ctx.text_edit_focused() => {
                // The Edit menu took the key: give it back to the text field next frame.
                let shift = if c == MenuCmd::Redo { Modifiers::SHIFT } else { Modifiers::NONE };
                let modifiers = Modifiers::COMMAND | Modifiers::MAC_CMD | shift;
                for pressed in [true, false] {
                    let (key, physical_key, repeat) = (Key::Z, Some(Key::Z), false);
                    self.forward.push(egui::Event::Key { key, physical_key, pressed, repeat, modifiers });
                }
                ctx.request_repaint();
            }
            MenuCmd::Undo => self.undo(true),
            MenuCmd::Redo => self.undo(false),
            MenuCmd::OpenRecent => {
                if let Some(p) = crate::menu::take_recent() {
                    self.open_path(p.into());
                }
                self.refresh_recent();
            }
            MenuCmd::ClearRecent => {
                self.p.recent.clear();
                self.refresh_recent();
            }
            MenuCmd::New => {
                if !self.confirm_discard() {
                    return;
                }
                self.p.label = Label::default();
                self.saved = Label::default();
                self.p.doc_path = None;
                self.p.sel_rows.clear();
                self.preview_row = None;
                self.last_click = None;
                self.collapsed.clear();
                self.data_collapsed = false;
                self.reload_table();
                self.history = History::new(self.p.label.clone());
            }
            MenuCmd::Open => self.open(ctx),
            MenuCmd::Save => self.save(ctx, false),
            MenuCmd::SaveAs => self.save(ctx, true),
            MenuCmd::ExportPng => self.export_png(ctx),
            MenuCmd::Print => self.print(),
        }
    }

    /// In-window File / Edit menu bar on Windows & Linux (macOS uses the native top menu bar).
    #[cfg(not(target_os = "macos"))]
    fn window_menu_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut cmd: Option<MenuCmd> = None;
        let mut open_recent: Option<String> = None;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("New").shortcut_text("Ctrl+N")).clicked() {
                    cmd = Some(MenuCmd::New);
                    ui.close();
                }
                if ui.add(egui::Button::new("Open…").shortcut_text("Ctrl+O")).clicked() {
                    cmd = Some(MenuCmd::Open);
                    ui.close();
                }
                ui.menu_button("Open Recent", |ui| {
                    for p in &self.p.recent {
                        let name = std::path::Path::new(p)
                            .file_name()
                            .map_or(p.clone(), |n| n.to_string_lossy().into_owned());
                        if ui.button(name).on_hover_text(p).clicked() {
                            open_recent = Some(p.clone());
                            ui.close();
                        }
                    }
                    if !self.p.recent.is_empty() {
                        ui.separator();
                    }
                    if ui.add_enabled(!self.p.recent.is_empty(), egui::Button::new("Clear Menu")).clicked() {
                        cmd = Some(MenuCmd::ClearRecent);
                        ui.close();
                    }
                });
                ui.separator();
                if ui.add(egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                    cmd = Some(MenuCmd::Save);
                    ui.close();
                }
                if ui.add(egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S")).clicked() {
                    cmd = Some(MenuCmd::SaveAs);
                    ui.close();
                }
                if ui.add(egui::Button::new("Export PNG…").shortcut_text("Ctrl+Shift+E")).clicked() {
                    cmd = Some(MenuCmd::ExportPng);
                    ui.close();
                }
                ui.separator();
                if ui.add(egui::Button::new("Print").shortcut_text("Ctrl+P")).clicked() {
                    cmd = Some(MenuCmd::Print);
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add(egui::Button::new("Undo").shortcut_text("Ctrl+Z")).clicked() {
                    cmd = Some(MenuCmd::Undo);
                    ui.close();
                }
                if ui.add(egui::Button::new("Redo").shortcut_text("Ctrl+Y")).clicked() {
                    cmd = Some(MenuCmd::Redo);
                    ui.close();
                }
            });
        });
        if let Some(path) = open_recent {
            self.open_path(path.into());
            self.refresh_recent();
        }
        if let Some(c) = cmd {
            self.run_menu_cmd(ctx, c);
        }
    }
}

/// Render GitHub release notes (headings, bullets, inline **bold** and `code`) inside the update dialog.
fn changelog_ui(ui: &mut egui::Ui, notes: &[crate::update::ReleaseNote], accent: Color32, body_sz: f32) {
    let ver_sz = body_sz + 2.0;
    let head_sz = body_sz + 1.0;
    for (idx, note) in notes.iter().enumerate() {
        if idx > 0 {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(10.0);
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("v{}", note.version)).size(ver_sz).strong().monospace().color(accent));
            if !note.date.is_empty() {
                ui.label(RichText::new(format!("·  {}", note.date)).size(body_sz - 1.0).weak());
            }
            let title = note.title.trim();
            if !title.is_empty() && !title.eq_ignore_ascii_case(&format!("v{}", note.version)) && !title.eq_ignore_ascii_case(&note.version) {
                ui.label(RichText::new(format!("—  {title}")).size(body_sz).strong());
            }
        });
        ui.add_space(6.0);
        if note.body.trim().is_empty() {
            ui.label(RichText::new("Bug fixes and improvements.").size(body_sz).weak());
            continue;
        }
        for raw_line in note.body.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                ui.add_space(6.0);
                continue;
            }
            if let Some(heading) = line.strip_prefix("### ").or_else(|| line.strip_prefix("## ")).or_else(|| line.strip_prefix("# ")) {
                ui.add_space(4.0);
                ui.label(RichText::new(heading.trim()).size(head_sz).strong());
                ui.add_space(2.0);
            } else if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                ui.horizontal_top(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("•").size(body_sz).weak());
                    ui.add(egui::Label::new(rich_markdown_line(ui, item.trim(), body_sz)).wrap());
                });
                ui.add_space(3.0);
            } else {
                ui.add(egui::Label::new(rich_markdown_line(ui, line, body_sz)).wrap());
                ui.add_space(3.0);
            }
        }
    }
}

/// Format inline `**bold**` and `` `code` `` spans in a single changelog line.
fn rich_markdown_line(ui: &egui::Ui, line: &str, body_sz: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let v = ui.visuals();
    let lh = Some((body_sz * 1.42).round());
    let normal = egui::TextFormat {
        font_id: FontId::proportional(body_sz),
        color: v.text_color(),
        line_height: lh,
        ..Default::default()
    };
    let bold = egui::TextFormat {
        font_id: FontId::new(body_sz, egui::FontFamily::Name("bold".into())),
        color: v.strong_text_color(),
        line_height: lh,
        ..Default::default()
    };
    let code = egui::TextFormat {
        font_id: FontId::monospace((body_sz - 1.0).max(11.0)),
        color: v.strong_text_color(),
        background: v.widgets.inactive.weak_bg_fill,
        line_height: lh,
        ..Default::default()
    };
    let mut rest = line;
    while !rest.is_empty() {
        let b_pos = rest.find("**");
        let c_pos = rest.find('`');
        let next = match (b_pos, c_pos) {
            (Some(bi), Some(ci)) if bi <= ci => Some((bi, true)),
            (Some(_), Some(ci)) => Some((ci, false)),
            (Some(bi), None) => Some((bi, true)),
            (None, Some(ci)) => Some((ci, false)),
            (None, None) => None,
        };
        match next {
            Some((bi, true)) => {
                if bi > 0 {
                    job.append(&rest[..bi], 0.0, normal.clone());
                }
                let after = &rest[bi + 2..];
                if let Some(end) = after.find("**") {
                    job.append(&after[..end], 0.0, bold.clone());
                    rest = &after[end + 2..];
                } else {
                    job.append(&rest[bi..], 0.0, normal.clone());
                    break;
                }
            }
            Some((ci, false)) => {
                if ci > 0 {
                    job.append(&rest[..ci], 0.0, normal.clone());
                }
                let after = &rest[ci + 1..];
                if let Some(end) = after.find('`') {
                    job.append(&format!(" {} ", &after[..end]), 0.0, code.clone());
                    rest = &after[end + 1..];
                } else {
                    job.append(&rest[ci..], 0.0, normal.clone());
                    break;
                }
            }
            None => {
                job.append(rest, 0.0, normal.clone());
                break;
            }
        }
    }
    job
}

/// Fixed lengths for lab containers, margins included. Tubes are labelled along their straight
/// wall: laminated tape lifts when wrapped around a 10.8 mm tube.
const LENGTH_PRESETS: [(&str, f32); 5] = [
    ("Microscope slide, across the frosted end", 25.0),
    ("1.5 ml tube, along the side", 25.0),
    ("2 ml tube, along the side", 28.0),
    ("15 ml Falcon, along the side", 50.0),
    ("50 ml Falcon, along the side", 60.0),
];

/// Two-column toolbar row: fixed-width centered uppercase label + full-height vertical divider
/// on the left, wrapping controls strictly to the right of the divider so wrapped buttons never
/// spill under the section label.
fn toolbar_section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    let label_w = 58.0;
    let sep_gap = 12.0;
    let resp = ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let (left_anchor, _) = ui.allocate_exact_size(egui::vec2(label_w + sep_gap, 32.0), egui::Sense::hover());
        let right = ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().interact_size.y = 32.0;
                add_contents(ui);
            });
        });
        (left_anchor.min.x, right.response.rect)
    });
    let (left_x, right_rect) = resp.inner;
    let y_min = right_rect.min.y;
    let y_max = right_rect.max.y.max(y_min + 32.0);
    let p = ui.painter();
    let label_rect = egui::Rect::from_min_max(egui::pos2(left_x, y_min), egui::pos2(left_x + label_w, y_max));
    p.text(
        egui::pos2(label_rect.center().x, label_rect.center().y + 0.5),
        egui::Align2::CENTER_CENTER,
        title,
        FontId::new(11.5, egui::FontFamily::Name("bold".into())),
        ui.visuals().weak_text_color(),
    );
    let sep_x = left_x + label_w + 3.0;
    let sep_col = ui.visuals().widgets.noninteractive.bg_stroke.color;
    p.line_segment(
        [egui::pos2(sep_x, y_min + 3.0), egui::pos2(sep_x, y_max - 3.0)],
        (1.0, sep_col),
    );
}

fn align_buttons(ui: &mut egui::Ui, a: &mut Align, prefix: &str) {
    let prev = ui.spacing().item_spacing.x;
    ui.spacing_mut().item_spacing.x = 3.0;
    if icons::toggle_icon(ui, *a == Align::Left, Icon::AlignLeft, &format!("{prefix} left")).clicked() {
        *a = Align::Left;
    }
    if icons::toggle_icon(ui, *a == Align::Center, Icon::AlignCenter, &format!("{prefix} center")).clicked() {
        *a = Align::Center;
    }
    if icons::toggle_icon(ui, *a == Align::Right, Icon::AlignRight, &format!("{prefix} right")).clicked() {
        *a = Align::Right;
    }
    ui.spacing_mut().item_spacing.x = prev;
}

/// Red zigzag under a misspelled word, like Word/Pages.
fn squiggle(p: &egui::Painter, x0: f32, x1: f32, y: f32) {
    let red = Color32::from_rgb(235, 64, 52);
    let mut pts = Vec::new();
    let mut x = x0;
    let mut up = true;
    while x < x1 {
        pts.push(egui::pos2(x, y + if up { -1.2 } else { 1.2 }));
        x += 2.0;
        up = !up;
    }
    pts.push(egui::pos2(x1, y));
    if pts.len() > 1 {
        p.add(egui::Shape::line(pts, egui::Stroke::new(1.1, red)));
    }
}

/// English spell check for a text box: squiggles + right-click suggestions.
fn spell_ui(
    ui: &egui::Ui,
    out: &egui::text_edit::TextEditOutput,
    text: &mut String,
    block: usize,
    speller: &mut Speller,
    target: &mut Option<(usize, Miss, Vec<String>)>,
) {
    use egui::text::CCursor;
    let misses = speller.check(text);
    let resp = &out.response.response;
    // Don't flag the word that is still being typed.
    let typing_at = if resp.has_focus() { out.cursor_range.map(|r| r.primary.index.0) } else { None };
    let painter = ui.painter().with_clip_rect(out.text_clip_rect);
    let off = out.galley_pos.to_vec2();
    for m in misses.iter() {
        if typing_at.is_some_and(|c| c == m.end) {
            continue;
        }
        let a = out.galley.pos_from_cursor(CCursor::new(m.start));
        let b = out.galley.pos_from_cursor(CCursor::new(m.end));
        if (a.min.y - b.min.y).abs() > 0.5 {
            continue; // wrapped across rows
        }
        squiggle(&painter, a.min.x + off.x, b.min.x + off.x, a.max.y + off.y - 2.0);
    }

    if resp.secondary_clicked() {
        *target = None;
        if let Some(p) = resp.interact_pointer_pos() {
            let c = out.galley.cursor_from_pos(p - out.galley_pos).index.0;
            if let Some(m) = misses.iter().find(|m| c >= m.start && c <= m.end) {
                *target = Some((block, m.clone(), speller.suggestions(text, m)));
            }
        }
    }
    let Some((bi, m, sugg)) = target.clone() else { return };
    if bi != block {
        return;
    }
    let mut done = false;
    resp.context_menu(|ui| {
        let still_there = text.chars().skip(m.start).take(m.end - m.start).eq(m.word.chars());
        if sugg.is_empty() {
            ui.weak("No suggestions");
        }
        for s in &sugg {
            if ui.button(RichText::new(s).strong()).clicked() {
                if still_there {
                    replace_chars(text, m.start, m.end, s);
                }
                done = true;
                ui.close();
            }
        }
        ui.separator();
        if ui.button(format!("Add “{}” to dictionary", m.word)).clicked() {
            speller.learn(&m.word);
            done = true;
            ui.close();
        }
    });
    if done {
        *target = None;
    }
}

#[allow(clippy::too_many_arguments)]
fn text_editor(
    ui: &mut egui::Ui,
    i: usize,
    t: &mut TextBlock,
    fonts: &[String],
    filter: &mut String,
    speller: &mut Speller,
    target: &mut Option<(usize, Miss, Vec<String>)>,
    headers: &[String],
) -> bool {
    let out = TextEdit::multiline(&mut t.text)
        .desired_rows(2)
        .desired_width(f32::INFINITY)
        .font(FontId::proportional(18.0))
        .hint_text("Text (Enter = new line)")
        .show(ui);
    let focused = out.response.response.has_focus();
    spell_ui(ui, &out, &mut t.text, i, speller, target);
    style_row(ui, i, t, fonts, filter, headers);
    focused
}

/// "+ Field" menu: appends `{Column}` placeholders from the spreadsheet (stays open so multiple
/// fields can be added in a row; click outside to close).
fn field_menu(ui: &mut egui::Ui, target: &mut String, headers: &[String], space: bool) {
    if headers.is_empty() {
        return;
    }
    egui::containers::menu::MenuButton::new("+ Field")
        .config(egui::containers::menu::MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside))
        .ui(ui, |ui| {
            ui.label(RichText::new("Click fields to insert").small().weak());
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for h in headers {
                    let ph = format!("{{{h}}}");
                    let used = target.contains(&ph);
                    let lbl = if used { format!("✓ {h}") } else { h.clone() };
                    if ui.selectable_label(used, lbl).clicked() {
                        if space && !target.is_empty() && !target.ends_with([' ', '\n']) {
                            target.push(' ');
                        }
                        target.push_str(&ph);
                    }
                }
            });
        })
        .0
        .on_hover_text("Insert one or more spreadsheet fields");
}

/// Font, size, B/I/U, alignment (shared by text and sequence blocks).
fn style_row(ui: &mut egui::Ui, i: usize, t: &mut TextBlock, fonts: &[String], filter: &mut String, headers: &[String]) {
    ui.horizontal_wrapped(|ui| {
        field_menu(ui, &mut t.text, headers, true);
        let shown = if t.font.is_empty() { "System font".to_string() } else { t.font.clone() };
        ComboBox::from_id_salt(("font", i))
            .width(190.0)
            .height(360.0)
            .selected_text(shown)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                ui.add(TextEdit::singleline(filter).hint_text("Search fonts…").desired_width(170.0))
                    .request_focus();
                let f = filter.to_lowercase();
                if f.is_empty() && ui.selectable_label(t.font.is_empty(), "System font").clicked() {
                    t.font.clear();
                    ui.close();
                }
                for fam in fonts.iter().filter(|x| f.is_empty() || x.to_lowercase().contains(&f)) {
                    if ui.selectable_label(&t.font == fam, fam).clicked() {
                        t.font = fam.clone();
                        filter.clear();
                        ui.close();
                    }
                }
            });
        let mut auto = t.size_pt.is_none();
        if ui.checkbox(&mut auto, "Auto size").on_hover_text("Fill the tape height").changed() {
            t.size_pt = if auto { None } else { Some(12.0) };
        }
        if let Some(pt) = &mut t.size_pt {
            ui.add(DragValue::new(pt).range(3.0..=200.0).speed(0.2).fixed_decimals(1).suffix(" pt"));
        }
        ui.separator();
        ui.toggle_value(&mut t.bold, RichText::new(" B ").strong());
        ui.toggle_value(&mut t.italic, RichText::new(" I ").italics());
        ui.toggle_value(&mut t.underline, RichText::new(" U ").underline());
        ui.separator();
        align_buttons(ui, &mut t.align, "Align text lines");
        let turn = ui.button(format!("↻ {}°", t.rotation.degrees()));
        if turn.on_hover_text("Rotate 90° clockwise; 90°/270° run the text across the tape").clicked() {
            t.rotation = t.rotation.next();
        }
        if t.text.contains('\n') {
            ui.separator();
            ui.label("Line spacing");
            ui.add(DragValue::new(&mut t.line_spacing).range(0.5..=3.0).speed(0.01).fixed_decimals(2));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn sequence_editor(
    ui: &mut egui::Ui,
    i: usize,
    q: &mut SequenceBlock,
    fonts: &[String],
    filter: &mut String,
    rows_drive: bool,
    by_columns: bool,
    in_text: bool,
    placeholder: &str,
) {
    let (unit, units) = if by_columns { ("column", "columns") } else { ("row", "rows") };
    ui.horizontal_wrapped(|ui| {
        ui.label("Prefix");
        ui.add(TextEdit::singleline(&mut q.prefix).desired_width(70.0).hint_text("A-"));
        ui.label("Start");
        ui.add(DragValue::new(&mut q.start).speed(0.2));
        ui.label("Step");
        ui.add(DragValue::new(&mut q.step).speed(0.1));
        ui.label("Digits");
        ui.add(DragValue::new(&mut q.digits).range(0..=12)).on_hover_text("Zero-pad the number: 3 digits → 007");
        ui.label("Suffix");
        ui.add(TextEdit::singleline(&mut q.suffix).desired_width(70.0));
        ui.add_enabled(!rows_drive, egui::Label::new("Count"));
        ui.add_enabled(!rows_drive, DragValue::new(&mut q.count).range(1..=99_999).speed(0.2))
            .on_disabled_hover_text(format!("The selected spreadsheet {units} set the count"));
    });
    let v = Vars::default();
    let txt = if rows_drive {
        format!("{}, {}, …  one number per selected {unit}", q.format(0, &v), q.format(1, &v))
    } else if q.count <= 1 {
        q.format(0, &v)
    } else {
        format!("{} → {}   ·   {} labels", q.format(0, &v), q.format(q.count as u64 - 1, &v), q.count)
    };
    if in_text {
        ui.weak(format!("{txt}   ·   shown via {placeholder}"));
    } else {
        ui.weak(txt);
        style_row(ui, i, &mut q.style, fonts, filter, &[]);
    }
}

/// Spreadsheet rows or columns: click toggles, ⇧-click selects a range; virtualised for big sheets.
/// Collapse multi-line cell text onto a single readable line for table display.
fn cell_one_line(s: &str) -> String {
    let mut out = String::new();
    for part in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

/// Spreadsheet rows or columns: click or drag-paint to select, ⇧-click for ranges, click column
/// headers to toggle fields, right-click a cell to select all matching rows or set header row.
#[allow(clippy::too_many_arguments)]
fn table_ui(
    ui: &mut egui::Ui,
    t: &Table,
    by_columns: bool,
    sel: &mut Vec<usize>,
    filter: &mut String,
    preview_row: &mut Option<usize>,
    last: &mut Option<usize>,
    drag: &mut Option<bool>,
    picked_fields: &[String],
    cfg: &UiConfig,
    table_max_h: f32,
    act: &mut TableAction,
) {
    use egui::{Align2, Rect, Sense, pos2, vec2};
    let (unit, units) = if by_columns { ("column", "columns") } else { ("row", "rows") };
    let f = filter.to_lowercase();
    let visible: Vec<usize> = (0..t.rows.len())
        .filter(|&i| f.is_empty() || t.rows[i].iter().any(|c| c.to_lowercase().contains(&f)))
        .collect();

    // Stop drag-painting as soon as the primary mouse button is released.
    if !ui.input(|i| i.pointer.primary_down()) {
        *drag = None;
    }

    // Determine the sequence of rows for the preview stepper (selected rows, else visible rows).
    let mut nav_rows: Vec<usize> = if sel.is_empty() { visible.clone() } else { sel.clone() };
    nav_rows.sort_unstable();
    nav_rows.dedup();
    let cur_nav_pos = preview_row
        .and_then(|r| nav_rows.iter().position(|&x| x == r))
        .unwrap_or(0);
    let active_preview_row = nav_rows.get(cur_nav_pos).copied();

    ui.horizontal_wrapped(|ui| {
        ui.add(TextEdit::singleline(filter).hint_text(format!("Filter {units}…")).desired_width(180.0));
        if !filter.is_empty() && ui.small_button("×").on_hover_text("Clear filter").clicked() {
            filter.clear();
        }
        if ui.button("All").on_hover_text(format!("Select all shown {units}")).clicked() {
            let have: std::collections::HashSet<usize> = sel.iter().copied().collect();
            sel.extend(visible.iter().filter(|i| !have.contains(i)));
        }
        if ui.add_enabled(!sel.is_empty(), egui::Button::new("None")).on_hover_text("Clear selection").clicked() {
            sel.clear();
        }
        let has_fields = !picked_fields.is_empty();
        let add_btn = if has_fields {
            egui::Button::new(RichText::new("Add to label").strong().color(Color32::WHITE)).fill(cfg.accent())
        } else {
            egui::Button::new("Add to label")
        };
        let add_tip = if has_fields {
            let tpl: String = picked_fields.iter().map(|h| format!("{{{h}}}")).collect::<Vec<_>>().join(" ");
            format!("Insert {tpl} into the label")
        } else {
            "Click column headers below to select columns, then click Add to label".to_string()
        };
        if ui
            .add_enabled(has_fields, add_btn)
            .on_hover_text(&add_tip)
            .on_disabled_hover_text(&add_tip)
            .clicked()
        {
            *act = TableAction::AddToLabel;
        }
        let sel_label = format!("{} / {} {units}", sel.len(), t.rows.len());
        if sel.is_empty() {
            ui.colored_label(Color32::from_rgb(220, 155, 25), sel_label);
        } else {
            ui.label(RichText::new(sel_label).strong());
        }
    });

    let font = FontId::proportional(cfg.table_font_size);
    let hdr_font = FontId::proportional((cfg.table_font_size - 0.5).max(10.0));
    // Measure each column from header + first 200 rows (using full single-line representation).
    let widths: Vec<f32> = (0..t.headers.len())
        .map(|c| {
            let mut max_w = ui
                .painter()
                .layout_no_wrap(format!("9 · {}", t.headers[c]), hdr_font.clone(), Color32::WHITE)
                .size()
                .x;
            for r in t.rows.iter().take(200) {
                let s = cell_one_line(&r[c]);
                let w = ui.painter().layout_no_wrap(s, font.clone(), Color32::WHITE).size().x;
                if w > max_w {
                    max_w = w;
                }
            }
            (max_w + cfg.table_cell_pad * 2.0 + 6.0).clamp(cfg.table_min_col_width, cfg.table_max_col_width)
        })
        .collect();
    let (chk_w, num_w, row_h, hdr_h) = (30.0, 42.0, cfg.table_row_height, cfg.table_header_height);
    let table_needs_vscroll = (visible.len() as f32 * row_h) > table_max_h;
    let scroll_w = if table_needs_vscroll { ui.spacing().scroll.allocated_width() } else { 0.0 };
    let total_w = (chk_w + num_w + widths.iter().sum::<f32>()).max((ui.available_width() - scroll_w).max(0.0));
    let (sel_bg, sel_fg, hov_bg, stripe, fg, weak, strong, grid_col) = {
        let v = ui.visuals();
        (
            v.selection.bg_fill,
            v.selection.stroke.color,
            v.widgets.hovered.weak_bg_fill,
            v.faint_bg_color,
            v.text_color(),
            v.weak_text_color(),
            v.strong_text_color(),
            v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.45),
        )
    };
    let accent = cfg.accent();
    let accent_tint = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 26);
    let merged_bar_col = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 110);

    // Precompute visible selection summary for the header tri-state checkbox.
    let sel_set: std::collections::HashSet<usize> = sel.iter().copied().collect();
    let vis_selected = visible.iter().filter(|ri| sel_set.contains(ri)).count();
    let all_vis = !visible.is_empty() && vis_selected == visible.len();
    let some_vis = vis_selected > 0 && vis_selected < visible.len();

    let mut group_op: Option<(usize, String, GroupOp)> = None;

    egui::ScrollArea::horizontal().id_salt("data_h").show(ui, |ui| {
        // Header row: tri-state select-all checkbox + clickable column headers.
        let (hrect, hresp) = ui.allocate_exact_size(vec2(total_w, hdr_h), Sense::click());
        let hp = ui.painter();
        hp.rect_filled(hrect, 3.0, stripe);

        // Tri-state checkbox in header
        let cb = Rect::from_center_size(pos2(hrect.min.x + chk_w / 2.0, hrect.center().y), Vec2::splat(14.0));
        if all_vis {
            hp.rect_filled(cb, 3.0, accent);
            let c = cb.center();
            hp.add(egui::Shape::line(
                vec![c + vec2(-3.5, 0.2), c + vec2(-1.0, 3.0), c + vec2(4.0, -3.0)],
                egui::Stroke::new(1.8, Color32::WHITE),
            ));
        } else if some_vis {
            hp.rect_filled(cb, 3.0, accent);
            let c = cb.center();
            hp.line_segment([c + vec2(-3.5, 0.0), c + vec2(3.5, 0.0)], egui::Stroke::new(2.0, Color32::WHITE));
        } else {
            hp.rect_stroke(cb, 3.0, (1.2, weak), egui::StrokeKind::Inside);
        }

        hp.text(
            pos2(hrect.min.x + chk_w + num_w - 8.0, hrect.center().y),
            Align2::RIGHT_CENTER,
            if by_columns { "Col" } else { "#" },
            FontId::proportional(12.0),
            weak,
        );

        let ptr = ui.input(|i| i.pointer.hover_pos());
        let mut x = hrect.min.x + chk_w + num_w;
        for (c, h) in t.headers.iter().enumerate() {
            let cell = Rect::from_min_size(pos2(x, hrect.min.y), vec2(widths[c], hdr_h));
            let picked_pos = picked_fields.iter().position(|p| p == h);
            let hovered_col = hresp.hovered() && ptr.is_some_and(|p| cell.contains(p));
            if picked_pos.is_some() {
                hp.rect_filled(cell, 2.0, accent_tint);
                hp.line_segment(
                    [pos2(cell.min.x + 2.0, cell.max.y - 1.5), pos2(cell.max.x - 2.0, cell.max.y - 1.5)],
                    egui::Stroke::new(2.5, accent),
                );
            } else if hovered_col {
                hp.rect_filled(cell, 2.0, hov_bg);
            }
            if cfg.table_grid_lines {
                hp.line_segment([pos2(x, hrect.min.y + 3.0), pos2(x, hrect.max.y - 3.0)], (1.0, grid_col));
            }
            let title = match picked_pos {
                Some(k) => format!("{} · {h}", k + 1),
                None => h.clone(),
            };
            let col = if picked_pos.is_some() { accent } else { strong };
            hp.with_clip_rect(cell.shrink2(vec2(4.0, 0.0))).text(
                pos2(x + cfg.table_cell_pad, hrect.center().y),
                Align2::LEFT_CENTER,
                title,
                hdr_font.clone(),
                col,
            );
            x += widths[c];
        }
        hp.line_segment([pos2(hrect.min.x, hrect.max.y), pos2(hrect.max.x, hrect.max.y)], (1.0, grid_col));

        if hresp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if hresp.clicked() && let Some(pos) = hresp.interact_pointer_pos() {
            if pos.x < hrect.min.x + chk_w + num_w {
                // Toggle all visible rows
                if all_vis {
                    let vis_set: std::collections::HashSet<usize> = visible.iter().copied().collect();
                    sel.retain(|r| !vis_set.contains(r));
                } else {
                    let have: std::collections::HashSet<usize> = sel.iter().copied().collect();
                    sel.extend(visible.iter().filter(|i| !have.contains(i)));
                }
            } else {
                // Toggle the clicked column in the multi-field builder
                let mut cx = hrect.min.x + chk_w + num_w;
                for (c, h) in t.headers.iter().enumerate() {
                    if pos.x >= cx && pos.x < cx + widths[c] {
                        *act = TableAction::ToggleHeader(h.clone());
                        break;
                    }
                    cx += widths[c];
                }
            }
        }
        hresp.on_hover_text("Click checkbox to select all shown · Click any column header to pick {Column} and select its non-empty rows");

        // Virtualised table body with drag-paint selection & right-click group selection.
        // Disable rubber-banding on the nested vertical table so trackpad scrolling isn't
        // trapped in overscroll at the bottom/top edge; NestedScrollGate adds a small detent
        // before continuing smoothly into the outer editor ScrollArea.
        let mut hover_tip: Option<String> = None;
        let prev_scroll_y = ui.input(|i| i.smooth_scroll_delta().y);
        let scroll_src = ui.input(|i| i.scroll_source());
        let now = ui.input(|i| i.time);
        let prev_rubber = ui.spacing().scroll.kinetic.rubber_band;
        ui.spacing_mut().scroll.kinetic.rubber_band = false;
        let table_scroll = egui::ScrollArea::vertical()
            .id_salt("data_v")
            .max_height(table_max_h)
            .auto_shrink([false, true])
            .show_rows(ui, row_h, visible.len(), |ui, range| {
                let primary_down = ui.input(|i| i.pointer.primary_down());
                let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
                let shift = ui.input(|i| i.modifiers.shift);
                let pointer_pos = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));

                for vi in range {
                    let ri = visible[vi];
                    let (rect, resp) = ui.allocate_exact_size(vec2(total_w, row_h), Sense::click());
                    let on = sel.contains(&ri);
                    let is_pv = active_preview_row == Some(ri);
                    let p = ui.painter();
                    if on {
                        p.rect_filled(rect, 2.0, sel_bg);
                    } else if resp.hovered() {
                        p.rect_filled(rect, 2.0, hov_bg);
                    } else if cfg.table_zebra && vi % 2 == 1 {
                        p.rect_filled(rect, 0.0, stripe);
                    }
                    // Subtle left bar marks the row currently shown in the live label preview.
                    if is_pv {
                        p.rect_filled(
                            Rect::from_min_size(rect.min, vec2(3.0, row_h)),
                            1.0,
                            accent,
                        );
                    }
                    let b = Rect::from_center_size(pos2(rect.min.x + chk_w / 2.0 + 1.0, rect.center().y), Vec2::splat(14.0));
                    if on {
                        p.rect_filled(b, 3.0, accent);
                        let c = b.center();
                        p.add(egui::Shape::line(
                            vec![c + vec2(-3.5, 0.2), c + vec2(-1.0, 3.0), c + vec2(4.0, -3.0)],
                            egui::Stroke::new(1.8, Color32::WHITE),
                        ));
                    } else {
                        p.rect_stroke(b, 3.0, (1.2, weak), egui::StrokeKind::Inside);
                    }
                    p.text(
                        pos2(rect.min.x + chk_w + num_w - 8.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        &t.row_labels[ri],
                        FontId::proportional(12.0),
                        if is_pv { accent } else { weak },
                    );

                    // Paint cells with single-line folding, grid dividers, and merged-cell continuation cue.
                    let base_col = if on { sel_fg } else { fg };
                    let merged_col = base_col.gamma_multiply(0.65);
                    let mut x = rect.min.x + chk_w + num_w;
                    let mut hovered_col_idx: Option<usize> = None;
                    for (c, v) in t.rows[ri].iter().enumerate() {
                        let cell = Rect::from_min_size(pos2(x, rect.min.y), vec2(widths[c], row_h));
                        if cfg.table_grid_lines {
                            p.line_segment([pos2(x, rect.min.y), pos2(x, rect.max.y)], (1.0, grid_col));
                        }
                        let inherited = cfg.table_show_merged_tint && t.is_merged(ri, c);
                        if inherited {
                            p.line_segment(
                                [pos2(x + 3.0, rect.min.y + 3.0), pos2(x + 3.0, rect.max.y - 3.0)],
                                (2.0, merged_bar_col),
                            );
                        }
                        let text = cell_one_line(v);
                        let col = if inherited { merged_col } else { base_col };
                        let pad = cfg.table_cell_pad + if inherited { 3.0 } else { 0.0 };
                        let galley = p.layout_no_wrap(text.clone(), font.clone(), col);
                        let clipped = galley.size().x > (widths[c] - pad - 6.0);
                        p.with_clip_rect(cell.shrink2(vec2(4.0, 0.0)))
                            .galley(pos2(x + pad, rect.center().y - galley.size().y / 2.0), galley, col);
                        if resp.hovered() && pointer_pos.is_some_and(|pt| cell.contains(pt)) {
                            hovered_col_idx = Some(c);
                            if (clipped || v.contains('\n')) && !v.trim().is_empty() {
                                hover_tip = Some(v.clone());
                            }
                        }
                        x += widths[c];
                    }

                    // Pointer press starts click/shift-range OR drag-painting!
                    if primary_pressed && pointer_pos.is_some_and(|pt| rect.contains(pt)) {
                        if shift {
                            click_row(sel, &visible, last, vi, true);
                        } else {
                            let target_state = !on;
                            if target_state {
                                if !sel.contains(&ri) {
                                    sel.push(ri);
                                }
                            } else {
                                sel.retain(|&r| r != ri);
                            }
                            *drag = Some(target_state);
                            *last = Some(ri);
                        }
                        *preview_row = Some(ri);
                    } else if primary_down && let Some(target_state) = *drag {
                        if pointer_pos.is_some_and(|pt| rect.contains(pt)) && *last != Some(ri) {
                            // Paint the entire visible span from `last` to `vi` so fast drags never skip rows.
                            let a = last.and_then(|r| visible.iter().position(|&x| x == r)).unwrap_or(vi);
                            for &r in &visible[a.min(vi)..=a.max(vi)] {
                                if target_state {
                                    if !sel.contains(&r) {
                                        sel.push(r);
                                    }
                                } else {
                                    sel.retain(|&x| x != r);
                                }
                            }
                            *last = Some(ri);
                            *preview_row = Some(ri);
                        }
                    }

                    // Remember which column was right-clicked for the context menu.
                    let col_id = ui.scope_id().with(("ctx_col", ri));
                    if resp.secondary_clicked() {
                        if let Some(c) = hovered_col_idx {
                            ui.data_mut(|d| d.insert_temp(col_id, c));
                        }
                    }
                    let ctx_col: Option<usize> = ui.data(|d| d.get_temp(col_id));
                    resp.context_menu(|ui| {
                        if let Some(c) = ctx_col.filter(|&c| c < t.headers.len()) {
                            let val = &t.rows[ri][c];
                            if !val.trim().is_empty() {
                                let hdr = &t.headers[c];
                                let short = cell_one_line(val);
                                let short_cut = if short.chars().count() > 26 {
                                    format!("{}…", short.chars().take(26).collect::<String>())
                                } else {
                                    short
                                };
                                let count = visible.iter().filter(|&&r| t.rows[r][c] == *val).count();
                                if ui.button(format!("Select all “{hdr} = {short_cut}” ({count})")).clicked() {
                                    group_op = Some((c, val.clone(), GroupOp::Add));
                                    ui.close();
                                }
                                if ui.button(format!("Only select “{hdr} = {short_cut}” ({count})")).clicked() {
                                    group_op = Some((c, val.clone(), GroupOp::Only));
                                    ui.close();
                                }
                                if ui.button(format!("Deselect “{hdr} = {short_cut}”")).clicked() {
                                    group_op = Some((c, val.clone(), GroupOp::Remove));
                                    ui.close();
                                }
                                ui.separator();
                            }
                        }
                        if ui.button(format!("Preview this {unit}")).clicked() {
                            *preview_row = Some(ri);
                            ui.close();
                        }
                        if !by_columns {
                            if let Ok(sheet_row) = t.row_labels[ri].parse::<usize>() {
                                if ui.button(format!("Use Row {sheet_row} as header row")).clicked() {
                                    *act = TableAction::UseHeaderRow(sheet_row);
                                    ui.close();
                                }
                            }
                        }
                    });
                }
            });
        ui.spacing_mut().scroll.kinetic.rubber_band = prev_rubber;
        let mut table_rect = table_scroll.inner_rect;
        table_rect.max.x += 20.0;
        let pointer_over_table = ui.rect_contains_pointer(table_rect);
        let inner_scrollable = table_scroll.content_size.y > table_scroll.inner_rect.height() + 0.5;
        let rem_scroll_y = ui.input(|i| i.smooth_scroll_delta().y);
        let gate_id = table_scroll.id.with("edge_gate");
        let mut gate: NestedScrollGate = ui.data(|d| d.get_temp(gate_id)).unwrap_or_default();
        let filtered_y = gate.filter(
            prev_scroll_y,
            rem_scroll_y,
            scroll_src,
            now,
            pointer_over_table,
            inner_scrollable,
        );
        ui.data_mut(|d| d.insert_temp(gate_id, gate));
        if filtered_y != rem_scroll_y {
            ui.ctx().input_mut(|i| i.smooth_scroll_delta.y = filtered_y);
        }
        if let Some(tip) = hover_tip {
            egui::Tooltip::always_open(
                ui.ctx().clone(),
                ui.layer_id(),
                egui::Id::unique("cell_tip"),
                egui::PopupAnchor::Pointer,
            )
            .show(|ui| {
                ui.set_max_width(360.0);
                ui.label(tip);
            });
        }
    });

    if let Some((col, val, op)) = group_op {
        let matching: Vec<usize> = visible.iter().copied().filter(|&r| t.rows[r][col] == val).collect();
        match op {
            GroupOp::Add => {
                for r in &matching {
                    if !sel.contains(r) {
                        sel.push(*r);
                    }
                }
            }
            GroupOp::Only => {
                *sel = matching.clone();
            }
            GroupOp::Remove => {
                let drop_set: std::collections::HashSet<usize> = matching.iter().copied().collect();
                sel.retain(|r| !drop_set.contains(r));
            }
        }
        if let Some(&first) = matching.first() {
            *preview_row = Some(first);
        }
    }

    #[cfg(target_os = "macos")]
    ui.label(
        RichText::new(format!(
            "Tip: drag across {units} to paint selection · ⇧-click for range · right-click any cell to select a group"
        ))
        .small()
        .weak(),
    );
    #[cfg(not(target_os = "macos"))]
    ui.label(
        RichText::new(format!(
            "Tip: drag across {units} to paint selection · Shift-click for range · right-click any cell to select a group"
        ))
        .small()
        .weak(),
    );
}

#[derive(Clone, Copy)]
enum GroupOp {
    Add,
    Only,
    Remove,
}

/// Row indices (among rows matching `filter`) that have non-empty text in at least one of `fields`.
fn non_empty_rows_for_fields(t: &Table, fields: &[String], filter: &str) -> Vec<usize> {
    let cols: Vec<usize> = fields
        .iter()
        .filter_map(|name| t.headers.iter().position(|h| h == name))
        .collect();
    if cols.is_empty() {
        return Vec::new();
    }
    let f = filter.to_lowercase();
    (0..t.rows.len())
        .filter(|&ri| {
            (f.is_empty() || t.rows[ri].iter().any(|c| c.to_lowercase().contains(&f)))
                && cols.iter().any(|&c| t.rows[ri].get(c).is_some_and(|v| !v.trim().is_empty()))
        })
        .collect()
}

/// Row click: toggle, or ⇧ adds the range from the last clicked row. `last` is a sheet row,
/// looked up in `visible` so the range stays right when the search filter changed in between.
fn click_row(sel: &mut Vec<usize>, visible: &[usize], last: &mut Option<usize>, vi: usize, shift: bool) {
    let ri = visible[vi];
    match (last.and_then(|a| visible.iter().position(|&r| r == a)), shift) {
        (Some(a), true) => {
            for &r in &visible[a.min(vi)..=a.max(vi)] {
                if !sel.contains(&r) {
                    sel.push(r);
                }
            }
        }
        _ if sel.contains(&ri) => sel.retain(|&r| r != ri),
        _ => sel.push(ri),
    }
    *last = Some(ri);
}

/// Selection after the sheet was re-read: a row that moved (rows inserted, deleted, sorted)
/// is found again by its cells, nearest first; a row edited in place keeps its index.
fn remap_rows(old: &[Vec<String>], new: &[Vec<String>], sel: &[usize]) -> Vec<usize> {
    let mut by_cells: Option<std::collections::HashMap<&[String], Vec<usize>>> = None;
    let mut taken = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(sel.len());
    for &i in sel {
        let j = match old.get(i) {
            Some(row) if new.get(i) != Some(row) || taken.contains(&i) => {
                let map = by_cells.get_or_insert_with(|| {
                    let mut m = std::collections::HashMap::<&[String], Vec<usize>>::new();
                    for (j, r) in new.iter().enumerate() {
                        m.entry(r.as_slice()).or_default().push(j);
                    }
                    m
                });
                map.get(row.as_slice())
                    .and_then(|js| js.iter().copied().filter(|j| !taken.contains(j)).min_by_key(|j| j.abs_diff(i)))
                    .unwrap_or(i)
            }
            _ => i,
        };
        if taken.insert(j) {
            out.push(j);
        }
    }
    out
}

/// The first `max_w` columns of a `w`×`h` RGBA image.
fn crop_width(rgba: Vec<u8>, w: usize, h: usize, max_w: usize) -> (usize, Vec<u8>) {
    if w <= max_w {
        return (w, rgba);
    }
    let mut out = Vec::with_capacity(max_w * h * 4);
    for row in rgba.chunks_exact(w * 4) {
        out.extend_from_slice(&row[..max_w * 4]);
    }
    (max_w, out)
}

fn block_icon(b: &Block) -> Icon {
    match b {
        Block::Text(_) => Icon::Text,
        Block::Sequence(_) => Icon::Sequence,
        Block::Qr(_) => Icon::Qr,
        Block::DataMatrix(_) => Icon::DataMatrix,
        Block::Barcode(_) => Icon::Barcode,
        Block::Image(_) => Icon::Image,
        Block::Spacer { .. } => Icon::Spacer,
    }
}

/// One-line summary shown in the header of a minimized block box.
fn block_summary(b: &Block) -> String {
    let cut = |s: &str| -> String {
        let s = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        if s.chars().count() > 40 {
            format!("{}…", s.chars().take(40).collect::<String>())
        } else {
            s.to_string()
        }
    };
    match b {
        Block::Text(t) => cut(&t.text),
        Block::Sequence(q) => {
            let v = Vars::default();
            if q.count <= 1 {
                q.format(0, &v)
            } else {
                format!("{} → {} ({})", q.format(0, &v), q.format(q.count as u64 - 1, &v), q.count)
            }
        }
        Block::Qr(q) => cut(&q.data),
        Block::DataMatrix(d) => cut(&d.data),
        Block::Barcode(bc) => {
            let d = cut(&bc.data);
            if d.is_empty() { bc.kind.name().into() } else { format!("{}: {d}", bc.kind.name()) }
        }
        Block::Image(im) => std::path::Path::new(&im.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "no image".into()),
        Block::Spacer { mm } => format!("{mm:.1} mm"),
    }
}

fn dm_editor(ui: &mut egui::Ui, _i: usize, d: &mut DataMatrixBlock, headers: &[String]) {
    ui.horizontal(|ui| {
        field_menu(ui, &mut d.data, headers, false);
        ui.add(TextEdit::singleline(&mut d.data).desired_width(f32::INFINITY).hint_text("Text, serial, URL"));
    });
    ui.horizontal(|ui| {
        ui.label("Shape");
        ui.selectable_value(&mut d.shape, DmShape::Auto, "Auto")
            .on_hover_text("Square when it prints large enough, rectangle on narrow tape");
        ui.selectable_value(&mut d.shape, DmShape::Square, "Square");
        ui.selectable_value(&mut d.shape, DmShape::Rectangle, "Rectangle")
            .on_hover_text("8–16 modules high: bigger modules on narrow tape");
    });
}

fn qr_editor(ui: &mut egui::Ui, i: usize, q: &mut QrBlock, headers: &[String]) {
    ui.horizontal(|ui| {
        field_menu(ui, &mut q.data, headers, false);
        ui.add(TextEdit::singleline(&mut q.data).desired_width(f32::INFINITY).hint_text("URL or text"));
    });
    ui.horizontal(|ui| {
        ui.label("Error correction");
        ComboBox::from_id_salt(("ec", i))
            .selected_text(format!("{:?}", q.ec))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut q.ec, Ec::L, "L  7% (densest data)");
                ui.selectable_value(&mut q.ec, Ec::M, "M 15%");
                ui.selectable_value(&mut q.ec, Ec::Q, "Q 25%");
                ui.selectable_value(&mut q.ec, Ec::H, "H 30%");
            });
    });
}

fn barcode_editor(ui: &mut egui::Ui, i: usize, b: &mut BarcodeBlock, headers: &[String]) {
    ui.horizontal(|ui| {
        ComboBox::from_id_salt(("bk", i)).selected_text(b.kind.name()).show_ui(ui, |ui| {
            for k in BarKind::ALL {
                ui.selectable_value(&mut b.kind, k, k.name());
            }
        });
        field_menu(ui, &mut b.data, headers, false);
        ui.add(TextEdit::singleline(&mut b.data).desired_width(f32::INFINITY).hint_text(match b.kind {
            BarKind::Ean13 => "12 or 13 digits",
            BarKind::Ean8 => "7 or 8 digits",
            BarKind::UpcA => "12 digits",
            _ => "data",
        }));
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut b.show_text, "Show text");
        ui.label("Bar width");
        ui.add(DragValue::new(&mut b.module).range(1..=4).suffix(" dot"))
            .on_hover_text("1 dot = 0.14 mm. 2 scans reliably on most readers.");
    });
}

/// Returns true when "Choose…" was clicked.
fn image_editor(ui: &mut egui::Ui, im: &mut ImageBlock) -> bool {
    let mut pick = false;
    ui.horizontal(|ui| {
        if ui.button("Choose…").clicked() {
            pick = true;
        }
        let name = std::path::Path::new(&im.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "no image".into());
        ui.label(name).on_hover_text(&im.path);
    });
    ui.horizontal(|ui| {
        ui.label("Threshold");
        ui.add(egui::Slider::new(&mut im.threshold, 1..=254));
        ui.checkbox(&mut im.dither, "Dither");
        ui.checkbox(&mut im.invert, "Invert");
    });
    pick
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.dev.poll(&ctx);
        self.dev.apply(&ctx);
        self.pump();
        self.pump_dialogs();
        self.handle_dropped_files(&ctx);
        self.updates(&ctx);
        self.check_update(&ctx);
        if self.handle.is_none() && self.p.auto_connect && self.p.printer.is_none() {
            let first = self.discovered.lock().unwrap().first().cloned();
            if let Some(id) = first {
                self.connect(id);
            }
        }
        self.track_edits(&ctx);
        self.shortcuts(&ctx);
        self.watch_data(&ctx);
        self.update_preview(&ctx);

        #[cfg(not(target_os = "macos"))]
        egui::Panel::top("menu").show(ui, |ui| {
            self.window_menu_bar(&ctx, ui);
        });
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::right("print").resizable(false).exact_size(self.dev.cfg.right_panel_width).show(ui, |ui| {
            egui::Panel::bottom("print_footer")
                .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 6, bottom: 4 }))
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.separator();
                    ui.add_space(4.0);
                    self.placeholders_ui(ui);
                    ui.add_space(8.0);
                    self.status_right(ui);
                });
            egui::ScrollArea::vertical().show(ui, |ui| self.print_panel(ui));
        });
        if let Some((msg_text, is_err, rem)) = self.active_msg().map(|(m, e, r)| (m.to_string(), e, r)) {
            let status_frame = egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(10, 4));
            egui::Panel::bottom("status").frame(status_frame).show(ui, |ui| {
                let c = if is_err { Color32::from_rgb(220, 60, 50) } else { ui.visuals().text_color() };
                ui.colored_label(c, RichText::new(msg_text).size(12.0));
                if !is_err {
                    ui.ctx().request_repaint_after(rem);
                }
            });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            self.preview_ui(ui);
            self.label_options(ui);
            ui.separator();
            self.add_block_bar(&ctx, ui);
            ui.separator();
            let editor_avail_h = ui.available_height();
            egui::ScrollArea::vertical()
                .id_salt("editor_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.data_panel(&ctx, ui, editor_avail_h);
                    self.blocks_ui(&ctx, ui);
                });
        });
        if self.dev.cfg.preview_update_modal {
            #[cfg(feature = "dev")]
            {
                if self.update.is_none() {
                    self.update = Some(crate::update::Release::dev_preview());
                }
                self.update_modal = true;
            }
        }
        if self.update_modal {
            self.update_dialog(&ctx);
        }
        self.dev.window(&ctx);
        self.update_title(&ctx);

        if self.screenshot_path.is_some() {
            ctx.set_theme(egui::Theme::Light);
            if let Ok(z) = std::env::var("PTLABEL_ZOOM").and_then(|s| s.parse::<f32>().map_err(|_| std::env::VarError::NotPresent)) {
                ctx.set_zoom_factor(z.clamp(0.5, 2.0));
            }
            if !self.screenshot_taken {
                ctx.request_repaint();
                if self.preview.is_some() && self.preview_job.is_none() {
                    self.screenshot_frames = self.screenshot_frames.saturating_add(1);
                    if self.screenshot_frames >= 6 {
                        self.screenshot_taken = true;
                        let path = self.screenshot_path.clone().unwrap();
                        let c = ctx.clone();
                        ctx.request_screenshot(move |img| {
                            let [w, h] = img.size;
                            if let Err(e) = image::save_buffer(
                                &path,
                                img.as_raw(),
                                w as u32,
                                h as u32,
                                image::ColorType::Rgba8,
                            ) {
                                eprintln!("ptlabel: screenshot save failed: {e}");
                            }
                            c.send_viewport_cmd(egui::ViewportCommand::Close);
                        });
                    }
                }
            }
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        raw_input.events.append(&mut self.forward);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY, &self.p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_renders_off_the_ui_thread() {
        let ctx = egui::Context::default();
        let renderer = Arc::new(Mutex::new(Renderer::new()));
        let tape = Tape::tze(24).unwrap();
        let opts = crate::proto::PageOpts::default();
        let (c, r, o) = (ctx.clone(), renderer.clone(), opts.clone());
        let pv = std::thread::spawn(move || render_preview(&c, &r, 7, &Label::default(), &tape, &Vars::default(), &o, 8192))
            .join()
            .unwrap();
        assert_eq!(pv.key, 7);
        assert!(pv.warnings.is_empty() && !pv.overflow);
        assert_eq!(pv.tex.size()[1], tape.tape_dots as usize * PREVIEW_SCALE);
        assert_eq!(pv.phys[1], tape.tape_dots as f32);
        // A small GPU texture limit drops the scale instead of exceeding it.
        let small = render_preview(&ctx, &renderer, 8, &Label::default(), &tape, &Vars::default(), &opts, 64);
        assert_eq!(small.tex.size()[1], tape.tape_dots as usize);
        assert_eq!(small.phys[1], pv.phys[1]);
    }

    fn rows(v: &[&str]) -> Vec<Vec<String>> {
        v.iter().map(|s| vec![s.to_string()]).collect()
    }

    #[test]
    fn shift_click_range_uses_sheet_rows_across_filter_changes() {
        let (mut sel, mut last) = (vec![], None);
        // Click sheet row 5 while the filter shows rows [1, 5, 9].
        click_row(&mut sel, &[1, 5, 9], &mut last, 1, false);
        assert_eq!((sel.clone(), last), (vec![5], Some(5)));
        // Filter cleared: ⇧-click row 7 selects 5..=7, not a range from visible index 1.
        let all: Vec<usize> = (0..10).collect();
        click_row(&mut sel, &all, &mut last, 7, true);
        assert_eq!(sel, vec![5, 6, 7]);
        // Anchor hidden by the filter: ⇧-click just toggles.
        click_row(&mut sel, &[2, 3], &mut last, 0, false);
        click_row(&mut sel, &[8, 9], &mut last, 1, true);
        assert_eq!(sel, vec![5, 6, 7, 2, 9]);
    }

    #[test]
    fn reload_keeps_selected_rows_when_rows_move() {
        let old = rows(&["A", "B", "C", "D"]);
        // Row inserted above (D fell off the end): A and C are found one row lower.
        let new = rows(&["X", "A", "B", "C"]);
        assert_eq!(remap_rows(&old, &new, &[0, 2]), vec![1, 3]);
        // Edited in place: keeps its index.
        let edited = rows(&["A", "B2", "C", "D"]);
        assert_eq!(remap_rows(&old, &edited, &[1, 3]), vec![1, 3]);
        // Identical rows map to distinct targets.
        let dup = rows(&["Q", "Q"]);
        assert_eq!(remap_rows(&dup, &rows(&["Z", "Q", "Q"]), &[0, 1]), vec![1, 2]);
    }

    #[test]
    fn align_buttons_toggle_on_click() {
        let ctx = egui::Context::default();
        let mut label_align = Align::Center;
        let mut text_align = Align::Center;
        let mut l_rects = [egui::Rect::NOTHING; 3];
        let mut t_rects = [egui::Rect::NOTHING; 3];
        // Frame 1: lay out BOTH align bars in the same frame and record each button's rect.
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let prev = ui.spacing().item_spacing.x;
                    ui.spacing_mut().item_spacing.x = 3.0;
                    l_rects[0] = icons::toggle_icon(ui, label_align == Align::Left, Icon::AlignLeft, "Align label left").rect;
                    l_rects[1] = icons::toggle_icon(ui, label_align == Align::Center, Icon::AlignCenter, "Align label center").rect;
                    l_rects[2] = icons::toggle_icon(ui, label_align == Align::Right, Icon::AlignRight, "Align label right").rect;
                    ui.spacing_mut().item_spacing.x = prev;
                });
                ui.horizontal_wrapped(|ui| {
                    let prev = ui.spacing().item_spacing.x;
                    ui.spacing_mut().item_spacing.x = 3.0;
                    t_rects[0] = icons::toggle_icon(ui, text_align == Align::Left, Icon::AlignLeft, "Align text lines left").rect;
                    t_rects[1] = icons::toggle_icon(ui, text_align == Align::Center, Icon::AlignCenter, "Align text lines center").rect;
                    t_rects[2] = icons::toggle_icon(ui, text_align == Align::Right, Icon::AlignRight, "Align text lines right").rect;
                    ui.spacing_mut().item_spacing.x = prev;
                });
            });
        });
        out.textures_delta.clear();
        let click = |pos: egui::Pos2, label_align: &mut Align, text_align: &mut Align| {
            for pressed in [true, false] {
                let mut raw = egui::RawInput::default();
                raw.events.push(egui::Event::PointerMoved(pos));
                raw.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
                let mut out = ctx.run_ui(raw, |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| align_buttons(ui, label_align, "Align label"));
                        ui.horizontal_wrapped(|ui| align_buttons(ui, text_align, "Align text lines"));
                    });
                });
                out.textures_delta.clear();
            }
        };
        for (target, rect) in [(Align::Left, l_rects[0]), (Align::Right, l_rects[2]), (Align::Center, l_rects[1])] {
            click(rect.center(), &mut label_align, &mut text_align);
            assert_eq!(label_align, target);
        }
        for (target, rect) in [(Align::Left, t_rects[0]), (Align::Right, t_rects[2]), (Align::Center, t_rects[1])] {
            click(rect.center(), &mut label_align, &mut text_align);
            assert_eq!(text_align, target);
        }
    }

    #[test]
    fn non_empty_rows_for_fields_skips_blank_rows_and_unions_columns() {
        let t = Table {
            sheet: "S".into(),
            sheets: vec!["S".into()],
            headers: Arc::new(vec!["Sample".into(), "Note".into()]),
            row_labels: vec!["2".into(), "3".into(), "4".into(), "5".into()],
            rows: vec![
                vec!["S1".into(), "".into()],
                vec!["  ".into(), "".into()],
                vec!["".into(), "ctrl".into()],
                vec!["S2".into(), "ok".into()],
            ],
            merged: vec![],
            merged_count: 0,
            header_index: Some(0),
            mtime: None,
        };
        assert_eq!(non_empty_rows_for_fields(&t, &["Sample".into()], ""), vec![0, 3]);
        assert_eq!(non_empty_rows_for_fields(&t, &["Note".into()], ""), vec![2, 3]);
        assert_eq!(non_empty_rows_for_fields(&t, &["Sample".into(), "Note".into()], ""), vec![0, 2, 3]);
        assert_eq!(non_empty_rows_for_fields(&t, &["Sample".into(), "Note".into()], "ctrl"), vec![2]);
        assert!(non_empty_rows_for_fields(&t, &[], "").is_empty());
    }

    #[test]
    fn crop_keeps_first_columns() {
        let px: Vec<u8> = (0..2 * 3 * 4).map(|v| v as u8).collect(); // 3 wide, 2 high
        let (w, out) = crop_width(px.clone(), 3, 2, 2);
        assert_eq!(w, 2);
        assert_eq!(out, [&px[0..8], &px[12..20]].concat());
        assert_eq!(crop_width(px.clone(), 3, 2, 8), (3, px));
    }

    #[test]
    fn nested_scroll_gate_breaks_through_on_trackpad_threshold() {
        use egui::MouseWheelSource;
        let mut g = NestedScrollGate::default();
        // Scroll inside table (consumed by inner ScrollArea).
        assert_eq!(g.filter(-20.0, 0.0, Some(MouseWheelSource::Trackpad), 1.00, true, true), 0.0);
        // Hit bottom edge: push below threshold (24 px) is absorbed, pushing past unlocks and passes excess.
        assert_eq!(g.filter(-16.0, -16.0, Some(MouseWheelSource::Trackpad), 1.02, true, true), 0.0);
        assert_eq!(g.filter(-16.0, -16.0, Some(MouseWheelSource::Trackpad), 1.04, true, true), -8.0);
        // Once unlocked, both Trackpad and Momentum pass straight through to the outer ScrollArea.
        assert_eq!(g.filter(-15.0, -15.0, Some(MouseWheelSource::Trackpad), 1.06, true, true), -15.0);
        assert_eq!(g.filter(-10.0, -10.0, Some(MouseWheelSource::Momentum), 1.08, true, true), -10.0);

        // Scrolling back inside resets the gate; momentum past the threshold also breaks through
        // smoothly in one continuous motion (no need to stop and scroll again).
        assert_eq!(g.filter(12.0, 0.0, Some(MouseWheelSource::Trackpad), 2.00, true, true), 0.0);
        assert_eq!(g.filter(-15.0, -15.0, Some(MouseWheelSource::Momentum), 2.02, true, true), 0.0);
        assert_eq!(g.filter(-15.0, -15.0, Some(MouseWheelSource::Momentum), 2.04, true, true), -6.0);
        assert_eq!(g.filter(-10.0, -10.0, Some(MouseWheelSource::Momentum), 2.06, true, true), -10.0);

        // Mouse wheel always passes straight through at the edge.
        assert_eq!(g.filter(-20.0, 0.0, Some(MouseWheelSource::Wheel), 3.00, true, true), 0.0);
        assert_eq!(g.filter(-20.0, -20.0, Some(MouseWheelSource::Wheel), 3.02, true, true), -20.0);
    }
}

/// Trackpad scroll-chaining gate for a nested vertical `ScrollArea` (the Excel table):
/// adds a small edge threshold (`TABLE_EDGE_SCROLL_THRESHOLD`), and once any trackpad motion
/// (active swipe or momentum) goes past it, continues smoothly into the outer editor ScrollArea.
const TABLE_EDGE_SCROLL_THRESHOLD: f32 = 24.0;
const TABLE_EDGE_PAUSE_SECS: f64 = 0.15;

#[derive(Clone, Copy, Debug)]
struct NestedScrollGate {
    last_event_time: f64,
    /// +1 = pushing bottom (scrolling down), -1 = pushing top (scrolling up), 0 = none.
    edge: i8,
    pushed: f32,
    unlocked: bool,
}

impl Default for NestedScrollGate {
    fn default() -> Self {
        Self {
            last_event_time: f64::NEG_INFINITY,
            edge: 0,
            pushed: 0.0,
            unlocked: false,
        }
    }
}

impl NestedScrollGate {
    fn filter(
        &mut self,
        prev_delta_y: f32,
        rem_delta_y: f32,
        source: Option<egui::MouseWheelSource>,
        now: f64,
        pointer_over_inner: bool,
        inner_scrollable: bool,
    ) -> f32 {
        // Inner scroll area consumed vertical scroll -> actively scrolling inside the table.
        if prev_delta_y != 0.0 && rem_delta_y == 0.0 {
            self.last_event_time = now;
            self.edge = 0;
            self.pushed = 0.0;
            self.unlocked = false;
            return 0.0;
        }
        if rem_delta_y == 0.0 || !pointer_over_inner || !inner_scrollable {
            return rem_delta_y;
        }

        let is_trackpad = matches!(
            source,
            Some(egui::MouseWheelSource::Trackpad | egui::MouseWheelSource::Momentum)
        );
        if !is_trackpad {
            self.edge = 0;
            self.pushed = 0.0;
            self.unlocked = false;
            return rem_delta_y;
        }

        let fresh_gesture = now - self.last_event_time > TABLE_EDGE_PAUSE_SECS;
        self.last_event_time = now;

        let dir: i8 = if rem_delta_y < 0.0 { 1 } else { -1 };

        // Starting a new trackpad swipe while already resting at the table edge passes straight through.
        if fresh_gesture {
            self.edge = dir;
            self.pushed = TABLE_EDGE_SCROLL_THRESHOLD;
            self.unlocked = true;
            return rem_delta_y;
        }

        if self.edge != dir {
            self.edge = dir;
            self.pushed = 0.0;
            self.unlocked = false;
        }

        if self.unlocked {
            return rem_delta_y;
        }

        self.pushed += rem_delta_y.abs();
        if self.pushed >= TABLE_EDGE_SCROLL_THRESHOLD {
            self.unlocked = true;
            let excess = self.pushed - TABLE_EDGE_SCROLL_THRESHOLD;
            return if rem_delta_y < 0.0 { -excess } else { excess };
        }
        0.0
    }
}
