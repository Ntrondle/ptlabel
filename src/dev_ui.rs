//! Live-reloaded UI tuning config (`dev-ui.json`) and in-app Dev Inspector.
//!
//! In `--features dev` builds, `PTLabel Dev` watches `dev-ui.json` (every 200 ms) so spacing,
//! sizing, table layout, colors, and UI element toggles can be iterated on live without
//! recompiling. In release builds `UiConfig::default()` is used with zero runtime overhead.

#[cfg(feature = "dev")]
use eframe::egui::{DragValue, RichText};
use eframe::egui::{self, Color32};
use serde::{Deserialize, Serialize};
#[cfg(feature = "dev")]
use std::path::PathBuf;
#[cfg(feature = "dev")]
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    // Global spacing & layout
    pub item_spacing_x: f32,
    pub item_spacing_y: f32,
    pub button_padding_x: f32,
    pub button_padding_y: f32,
    pub interact_height: f32,
    pub right_panel_width: f32,
    pub preview_max_height: f32,
    pub accent_rgb: [u8; 3],
    pub theme: ThemeMode,

    // Excel panel & table tuning
    pub table_row_height: f32,
    pub table_header_height: f32,
    pub table_max_height: f32,
    pub table_font_size: f32,
    pub table_min_col_width: f32,
    pub table_max_col_width: f32,
    pub table_cell_pad: f32,
    pub table_grid_lines: bool,
    pub table_zebra: bool,
    pub table_show_merged_tint: bool,
    pub show_preview_stepper: bool,

    // Live egui widget inspector (hover any widget to see bounds/size/id)
    pub debug_on_hover: bool,
    // Update dialog & changelog readability
    pub preview_update_modal: bool,
    pub update_modal_width: f32,
    pub changelog_font_size: f32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            item_spacing_x: 8.0,
            item_spacing_y: 6.0,
            button_padding_x: 10.0,
            button_padding_y: 6.0,
            interact_height: 28.0,
            right_panel_width: 300.0,
            preview_max_height: 230.0,
            accent_rgb: [58, 123, 234],
            theme: ThemeMode::System,
            table_row_height: 26.0,
            table_header_height: 28.0,
            table_max_height: 300.0,
            table_font_size: 13.5,
            table_min_col_width: 56.0,
            table_max_col_width: 240.0,
            table_cell_pad: 10.0,
            table_grid_lines: true,
            table_zebra: true,
            table_show_merged_tint: true,
            show_preview_stepper: true,
            debug_on_hover: false,
            preview_update_modal: false,
            update_modal_width: 560.0,
            changelog_font_size: 14.5,
        }
    }
}

impl UiConfig {
    pub fn accent(&self) -> Color32 {
        Color32::from_rgb(self.accent_rgb[0], self.accent_rgb[1], self.accent_rgb[2])
    }

    #[cfg(feature = "dev")]
    pub fn compact() -> Self {
        Self {
            item_spacing_x: 6.0,
            item_spacing_y: 4.0,
            button_padding_x: 8.0,
            button_padding_y: 4.0,
            interact_height: 24.0,
            table_row_height: 22.0,
            table_header_height: 24.0,
            table_max_height: 260.0,
            table_font_size: 12.5,
            table_cell_pad: 8.0,
            ..Self::default()
        }
    }

    #[cfg(feature = "dev")]
    pub fn roomy() -> Self {
        Self {
            item_spacing_x: 10.0,
            item_spacing_y: 8.0,
            button_padding_x: 12.0,
            button_padding_y: 7.0,
            interact_height: 30.0,
            table_row_height: 30.0,
            table_header_height: 32.0,
            table_max_height: 380.0,
            table_font_size: 14.5,
            table_cell_pad: 12.0,
            ..Self::default()
        }
    }

    pub fn apply(&self, ctx: &egui::Context) {
        ctx.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(self.item_spacing_x, self.item_spacing_y);
            s.spacing.button_padding = egui::vec2(self.button_padding_x, self.button_padding_y);
            s.spacing.interact_size.y = self.interact_height;
            let mut scroll = egui::style::ScrollStyle::solid();
            scroll.bar_width = 8.0;
            scroll.bar_inner_margin = 6.0;
            scroll.bar_outer_margin = 2.0;
            scroll.handle_min_length = 28.0;
            s.spacing.scroll = scroll;
        });
        match self.theme {
            ThemeMode::System => {}
            ThemeMode::Light => ctx.set_theme(egui::Theme::Light),
            ThemeMode::Dark => ctx.set_theme(egui::Theme::Dark),
        }
        #[cfg(feature = "dev")]
        ctx.set_debug_on_hover(self.debug_on_hover);
    }
}

