//! egui front end: printer bar, live preview, block editor, print panel.

use crate::data::{self, DataSource, Record, Table};
use crate::job::{Pager, PrintJob, PrintSettings};
use crate::label::render::{PREVIEW_SCALE, Renderer, canvas_length_mm, preview_smooth};
use crate::icons::{self, Icon};
use crate::menu::MenuCmd;
use crate::label::{
    Align, BarKind, BarcodeBlock, Block, DataMatrixBlock, DmShape, Ec, ImageBlock, Label, QrBlock, SequenceBlock,
    TextBlock, Vars, expand,
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

/// Dock/window icon when running outside the .app bundle (the bundle uses PTLabel.icns).
fn window_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png")).expect("icon.png").to_rgba8();
    egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() }
}

pub fn run() -> anyhow::Result<()> {
    // PTLABEL_PROFILE=x runs with its own saved state ("PTLabel-x"), e.g. for testing.
    let name = match std::env::var("PTLABEL_PROFILE") {
        Ok(p) if !p.is_empty() => format!("PTLabel-{p}"),
        _ => "PTLabel".to_string(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(&name)
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([960.0, 600.0])
            .with_icon(window_icon()),
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
    /// Selected spreadsheet rows (indices into the loaded table).
    sel_rows: Vec<usize>,
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
            sel_rows: vec![],
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
    for b in &label.blocks {
        match b {
            Block::Text(t) => {
                if let Some(l) = expand(&t.text, v).lines().map(str::trim).find(|l| !l.is_empty()) {
                    return cut(l, 26);
                }
            }
            Block::Sequence(q) => return cut(&q.format(v.index, v), 26),
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
    msg: Option<(String, bool, Instant)>,
    preview: Option<Preview>,
    new_host: String,
    notify: Arc<dyn Fn() + Send + Sync>,
    speller: Speller,
    menu_rx: Option<crossbeam_channel::Receiver<MenuCmd>>,
    /// Right-clicked misspelling: (block index, word, suggestions).
    spell_target: Option<(usize, Miss, Vec<String>)>,
    /// Loaded spreadsheet for `p.label.data`.
    table: Option<Table>,
    table_err: Option<String>,
    table_filter: String,
    /// Row shown in the preview (last clicked).
    preview_row: Option<usize>,
    /// Last clicked visible row, for ⇧-click ranges.
    last_click: Option<usize>,
    data_checked: Instant,
}

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
            s.spacing.button_padding = egui::vec2(8.0, 4.0);
        });
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
            msg: None,
            preview: None,
            new_host: String::new(),
            notify,
            speller: Speller::new(),
            menu_rx: crate::menu::install(&cc.egui_ctx),
            spell_target: None,
            table: None,
            table_err: None,
            table_filter: String::new(),
            preview_row: None,
            last_click: None,
            data_checked: Instant::now(),
        };
        app.reload_table();
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
        self.p.printer = Some(id);
        self.p.auto_connect = true;
    }

    fn warn_dropped_jobs(&mut self) {
        let n = self.handle.as_ref().map_or(0, |h| h.queued().len());
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
                    let key = |t: &Tape| (t.width_code, t.kind, t.tape_color, t.text_color);
                    if t.as_ref().map(key) != self.live_tape.as_ref().map(key) {
                        match (&self.live_tape, &t) {
                            (_, Some(t)) => self.info(format!("Tape detected: {}", t.describe())),
                            (Some(_), None) => self.info("Tape removed".into()),
                            _ => {}
                        }
                    }
                    self.live_tape = t;
                    if let Some(m) = s.model_name() {
                        if let Some(p) = &mut self.p.printer {
                            if p.model != m {
                                p.model = m.into();
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
                Event::Done { id, printed } => {
                    let title = self.job.take().filter(|r| r.id == id).map(|r| r.title).unwrap_or_default();
                    self.stopping = false;
                    self.info(format!("Printed {} · {title}", count_text(Some(printed))));
                }
                Event::Failed { id, error } => {
                    let title = self.job.take().filter(|r| r.id == id).map(|r| r.title).unwrap_or_default();
                    self.stopping = false;
                    self.error(format!("Print failed ({title}): {error}"));
                }
            }
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
        let rows = if self.p.label.data.is_some() {
            let rows = self.selected_records();
            if rows.is_empty() {
                let why = if self.table.is_some() { "Select the spreadsheet rows to print" } else { "Spreadsheet not loaded" };
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

    // ---------------------------------------------------------------- spreadsheet

    fn reload_table(&mut self) {
        let Some(src) = &self.p.label.data else {
            self.table = None;
            self.table_err = None;
            return;
        };
        match data::load(src) {
            Ok(t) => {
                let n = t.rows.len();
                self.p.sel_rows.retain(|&r| r < n);
                if self.preview_row.is_some_and(|r| r >= n) {
                    self.preview_row = None;
                }
                self.table = Some(t);
                self.table_err = None;
            }
            Err(e) => {
                self.table = None;
                self.table_err = Some(format!("Can't read {}: {e:#}", src.file_name()));
            }
        }
    }

    fn pick_data_file(&mut self) -> bool {
        let Some(path) = rfd::FileDialog::new().add_filter("Spreadsheet", data::EXTENSIONS).pick_file() else {
            return false;
        };
        self.p.label.data = Some(DataSource { path: path.display().to_string(), ..Default::default() });
        self.p.sel_rows.clear();
        self.preview_row = None;
        self.last_click = None;
        self.table_filter.clear();
        self.reload_table();
        self.table.is_some()
    }

    /// "Excel" in the Add row: attach a spreadsheet (first time), then add a text
    /// block showing the next column that isn't on the label yet.
    fn add_excel(&mut self) {
        if self.p.label.data.is_none() && !self.pick_data_file() {
            return;
        }
        let Some(t) = &self.table else { return };
        let col = t.headers.iter().find(|h| !self.p.label.uses(h)).or(t.headers.first()).cloned();
        if let Some(c) = col {
            self.p.label.blocks.push(Block::Text(TextBlock { text: format!("{{{c}}}"), ..Default::default() }));
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

    fn data_panel(&mut self, ui: &mut egui::Ui) {
        let Some(src) = self.p.label.data.clone() else { return };
        enum DAct {
            Change,
            Reload,
            Remove,
            Sheet(String),
            Header(bool),
        }
        let mut act: Option<DAct> = None;
        egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                icons::show(ui, Icon::Sheet, 18.0);
                ui.label(RichText::new(src.file_name()).strong()).on_hover_text(&src.path);
                if let Some(t) = &self.table {
                    if t.sheets.len() > 1 {
                        ComboBox::from_id_salt("sheet").selected_text(&t.sheet).show_ui(ui, |ui| {
                            for sh in &t.sheets {
                                if ui.selectable_label(*sh == t.sheet, sh).clicked() {
                                    act = Some(DAct::Sheet(sh.clone()));
                                }
                            }
                        });
                    }
                }
                let mut hdr = src.header;
                if ui.checkbox(&mut hdr, "First row = column names").changed() {
                    act = Some(DAct::Header(hdr));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remove").on_hover_text("Stop using this spreadsheet").clicked() {
                        act = Some(DAct::Remove);
                    }
                    if ui.small_button("Change…").clicked() {
                        act = Some(DAct::Change);
                    }
                    if ui.small_button("Reload").on_hover_text("Re-read the file (automatic when it is saved)").clicked() {
                        act = Some(DAct::Reload);
                    }
                });
            });
            if let Some(e) = &self.table_err {
                ui.colored_label(Color32::from_rgb(220, 60, 50), e);
            }
            if let Some(t) = &self.table {
                table_ui(ui, t, &mut self.p.sel_rows, &mut self.table_filter, &mut self.preview_row, &mut self.last_click);
            }
        });
        ui.add_space(6.0);
        let reset = |app: &mut Self| {
            app.p.sel_rows.clear();
            app.preview_row = None;
            app.last_click = None;
        };
        match act {
            Some(DAct::Change) => {
                self.pick_data_file();
            }
            Some(DAct::Reload) => {
                self.reload_table();
                if self.table.is_some() {
                    self.info(format!("Reloaded {}", src.file_name()));
                }
            }
            Some(DAct::Remove) => {
                self.p.label.data = None;
                reset(self);
                self.reload_table();
            }
            Some(DAct::Sheet(s)) => {
                if let Some(d) = &mut self.p.label.data {
                    d.sheet = s;
                }
                reset(self);
                self.reload_table();
            }
            Some(DAct::Header(h)) => {
                if let Some(d) = &mut self.p.label.data {
                    d.header = h;
                }
                reset(self);
                self.reload_table();
            }
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

    fn save(&mut self, as_new: bool) {
        let path = match (&self.p.doc_path, as_new) {
            (Some(p), false) => Some(std::path::PathBuf::from(p)),
            _ => rfd::FileDialog::new()
                .add_filter("Label", &["ptlabel", "json"])
                .set_file_name("label.ptlabel")
                .save_file(),
        };
        let Some(path) = path else { return };
        match serde_json::to_string_pretty(&self.p.label)
            .map_err(anyhow::Error::from)
            .and_then(|s| std::fs::write(&path, s).map_err(Into::into))
        {
            Ok(()) => {
                self.info(format!("Saved {}", path.display()));
                self.p.doc_path = Some(path.display().to_string());
            }
            Err(e) => self.error(format!("Save failed: {e}")),
        }
    }

    fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("Label", &["ptlabel", "json"]).pick_file() else {
            return;
        };
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|s| serde_json::from_str::<Label>(&s).map_err(Into::into))
        {
            Ok(l) => {
                self.p.label = l;
                self.p.sel_rows.clear();
                self.preview_row = None;
                self.reload_table();
                self.p.doc_path = Some(path.display().to_string());
                self.info(format!("Opened {}", path.display()));
            }
            Err(e) => self.error(format!("Open failed: {e}")),
        }
    }

    // ---------------------------------------------------------------- preview

    fn update_preview(&mut self, ctx: &egui::Context) {
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
        let Ok(mut r) = self.renderer.try_lock() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        };
        // Smooth high-res render; drop the scale for very long labels so the texture fits.
        let max_side = ctx.input(|i| i.max_texture_side);
        let mut s = PREVIEW_SCALE;
        let out = loop {
            let o = r.render_canvas(&self.p.label, &tape, &vars, opts, s);
            let w = o.canvas.w + 2 * crate::proto::raster::margin_dots(opts.margin_mm, false) as usize * s;
            if w <= max_side || s == 1 {
                break o;
            }
            s /= 2;
        };
        drop(r);
        let (w, hh, rgba) = preview_smooth(&out.canvas, s, &tape, opts);
        let img = egui::ColorImage::from_rgba_unmultiplied([w, hh], &rgba);
        let len_mm = canvas_length_mm(&out.canvas, s, opts);
        let phys = [w as f32 / s as f32, hh as f32 / s as f32];
        let filter = TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
        match &mut self.preview {
            Some(p) => {
                p.tex.set(img, filter);
                p.key = key;
                p.phys = phys;
                p.len_mm = len_mm;
                p.overflow = out.overflow;
                p.warnings = out.warnings;
            }
            None => {
                self.preview = Some(Preview {
                    key,
                    tex: ctx.load_texture("preview", img, filter),
                    phys,
                    len_mm,
                    overflow: out.overflow,
                    warnings: out.warnings,
                });
            }
        }
    }

    // ---------------------------------------------------------------- UI parts

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            icons::logo(ui, 22.0);
            ui.label(RichText::new("PTLabel").strong().size(16.0));
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
                .width(330.0)
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
                                    && ui.small_button("Forget").on_hover_text("Remove this IP from the list").clicked()
                                {
                                    forget = Some(h.clone());
                                }
                            }
                        });
                    }
                    if list.is_empty() {
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
            if ui.small_button("↻").on_hover_text("Re-read printer status").clicked() {
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
        let (records, _) = Pager::counts(&self.p.label, &self.p.settings, n_sel);
        let multi = has_data || records != Some(1);
        let s = &mut self.p.settings;
        ui.add_space(6.0);
        ui.heading("Print");
        ui.add_space(4.0);

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
                0 => ui.colored_label(Color32::from_rgb(230, 170, 30), "Select spreadsheet rows to print"),
                n => ui.weak(format!("{n} row{}{times}", if n == 1 { "" } else { "s" })),
            };
        } else if let Some(q) = self.p.label.sequence() {
            let v = Vars::default();
            let span = match records {
                Some(1) => q.format(0, &v),
                Some(n) => format!("{} → {}", q.format(0, &v), q.format(n as u64 - 1, &v)),
                None => format!("{} → …", q.format(0, &v)),
            };
            ui.weak(format!("Sequence {span}{times}"));
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
            .on_hover_text("Don't feed and cut after the last label. Saves ~25 mm of tape; it's cut when the next job starts.");
        ui.checkbox(&mut o.special_tape, "Never cut (fabric / special tape)");

        ui.separator();
        ui.label(RichText::new("Output").strong());
        ui.checkbox(&mut o.high_res, "High resolution (180×360 dpi)");
        ui.checkbox(&mut o.mirror, "Mirror");
        ui.horizontal(|ui| {
            ui.label("Margins");
            ui.add(DragValue::new(&mut o.margin_mm).range(2.0..=127.0).speed(0.1).fixed_decimals(1).suffix(" mm"));
        });

        ui.add_space(10.0);
        let total = Pager::counts(&self.p.label, s, n_sel).1;
        let label = match total {
            Some(1) => "Print".to_string(),
            Some(n) => format!("Print {n} labels"),
            None => "Print until stopped".to_string(),
        };
        let queued = self.handle.as_ref().map(|h| h.queued()).unwrap_or_default();
        let busy = self.job.is_some() || !queued.is_empty();
        let label = if busy { format!("Add to queue · {}", count_text(total)) } else { label };
        let icon_id = egui::IdSalt::new("print-icon");
        let btn = egui::Button::new((egui::Atom::custom(icon_id, egui::Vec2::splat(22.0)), RichText::new(label).size(17.0).strong()))
            .min_size(egui::vec2(ui.available_width(), 42.0));
        let out = ui.add_enabled_ui(self.handle.is_some(), |ui| btn.atom_ui(ui)).inner;
        if let Some(r) = out.rect(icon_id) {
            let col = ui.style().interact(&out.response).text_color();
            icons::paint(ui.painter(), r, Icon::Printer, col);
        }
        if out.response.on_hover_text("⌘P · jobs queue up while printing").clicked() {
            self.print();
        }
        if self.handle.is_none() {
            ui.weak("Select a printer");
        } else if has_data && n_sel == 0 {
            ui.weak("Select rows in the spreadsheet table");
        } else if let Some(w) = self.waiting_reason() {
            ui.weak(format!("{w}. Jobs wait and print automatically."));
        }

        if let Some(r) = &self.job {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("Printing: {}", r.title)).strong());
            let frac = r.total.map_or(0.0, |n| r.done as f32 / n.max(1) as f32);
            let txt = match r.total {
                Some(n) => format!("{} / {n}", r.done),
                None => format!("{} printed", r.done),
            };
            ui.add(egui::ProgressBar::new(frac).text(txt).animate(r.total.is_none()));
            let stop = egui::Button::new(if self.stopping { "Stopping after this label…" } else { "Stop this job" })
                .min_size(egui::vec2(ui.available_width(), 28.0));
            if ui.add_enabled(!self.stopping, stop).clicked() {
                if let Some(h) = &self.handle {
                    h.stop();
                    self.stopping = true;
                }
            }
        }

        if !queued.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("Queue ({})", queued.len())).strong());
            let mut cancel = None;
            for (k, (id, title, total)) in queued.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{}. {title}", k + 1));
                    ui.weak(count_text(*total));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Cancel").clicked() {
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
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{:.1} mm", pv.len_mm)).strong().size(15.0));
            ui.label(format!("on {}", tape.describe()));
            ui.weak(format!("· print area {:.1} mm", tape.print_pins as f32 * 25.4 / 180.0));
            if self.live_tape.is_none() {
                ui.weak("· design only");
            }
        });
        for w in &pv.warnings {
            ui.colored_label(Color32::from_rgb(220, 60, 50), w);
        }
        if pv.overflow {
            ui.colored_label(Color32::from_rgb(230, 150, 20), "Content is longer than the fixed length; the label will be longer.");
        }
        let [w, h] = pv.phys;
        let avail = ui.available_width() - 24.0;
        let scale = (avail / w).min(230.0 / h).clamp(0.35, 3.0);
        let size = Vec2::new(w, h) * scale;
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(6.0)
            .inner_margin(12.0)
            .show(ui, |ui| {
                egui::ScrollArea::horizontal().id_salt("pv").show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        let img = egui::Image::from_texture((pv.tex.id(), size)).corner_radius(2.0);
                        ui.add(img).on_hover_text("Edges are the cut lines");
                    });
                });
            });
    }

    fn label_options(&mut self, ui: &mut egui::Ui) {
        let l = &mut self.p.label;
        ui.horizontal_wrapped(|ui| {
            ui.label("Length");
            ui.selectable_value(&mut l.fixed_length, false, "Auto");
            ui.selectable_value(&mut l.fixed_length, true, "Fixed");
            if l.fixed_length {
                ui.add(DragValue::new(&mut l.length_mm).range(5.0..=1000.0).speed(0.2).fixed_decimals(1).suffix(" mm"));
                align_buttons(ui, &mut l.align);
            }
            ui.separator();
            ui.label("Gap");
            ui.add(DragValue::new(&mut l.gap_mm).range(0.0..=50.0).speed(0.1).fixed_decimals(1).suffix(" mm"));
            ui.separator();
            ui.checkbox(&mut l.frame, "Frame");
            ui.checkbox(&mut l.invert, "Invert");
        });
    }

    fn blocks_ui(&mut self, ui: &mut egui::Ui) {
        enum Act {
            Up,
            Down,
            Del,
            Dup,
        }
        let mut act: Option<(usize, Act)> = None;
        let n = self.p.label.blocks.len();
        let fonts = &self.fonts;
        let filter = &mut self.font_filter;
        let speller = &mut self.speller;
        let target = &mut self.spell_target;
        let headers: Vec<String> = self.table.as_ref().map(|t| t.headers.to_vec()).unwrap_or_default();
        let rows_drive = self.p.label.data.is_some();
        let mut picked_image: Option<usize> = None;
        for i in 0..n {
            let b = &mut self.p.label.blocks[i];
            egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    icons::show(ui, block_icon(b), 18.0);
                    ui.label(RichText::new(b.kind_name()).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").clicked() {
                            act = Some((i, Act::Del));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("↓").small()).clicked() {
                            act = Some((i, Act::Down));
                        }
                        if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                            act = Some((i, Act::Up));
                        }
                        if ui.small_button("Duplicate").clicked() {
                            act = Some((i, Act::Dup));
                        }
                    });
                });
                match b {
                    Block::Text(t) => text_editor(ui, i, t, fonts, filter, speller, target, &headers),
                    Block::Sequence(q) => sequence_editor(ui, i, q, fonts, filter, rows_drive),
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
            });
        }
        if let Some(i) = picked_image {
            if let Some(path) = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg"]).pick_file() {
                if let Block::Image(im) = &mut self.p.label.blocks[i] {
                    im.path = path.display().to_string();
                }
            }
        }
        let blocks = &mut self.p.label.blocks;
        match act {
            Some((i, Act::Up)) => blocks.swap(i, i - 1),
            Some((i, Act::Down)) => blocks.swap(i, i + 1),
            Some((i, Act::Del)) => {
                blocks.remove(i);
            }
            Some((i, Act::Dup)) => {
                let c = blocks[i].clone();
                blocks.insert(i + 1, c);
            }
            None => {}
        }

        ui.add_space(4.0);
        let mut add_excel = false;
        ui.horizontal_wrapped(|ui| {
            ui.weak("Add");
            if icons::button(ui, Icon::Text, "Text").clicked() {
                blocks.push(Block::Text(TextBlock { text: "Text".into(), ..Default::default() }));
            }
            if icons::button(ui, Icon::Sequence, "Sequence")
                .on_hover_text("Numbered labels: prefix, start, step, count, digits")
                .clicked()
            {
                blocks.push(Block::Sequence(SequenceBlock::default()));
            }
            if icons::button(ui, Icon::Sheet, "Excel")
                .on_hover_text("Print from spreadsheet rows (xlsx, xls, ods, csv)")
                .clicked()
            {
                add_excel = true;
            }
            if icons::button(ui, Icon::Qr, "QR code").clicked() {
                blocks.push(Block::Qr(QrBlock { data: "https://".into(), ..Default::default() }));
            }
            if icons::button(ui, Icon::DataMatrix, "Data Matrix").clicked() {
                blocks.push(Block::DataMatrix(DataMatrixBlock { data: "PTL-{n:04}".into(), ..Default::default() }));
            }
            if icons::button(ui, Icon::Barcode, "Barcode").clicked() {
                blocks.push(Block::Barcode(BarcodeBlock { data: "12345678".into(), ..Default::default() }));
            }
            if icons::button(ui, Icon::Image, "Image").clicked() {
                blocks.push(Block::Image(ImageBlock::default()));
            }
            if icons::button(ui, Icon::Spacer, "Spacer").clicked() {
                blocks.push(Block::Spacer { mm: 3.0 });
            }
        });
        let mut hint = "Placeholders: {n}  {n:03}  {seq}  {date}  {time}".to_string();
        if !headers.is_empty() {
            let cols: Vec<String> = headers.iter().take(8).map(|h| format!("{{{h}}}")).collect();
            hint += &format!("   ·   Columns: {}", cols.join("  "));
        }
        ui.weak(hint);
        if add_excel {
            self.add_excel();
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some((m, err, t)) = &self.msg {
                if t.elapsed().as_secs() < 12 || *err {
                    let c = if *err { Color32::from_rgb(220, 60, 50) } else { ui.visuals().text_color() };
                    ui.colored_label(c, m);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("⌘P print   ⌘S save   ⌘O open   ⌘N new");
            });
        });
    }

    /// File commands from the native menu bar and keyboard shortcuts. The menu
    /// normally takes the key equivalents; a command runs at most once per frame.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let sc = |m, k| KeyboardShortcut::new(m, k);
        let shift_cmd = Modifiers::COMMAND | Modifiers::SHIFT;
        let keys = [
            (sc(shift_cmd, Key::S), MenuCmd::SaveAs),
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
        let mut seen = Vec::new();
        for c in cmds {
            if seen.contains(&c) {
                continue;
            }
            seen.push(c);
            match c {
                MenuCmd::New => {
                    self.p.label = Label::default();
                    self.p.doc_path = None;
                    self.p.sel_rows.clear();
                    self.reload_table();
                }
                MenuCmd::Open => self.open(),
                MenuCmd::Save => self.save(false),
                MenuCmd::SaveAs => self.save(true),
                MenuCmd::Print => self.print(),
            }
        }
    }
}

