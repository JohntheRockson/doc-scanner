//! Editor opened by `--edit` / `-edit` after a scan.
//!
//! Page coordinates are pixels of the scanned image. The window only scales
//! that image to fit; screenshots, text, and ink are stored in those pixels
//! and burned in when the window closes, so the PDF matches the page.

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};
use eframe::egui;
use image::{RgbImage, Rgba, RgbaImage};
use std::path::Path;
use std::sync::{Arc, Mutex};

const MAX_STAMP_PX: u32 = 4096;

pub fn run(pages: Vec<RgbImage>, output: &Path) -> Result<Vec<RgbImage>> {
    if pages.is_empty() {
        anyhow::bail!("no scanned pages to edit");
    }
    if pages
        .iter()
        .any(|page| page.width() == 0 || page.height() == 0)
    {
        anyhow::bail!("a scanned page came out empty");
    }

    let slot = Arc::new(Mutex::new(None));
    let slot_for_app = Arc::clone(&slot);
    let hint = output.display().to_string();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Scan editor")
            .with_inner_size(egui::vec2(1360.0, 900.0))
            .with_min_inner_size(egui::vec2(960.0, 640.0))
            .with_drag_and_drop(true),
        run_and_return: true,
        persist_window: false,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "paper_scanner_editor",
        options,
        Box::new(move |cc| Ok(Box::new(EditorApp::new(cc, pages, hint, slot_for_app)))),
    )
    .map_err(|err| anyhow::anyhow!("could not open the editor: {err}"))?;

    slot.lock()
        .unwrap_or_else(|err| err.into_inner())
        .take()
        .context("the editor closed before it could produce a PDF")
}

struct EditorApp {
    pages: Vec<Page>,
    page_idx: usize,
    selected: Option<usize>,
    tool: Tool,
    /// RGB swatch. Alpha is applied when a stroke is created.
    color: [u8; 4],
    /// Pen and eraser thickness, in points on a letter-sized page.
    pen_pt: f32,
    highlight_pt: f32,
    drag: Drag,
    /// Stroke currently being drawn, in page pixels. Committed on release.
    live: Option<LiveStroke>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// True after this gesture has already stored one undo snapshot.
    gesture_undo: bool,
    slider_undo: bool,
    zoom: f32,
    pan: egui::Vec2,
    /// Page-pixels to screen-points, from the last drawn frame. Used so arrow
    /// keys nudge by about one screen pixel instead of one scan pixel.
    view_scale: f32,
    /// Last pointer position over the page, in page pixels. Ctrl+V drops a
    /// screenshot here.
    hover_page: Option<(f32, f32)>,
    paste_nudge: u32,
    status: String,
    output_hint: String,
    font: Option<FontArc>,
    text_edit_id: Option<egui::Id>,
    focus_text: bool,
    slot: Arc<Mutex<Option<Vec<RgbImage>>>>,
    persisted: bool,
}

struct Page {
    image: RgbImage,
    texture: egui::TextureHandle,
    items: Vec<Item>,
}

#[derive(Clone)]
struct Item {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    dirty: bool,
    kind: Kind,
}

#[derive(Clone)]
enum Kind {
    Image {
        source: Arc<RgbaImage>,
        texture: egui::TextureHandle,
        /// height / width of `source`, so resizing keeps the picture's shape.
        aspect: f32,
    },
    Text {
        text: String,
        size: f32,
        color: [u8; 4],
        raster: Arc<RgbaImage>,
        texture: Option<egui::TextureHandle>,
    },
    /// `points` are relative to `Item::x` / `Item::y`. `width` is the stroke
    /// diameter in page pixels.
    Ink {
        points: Vec<(f32, f32)>,
        color: [u8; 4],
        width: f32,
        highlighter: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayerTo {
    Front,
    Back,
    /// One step toward the viewer. Higher indexes are drawn later, so they sit in front.
    Forward,
    Backward,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Pen,
    Highlighter,
    Eraser,
}

struct LiveStroke {
    points: Vec<(f32, f32)>,
    color: [u8; 4],
    width: f32,
    highlighter: bool,
}

struct Snapshot {
    page: usize,
    items: Vec<Item>,
}

const PALETTE: [[u8; 4]; 10] = [
    [23, 23, 23, 255],
    [98, 98, 98, 255],
    [220, 38, 38, 255],
    [234, 88, 12, 255],
    [234, 179, 8, 255],
    [22, 163, 74, 255],
    [37, 99, 235, 255],
    [124, 58, 237, 255],
    [219, 39, 119, 255],
    [255, 255, 255, 255],
];

#[derive(Clone, Copy)]
enum Drag {
    None,
    Move {
        index: usize,
        grab_x: f32,
        grab_y: f32,
    },
    Resize {
        index: usize,
    },
    Pan {
        last: egui::Pos2,
    },
    Draw,
    Erase,
}

#[derive(Clone, Copy)]
struct ViewGeom {
    scale: f32,
    origin: egui::Pos2,
    page_rect: egui::Rect,
}

impl EditorApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        pages: Vec<RgbImage>,
        output_hint: String,
        slot: Arc<Mutex<Option<Vec<RgbImage>>>>,
    ) -> Self {
        let ctx = &cc.egui_ctx;
        let font = load_font();
        let mut status =
            "Select to move things, or press P and draw. Ctrl+V pastes a screenshot.".to_string();
        if font.is_none() {
            status = "No system font found, so text boxes are disabled. Screenshots and images still work.".into();
        }
        let pages = pages
            .into_iter()
            .enumerate()
            .map(|(i, image)| {
                let texture = upload_page_texture(ctx, &image, &format!("page-{i}"));
                Page {
                    image,
                    texture,
                    items: Vec::new(),
                }
            })
            .collect();

        Self {
            pages,
            page_idx: 0,
            selected: None,
            tool: Tool::Select,
            color: PALETTE[0],
            pen_pt: 2.4,
            highlight_pt: 18.0,
            drag: Drag::None,
            live: None,
            undo: Vec::new(),
            redo: Vec::new(),
            gesture_undo: false,
            slider_undo: false,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            view_scale: 1.0,
            hover_page: None,
            paste_nudge: 0,
            status,
            output_hint,
            font,
            text_edit_id: None,
            focus_text: false,
            slot,
            persisted: false,
        }
    }

