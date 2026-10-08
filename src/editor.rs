//! Editor opened by `--edit` / `-edit` after a scan.
//!
//! Page coordinates are pixels of the scanned image. The window only scales
//! that image to fit; screenshots and text are stored in those pixels and
//! burned in when the window closes, so the PDF matches what was on the page.

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
            .with_title("Scan editor — Ctrl+V pastes a screenshot")
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
    drag: Drag,
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

struct Item {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    dirty: bool,
    kind: Kind,
}

enum Kind {
    Image {
        source: RgbaImage,
        texture: egui::TextureHandle,
        /// height / width of `source`, so resizing keeps the picture's shape.
        aspect: f32,
    },
    Text {
        text: String,
        size: f32,
        raster: RgbaImage,
        texture: Option<egui::TextureHandle>,
    },
}

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
        let mut status = "Copy a screenshot and press Ctrl+V to drop it on the page.".to_string();
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
            drag: Drag::None,
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
        egui::Panel::right("props")
            .resizable(true)
            .min_size(240.0)
            .default_size(300.0)
            .max_size(420.0)
            .show(ui, |ui| self.props(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.canvas(ui));
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
                .button("Add image…")
                .on_hover_text("Place a PNG or JPEG on this page")
                .clicked()
            {
                self.pick_images(ui.ctx());
            }
            if ui
                .add_enabled(self.font.is_some(), egui::Button::new("Add text"))
                .on_hover_text("Drop a text box on this page")
                .clicked()
            {
                self.add_text(None, ui.ctx());
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
                .add_enabled(self.selected.is_some(), egui::Button::new("Delete"))
                .clicked()
            {
                self.delete_selected();
            }
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
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "Ctrl+V pastes a screenshot · drag to move · blue corner resizes · scroll zooms · drag the page background to pan",
                )
                .weak()
                .small(),
            )
            .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }

    fn props(&mut self, ui: &mut egui::Ui) {
        ui.heading("Selection");
        ui.separator();

        let Some(index) = self.selected else {
            self.text_edit_id = None;
            ui.label("Nothing selected.");
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(
                    "Press Ctrl+V to paste a screenshot, or use Add image / Add text.",
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
            return;
        };
        if index >= self.pages[self.page_idx].items.len() {
            self.selected = None;
            self.text_edit_id = None;
            return;
        }

        let is_text = matches!(
            self.pages[self.page_idx].items[index].kind,
            Kind::Text { .. }
        );

        if is_text {
            let (mut text, mut size) = match &self.pages[self.page_idx].items[index].kind {
                Kind::Text { text, size, .. } => (text.clone(), *size),
                Kind::Image { .. } => (String::new(), 32.0),
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
                egui::Label::new("Drag the blue corner to change how wide the text wraps.")
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
        } else {
            self.text_edit_id = None;
            let (w, h) = {
                let item = &self.pages[self.page_idx].items[index];
                (item.w, item.h)
            };
            ui.label(format!("{w:.0} × {h:.0} px on the page"));
            ui.add(
                egui::Label::new("Drag the image to move it. Drag the blue corner to resize. The picture keeps its shape.")
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        }

        ui.add_space(12.0);
        if ui.button("Delete").clicked() {
            self.delete_selected();
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
            self.drag = Drag::None;
        }

        if any_down {
            if primary_pressed
                && hover.is_some_and(|pos| canvas.contains(pos))
                && matches!(self.drag, Drag::None)
            {
                let pos = hover.unwrap();
                let (page_x, page_y) = to_page(pos, geom);
                let on_handle = self
                    .selected
                    .is_some_and(|index| self.handle_contains(index, pos, geom));
                if on_handle {
                    let index = self.selected.unwrap();
                    self.drag = Drag::Resize { index };
                } else if let Some(hit) = self.hit_test(page_x, page_y) {
                    let index = self.bring_to_front(hit);
                    self.selected = Some(index);
                    self.focus_text = false;
                    let item = &self.pages[self.page_idx].items[index];
                    self.drag = Drag::Move {
                        index,
                        grab_x: page_x - item.x,
                        grab_y: page_y - item.y,
                    };
                } else {
                    self.selected = None;
                    self.focus_text = false;
                    self.drag = Drag::Pan { last: pos };
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

        if matches!(self.drag, Drag::Move { .. }) {
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
                if let Some(item) = self.pages[self.page_idx].items.get_mut(index) {
                    let (x, y) =
                        keep_on_page(page_x - grab_x, page_y - grab_y, item.w, item.h, pw, ph);
                    item.x = x;
                    item.y = y;
                }
            }
            Drag::Resize { index } => self.resize_item(index, page_x, pw, ph),
            Drag::Pan { last } => {
                self.pan += pos - last;
                self.drag = Drag::Pan { last: pos };
            }
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
        if let Some(index) = self.selected {
            if let Some(item) = self.pages[self.page_idx].items.get(index) {
                let rect = item_screen_rect(item, geom.origin, geom.scale);
                painter.rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(47, 111, 235)),
                    egui::StrokeKind::Outside,
                );
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
        self.pages[self.page_idx].items.push(Item {
            x,
            y,
            w,
            h,
            dirty: false,
            kind: Kind::Image {
                source: img,
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
        self.pages[self.page_idx].items.push(Item {
            x,
            y,
            w,
            h,
            dirty,
            kind: Kind::Text {
                text,
                size,
                raster: RgbaImage::new(1, 1),
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
        let items = &mut self.pages[self.page_idx].items;
        if index < items.len() {
            items.remove(index);
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
    }

    fn bring_to_front(&mut self, index: usize) -> usize {
        let items = &mut self.pages[self.page_idx].items;
        if index >= items.len() || index + 1 == items.len() {
            return index.min(items.len().saturating_sub(1));
        }
        let item = items.remove(index);
        items.push(item);
        items.len() - 1
    }

    fn hit_test(&self, page_x: f32, page_y: f32) -> Option<usize> {
        self.pages[self.page_idx]
            .items
            .iter()
            .enumerate()
            .rev()
            .find(|(_, item)| {
                page_x >= item.x
                    && page_y >= item.y
                    && page_x <= item.x + item.w
                    && page_y <= item.y + item.h
            })
            .map(|(index, _)| index)
    }

    fn handle_contains(&self, index: usize, pos: egui::Pos2, geom: ViewGeom) -> bool {
        let Some(item) = self.pages[self.page_idx].items.get(index) else {
            return false;
        };
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
            let (text, size) = match &item.kind {
                Kind::Text { text, size, .. } => (text.clone(), *size),
                Kind::Image { .. } => {
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
                    *raster = RgbaImage::new(1, 1);
                    *texture = None;
                }
                continue;
            }
            let image = rasterize_text(&font, &text, size, item.w);
            item.h = image.height() as f32;
            let handle = upload_rgba(ctx, &image, "text");
            if let Kind::Text {
                raster, texture, ..
            } = &mut item.kind
            {
                *raster = image;
                *texture = Some(handle);
            }
        }
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
                        image: raster.clone(),
                        x: item.x.round() as i64,
                        y: item.y.round() as i64,
                    })
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

fn rasterize_text(font: &FontArc, text: &str, size: f32, max_width: f32) -> RgbaImage {
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
        draw_text_line(&mut image, font, size, 0.0, y, line);
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
                blend_coverage(image.get_pixel_mut(px, py), coverage);
            });
        }
        caret += scaled.h_advance(id);
        previous = Some(id);
    }
}

fn blend_coverage(dst: &mut Rgba<u8>, coverage: f32) {
    let alpha = coverage.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }
    let inv = 1.0 - alpha;
    // Black ink over whatever is already in the raster (usually transparent).
    for channel in 0..3 {
        dst[channel] = (dst[channel] as f32 * inv).round() as u8;
    }
    let dst_a = dst[3] as f32 / 255.0;
    let out_a = alpha + dst_a * inv;
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::{
        Stamp, burn, default_image_box, force_visible_alpha, keep_on_page, load_font, measure_text,
        rasterize_text, wrap_lines,
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

        let image = rasterize_text(&font, "Hi", 48.0, 400.0);
        assert!(image.pixels().any(|pixel| pixel[3] > 0));
        assert!(image.width() >= 8 && image.height() >= 8);
    }
}
