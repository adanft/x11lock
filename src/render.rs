use anyhow::{Context, Result};
use cairo::{Format, ImageSurface};
use chrono::Local;
use pangocairo::functions::{create_layout, show_layout};
use std::fs::File;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as XprotoExt, Gcontext, ImageFormat, Pixmap, Screen, Window,
};
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthFeedback {
    None,
    Message,
}

pub(crate) const COLOR_BASE: u32 = 0x1e1e2e;

const MOCHA_TEXT: (f64, f64, f64) = (0.804, 0.839, 0.957); // #cdd6f4
const MOCHA_MAUVE: (f64, f64, f64) = (0.796, 0.651, 0.969); // #cba6f7
const MOCHA_RED: (f64, f64, f64) = (0.953, 0.545, 0.659); // #f38ba8
const MOCHA_BASE: (f64, f64, f64) = (0.118, 0.118, 0.180); // #1e1e2e

const FONT_FAMILY: &str = "IosevkaTerm Nerd Font";
const INPUT_MIN_WIDTH: f64 = 200.0;
const INPUT_PADDING: f64 = 30.0;
const INPUT_HEIGHT: f64 = 40.0;
const INPUT_RADIUS: f64 = 20.0;
const BORDER_WIDTH: f64 = 2.0;
const DOT_RADIUS: f64 = 5.0;
const DOT_SPACING: f64 = 16.0;

// Opacity constants
const OVERLAY_OPACITY: f64 = 0.4;
const INPUT_BG_OPACITY: f64 = 0.8;

// Layout spacing constants (in pixels)
const TIME_OFFSET_TOP: f64 = 60.0;
const DATE_SPACING: f64 = 8.0;
const INPUT_SPACING: f64 = 24.0;
const ERROR_MSG_SPACING: f64 = 12.0;

// Font sizes (in pixels)
const FONT_SIZE_TIME: f64 = 64.0;
const FONT_SIZE_DATE: f64 = 24.0;
const FONT_SIZE_ERROR: f64 = 16.0;

const WALLPAPER_PATH: &str = ".config/x11lock/wallpaper.png";

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LockState {
    Idle,
    Typing,
    Error,
}

pub(crate) struct LockWindow {
    pub(crate) id: Window,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

pub(crate) struct RenderContext {
    backgrounds: Vec<Option<ImageSurface>>,
    frame_surfaces: Vec<ImageSurface>,
    blit_resources: Vec<BlitResources>,
}

struct BlitResources {
    pixmap: Pixmap,
    gc: Gcontext,
}

impl RenderContext {
    pub(crate) fn load(
        conn: &RustConnection,
        screen: &Screen,
        windows: &[LockWindow],
    ) -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let path = format!("{}/{}", home, WALLPAPER_PATH);

        let mut backgrounds = Vec::with_capacity(windows.len());
        let mut frame_surfaces = Vec::with_capacity(windows.len());
        let mut blit_resources = Vec::with_capacity(windows.len());

        for win in windows {
            frame_surfaces.push(
                ImageSurface::create(Format::ARgb32, i32::from(win.width), i32::from(win.height))
                    .context("Failed to create reusable frame surface")?,
            );

            let pixmap = conn.generate_id()?;
            conn.create_pixmap(
                screen.root_depth,
                pixmap,
                screen.root,
                win.width,
                win.height,
            )?;

            let gc = conn.generate_id()?;
            conn.create_gc(gc, pixmap, &x11rb::protocol::xproto::CreateGCAux::new())?;

            blit_resources.push(BlitResources { pixmap, gc });
        }

        if let Ok(mut file) = File::open(&path) {
            let wallpaper = ImageSurface::create_from_png(&mut file)
                .with_context(|| format!("Failed to decode PNG: {}", path))?;

            for win in windows {
                backgrounds.push(Some(scale_background(&wallpaper, win.width, win.height)?));
            }
        } else {
            backgrounds.resize_with(windows.len(), || None);
        }

        Ok(Self {
            backgrounds,
            frame_surfaces,
            blit_resources,
        })
    }

    pub(crate) fn cleanup(&self, conn: &RustConnection) -> Result<()> {
        for resources in &self.blit_resources {
            conn.free_gc(resources.gc)?;
            conn.free_pixmap(resources.pixmap)?;
        }

        Ok(())
    }
}