    fn ui_frame(&mut self, ui: &mut egui::Ui) {
        self.handle_shortcuts(ui.ctx());
        self.handle_drops(ui.ctx());

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::left("tools")
            .resizable(false)
            .exact_size(188.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.tools(ui));
            });
        egui::Panel::right("props")
            .resizable(true)
            .min_size(240.0)
            .default_size(280.0)
            .max_size(420.0)
            .show(ui, |ui| self.props(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.canvas(ui));
    }

    fn tools(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Tools").strong());
        ui.add_space(4.0);
        self.tool_button(ui, Tool::Select, "Select", "V");
        self.tool_button(ui, Tool::Pen, "Pen", "P");
        self.tool_button(ui, Tool::Highlighter, "Highlight", "H");
        self.tool_button(ui, Tool::Eraser, "Eraser", "E");

        ui.add_space(12.0);
        ui.label(egui::RichText::new("Colors").strong());
        ui.add_space(4.0);
        let chosen = self.color;
        let mut picked = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for color in PALETTE {
                if color_swatch(ui, color, same_rgb(color, chosen)).clicked() {
                    picked = Some(color);
                }
            }
        });
        if let Some(color) = picked {
            self.set_color(color);
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Keys 1–9 and 0 pick a color")
                .weak()
                .small(),
        );

        ui.add_space(10.0);
        let highlight = self.editing_highlighter();
        let (label, mut value, range) = if highlight {
            ("Highlight", self.highlight_pt, 8.0..=32.0)
        } else if self.tool == Tool::Eraser {
            ("Eraser", self.pen_pt, 0.8..=10.0)
        } else {
            ("Thickness", self.pen_pt, 0.8..=10.0)
        };
        let response = ui.add(egui::Slider::new(&mut value, range).text(label));
        if response.changed() {
            self.set_thickness(value);
        }
        ui.label(
            egui::RichText::new("[ and ] change thickness")
                .weak()
                .small(),
        );
    }

    fn tool_button(&mut self, ui: &mut egui::Ui, tool: Tool, name: &str, key: &str) {
        let on = self.tool == tool;
        let label = if on {
            egui::RichText::new(format!("{name}   {key}")).color(egui::Color32::WHITE)
        } else {
            egui::RichText::new(format!("{name}   {key}"))
        };
        let mut button = egui::Button::new(label);
        if on {
            button = button.fill(egui::Color32::from_rgb(37, 99, 235));
        }
        if ui.add_sized([ui.available_width(), 30.0], button).clicked() {
            self.set_tool(tool);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let n = self.pages.len();
            let idx = self.page_idx;
            if ui
                .add_enabled(idx > 0, egui::Button::new("◀"))
                .on_hover_text("Previous page")
                .clicked()
            {
                self.set_page(idx - 1);
            }
            ui.label(format!("Page {} / {n}", idx + 1));
            if ui
                .add_enabled(idx + 1 < n, egui::Button::new("▶"))
                .on_hover_text("Next page")
                .clicked()
            {
                self.set_page(idx + 1);
            }
            ui.separator();
            if ui
                .button("Image")
                .on_hover_text("Place a PNG or JPEG on this page")
                .clicked()
            {
                self.pick_images(ui.ctx());
            }
            if ui
                .add_enabled(self.font.is_some(), egui::Button::new("Text"))
                .on_hover_text("Drop a text box on this page (T)")
                .clicked()
            {
                self.add_text(None, ui.ctx());
                self.set_tool(Tool::Select);
            }
            if ui
                .button("Paste")
                .on_hover_text("Paste a screenshot or text from the clipboard (Ctrl+V)")
                .clicked()
            {
                self.paste(ui.ctx());
            }
            ui.separator();
            if ui
                .add_enabled(!self.undo.is_empty(), egui::Button::new("Undo"))
                .on_hover_text("Ctrl+Z")
                .clicked()
            {
                self.undo_edit();
            }
            if ui
                .add_enabled(!self.redo.is_empty(), egui::Button::new("Redo"))
                .on_hover_text("Ctrl+Y")
                .clicked()
            {
                self.redo_edit();
            }
            if ui
                .add_enabled(self.selected.is_some(), egui::Button::new("Delete"))
                .clicked()
            {
                self.delete_selected();
            }
            ui.separator();
            if ui
                .button("Fit")
                .on_hover_text("Fit the page in the window")
                .clicked()
            {
                self.zoom = 1.0;
                self.pan = egui::Vec2::ZERO;
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            ui.separator();
            if ui
                .button(egui::RichText::new("Save PDF").strong())
                .on_hover_text("Save and close (Ctrl+S). Closing the window also saves.")
                .clicked()
            {
                self.finish(ui.ctx());
            }
        });
    }

    fn props(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Selection").strong());
        ui.separator();

        let Some(index) = self.selected else {
            self.text_edit_id = None;
            ui.add(egui::Label::new(self.tool_help()).wrap_mode(egui::TextWrapMode::Wrap));
            return;
        };
        if index >= self.pages[self.page_idx].items.len() {
            self.selected = None;
            self.text_edit_id = None;
            return;
        }

        match &self.pages[self.page_idx].items[index].kind {
            Kind::Text { .. } => self.props_text(ui, index),
            Kind::Image { .. } => self.props_image(ui, index),
            Kind::Ink { highlighter, .. } => {
                self.text_edit_id = None;
                ui.label(if *highlighter {
                    "Highlighter stroke"
                } else {
                    "Pen stroke"
                });
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(
                        "Drag it to move. The color palette and thickness slider edit this stroke.",
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            }
        }

        ui.add_space(12.0);
        self.layer_controls(ui, index);
        ui.add_space(8.0);
        if ui.button("Delete").clicked() {
            self.delete_selected();
        }
    }

    fn layer_controls(&mut self, ui: &mut egui::Ui, index: usize) {
        let len = self.pages[self.page_idx].items.len();
        ui.label(egui::RichText::new("Layer").strong());
        ui.label(
            egui::RichText::new(format!("{} of {len}  ·  higher is in front", index + 1))
                .weak()
                .small(),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(index + 1 < len, egui::Button::new("To front"))
                .on_hover_text("Bring in front of everything (Ctrl+Shift+Up)")
                .clicked()
            {
                self.reorder_selected(LayerTo::Front);
            }
            if ui
                .add_enabled(index + 1 < len, egui::Button::new("Forward"))
                .on_hover_text("Move up one layer (Ctrl+Up)")
                .clicked()
            {
                self.reorder_selected(LayerTo::Forward);
            }
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(index > 0, egui::Button::new("To back"))
                .on_hover_text("Send behind everything (Ctrl+Shift+Down)")
                .clicked()
            {
                self.reorder_selected(LayerTo::Back);
            }
            if ui
                .add_enabled(index > 0, egui::Button::new("Backward"))
                .on_hover_text("Move down one layer (Ctrl+Down)")
                .clicked()
            {
                self.reorder_selected(LayerTo::Backward);
            }
        });
    }

    fn props_text(&mut self, ui: &mut egui::Ui, index: usize) {
        let (mut text, mut size) = match &self.pages[self.page_idx].items[index].kind {
            Kind::Text { text, size, .. } => (text.clone(), *size),
            _ => return,
        };
        let response = ui.add(
            egui::TextEdit::multiline(&mut text)
                .id(egui::Id::new(("anno-text", self.page_idx, index)))
                .desired_rows(8)
                .desired_width(f32::INFINITY)
                .hint_text("Type here"),
        );
        self.text_edit_id = Some(response.id);
        if self.focus_text {
            response.request_focus();
            self.focus_text = false;
        }
        let (_, page_h) = self.page_px();
        let max_size = (page_h / 10.0).clamp(72.0, 280.0);
        ui.add(egui::Slider::new(&mut size, 18.0..=max_size).text("Font size"));
        ui.add(
            egui::Label::new(
                "Color comes from the palette. Drag the corner to set the wrap width.",
            )
            .wrap_mode(egui::TextWrapMode::Wrap),
        );

        let item = &mut self.pages[self.page_idx].items[index];
        if let Kind::Text {
            text: stored,
            size: stored_size,
            ..
        } = &mut item.kind
        {
            if *stored != text {
                *stored = text;
                item.dirty = true;
            }
            let size = size.round();
            if (*stored_size - size).abs() > 0.1 {
                *stored_size = size;
                item.dirty = true;
            }
        }
    }

    fn props_image(&mut self, ui: &mut egui::Ui, index: usize) {
        self.text_edit_id = None;
        let (w, h) = {
            let item = &self.pages[self.page_idx].items[index];
            (item.w, item.h)
        };
        ui.label(format!("{w:.0} × {h:.0} px on the page"));
        ui.add(
            egui::Label::new(
                "Drag the image to move it. Drag the blue corner to resize. The picture keeps its shape.",
            )
            .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }

    fn tool_help(&self) -> &'static str {
        match self.tool {
            Tool::Select => {
                "Nothing selected. Drag an item to move it, or press P to draw. Ctrl+V pastes a screenshot."
            }
            Tool::Pen => "Pen is on. Draw on the page. Right-drag pans, scroll zooms.",
            Tool::Highlighter => "Highlighter is on. Drag across the words you want to mark.",
            Tool::Eraser => {
                "Eraser removes pen and highlighter strokes. It leaves the scan, text, and pictures."
            }
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.add(
            egui::Label::new(egui::RichText::new(&self.status))
                .wrap_mode(egui::TextWrapMode::Truncate),
        );
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!(
                    "Saves to {} — closing the window saves",
                    self.output_hint
                ))
                .weak()
                .small(),
            )
            .wrap_mode(egui::TextWrapMode::Truncate),
        );
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        if self.pages.is_empty() {
            ui.label("No pages.");
            return;
        }

        let available = ui.available_size();
        let (response, painter) = ui.allocate_painter(available, egui::Sense::click_and_drag());
        let canvas = response.rect;
        painter.rect_filled(canvas, 0.0, egui::Color32::from_rgb(32, 33, 38));

        let (pw, ph, page_tex) = {
            let page = &self.pages[self.page_idx];
            (
                page.image.width() as f32,
                page.image.height() as f32,
                page.texture.id(),
            )
        };
        if pw < 1.0 || ph < 1.0 || canvas.width() < 2.0 || canvas.height() < 2.0 {
            return;
        }

        self.apply_scroll_zoom(ui, canvas, pw, ph, response.hovered());
        let geom = self.view_geom(canvas, pw, ph);
        self.view_scale = geom.scale;
        self.interact(ui.ctx(), canvas, geom, pw, ph);
        self.rebuild_dirty_text(ui.ctx());
        self.draw_page(&painter, geom, page_tex, pw, ph);
    }

    fn apply_scroll_zoom(
        &mut self,
        ui: &egui::Ui,
        canvas: egui::Rect,
        pw: f32,
        ph: f32,
        hovered: bool,
    ) {
        if !hovered || !matches!(self.drag, Drag::None) {
            return;
        }
        let (dy, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        // Ctrl-scroll and pinch arrive as a scale factor. A plain wheel notch
        // arrives as points; ~50 points should zoom by about 10%.
        let factor = if pinch.is_finite() && (pinch - 1.0).abs() > 0.001 {
            pinch
        } else if dy.is_finite() && dy != 0.0 {
            (dy * 0.002).exp()
        } else {
            return;
        };
        let Some(ptr) = ui.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        if !canvas.contains(ptr) {
            return;
        }

        let before = self.view_geom(canvas, pw, ph);
        let page_pt = (ptr - before.origin) / before.scale;
        self.zoom = (self.zoom * factor).clamp(0.25, 12.0);
        let after = self.view_geom(canvas, pw, ph);
        let page_size = egui::vec2(pw * after.scale, ph * after.scale);
        let origin_without_pan = canvas.center() - page_size * 0.5;
        let desired_origin = ptr - page_pt * after.scale;
        self.pan = desired_origin - origin_without_pan;
    }

    fn view_geom(&self, canvas: egui::Rect, pw: f32, ph: f32) -> ViewGeom {
        let fit = (canvas.width() / pw).min(canvas.height() / ph) * 0.96;
        let scale = (fit * self.zoom).max(0.000_1);
        let page_size = egui::vec2(pw * scale, ph * scale);
        let origin = canvas.center() - page_size * 0.5 + self.pan;
        ViewGeom {
            scale,
            origin,
            page_rect: egui::Rect::from_min_size(origin, page_size),
        }
    }

    fn interact(
        &mut self,
        ctx: &egui::Context,
        canvas: egui::Rect,
        geom: ViewGeom,
        pw: f32,
        ph: f32,
    ) {
        let hover = ctx.input(|i| i.pointer.hover_pos());
        let latest = ctx.input(|i| i.pointer.latest_pos());
        let primary_pressed = ctx.input(|i| i.pointer.primary_pressed());
        let secondary_pressed = ctx.input(|i| i.pointer.secondary_pressed());
        let middle_pressed = ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Middle));
        let any_down = ctx.input(|i| i.pointer.any_down());

        if let Some(pos) = hover.filter(|pos| geom.page_rect.contains(*pos)) {
            let rel = (pos - geom.origin) / geom.scale;
            self.hover_page = Some((rel.x, rel.y));
        }

        if !any_down {
            if matches!(self.drag, Drag::Draw) {
                self.commit_live_stroke();
            }
            self.drag = Drag::None;
            self.gesture_undo = false;
            self.slider_undo = false;
        }

        if any_down {
            if primary_pressed
                && hover.is_some_and(|pos| canvas.contains(pos))
                && matches!(self.drag, Drag::None)
            {
                let pos = hover.unwrap();
                let (page_x, page_y) = to_page(pos, geom);
                match self.tool {
                    Tool::Pen | Tool::Highlighter if geom.page_rect.contains(pos) => {
                        self.start_stroke(page_x, page_y);
                    }
                    Tool::Eraser if geom.page_rect.contains(pos) => {
                        self.drag = Drag::Erase;
                        self.erase_at(page_x, page_y);
                    }
                    Tool::Select | Tool::Pen | Tool::Highlighter | Tool::Eraser => {
                        let on_handle = self
                            .selected
                            .is_some_and(|index| self.handle_contains(index, pos, geom));
                        if on_handle {
                            let index = self.selected.unwrap();
                            self.drag = Drag::Resize { index };
                        } else if let Some(hit) = self.hit_test(page_x, page_y) {
                            self.selected = Some(hit);
                            self.focus_text = false;
                            let item = &self.pages[self.page_idx].items[hit];
                            self.drag = Drag::Move {
                                index: hit,
                                grab_x: page_x - item.x,
                                grab_y: page_y - item.y,
                            };
                        } else {
                            self.selected = None;
                            self.focus_text = false;
                            self.drag = Drag::Pan { last: pos };
                        }
                    }
                }
            } else if (secondary_pressed || middle_pressed)
                && hover.is_some_and(|pos| canvas.contains(pos))
                && matches!(self.drag, Drag::None)
            {
                if let Some(pos) = hover {
                    self.drag = Drag::Pan { last: pos };
                }
            }

            if let Some(pos) = latest.or(hover) {
                self.apply_drag(pos, geom, pw, ph);
            }
        }

        if matches!(self.tool, Tool::Pen | Tool::Highlighter | Tool::Eraser)
            && !matches!(self.drag, Drag::Pan { .. })
        {
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        } else if matches!(self.drag, Drag::Move { .. }) {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if matches!(self.drag, Drag::Resize { .. }) {
            ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
        } else if let Some(pos) = hover {
            if self
                .selected
                .is_some_and(|index| self.handle_contains(index, pos, geom))
            {
                ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            } else if geom.page_rect.contains(pos) {
                let (page_x, page_y) = to_page(pos, geom);
                if self.hit_test(page_x, page_y).is_some() {
                    ctx.set_cursor_icon(egui::CursorIcon::Grab);
                }
            }
        }
    }

    fn apply_drag(&mut self, pos: egui::Pos2, geom: ViewGeom, pw: f32, ph: f32) {
        let (page_x, page_y) = to_page(pos, geom);
        match self.drag {
            Drag::Move {
                index,
                grab_x,
                grab_y,
            } => {
                let Some(item) = self.pages[self.page_idx].items.get(index) else {
                    return;
                };
                let (x, y) = keep_on_page(page_x - grab_x, page_y - grab_y, item.w, item.h, pw, ph);
                if (x - item.x).abs() > 0.2 || (y - item.y).abs() > 0.2 {
                    if !self.gesture_undo {
                        self.push_undo();
                        self.gesture_undo = true;
                    }
                    if let Some(item) = self.pages[self.page_idx].items.get_mut(index) {
                        item.x = x;
                        item.y = y;
                    }
                }
            }
            Drag::Resize { index } => self.resize_item(index, page_x, pw, ph),
            Drag::Pan { last } => {
                self.pan += pos - last;
                self.drag = Drag::Pan { last: pos };
            }
            Drag::Draw => {
                if let Some(live) = &mut self.live {
                    let min = (live.width * 0.28).max(0.75);
                    extend_stroke(&mut live.points, page_x, page_y, min);
                }
            }
            Drag::Erase => self.erase_at(page_x, page_y),
            Drag::None => {}
        }
    }

    fn resize_item(&mut self, index: usize, page_x: f32, pw: f32, ph: f32) {
        let Some(item) = self.pages[self.page_idx].items.get_mut(index) else {
            return;
        };
        let max_side = pw.max(ph).max(64.0);
        match &item.kind {
            Kind::Image { aspect, .. } => {
                let aspect = (*aspect).max(0.01);
                let max_w = (max_side / aspect).min(max_side).max(24.0);
                item.w = (page_x - item.x).clamp(24.0, max_w);
                item.h = item.w * aspect;
            }
            Kind::Text { .. } => {
                let next = (page_x - item.x).clamp(48.0, max_side);
                if (item.w - next).abs() > 0.5 {
                    item.w = next;
                    item.dirty = true;
                }
            }
            Kind::Ink { .. } => {}
        }
    }

    fn draw_page(
        &self,
        painter: &egui::Painter,
        geom: ViewGeom,
        page_tex: egui::TextureId,
        pw: f32,
        ph: f32,
    ) {
        let _ = (pw, ph);
        painter.rect_filled(
            geom.page_rect.translate(egui::vec2(4.0, 5.0)),
            0.0,
            egui::Color32::from_black_alpha(70),
        );
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        painter.image(page_tex, geom.page_rect, uv, egui::Color32::WHITE);
        painter.rect_stroke(
            geom.page_rect,
            0.0,
            egui::Stroke::new(1.0, egui::Color32::from_gray(170)),
            egui::StrokeKind::Outside,
        );

        let item_painter = painter.with_clip_rect(geom.page_rect);
        for item in &self.pages[self.page_idx].items {
            draw_item(&item_painter, item, geom, uv);
        }
        if let Some(live) = &self.live {
            draw_page_stroke(painter, &live.points, live.color, live.width, geom);
        }
        if let Some(index) = self.selected {
            if let Some(item) = self.pages[self.page_idx].items.get(index) {
                let rect = item_screen_rect(item, geom.origin, geom.scale);
                let ink = matches!(item.kind, Kind::Ink { .. });
                painter.rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(47, 111, 235)),
                    egui::StrokeKind::Outside,
                );
                if !ink {
                    let handle = handle_screen_rect(rect);
                    painter.rect_filled(handle, 2.0, egui::Color32::WHITE);
                    painter.rect_stroke(
                        handle,
                        2.0,
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(47, 111, 235)),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }
        if matches!(self.tool, Tool::Pen | Tool::Highlighter | Tool::Eraser) {
            if let Some((x, y)) = self.hover_page {
                let center = geom.origin + egui::vec2(x, y) * geom.scale;
                let radius = (self.tool_width_px() * 0.5 * geom.scale).max(2.0);
                painter.circle_stroke(center, radius, egui::Stroke::new(1.5, egui::Color32::WHITE));
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = self.typing(ctx);
        let (command, shift) =
            ctx.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));

        // Ignore key-repeat so holding Ctrl+V doesn't stamp the screenshot over and over.
        let ctrl_v = command && pressed_once(ctx, egui::Key::V);
        let paste_key = pressed_once(ctx, egui::Key::Paste);
        if ctrl_v || paste_key {
            // One clipboard read. Screenshots are placed on the page even when a
            // text box is focused; copied text is left for that text box.
            self.paste_shortcut(ctx, typing);
        }

        if command && pressed_once(ctx, egui::Key::S) {
            self.finish(ctx);
            ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::S);
            });
        }

        if command && !typing && pressed_once(ctx, egui::Key::Z) {
            if shift {
                self.redo_edit();
            } else {
                self.undo_edit();
            }
        }
        if command && !typing && pressed_once(ctx, egui::Key::Y) {
            self.redo_edit();
        }

        if !typing && !command {
            if pressed_once(ctx, egui::Key::P) {
                self.set_tool(Tool::Pen);
            } else if pressed_once(ctx, egui::Key::H) {
                self.set_tool(Tool::Highlighter);
            } else if pressed_once(ctx, egui::Key::E) {
                self.set_tool(Tool::Eraser);
            } else if pressed_once(ctx, egui::Key::V) {
                self.set_tool(Tool::Select);
            } else if pressed_once(ctx, egui::Key::T) {
                self.add_text(None, ctx);
                self.set_tool(Tool::Select);
            }
            if let Some(color) = palette_key(ctx) {
                self.set_color(color);
            }
            if pressed_once(ctx, egui::Key::OpenBracket) {
                self.bump_thickness(-0.4);
            }
            if pressed_once(ctx, egui::Key::CloseBracket) {
                self.bump_thickness(0.4);
            }
        }

        if !typing {
            if ctx
                .input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
                && self.selected.is_some()
            {
                self.delete_selected();
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.selected = None;
            }

            if command && self.selected.is_some() {
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    self.reorder_selected(if shift {
                        LayerTo::Front
                    } else {
                        LayerTo::Forward
                    });
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    self.reorder_selected(if shift {
                        LayerTo::Back
                    } else {
                        LayerTo::Backward
                    });
                }
            } else {
                let mut dx = 0.0;
                let mut dy = 0.0;
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                    dx -= 1.0;
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                    dx += 1.0;
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    dy -= 1.0;
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    dy += 1.0;
                }
                if dx != 0.0 || dy != 0.0 {
                    self.nudge_selected(dx, dy, shift);
                }
            }

            if ctx.input(|i| i.key_pressed(egui::Key::Equals) || i.key_pressed(egui::Key::Plus)) {
                self.zoom = (self.zoom * 1.1).clamp(0.25, 12.0);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Minus)) {
                self.zoom = (self.zoom / 1.1).clamp(0.25, 12.0);
            }
        }
    }

    fn typing(&self, ctx: &egui::Context) -> bool {
        self.text_edit_id
            .is_some_and(|id| ctx.memory(|mem| mem.focused() == Some(id)))
    }

    fn nudge_selected(&mut self, dx: f32, dy: f32, shift: bool) {
        let Some(index) = self.selected else {
            return;
        };
        let (pw, ph) = self.page_px();
        let scale = self.view_scale.max(0.05);
        let step = if shift { 12.0 / scale } else { 1.0 / scale }.max(1.0);
        if let Some(item) = self.pages[self.page_idx].items.get_mut(index) {
            let (x, y) = keep_on_page(
                item.x + dx * step,
                item.y + dy * step,
                item.w,
                item.h,
                pw,
                ph,
            );
            item.x = x;
            item.y = y;
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            let loaded = load_dropped(file.as_ref());
            match loaded {
                Ok(img) => self.add_image(img, ctx),
                Err(err) => self.status = format!("{err:#}"),
            }
        }
    }

    fn pick_images(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Add a PNG or JPEG")
            .add_filter("PNG or JPEG", &["png", "jpg", "jpeg"]);
        if let Some(dir) = dirs::picture_dir().or_else(dirs::download_dir) {
            dialog = dialog.set_directory(dir);
        }
        let Some(paths) = dialog.pick_files() else {
            return;
        };
        for path in paths {
            match load_rgba_path(&path) {
                Ok(img) => self.add_image(img, ctx),
                Err(err) => self.status = format!("{err:#}"),
            }
        }
    }

    fn paste(&mut self, ctx: &egui::Context) {
        self.apply_clipboard(ctx, read_clipboard());
    }

    fn paste_shortcut(&mut self, ctx: &egui::Context, typing: bool) {
        let contents = read_clipboard();
        let place_on_page = match &contents {
            ClipboardContents::Image(_) => true,
            ClipboardContents::Text(_) | ClipboardContents::Empty | ClipboardContents::Error => {
                !typing
            }
        };
        if !place_on_page {
            return;
        }
        self.apply_clipboard(ctx, contents);
        let mods = ctx.input(|i| i.modifiers);
        ctx.input_mut(|i| {
            i.consume_key(mods, egui::Key::V);
            i.consume_key(mods, egui::Key::Paste);
            i.consume_key(egui::Modifiers::NONE, egui::Key::Paste);
        });
    }

    fn apply_clipboard(&mut self, ctx: &egui::Context, contents: ClipboardContents) {
        match contents {
            ClipboardContents::Image(img) => self.add_image(img, ctx),
            ClipboardContents::Text(text) => self.add_text(Some(text), ctx),
            ClipboardContents::Empty => {
                self.status = "Clipboard is empty. Copy a screenshot, then press Ctrl+V.".into();
            }
            ClipboardContents::Error => {
                self.status = "Couldn't read the clipboard.".into();
            }
        }
    }

    fn add_image(&mut self, img: RgbaImage, ctx: &egui::Context) {
        let img = limit_long_side(img, MAX_STAMP_PX);
        let (iw, ih) = img.dimensions();
        if iw == 0 || ih == 0 {
            self.status = "That image is empty.".into();
            return;
        }
        let (pw, ph) = self.page_px();
        let (w, h) = default_image_box(pw, ph, iw as f32, ih as f32);
        let nudge = self.take_nudge();
        let (x, y) = place_centered(pw, ph, w, h, self.hover_page, nudge);
        let texture = upload_rgba(ctx, &img, "stamp");
        let aspect = ih as f32 / iw as f32;
        self.push_undo();
        self.set_tool(Tool::Select);
        self.pages[self.page_idx].items.push(Item {
            x,
            y,
            w,
            h,
            dirty: false,
            kind: Kind::Image {
                source: Arc::new(img),
                texture,
                aspect,
            },
        });
        self.selected = Some(self.pages[self.page_idx].items.len() - 1);
        self.focus_text = false;
        self.status = "Image added. Drag it into place, or drag the blue corner to resize.".into();
    }

    fn add_text(&mut self, text: Option<String>, _ctx: &egui::Context) {
        let Some(font) = self.font.clone() else {
            self.status = "Text boxes need a system font, and none was found.".into();
            return;
        };
        let (pw, ph) = self.page_px();
        let size = default_font_px(ph);
        let w = (pw * 0.4).clamp(80.0, 1400.0);
        let h = line_box_height(&font, size);
        let nudge = self.take_nudge();
        let (x, y) = place_top_left(pw, ph, w, h, self.hover_page, nudge);
        let text = text.unwrap_or_default();
        let dirty = !text.trim().is_empty();
        let mut color = self.color;
        color[3] = 255;
        self.push_undo();
        self.pages[self.page_idx].items.push(Item {
            x,
            y,
            w,
            h,
            dirty,
            kind: Kind::Text {
                text,
                size,
                color,
                raster: Arc::new(RgbaImage::new(1, 1)),
                texture: None,
            },
        });
        self.selected = Some(self.pages[self.page_idx].items.len() - 1);
        self.focus_text = true;
        self.status =
            "Text box added. Type in the panel on the right, then drag it into place.".into();
    }

    fn take_nudge(&mut self) -> f32 {
        let nudge = (self.paste_nudge % 8) as f32 * 28.0;
        self.paste_nudge = self.paste_nudge.wrapping_add(1);
        nudge
    }

    fn delete_selected(&mut self) {
        let Some(index) = self.selected else {
            return;
        };
        if index < self.pages[self.page_idx].items.len() {
            self.push_undo();
            self.pages[self.page_idx].items.remove(index);
        }
        self.selected = None;
        self.focus_text = false;
        self.text_edit_id = None;
        self.drag = Drag::None;
        self.status = "Deleted.".into();
    }

    fn set_page(&mut self, idx: usize) {
        if idx == self.page_idx || idx >= self.pages.len() {
            return;
        }
        self.page_idx = idx;
        self.selected = None;
        self.focus_text = false;
        self.text_edit_id = None;
        self.drag = Drag::None;
        self.zoom = 1.0;
        self.pan = egui::Vec2::ZERO;
        self.hover_page = None;
        self.live = None;
    }

    fn reorder_selected(&mut self, to: LayerTo) {
        let Some(index) = self.selected else {
            return;
        };
        let len = self.pages[self.page_idx].items.len();
        let Some(dest) = layer_destination(index, len, to) else {
            return;
        };
        self.push_undo();
        let item = self.pages[self.page_idx].items.remove(index);
        self.pages[self.page_idx].items.insert(dest, item);
        self.selected = Some(dest);
        self.status = match to {
            LayerTo::Front => "Brought to front.".into(),
            LayerTo::Back => "Sent to back.".into(),
            LayerTo::Forward => "Moved forward one layer.".into(),
            LayerTo::Backward => "Moved backward one layer.".into(),
        };
    }

    fn hit_test(&self, page_x: f32, page_y: f32) -> Option<usize> {
        self.pages[self.page_idx]
            .items
            .iter()
            .enumerate()
            .rev()
            .find(|(_, item)| item_contains(item, page_x, page_y))
            .map(|(index, _)| index)
    }

    fn handle_contains(&self, index: usize, pos: egui::Pos2, geom: ViewGeom) -> bool {
        let Some(item) = self.pages[self.page_idx].items.get(index) else {
            return false;
        };
        if matches!(item.kind, Kind::Ink { .. }) {
            return false;
        }
        let rect = item_screen_rect(item, geom.origin, geom.scale);
        handle_screen_rect(rect).contains(pos)
    }

    fn page_px(&self) -> (f32, f32) {
        let page = &self.pages[self.page_idx];
        (page.image.width() as f32, page.image.height() as f32)
    }

    fn rebuild_dirty_text(&mut self, ctx: &egui::Context) {
        let Some(font) = self.font.clone() else {
            return;
        };
        let page = &mut self.pages[self.page_idx];
        for item in &mut page.items {
            if !item.dirty {
                continue;
            }
            let (text, size, color) = match &item.kind {
                Kind::Text {
                    text, size, color, ..
                } => (text.clone(), *size, *color),
                Kind::Image { .. } | Kind::Ink { .. } => {
                    item.dirty = false;
                    continue;
                }
            };
            item.dirty = false;
            if text.trim().is_empty() {
                item.h = line_box_height(&font, size);
                if let Kind::Text {
                    raster, texture, ..
                } = &mut item.kind
                {
                    *raster = Arc::new(RgbaImage::new(1, 1));
                    *texture = None;
                }
                continue;
            }
            let image = rasterize_text(&font, &text, size, item.w, color);
            item.h = image.height() as f32;
            let handle = upload_rgba(ctx, &image, "text");
            if let Kind::Text {
                raster, texture, ..
            } = &mut item.kind
            {
                *raster = Arc::new(image);
                *texture = Some(handle);
            }
        }
    }

    fn set_tool(&mut self, tool: Tool) {
        if self.tool == tool {
            return;
        }
        self.tool = tool;
        self.live = None;
        if !matches!(self.drag, Drag::None) && !matches!(tool, Tool::Select) {
            self.drag = Drag::None;
        }
        self.status = self.tool_help().to_string();
    }

    fn set_color(&mut self, color: [u8; 4]) {
        if same_rgb(self.color, color) {
            return;
        }
        if self.selected_accepts_color() {
            self.push_undo();
        }
        self.color = color;
        self.apply_color_to_selected();
    }

    fn editing_highlighter(&self) -> bool {
        if self.tool == Tool::Highlighter {
            return true;
        }
        self.selected_ink_highlighter() == Some(true) && self.tool == Tool::Select
    }

    fn selected_ink_highlighter(&self) -> Option<bool> {
        let index = self.selected?;
        match self.pages[self.page_idx]
            .items
            .get(index)
            .map(|item| &item.kind)
        {
            Some(Kind::Ink { highlighter, .. }) => Some(*highlighter),
            _ => None,
        }
    }

    fn set_thickness(&mut self, pt: f32) {
        let highlight = self.editing_highlighter();
        let pt = if highlight {
            pt.clamp(8.0, 32.0)
        } else {
            pt.clamp(0.8, 10.0)
        };
        let old = if highlight {
            self.highlight_pt
        } else {
            self.pen_pt
        };
        if (old - pt).abs() < 0.05 {
            return;
        }
        let can_restyle = self.selected_ink_highlighter() == Some(highlight);
        if can_restyle && !self.slider_undo {
            self.push_undo();
            self.slider_undo = true;
        }
        if highlight {
            self.highlight_pt = pt;
        } else {
            self.pen_pt = pt;
        }
        if can_restyle {
            if let Some(index) = self.selected_ink() {
                self.set_ink_width(index, self.pt_to_px(pt));
            }
        }
    }

    fn bump_thickness(&mut self, delta: f32) {
        let next = match self.tool {
            Tool::Highlighter => self.highlight_pt + delta * 2.0,
            _ => self.pen_pt + delta,
        };
        self.set_thickness(next);
    }

    fn selected_accepts_color(&self) -> bool {
        self.selected
            .and_then(|index| self.pages[self.page_idx].items.get(index))
            .is_some_and(|item| matches!(item.kind, Kind::Text { .. } | Kind::Ink { .. }))
    }

    fn selected_ink(&self) -> Option<usize> {
        self.selected.filter(|index| {
            self.pages[self.page_idx]
                .items
                .get(*index)
                .is_some_and(|item| matches!(item.kind, Kind::Ink { .. }))
        })
    }

    fn apply_color_to_selected(&mut self) {
        let Some(index) = self.selected else {
            return;
        };
        let rgb = self.color;
        let Some(item) = self.pages[self.page_idx].items.get_mut(index) else {
            return;
        };
        match &mut item.kind {
            Kind::Text { color, .. } => {
                let next = [rgb[0], rgb[1], rgb[2], 255];
                if *color != next {
                    *color = next;
                    item.dirty = true;
                }
            }
            Kind::Ink {
                color, highlighter, ..
            } => {
                *color = [rgb[0], rgb[1], rgb[2], if *highlighter { 120 } else { 255 }];
            }
            Kind::Image { .. } => {}
        }
    }

    fn set_ink_width(&mut self, index: usize, width: f32) {
        let Some(item) = self.pages[self.page_idx].items.get_mut(index) else {
            return;
        };
        let Kind::Ink {
            points,
            color,
            highlighter,
            ..
        } = &item.kind
        else {
            return;
        };
        let page_points: Vec<(f32, f32)> = points
            .iter()
            .map(|(x, y)| (item.x + x, item.y + y))
            .collect();
        let color = *color;
        let highlighter = *highlighter;
        let replacement = make_ink_item(page_points, color, width, highlighter);
        self.pages[self.page_idx].items[index] = replacement;
    }

    fn tool_width_px(&self) -> f32 {
        let pt = match self.tool {
            Tool::Highlighter => self.highlight_pt,
            Tool::Eraser => (self.pen_pt * 4.5).clamp(3.0, 36.0),
            _ => self.pen_pt,
        };
        self.pt_to_px(pt)
    }

    fn pt_to_px(&self, pt: f32) -> f32 {
        let (_, page_h) = self.page_px();
        (pt * page_h / 792.0).clamp(1.4, page_h * 0.2)
    }

    fn start_stroke(&mut self, x: f32, y: f32) {
        let highlighter = self.tool == Tool::Highlighter;
        let mut color = self.color;
        color[3] = if highlighter { 120 } else { 255 };
        let width = self.pt_to_px(if highlighter {
            self.highlight_pt
        } else {
            self.pen_pt
        });
        self.selected = None;
        self.focus_text = false;
        self.live = Some(LiveStroke {
            points: vec![(x, y)],
            color,
            width,
            highlighter,
        });
        self.drag = Drag::Draw;
    }

    fn commit_live_stroke(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };
        if let Some((x, y)) = self.hover_page {
            extend_stroke(&mut live.points, x, y, 0.4);
        }
        if live.points.is_empty() {
            return;
        }
        self.push_undo();
        let item = make_ink_item(live.points, live.color, live.width, live.highlighter);
        self.pages[self.page_idx].items.push(item);
        self.selected = Some(self.pages[self.page_idx].items.len() - 1);
        self.status = "Stroke added. Press V to move it.".into();
    }

    fn erase_at(&mut self, x: f32, y: f32) {
        let radius = self.tool_width_px();
        if !self.ink_hit(x, y, radius) {
            return;
        }
        if !self.gesture_undo {
            self.push_undo();
            self.gesture_undo = true;
        }
        self.selected = None;
        let items = std::mem::take(&mut self.pages[self.page_idx].items);
        let mut next = Vec::with_capacity(items.len());
        for item in items {
            let Item {
                x: item_x,
                y: item_y,
                w,
                h,
                dirty,
                kind,
            } = item;
            let Kind::Ink {
                points,
                color,
                width,
                highlighter,
            } = kind
            else {
                next.push(Item {
                    x: item_x,
                    y: item_y,
                    w,
                    h,
                    dirty,
                    kind,
                });
                continue;
            };
            let page_points: Vec<(f32, f32)> = points
                .iter()
                .map(|(px, py)| (px + item_x, py + item_y))
                .collect();
            let reach = radius + width * 0.45;
            let runs = split_stroke(&page_points, x, y, reach);
            if runs.len() == 1 && runs[0].len() == page_points.len() {
                next.push(Item {
                    x: item_x,
                    y: item_y,
                    w,
                    h,
                    dirty,
                    kind: Kind::Ink {
                        points,
                        color,
                        width,
                        highlighter,
                    },
                });
            } else {
                for run in runs {
                    next.push(make_ink_item(run, color, width, highlighter));
                }
            }
        }
        self.pages[self.page_idx].items = next;
        self.status = "Erased.".into();
    }

    fn ink_hit(&self, x: f32, y: f32, radius: f32) -> bool {
        self.pages[self.page_idx].items.iter().any(|item| {
            let Kind::Ink { points, width, .. } = &item.kind else {
                return false;
            };
            let reach = radius + width * 0.45;
            points.iter().any(|(px, py)| {
                let dx = x - (item.x + px);
                let dy = y - (item.y + py);
                dx * dx + dy * dy <= reach * reach
            }) || points.windows(2).any(|pair| {
                dist_point_seg(
                    x,
                    y,
                    item.x + pair[0].0,
                    item.y + pair[0].1,
                    item.x + pair[1].0,
                    item.y + pair[1].1,
                ) <= reach
            })
        })
    }

    fn push_undo(&mut self) {
        let page = self.page_idx;
        let items = self.pages[page].items.clone();
        self.undo.push(Snapshot { page, items });
        self.redo.clear();
        if self.undo.len() > 30 {
            self.undo.remove(0);
        }
    }

    fn undo_edit(&mut self) {
        let Some(snapshot) = self.undo.pop() else {
            self.status = "Nothing to undo.".into();
            return;
        };
        self.store_redo(snapshot.page);
        self.pages[snapshot.page].items = snapshot.items;
        self.page_idx = snapshot.page;
        self.selected = None;
        self.live = None;
        self.status = "Undid the last change.".into();
    }

    fn redo_edit(&mut self) {
        let Some(snapshot) = self.redo.pop() else {
            self.status = "Nothing to redo.".into();
            return;
        };
        let page = snapshot.page;
        let current = std::mem::take(&mut self.pages[page].items);
        self.undo.push(Snapshot {
            page,
            items: current,
        });
        self.pages[page].items = snapshot.items;
        self.page_idx = page;
        self.selected = None;
        self.status = "Redid the change.".into();
    }

    fn store_redo(&mut self, page: usize) {
        let items = self.pages[page].items.clone();
        self.redo.push(Snapshot { page, items });
    }

    fn finish(&mut self, ctx: &egui::Context) {
        self.status = "Saving PDF…".into();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn persist(&mut self) {
        if self.persisted {
            return;
        }
        if self
            .pages
            .iter()
            .any(|page| page.items.iter().any(item_is_visible))
        {
            println!("Flattening edits onto the scan…");
        }
        let pages = std::mem::take(&mut self.pages)
            .into_iter()
            .map(flatten_page)
            .collect();
        *self.slot.lock().unwrap_or_else(|err| err.into_inner()) = Some(pages);
        self.persisted = true;
    }
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_frame(ui);
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }
}