pub struct DevUi {
    pub cfg: UiConfig,
    #[cfg(feature = "dev")]
    pub open: bool,
    #[cfg(feature = "dev")]
    path: PathBuf,
    #[cfg(feature = "dev")]
    mtime: Option<SystemTime>,
    #[cfg(feature = "dev")]
    last_check: Instant,
    #[cfg(feature = "dev")]
    err: Option<String>,
    #[cfg(feature = "dev")]
    auto_save: bool,
    applied_theme: ThemeMode,
}

impl DevUi {
    pub fn new() -> Self {
        #[cfg(feature = "dev")]
        {
            let path = std::env::var("PTLABEL_DEV_UI")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dev-ui.json"));
            let mut dev = Self {
                cfg: UiConfig::default(),
                open: false,
                path,
                mtime: None,
                last_check: Instant::now() - Duration::from_secs(1),
                err: None,
                auto_save: true,
                applied_theme: ThemeMode::System,
            };
            if dev.path.exists() {
                dev.reload();
            } else {
                dev.save();
            }
            dev
        }
        #[cfg(not(feature = "dev"))]
        {
            Self { cfg: UiConfig::default(), applied_theme: ThemeMode::System }
        }
    }

    #[cfg(feature = "dev")]
    pub fn poll(&mut self, ctx: &egui::Context) {
        // Re-check every 200 ms so external edits to dev-ui.json appear live with zero recompile.
        ctx.request_repaint_after(Duration::from_millis(200));
        if self.last_check.elapsed() < Duration::from_millis(200) {
            return;
        }
        self.last_check = Instant::now();
        let cur = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if cur.is_some() && cur != self.mtime {
            self.reload();
        }
    }

    #[cfg(not(feature = "dev"))]
    pub fn poll(&mut self, _ctx: &egui::Context) {}