fn scale_background(wallpaper: &ImageSurface, width: u16, height: u16) -> Result<ImageSurface> {
    let w = f64::from(width);
    let h = f64::from(height);
    let bg_w = f64::from(wallpaper.width());
    let bg_h = f64::from(wallpaper.height());

    let surface = ImageSurface::create(Format::ARgb32, i32::from(width), i32::from(height))
        .context("Failed to create cached background surface")?;
    let cr = cairo::Context::new(&surface).context("Failed to create Cairo context")?;

    let scale = (w / bg_w).max(h / bg_h);
    let scaled_w = bg_w * scale;
    let scaled_h = bg_h * scale;

    cr.translate((w - scaled_w) / 2.0, (h - scaled_h) / 2.0);
    cr.scale(scale, scale);
    cr.set_source_surface(wallpaper, 0.0, 0.0)
        .context("Failed to set cached background source")?;
    cr.paint().context("Failed to paint cached background")?;
    drop(cr);
    surface.flush();

    Ok(surface)
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

/// Render background (wallpaper or solid color) with overlay
fn render_background(cr: &cairo::Context, bg: &Option<ImageSurface>) -> Result<()> {
    if let Some(bg) = bg {
        cr.set_source_surface(bg, 0.0, 0.0)
            .context("Failed to set background surface")?;
        cr.paint().context("Failed to paint background surface")?;
    } else {
        cr.set_source_rgb(MOCHA_BASE.0, MOCHA_BASE.1, MOCHA_BASE.2);
        cr.paint().context("Failed to paint solid background")?;
    }

    // Semi-transparent overlay
    cr.set_source_rgba(MOCHA_BASE.0, MOCHA_BASE.1, MOCHA_BASE.2, OVERLAY_OPACITY);
    cr.paint().context("Failed to paint background overlay")?;

    Ok(())
}

/// Render clock and date
fn render_time(cr: &cairo::Context, center_x: f64, center_y: f64) -> f64 {
    let now = Local::now();
    let time_str = now.format("%H:%M").to_string();
    let date_str = now.format("%A, %B %d").to_string();

    let layout = create_layout(cr);
    let mut font_desc = pango::FontDescription::new();
    font_desc.set_family(FONT_FAMILY);

    // Time
    font_desc.set_absolute_size(FONT_SIZE_TIME * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font_desc));
    layout.set_text(&time_str);
    let (time_w, time_h) = layout.pixel_size();

    let time_y = center_y - (time_h as f64) - TIME_OFFSET_TOP;
    cr.move_to(center_x - (time_w as f64) / 2.0, time_y);
    cr.set_source_rgb(MOCHA_TEXT.0, MOCHA_TEXT.1, MOCHA_TEXT.2);
    show_layout(cr, &layout);

    // Date
    font_desc.set_absolute_size(FONT_SIZE_DATE * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font_desc));
    layout.set_text(&date_str);
    let (date_w, date_h) = layout.pixel_size();

    let date_y = time_y + (time_h as f64) + DATE_SPACING;
    cr.move_to(center_x - (date_w as f64) / 2.0, date_y);
    cr.set_source_rgb(MOCHA_TEXT.0, MOCHA_TEXT.1, MOCHA_TEXT.2);
    show_layout(cr, &layout);

    date_y + date_h as f64
}