impl Drop for EditorApp {
    fn drop(&mut self) {
        self.persist();
    }
}

fn draw_item(painter: &egui::Painter, item: &Item, geom: ViewGeom, uv: egui::Rect) {
    match &item.kind {
        Kind::Image { texture, .. } => {
            let rect = item_screen_rect(item, geom.origin, geom.scale);
            painter.image(texture.id(), rect, uv, egui::Color32::WHITE);
        }
        Kind::Ink {
            points,
            color,
            width,
            ..
        } => {
            let page_points: Vec<(f32, f32)> = points
                .iter()
                .map(|(x, y)| (item.x + x, item.y + y))
                .collect();
            draw_page_stroke(painter, &page_points, *color, *width, geom);
        }
        Kind::Text {
            text,
            size,
            texture,
            ..
        } => {
            let rect = item_screen_rect(item, geom.origin, geom.scale);
            if text.trim().is_empty() {
                painter.rect_filled(
                    rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(47, 111, 235, 28),
                );
                painter.text(
                    rect.left_top() + egui::vec2(4.0, 2.0),
                    egui::Align2::LEFT_TOP,
                    "Text",
                    egui::FontId::proportional((*size * geom.scale).clamp(10.0, 72.0)),
                    egui::Color32::from_rgba_unmultiplied(70, 70, 70, 170),
                );
            } else if let Some(texture) = texture {
                painter.image(texture.id(), rect, uv, egui::Color32::WHITE);
            }
        }
    }
}