    pub fn apply(&mut self, ctx: &egui::Context) {
        if self.cfg.theme != self.applied_theme {
            if self.cfg.theme == ThemeMode::System {
                ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::System);
            }
            self.applied_theme = self.cfg.theme;
        }
        self.cfg.apply(ctx);
    }

    #[cfg(feature = "dev")]
    fn reload(&mut self) {
        self.mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        match std::fs::read_to_string(&self.path)
            .map_err(|e| format!("read {}: {e}", self.path.display()))
            .and_then(|s| serde_json::from_str::<UiConfig>(&s).map_err(|e| format!("parse JSON: {e}")))
        {
            Ok(cfg) => {
                self.cfg = cfg;
                self.err = None;
            }
            Err(e) => self.err = Some(e),
        }
    }

    #[cfg(feature = "dev")]
    pub fn set_update_preview(&mut self, on: bool) {
        if self.cfg.preview_update_modal != on {
            self.cfg.preview_update_modal = on;
            if self.auto_save {
                self.save();
            }
        }
    }

    #[cfg(feature = "dev")]
    fn save(&mut self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.cfg) {
            if let Err(e) = std::fs::write(&self.path, format!("{json}\n")) {
                self.err = Some(format!("write {}: {e}", self.path.display()));
            } else {
                self.mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
                self.err = None;
            }
        }
    }

    /// Floating Dev UI Inspector window (only compiled in `--features dev`).
    #[cfg(feature = "dev")]
    pub fn window(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        let before = self.cfg.clone();
        let mut do_save = false;
        let mut do_reload = false;
        egui::Window::new("Dev UI Inspector")
            .id(egui::Id::unique("dev_ui_inspector"))
            .open(&mut open)
            .default_width(340.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Live JSON:").small().weak());
                    ui.label(RichText::new(self.path.display().to_string()).small().monospace());
                });
                if let Some(e) = &self.err {
                    ui.colored_label(Color32::from_rgb(220, 60, 50), e);
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Default").clicked() {
                        self.cfg = UiConfig::default();
                    }
                    if ui.button("Compact").clicked() {
                        self.cfg = UiConfig::compact();
                    }
                    if ui.button("Roomy").clicked() {
                        self.cfg = UiConfig::roomy();
                    }
                    ui.separator();
                    if ui.button("Reload").clicked() {
                        do_reload = true;
                    }
                    if ui.button("Save").clicked() {
                        do_save = true;
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.auto_save, "Auto-save edits to dev-ui.json");
                    ui.checkbox(&mut self.cfg.debug_on_hover, "Inspect widget bounds");
                    if ui
                        .button(if self.cfg.preview_update_modal { "Hide update window" } else { "Preview update window" })
                        .on_hover_text("Open the Update Available modal with sample changelog")
                        .clicked()
                    {
                        self.cfg.preview_update_modal = !self.cfg.preview_update_modal;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    ui.label(RichText::new("Excel Table & Selection").strong());
                    egui::Grid::new("dev_tbl").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                        let c = &mut self.cfg;
                        ui.label("Row height");
                        ui.add(DragValue::new(&mut c.table_row_height).range(18.0..=44.0).speed(0.2).suffix(" px"));
                        ui.end_row();
                        ui.label("Header height");
                        ui.add(DragValue::new(&mut c.table_header_height).range(20.0..=48.0).speed(0.2).suffix(" px"));
                        ui.end_row();
                        ui.label("Table max height");
                        ui.add(DragValue::new(&mut c.table_max_height).range(140.0..=640.0).speed(1.0).suffix(" px"));
                        ui.end_row();
                        ui.label("Table font size");
                        ui.add(DragValue::new(&mut c.table_font_size).range(10.0..=20.0).speed(0.1).suffix(" pt"));
                        ui.end_row();
                        ui.label("Min / Max col width");
                        ui.horizontal(|ui| {
                            ui.add(DragValue::new(&mut c.table_min_col_width).range(32.0..=160.0).speed(0.5));
                            ui.label("–");
                            ui.add(DragValue::new(&mut c.table_max_col_width).range(100.0..=480.0).speed(1.0));
                        });
                        ui.end_row();
                        ui.label("Cell horizontal pad");
                        ui.add(DragValue::new(&mut c.table_cell_pad).range(4.0..=24.0).speed(0.2).suffix(" px"));
                        ui.end_row();
                    });
                    let c = &mut self.cfg;
                    ui.checkbox(&mut c.table_grid_lines, "Table grid lines");
                    ui.checkbox(&mut c.table_zebra, "Zebra row striping");
                    ui.checkbox(&mut c.table_show_merged_tint, "Highlight inherited merged cells");
                    ui.checkbox(&mut c.show_preview_stepper, "Show preview row stepper");

                    ui.separator();
                    ui.label(RichText::new("Global Layout & Theme").strong());
                    ui.horizontal(|ui| {
                        ui.label("Theme");
                        ui.selectable_value(&mut c.theme, ThemeMode::System, "System");
                        ui.selectable_value(&mut c.theme, ThemeMode::Light, "Light");
                        ui.selectable_value(&mut c.theme, ThemeMode::Dark, "Dark");
                    });
                    egui::Grid::new("dev_glob").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                        ui.label("Item spacing");
                        ui.horizontal(|ui| {
                            ui.add(DragValue::new(&mut c.item_spacing_x).range(2.0..=20.0).speed(0.1));
                            ui.label("×");
                            ui.add(DragValue::new(&mut c.item_spacing_y).range(2.0..=20.0).speed(0.1));
                        });
                        ui.end_row();
                        ui.label("Button padding");
                        ui.horizontal(|ui| {
                            ui.add(DragValue::new(&mut c.button_padding_x).range(2.0..=24.0).speed(0.1));
                            ui.label("×");
                            ui.add(DragValue::new(&mut c.button_padding_y).range(2.0..=16.0).speed(0.1));
                        });
                        ui.end_row();
                        ui.label("Interact height");
                        ui.add(DragValue::new(&mut c.interact_height).range(20.0..=40.0).speed(0.2).suffix(" px"));
                        ui.end_row();
                        ui.label("Print panel width");
                        ui.add(DragValue::new(&mut c.right_panel_width).range(240.0..=420.0).speed(1.0).suffix(" px"));
                        ui.end_row();
                        ui.label("Preview max height");
                        ui.add(DragValue::new(&mut c.preview_max_height).range(120.0..=360.0).speed(1.0).suffix(" px"));
                        ui.end_row();
                        ui.label("Update modal width");
                        ui.add(DragValue::new(&mut c.update_modal_width).range(440.0..=760.0).speed(2.0).suffix(" px"));
                        ui.end_row();
                        ui.label("Changelog font size");
                        ui.add(DragValue::new(&mut c.changelog_font_size).range(12.0..=22.0).speed(0.1).suffix(" pt"));
                        ui.end_row();
                        ui.label("Accent color");
                        let mut col = Color32::from_rgb(c.accent_rgb[0], c.accent_rgb[1], c.accent_rgb[2]);
                        if ui.color_edit_button_srgba(&mut col).changed() {
                            c.accent_rgb = [col.r(), col.g(), col.b()];
                        }
                        ui.end_row();
                    });
                });
            });
        self.open = open;
        if do_reload {
            self.reload();
        } else if do_save || (self.auto_save && self.cfg != before) {
            self.save();
        }
    }

    #[cfg(not(feature = "dev"))]
    pub fn window(&mut self, _ctx: &egui::Context) {}
}