/// Render input box with password dots.
/// The locker intentionally hides typed input during normal operation.
fn render_input_box(
    cr: &cairo::Context,
    center_x: f64,
    date_bottom_y: f64,
    state: LockState,
    password_len: usize,
) -> Result<()> {
    let dots_width = if password_len > 0 {
        (password_len as f64) * DOT_SPACING + INPUT_PADDING * 2.0
    } else {
        0.0
    };
    let input_w = dots_width.max(INPUT_MIN_WIDTH);
    let input_y = date_bottom_y + INPUT_SPACING;
    let input_x = center_x - input_w / 2.0;

    let border_color = match state {
        LockState::Error => MOCHA_RED,
        _ => MOCHA_MAUVE,
    };

    // Fill background
    rounded_rect(cr, input_x, input_y, input_w, INPUT_HEIGHT, INPUT_RADIUS);
    cr.set_source_rgba(MOCHA_BASE.0, MOCHA_BASE.1, MOCHA_BASE.2, INPUT_BG_OPACITY);
    cr.fill().context("Failed to fill input background")?;

    // Draw border
    rounded_rect(cr, input_x, input_y, input_w, INPUT_HEIGHT, INPUT_RADIUS);
    cr.set_source_rgb(border_color.0, border_color.1, border_color.2);
    cr.set_line_width(BORDER_WIDTH);
    cr.stroke().context("Failed to stroke input border")?;

    // Draw dots
    if password_len > 0 {
        let dot_color = match state {
            LockState::Error => MOCHA_RED,
            _ => MOCHA_TEXT,
        };
        cr.set_source_rgb(dot_color.0, dot_color.1, dot_color.2);

        let total_width = (password_len as f64 - 1.0) * DOT_SPACING;
        let first_dot_x = center_x - total_width / 2.0;
        let dot_y = input_y + INPUT_HEIGHT / 2.0;

        for j in 0..password_len {
            let dx = first_dot_x + (j as f64) * DOT_SPACING;
            cr.arc(dx, dot_y, DOT_RADIUS, 0.0, 2.0 * std::f64::consts::PI);
            cr.fill().context("Failed to fill password dot")?;
        }
    }

    Ok(())
}

/// Render authentication error message
fn render_auth_message(cr: &cairo::Context, center_x: f64, input_y: f64, message: &str) {
    let layout = create_layout(cr);
    let mut font_desc = pango::FontDescription::new();
    font_desc.set_family(FONT_FAMILY);
    font_desc.set_absolute_size(FONT_SIZE_ERROR * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font_desc));
    layout.set_text(message);
    let (fail_w, _) = layout.pixel_size();

    let fail_y = input_y + INPUT_HEIGHT + ERROR_MSG_SPACING;
    cr.move_to(center_x - (fail_w as f64) / 2.0, fail_y);
    cr.set_source_rgb(MOCHA_RED.0, MOCHA_RED.1, MOCHA_RED.2);
    show_layout(cr, &layout);
}

/// Blit rendered surface to X11 window
fn blit_to_window(
    conn: &RustConnection,
    root_depth: u8,
    win: &LockWindow,
    resources: &BlitResources,
    surface: &mut ImageSurface,
) -> Result<()> {
    let frame_data = surface.data().context("Failed to read frame data")?;

    conn.put_image(
        ImageFormat::Z_PIXMAP,
        resources.pixmap,
        resources.gc,
        win.width,
        win.height,
        0,
        0,
        0,
        root_depth,
        &frame_data,
    )?;

    conn.change_window_attributes(
        win.id,
        &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
            .background_pixmap(resources.pixmap),
    )?;
    conn.clear_area(false, win.id, 0, 0, win.width, win.height)?;

    Ok(())
}

pub(crate) fn render_frame(
    conn: &RustConnection,
    windows: &[LockWindow],
    render_ctx: &mut RenderContext,
    state: LockState,
    password_len: usize,
    auth_feedback: AuthFeedback,
    auth_message: Option<&str>,
    screen_num: usize,
) -> Result<()> {
    let screen = &conn.setup().roots[screen_num];

    for (i, win) in windows.iter().enumerate() {
        let w = win.width as f64;
        let h = win.height as f64;
        let bg = &render_ctx.backgrounds[i];
        let surface = &mut render_ctx.frame_surfaces[i];
        let resources = &render_ctx.blit_resources[i];

        let cr = cairo::Context::new(&*surface).context("Failed to create Cairo context")?;

        // Render background with overlay
        render_background(&cr, bg)?;

        let center_x = w / 2.0;
        let center_y = h / 2.0;

        // Render clock and date, get bottom position
        let date_bottom_y = render_time(&cr, center_x, center_y);

        // Render input box
        render_input_box(&cr, center_x, date_bottom_y, state, password_len)?;

        // Render auth error message if present
        if auth_feedback == AuthFeedback::Message {
            let input_y = date_bottom_y + INPUT_SPACING;
            let fail_text = auth_message.unwrap_or("Authentication failed");
            render_auth_message(&cr, center_x, input_y, fail_text);
        }

        drop(cr);
        surface.flush();

        // Blit to X11 window
        blit_to_window(conn, screen.root_depth, win, resources, surface)?;
    }

    conn.flush()?;
    Ok(())
}