fn item_screen_rect(item: &Item, origin: egui::Pos2, scale: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        origin + egui::vec2(item.x, item.y) * scale,
        egui::vec2(item.w.max(1.0), item.h.max(1.0)) * scale,
    )
}

fn handle_screen_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_center_size(rect.right_bottom(), egui::vec2(14.0, 14.0))
}

fn pressed_once(ctx: &egui::Context, key: egui::Key) -> bool {
    ctx.input(|input| {
        input.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: pressed,
                    pressed: true,
                    repeat: false,
                    ..
                } if *pressed == key
            )
        })
    })
}

fn to_page(pos: egui::Pos2, geom: ViewGeom) -> (f32, f32) {
    let rel = (pos - geom.origin) / geom.scale;
    (rel.x, rel.y)
}

fn item_is_visible(item: &Item) -> bool {
    match &item.kind {
        Kind::Image { .. } => true,
        Kind::Text { text, .. } => !text.trim().is_empty(),
        Kind::Ink { points, .. } => !points.is_empty(),
    }
}

fn flatten_page(page: Page) -> RgbImage {
    let stamps = stamps_from(&page.items);
    if stamps.is_empty() {
        page.image
    } else {
        burn(&page.image, &stamps)
    }
}

struct Stamp {
    image: RgbaImage,
    x: i64,
    y: i64,
}