fn align_buttons(ui: &mut egui::Ui, a: &mut Align) {
    ui.selectable_value(a, Align::Left, "Left");
    ui.selectable_value(a, Align::Center, "Center");
    ui.selectable_value(a, Align::Right, "Right");
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
) {
    let out = TextEdit::multiline(&mut t.text)
        .desired_rows(2)
        .desired_width(f32::INFINITY)
        .font(FontId::proportional(18.0))
        .hint_text("Text (Enter = new line)")
        .show(ui);
    spell_ui(ui, &out, &mut t.text, i, speller, target);
    style_row(ui, i, t, fonts, filter, headers);
}

/// "+ Field" menu: appends a `{Column}` placeholder from the spreadsheet.
fn field_menu(ui: &mut egui::Ui, target: &mut String, headers: &[String], space: bool) {
    if headers.is_empty() {
        return;
    }
    ui.menu_button("+ Field", |ui| {
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for h in headers {
                if ui.button(h).clicked() {
                    if space && !target.is_empty() && !target.ends_with([' ', '\n']) {
                        target.push(' ');
                    }
                    target.push_str(&format!("{{{h}}}"));
                    ui.close();
                }
            }
        });
    })
    .response
    .on_hover_text("Insert a spreadsheet column");
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
        align_buttons(ui, &mut t.align);
        if t.text.contains('\n') {
            ui.separator();
            ui.label("Line spacing");
            ui.add(DragValue::new(&mut t.line_spacing).range(0.5..=3.0).speed(0.01).fixed_decimals(2));
        }
    });
}