fn stamps_from(items: &[Item]) -> Vec<Stamp> {
    items
        .iter()
        .filter_map(|item| match &item.kind {
            Kind::Image { source, .. } => {
                let w = item.w.round().clamp(1.0, MAX_STAMP_PX as f32) as u32;
                let h = item.h.round().clamp(1.0, MAX_STAMP_PX as f32) as u32;
                Some(Stamp {
                    image: resize_rgba(source, w, h),
                    x: item.x.round() as i64,
                    y: item.y.round() as i64,
                })
            }
            Kind::Text { text, raster, .. } => {
                let placeholder = raster.width() <= 1 && raster.height() <= 1;
                if text.trim().is_empty() || placeholder {
                    None
                } else {
                    Some(Stamp {
                        image: (**raster).clone(),
                        x: item.x.round() as i64,
                        y: item.y.round() as i64,
                    })
                }
            }
            Kind::Ink {
                points,
                color,
                width,
                ..
            } => {
                if points.is_empty() {
                    None
                } else {
                    let image = rasterize_ink(item.w, item.h, points, *width, *color);
                    if image.pixels().all(|pixel| pixel[3] == 0) {
                        None
                    } else {
                        Some(Stamp {
                            image,
                            x: item.x.round() as i64,
                            y: item.y.round() as i64,
                        })
                    }
                }
            }
        })
        .collect()
}

fn burn(base: &RgbImage, stamps: &[Stamp]) -> RgbImage {
    if stamps.is_empty() {
        return base.clone();
    }
    let mut canvas = image::DynamicImage::ImageRgb8(base.clone()).into_rgba8();
    for stamp in stamps {
        image::imageops::overlay(&mut canvas, &stamp.image, stamp.x, stamp.y);
    }
    image::DynamicImage::ImageRgba8(canvas).into_rgb8()
}

fn resize_rgba(img: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    let w = w.max(1);
    let h = h.max(1);
    if img.width() == w && img.height() == h {
        img.clone()
    } else {
        image::imageops::resize(img, w, h, image::imageops::FilterType::CatmullRom)
    }
}

fn default_image_box(page_w: f32, page_h: f32, image_w: f32, image_h: f32) -> (f32, f32) {
    let max_w = (page_w * 0.42).max(1.0);
    let max_h = (page_h * 0.42).max(1.0);
    let scale = (max_w / image_w.max(1.0))
        .min(max_h / image_h.max(1.0))
        .min(1.0)
        .max(0.01);
    ((image_w * scale).max(1.0), (image_h * scale).max(1.0))
}

fn default_font_px(page_h: f32) -> f32 {
    (page_h / 45.0).clamp(28.0, 120.0)
}

fn place_centered(
    page_w: f32,
    page_h: f32,
    w: f32,
    h: f32,
    cursor: Option<(f32, f32)>,
    nudge: f32,
) -> (f32, f32) {
    let (cx, cy) = cursor.unwrap_or((page_w * 0.5, page_h * 0.5));
    keep_on_page(
        cx - w * 0.5 + nudge,
        cy - h * 0.5 + nudge,
        w,
        h,
        page_w,
        page_h,
    )
}

fn place_top_left(
    page_w: f32,
    page_h: f32,
    w: f32,
    h: f32,
    cursor: Option<(f32, f32)>,
    nudge: f32,
) -> (f32, f32) {
    let (x, y) = cursor.unwrap_or((page_w * 0.08, page_h * 0.08));
    keep_on_page(x + nudge, y + nudge, w, h, page_w, page_h)
}

/// Keeps at least ~24px of the item on the page so a drag can't lose it.
fn keep_on_page(x: f32, y: f32, w: f32, h: f32, page_w: f32, page_h: f32) -> (f32, f32) {
    let keep_x = 24.0_f32.min(w.max(1.0));
    let keep_y = 24.0_f32.min(h.max(1.0));
    let min_x = keep_x - w;
    let max_x = page_w - keep_x;
    let min_y = keep_y - h;
    let max_y = page_h - keep_y;
    (
        clamp_range(x, min_x.min(max_x), min_x.max(max_x)),
        clamp_range(y, min_y.min(max_y), min_y.max(max_y)),
    )
}

fn clamp_range(value: f32, min: f32, max: f32) -> f32 {
    if !value.is_finite() {
        return min;
    }
    if min > max {
        min
    } else {
        value.clamp(min, max)
    }
}

fn upload_page_texture(ctx: &egui::Context, img: &RgbImage, name: &str) -> egui::TextureHandle {
    const MAX: u32 = 4096;
    let (w, h) = img.dimensions();
    if w.max(h) <= MAX {
        return upload_rgb(ctx, img, name);
    }
    let scale = MAX as f32 / w.max(h) as f32;
    let nw = ((w as f32) * scale).round().max(1.0) as u32;
    let nh = ((h as f32) * scale).round().max(1.0) as u32;
    let small = image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle);
    upload_rgb(ctx, &small, name)
}

fn upload_rgb(ctx: &egui::Context, img: &RgbImage, name: &str) -> egui::TextureHandle {
    let color =
        egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw());
    ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
}

fn upload_rgba(ctx: &egui::Context, img: &RgbaImage, name: &str) -> egui::TextureHandle {
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [img.width() as usize, img.height() as usize],
        img.as_raw(),
    );
    ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
}

fn load_rgba_path(path: &Path) -> Result<RgbaImage> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png" | "jpg" | "jpeg") => {}
        _ => anyhow::bail!("only PNG and JPEG images can be added"),
    }
    let img = image::open(path).with_context(|| format!("couldn't open {}", path.display()))?;
    Ok(limit_long_side(img.to_rgba8(), MAX_STAMP_PX))
}

fn load_dropped(file: &dyn egui::DroppedFile) -> Result<RgbaImage> {
    let path = file.path();
    if let Ok(img) = load_rgba_path(path) {
        return Ok(img);
    }
    let bytes = file.bytes().map_err(|err| anyhow::anyhow!(err))?;
    load_rgba_bytes(&bytes)
}

fn load_rgba_bytes(bytes: &[u8]) -> Result<RgbaImage> {
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png | image::ImageFormat::Jpeg) => {}
        _ => anyhow::bail!("only PNG and JPEG images can be added"),
    }
    let img = image::load_from_memory(bytes).context("couldn't read that image")?;
    Ok(limit_long_side(img.to_rgba8(), MAX_STAMP_PX))
}

fn limit_long_side(img: RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let long = w.max(h);
    if long <= max_side || long == 0 {
        return img;
    }
    let scale = max_side as f32 / long as f32;
    let nw = ((w as f32) * scale).round().max(1.0) as u32;
    let nh = ((h as f32) * scale).round().max(1.0) as u32;
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
}