fn sequence_editor(
    ui: &mut egui::Ui,
    i: usize,
    q: &mut SequenceBlock,
    fonts: &[String],
    filter: &mut String,
    rows_drive: bool,
) {
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
            .on_disabled_hover_text("The selected spreadsheet rows set the count");
    });
    let v = Vars::default();
    let txt = if rows_drive {
        format!("{}, {}, …  one number per selected row", q.format(0, &v), q.format(1, &v))
    } else if q.count <= 1 {
        q.format(0, &v)
    } else {
        format!("{} → {}   ·   {} labels", q.format(0, &v), q.format(q.count as u64 - 1, &v), q.count)
    };
    ui.weak(txt);
    style_row(ui, i, &mut q.style, fonts, filter, &[]);
}

/// Spreadsheet rows: click toggles, ⇧-click selects a range; virtualised for big sheets.
fn table_ui(
    ui: &mut egui::Ui,
    t: &Table,
    sel: &mut Vec<usize>,
    filter: &mut String,
    preview_row: &mut Option<usize>,
    last: &mut Option<usize>,
) {
    use egui::{Align2, Rect, Sense, pos2, vec2};
    let f = filter.to_lowercase();
    let visible: Vec<usize> = (0..t.rows.len())
        .filter(|&i| f.is_empty() || t.rows[i].iter().any(|c| c.to_lowercase().contains(&f)))
        .collect();
    ui.horizontal(|ui| {
        ui.add(TextEdit::singleline(filter).hint_text("Search rows…").desired_width(200.0));
        if ui.small_button("Select all").on_hover_text("All rows shown").clicked() {
            let have: std::collections::HashSet<usize> = sel.iter().copied().collect();
            sel.extend(visible.iter().filter(|i| !have.contains(i)));
        }
        if ui.small_button("Clear").clicked() {
            sel.clear();
        }
        ui.weak(format!("{} of {} rows selected  ·  click a row, ⇧-click for a range", sel.len(), t.rows.len()));
    });

    let font = FontId::proportional(14.0);
    // Width of each column's longest entry (header + first 200 rows), measured.
    let widths: Vec<f32> = (0..t.headers.len())
        .map(|c| {
            let longest = std::iter::once(t.headers[c].as_str())
                .chain(t.rows.iter().take(200).map(|r| r[c].lines().next().unwrap_or("")))
                .max_by_key(|s| s.chars().count())
                .unwrap_or("");
            let w = ui.painter().layout_no_wrap(longest.to_string(), font.clone(), Color32::WHITE).size().x;
            (w + 22.0).clamp(40.0, 280.0)
        })
        .collect();
    let (chk_w, num_w, row_h) = (26.0, 40.0, 22.0);
    let total_w = (chk_w + num_w + widths.iter().sum::<f32>()).max(ui.available_width());
    let (sel_bg, sel_fg, hov_bg, stripe, fg, weak, strong) = {
        let v = ui.visuals();
        (
            v.selection.bg_fill,
            v.selection.stroke.color,
            v.widgets.hovered.weak_bg_fill,
            v.faint_bg_color,
            v.text_color(),
            v.weak_text_color(),
            v.strong_text_color(),
        )
    };
    let accent = Color32::from_rgb(58, 123, 234);
    let cells = |p: &egui::Painter, rect: Rect, values: &[String], col: Color32| {
        let mut x = rect.min.x + chk_w + num_w;
        for (c, v) in values.iter().enumerate() {
            let cell = Rect::from_min_size(pos2(x, rect.min.y), vec2(widths[c], row_h));
            let text = v.lines().next().unwrap_or("");
            p.with_clip_rect(cell.shrink2(vec2(6.0, 0.0)))
                .text(pos2(x + 8.0, rect.center().y), Align2::LEFT_CENTER, text, font.clone(), col);
            x += widths[c];
        }
    };

    egui::ScrollArea::horizontal().id_salt("data_h").show(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(total_w, row_h), Sense::hover());
        ui.painter().rect_filled(rect, 3.0, stripe);
        cells(ui.painter(), rect, &t.headers, strong);
        egui::ScrollArea::vertical()
            .id_salt("data_v")
            .max_height(260.0)
            .auto_shrink([false, true])
            .show_rows(ui, row_h, visible.len(), |ui, range| {
                for vi in range {
                    let ri = visible[vi];
                    let (rect, resp) = ui.allocate_exact_size(vec2(total_w, row_h), Sense::click());
                    let on = sel.contains(&ri);
                    let p = ui.painter();
                    if on {
                        p.rect_filled(rect, 3.0, sel_bg);
                    } else if resp.hovered() {
                        p.rect_filled(rect, 3.0, hov_bg);
                    } else if vi % 2 == 1 {
                        p.rect_filled(rect, 0.0, stripe);
                    }
                    let b = Rect::from_center_size(pos2(rect.min.x + chk_w / 2.0, rect.center().y), egui::Vec2::splat(13.0));
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
                        t.row_numbers[ri].to_string(),
                        FontId::proportional(12.0),
                        weak,
                    );
                    cells(p, rect, &t.rows[ri], if on { sel_fg } else { fg });
                    if resp.clicked() {
                        let shift = ui.input(|i| i.modifiers.shift);
                        match (*last, shift) {
                            (Some(a), true) if a < visible.len() => {
                                for &r in &visible[a.min(vi)..=a.max(vi)] {
                                    if !sel.contains(&r) {
                                        sel.push(r);
                                    }
                                }
                            }
                            _ if on => sel.retain(|&r| r != ri),
                            _ => sel.push(ri),
                        }
                        *last = Some(vi);
                        *preview_row = Some(ri);
                    }
                }
            });
    });
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
        self.pump();
        if self.handle.is_none() && self.p.auto_connect && self.p.printer.is_none() {
            let first = self.discovered.lock().unwrap().first().cloned();
            if let Some(id) = first {
                self.connect(id);
            }
        }
        self.shortcuts(&ctx);
        self.watch_data(&ctx);
        self.update_preview(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::right("print").resizable(false).exact_size(300.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.print_panel(ui));
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.preview_ui(ui);
            ui.add_space(6.0);
            self.label_options(ui);
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.data_panel(ui);
                self.blocks_ui(ui);
            });
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY, &self.p);
    }
}