enum ClipboardContents {
    Image(RgbaImage),
    Text(String),
    Empty,
    Error,
}

fn read_clipboard() -> ClipboardContents {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return ClipboardContents::Error;
    };
    if let Ok(data) = clipboard.get_image() {
        return match rgba_from_clipboard(data) {
            Some(img) => ClipboardContents::Image(img),
            None => ClipboardContents::Error,
        };
    }
    // A failed image read can leave the Windows clipboard open; start over
    // before asking for text.
    drop(clipboard);
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return ClipboardContents::Error;
    };
    match clipboard.get_text() {
        Ok(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                ClipboardContents::Empty
            } else {
                ClipboardContents::Text(text)
            }
        }
        Err(_) => ClipboardContents::Empty,
    }
}

fn rgba_from_clipboard(data: arboard::ImageData<'_>) -> Option<RgbaImage> {
    let w = u32::try_from(data.width).ok()?;
    let h = u32::try_from(data.height).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let need = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    let bytes = data.bytes.into_owned();
    if bytes.len() < need {
        return None;
    }
    let bytes = if bytes.len() == need {
        bytes
    } else {
        bytes[..need].to_vec()
    };
    let mut img = RgbaImage::from_raw(w, h, bytes)?;
    // Windows clipboard DIBs often store a screenshot with the alpha channel
    // left at 0. Treating that as transparency would paste an invisible image.
    force_visible_alpha(&mut img);
    Some(img)
}

fn force_visible_alpha(img: &mut RgbaImage) {
    let any_visible = img.pixels().any(|pixel| pixel[3] > 0);
    if !any_visible {
        for pixel in img.pixels_mut() {
            pixel[3] = 255;
        }
    }
}

fn load_font() -> Option<FontArc> {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        r"C:\Windows\Fonts\calibri.ttf",
        r"C:\Windows\Fonts\verdana.ttf",
    ];
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if let Ok(font) = FontArc::try_from_vec(bytes) {
            return Some(font);
        }
    }
    None
}

fn line_box_height(font: &FontArc, size: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size.max(8.0)));
    (scaled.ascent() - scaled.descent()).ceil().max(8.0) + 2.0
}

fn measure_text(font: &FontArc, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size.max(8.0)));
    let mut width = 0.0;
    let mut previous = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(prev) = previous {
            width += scaled.kern(prev, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

fn wrap_lines(font: &FontArc, text: &str, size: f32, max_width: f32) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let words: Vec<&str> = paragraph.split_whitespace().collect();
        if words.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut width = 0.0_f32;
        for word in words {
            for piece in split_word(font, size, word, max_width) {
                let piece_w = measure_text(font, size, &piece);
                if current.is_empty() {
                    current = piece;
                    width = piece_w;
                    continue;
                }
                let space_w = measure_text(font, size, " ");
                if width + space_w + piece_w <= max_width {
                    current.push(' ');
                    current.push_str(&piece);
                    width += space_w + piece_w;
                } else {
                    lines.push(std::mem::take(&mut current));
                    current = piece;
                    width = piece_w;
                }
            }
        }
        lines.push(current);
    }
    lines
}

fn split_word(font: &FontArc, size: f32, word: &str, max_width: f32) -> Vec<String> {
    if word.is_empty() {
        return Vec::new();
    }
    if measure_text(font, size, word) <= max_width {
        return vec![word.to_string()];
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in word.chars() {
        let mut trial = current.clone();
        trial.push(ch);
        if current.is_empty() || measure_text(font, size, &trial) <= max_width {
            current = trial;
        } else {
            parts.push(std::mem::take(&mut current));
            current.push(ch);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn rasterize_text(
    font: &FontArc,
    text: &str,
    size: f32,
    max_width: f32,
    color: [u8; 4],
) -> RgbaImage {
    let size = size.clamp(8.0, 400.0);
    let max_width = max_width.clamp(8.0, 8000.0);
    let line_h = line_box_height(font, size);
    let lines = wrap_lines(font, text, size, max_width);
    let width = max_width.ceil() as u32;
    let height = ((lines.len().max(1) as f32) * line_h)
        .ceil()
        .clamp(1.0, 8000.0) as u32;
    let mut image = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 0]));
    for (i, line) in lines.iter().enumerate() {
        let y = i as f32 * line_h;
        if y >= height as f32 {
            break;
        }
        draw_text_line(&mut image, font, size, 0.0, y, line, color);
    }
    image
}

fn draw_text_line(
    image: &mut RgbaImage,
    font: &FontArc,
    size: f32,
    x: f32,
    y_top: f32,
    text: &str,
    color: [u8; 4],
) {
    let scale = PxScale::from(size.max(8.0));
    let scaled = font.as_scaled(scale);
    let mut caret = x;
    let baseline = y_top + scaled.ascent();
    let mut previous = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(prev) = previous {
            caret += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(caret, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                if coverage <= 0.0 {
                    return;
                }
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px < 0 || py < 0 {
                    return;
                }
                let (px, py) = (px as u32, py as u32);
                if px >= image.width() || py >= image.height() {
                    return;
                }
                blend_coverage(image.get_pixel_mut(px, py), coverage, color);
            });
        }
        caret += scaled.h_advance(id);
        previous = Some(id);
    }
}

fn color_swatch(ui: &mut egui::Ui, color: [u8; 4], selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.circle_filled(rect.center(), 10.0, color32(color));
        let ring = if selected {
            egui::Stroke::new(2.5, egui::Color32::from_rgb(37, 99, 235))
        } else if color[0] > 240 && color[1] > 240 && color[2] > 240 {
            egui::Stroke::new(1.0, egui::Color32::from_gray(160))
        } else {
            egui::Stroke::new(1.0, egui::Color32::from_black_alpha(30))
        };
        painter.circle_stroke(rect.center(), if selected { 12.0 } else { 10.0 }, ring);
    }
    response
}

fn same_rgb(a: [u8; 4], b: [u8; 4]) -> bool {
    a[0] == b[0] && a[1] == b[1] && a[2] == b[2]
}

fn palette_key(ctx: &egui::Context) -> Option<[u8; 4]> {
    const KEYS: [egui::Key; 10] = [
        egui::Key::Num1,
        egui::Key::Num2,
        egui::Key::Num3,
        egui::Key::Num4,
        egui::Key::Num5,
        egui::Key::Num6,
        egui::Key::Num7,
        egui::Key::Num8,
        egui::Key::Num9,
        egui::Key::Num0,
    ];
    KEYS.iter()
        .position(|key| pressed_once(ctx, *key))
        .map(|index| PALETTE[index])
}

fn color32(color: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}

fn extend_stroke(points: &mut Vec<(f32, f32)>, x: f32, y: f32, min_dist: f32) {
    if let Some(&(px, py)) = points.last() {
        let dx = x - px;
        let dy = y - py;
        if dx * dx + dy * dy < min_dist * min_dist {
            return;
        }
    }
    points.push((x, y));
}

fn draw_page_stroke(
    painter: &egui::Painter,
    points: &[(f32, f32)],
    color: [u8; 4],
    width: f32,
    geom: ViewGeom,
) {
    if points.is_empty() {
        return;
    }
    let radius = (width * 0.5 * geom.scale).max(0.6);
    let paint = color32(color);
    let mut previous = None;
    for &(x, y) in points {
        let at = geom.origin + egui::vec2(x, y) * geom.scale;
        if let Some(from) = previous {
            painter.line_segment([from, at], egui::Stroke::new(radius * 2.0, paint));
        }
        painter.circle_filled(at, radius, paint);
        previous = Some(at);
    }
}

fn item_contains(item: &Item, page_x: f32, page_y: f32) -> bool {
    match &item.kind {
        Kind::Ink { points, width, .. } => {
            let reach = width * 0.5 + 6.0;
            let near_point = points.iter().any(|(x, y)| {
                let dx = page_x - (item.x + x);
                let dy = page_y - (item.y + y);
                dx * dx + dy * dy <= reach * reach
            });
            near_point
                || points.windows(2).any(|pair| {
                    dist_point_seg(
                        page_x,
                        page_y,
                        item.x + pair[0].0,
                        item.y + pair[0].1,
                        item.x + pair[1].0,
                        item.y + pair[1].1,
                    ) <= reach
                })
        }
        _ => {
            page_x >= item.x
                && page_y >= item.y
                && page_x <= item.x + item.w
                && page_y <= item.y + item.h
        }
    }
}

fn make_ink_item(points: Vec<(f32, f32)>, color: [u8; 4], width: f32, highlighter: bool) -> Item {
    let (x, y, w, h, local) = normalize_ink(&points, width);
    Item {
        x,
        y,
        w,
        h,
        dirty: false,
        kind: Kind::Ink {
            points: local,
            color,
            width,
            highlighter,
        },
    }
}

fn normalize_ink(points: &[(f32, f32)], width: f32) -> (f32, f32, f32, f32, Vec<(f32, f32)>) {
    let pad = width * 0.5 + 2.0;
    let min_x = points.iter().map(|point| point.0).fold(f32::MAX, f32::min) - pad;
    let min_y = points.iter().map(|point| point.1).fold(f32::MAX, f32::min) - pad;
    let max_x = points.iter().map(|point| point.0).fold(f32::MIN, f32::max) + pad;
    let max_y = points.iter().map(|point| point.1).fold(f32::MIN, f32::max) + pad;
    let local = points.iter().map(|(x, y)| (x - min_x, y - min_y)).collect();
    (
        min_x,
        min_y,
        (max_x - min_x).max(1.0),
        (max_y - min_y).max(1.0),
        local,
    )
}

fn rasterize_ink(
    width: f32,
    height: f32,
    points: &[(f32, f32)],
    stroke: f32,
    color: [u8; 4],
) -> RgbaImage {
    let w = width.round().clamp(1.0, 8000.0) as u32;
    let h = height.round().clamp(1.0, 8000.0) as u32;
    let mut cover = vec![0u8; (w as usize) * (h as usize)];
    let radius = (stroke * 0.5).max(0.6);
    if points.len() == 1 {
        stamp_disc(&mut cover, w, h, points[0].0, points[0].1, radius);
    } else {
        for pair in points.windows(2) {
            stamp_segment(
                &mut cover, w, h, pair[0].0, pair[0].1, pair[1].0, pair[1].1, radius,
            );
        }
    }
    let mut image = RgbaImage::new(w, h);
    for (index, coverage) in cover.into_iter().enumerate() {
        if coverage == 0 {
            continue;
        }
        let alpha = ((color[3] as u16 * coverage as u16) / 255) as u8;
        if alpha == 0 {
            continue;
        }
        let x = (index as u32) % w;
        let y = (index as u32) / w;
        image.put_pixel(x, y, Rgba([color[0], color[1], color[2], alpha]));
    }
    image
}

fn stamp_segment(
    cover: &mut [u8],
    w: u32,
    h: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt();
    let step = (radius * 0.45).max(0.45);
    let steps = (len / step).ceil().max(1.0) as i32;
    for step_index in 0..=steps {
        let t = step_index as f32 / steps as f32;
        stamp_disc(cover, w, h, x0 + dx * t, y0 + dy * t, radius);
    }
}

fn stamp_disc(cover: &mut [u8], w: u32, h: u32, cx: f32, cy: f32, radius: f32) {
    let reach = radius.ceil() as i32 + 1;
    let x_start = (cx as i32 - reach).max(0);
    let y_start = (cy as i32 - reach).max(0);
    let x_end = (cx as i32 + reach).min(w as i32 - 1);
    let y_end = (cy as i32 + reach).min(h as i32 - 1);
    for y in y_start..=y_end {
        for x in x_start..=x_end {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let edge = (radius + 0.65 - dist).clamp(0.0, 1.0);
            if edge <= 0.0 {
                continue;
            }
            let value = (edge * 255.0) as u8;
            let index = (y as u32 * w + x as u32) as usize;
            if value > cover[index] {
                cover[index] = value;
            }
        }
    }
}

fn split_stroke(points: &[(f32, f32)], cx: f32, cy: f32, radius: f32) -> Vec<Vec<(f32, f32)>> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut dead = vec![false; points.len()];
    for (index, &(x, y)) in points.iter().enumerate() {
        if dist(x, y, cx, cy) <= radius {
            dead[index] = true;
        }
    }
    for index in 0..points.len().saturating_sub(1) {
        if dist_point_seg(
            cx,
            cy,
            points[index].0,
            points[index].1,
            points[index + 1].0,
            points[index + 1].1,
        ) <= radius
        {
            dead[index] = true;
            dead[index + 1] = true;
        }
    }
    let mut runs = Vec::new();
    let mut current = Vec::new();
    for (index, point) in points.iter().enumerate() {
        if dead[index] {
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(*point);
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// Where `index` should land in a list of `len` items. `None` when it is already there.
fn layer_destination(index: usize, len: usize, to: LayerTo) -> Option<usize> {
    if len < 2 || index >= len {
        return None;
    }
    let dest = match to {
        LayerTo::Front => len - 1,
        LayerTo::Back => 0,
        LayerTo::Forward => (index + 1).min(len - 1),
        LayerTo::Backward => index.saturating_sub(1),
    };
    (dest != index).then_some(dest)
}

fn dist(x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let dx = x0 - x1;
    let dy = y0 - y1;
    (dx * dx + dy * dy).sqrt()
}

fn dist_point_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let abx = bx - ax;
    let aby = by - ay;
    let len2 = abx * abx + aby * aby;
    if len2 < 1.0e-4 {
        return dist(px, py, ax, ay);
    }
    let t = (((px - ax) * abx + (py - ay) * aby) / len2).clamp(0.0, 1.0);
    dist(px, py, ax + abx * t, ay + aby * t)
}

fn blend_coverage(dst: &mut Rgba<u8>, coverage: f32, color: [u8; 4]) {
    let alpha = coverage.clamp(0.0, 1.0) * (color[3] as f32 / 255.0);
    if alpha <= 0.0 {
        return;
    }
    let inv = 1.0 - alpha;
    for channel in 0..3 {
        dst[channel] = (color[channel] as f32 * alpha + dst[channel] as f32 * inv).round() as u8;
    }
    let dst_a = dst[3] as f32 / 255.0;
    let out_a = alpha + dst_a * inv;
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::{
        LayerTo, Stamp, burn, default_image_box, force_visible_alpha, keep_on_page,
        layer_destination, load_font, measure_text, rasterize_ink, rasterize_text, split_stroke,
        wrap_lines,
    };
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    #[test]
    fn large_images_default_to_a_fraction_of_the_page() {
        let (w, h) = default_image_box(1000.0, 2000.0, 2000.0, 1000.0);
        assert!(w <= 1000.0 * 0.42 + 0.5);
        assert!((w / h - 2.0).abs() < 0.02, "{w}x{h}");
    }

    #[test]
    fn small_images_keep_their_pixel_size() {
        let (w, h) = default_image_box(4000.0, 5000.0, 400.0, 200.0);
        assert!((w - 400.0).abs() < 0.5);
        assert!((h - 200.0).abs() < 0.5);
    }

    #[test]
    fn item_cannot_be_dragged_fully_off_the_page() {
        let (x, y) = keep_on_page(-500.0, 10_000.0, 100.0, 80.0, 400.0, 500.0);
        assert!((x - (24.0 - 100.0)).abs() < 0.01);
        assert!((y - (500.0 - 24.0)).abs() < 0.01);
    }

    #[test]
    fn blank_clipboard_alpha_becomes_opaque() {
        let mut image = RgbaImage::from_pixel(2, 1, Rgba([1, 2, 3, 0]));
        force_visible_alpha(&mut image);
        assert_eq!(image.get_pixel(0, 0)[3], 255);
        assert_eq!(image.get_pixel(1, 0)[0], 1);

        let mut image = RgbaImage::from_pixel(1, 1, Rgba([4, 5, 6, 128]));
        force_visible_alpha(&mut image);
        assert_eq!(image.get_pixel(0, 0)[3], 128);
    }

    #[test]
    fn burn_places_an_opaque_pixel_and_ignores_stamps_outside_the_page() {
        let base = RgbImage::from_pixel(3, 3, Rgb([255, 255, 255]));
        let dot = RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let out = burn(
            &base,
            &[Stamp {
                image: dot,
                x: 1,
                y: 2,
            }],
        );
        assert_eq!(out.get_pixel(1, 2).0, [10, 20, 30]);
        assert_eq!(out.get_pixel(0, 0).0, [255, 255, 255]);

        let base = RgbImage::from_pixel(2, 2, Rgb([9, 9, 9]));
        let dot = RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 255]));
        let out = burn(
            &base,
            &[Stamp {
                image: dot,
                x: 20,
                y: -5,
            }],
        );
        assert_eq!(out.get_pixel(0, 0).0, [9, 9, 9]);
    }

    #[test]
    fn text_wraps_on_newlines_and_width() {
        let Some(font) = load_font() else {
            return;
        };
        let lines = wrap_lines(&font, "one\ntwo", 32.0, 10_000.0);
        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);

        let word_w = measure_text(&font, 32.0, "aaa");
        let lines = wrap_lines(&font, "aaa aaa", 32.0, word_w + 0.5);
        assert_eq!(lines.len(), 2, "{lines:?}");

        let image = rasterize_text(&font, "Hi", 48.0, 400.0, [0, 0, 0, 255]);
        assert!(image.pixels().any(|pixel| pixel[3] > 0));
        assert!(image.width() >= 8 && image.height() >= 8);
    }

    #[test]
    fn pen_stroke_raster_is_colored_and_leaves_the_corner_clear() {
        let image = rasterize_ink(
            50.0,
            24.0,
            &[(8.0, 12.0), (40.0, 12.0)],
            5.0,
            [200, 10, 10, 255],
        );
        assert!(image.get_pixel(12, 12)[0] > 150);
        assert_eq!(image.get_pixel(0, 0)[3], 0);
    }

    #[test]
    fn eraser_splits_a_stroke_where_it_crosses() {
        let runs = split_stroke(
            &[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0), (30.0, 0.0)],
            15.0,
            0.0,
            3.0,
        );
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], vec![(0.0, 0.0)]);
        assert_eq!(runs[1], vec![(30.0, 0.0)]);
    }

    #[test]
    fn layer_moves_stop_at_the_ends() {
        assert_eq!(layer_destination(0, 3, LayerTo::Forward), Some(1));
        assert_eq!(layer_destination(1, 3, LayerTo::Backward), Some(0));
        assert_eq!(layer_destination(0, 3, LayerTo::Front), Some(2));
        assert_eq!(layer_destination(2, 3, LayerTo::Back), Some(0));
        assert_eq!(layer_destination(2, 3, LayerTo::Front), None);
        assert_eq!(layer_destination(0, 3, LayerTo::Back), None);
        assert_eq!(layer_destination(0, 1, LayerTo::Forward), None);
    }
}
