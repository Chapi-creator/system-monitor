//! ui.rs — Dibujo Direct2D/DirectWrite del widget nativo.
//!
//! Presupuesto: cero capas intermedias — dibujamos rectángulos redondeados,
//! líneas y texto directamente con D2D. Los hit-rects de los controles se
//! registran durante el propio pase de dibujo (inmediatez pura, sin árbol UI).

use std::sync::Arc;

use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_BOLD,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    IDWriteFactory, IDWriteTextFormat, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
};
use windows_numerics::Vector2;

use sysmon_core::format as fmt;
use sysmon_core::{AppState, MINI_H, WIDGET_H, WIDGET_W};

// ---------------------------------------------------------------------------
// Paleta (port de src/styles.css)
// ---------------------------------------------------------------------------
const fn rgb(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

pub const COL_BG: D2D1_COLOR_F = rgb(0x0a, 0x0c, 0x14);
pub const COL_CARD: D2D1_COLOR_F = rgb(0x12, 0x14, 0x1f);
pub const COL_CARD_HOVER: D2D1_COLOR_F = rgb(0x1a, 0x1e, 0x2e);
pub const COL_BORDER: D2D1_COLOR_F = rgb(0x23, 0x2a, 0x3d);
pub const COL_TRACK: D2D1_COLOR_F = rgb(0x1c, 0x20, 0x30);
pub const COL_TXT: D2D1_COLOR_F = rgb(0xe8, 0xec, 0xf4);
pub const COL_DIM: D2D1_COLOR_F = rgb(0x8b, 0x93, 0xa7);
pub const COL_CYAN: D2D1_COLOR_F = rgb(0x00, 0xff, 0xe5);
pub const COL_MAGENTA: D2D1_COLOR_F = rgb(0xff, 0x2b, 0xd6);
pub const COL_AMBER: D2D1_COLOR_F = rgb(0xff, 0xb0, 0x20);
pub const COL_VIOLET: D2D1_COLOR_F = rgb(0x9d, 0x6b, 0xff);
pub const COL_GREEN: D2D1_COLOR_F = rgb(0x00, 0xff, 0x88);
pub const COL_BLUE: D2D1_COLOR_F = rgb(0x00, 0xc8, 0xff);
pub const COL_ALERT: D2D1_COLOR_F = rgb(0xff, 0x4d, 0x5e);

/// Acciones de los controles golpeables (hit-test registrado al dibujar).
#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    Pin,
    Mode(&'static str),
    Close,
    Kill(u32),
    Expand, // desde mini → dev
}

pub struct HitRect {
    pub rect: RECT_F,
    pub action: Action,
}

#[derive(Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct RECT_F {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RECT_F {
    fn new(l: f32, t: f32, r: f32, b: f32) -> Self {
        Self { l, t, r, b }
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
}

// ---------------------------------------------------------------------------
// Gfx: render target + brushes + formatos de texto
// ---------------------------------------------------------------------------
pub struct Gfx {
    pub rt: ID2D1HwndRenderTarget,
    pub factory: ID2D1Factory,
    pub dwrite: IDWriteFactory,
    b: Brushes,
    f_title: IDWriteTextFormat,
    f_big: IDWriteTextFormat,
    f11_l: IDWriteTextFormat,
    f11_r: IDWriteTextFormat,
    f10_l: IDWriteTextFormat,
    f10_r: IDWriteTextFormat,
    f10_b: IDWriteTextFormat,
    f9_l: IDWriteTextFormat,
    f9_r: IDWriteTextFormat,
    f_sym: IDWriteTextFormat,
}

struct Brushes {
    bg: ID2D1SolidColorBrush,
    card: ID2D1SolidColorBrush,
    card_hover: ID2D1SolidColorBrush,
    border: ID2D1SolidColorBrush,
    track: ID2D1SolidColorBrush,
    txt: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    cyan: ID2D1SolidColorBrush,
    magenta: ID2D1SolidColorBrush,
    amber: ID2D1SolidColorBrush,
    violet: ID2D1SolidColorBrush,
    green: ID2D1SolidColorBrush,
    blue: ID2D1SolidColorBrush,
    alert: ID2D1SolidColorBrush,
}

/// Crea el factory D2D + el render target sobre el HWND.
pub fn create_gfx(hwnd: HWND, width: u32, height: u32) -> windows::core::Result<Gfx> {
    unsafe {
        let factory: ID2D1Factory =
            D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

        let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT::default(), // 0,0 = defaults
            dpiX: 0.0,
            dpiY: 0.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width, height },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let rt = factory.CreateHwndRenderTarget(&rt_props, &hwnd_props)?;

        let mk = |c: D2D1_COLOR_F| -> windows::core::Result<ID2D1SolidColorBrush> {
            rt.CreateSolidColorBrush(&c, None)
        };
        let b = Brushes {
            bg: mk(COL_BG)?,
            card: mk(COL_CARD)?,
            card_hover: mk(COL_CARD_HOVER)?,
            border: mk(COL_BORDER)?,
            track: mk(COL_TRACK)?,
            txt: mk(COL_TXT)?,
            dim: mk(COL_DIM)?,
            cyan: mk(COL_CYAN)?,
            magenta: mk(COL_MAGENTA)?,
            amber: mk(COL_AMBER)?,
            violet: mk(COL_VIOLET)?,
            green: mk(COL_GREEN)?,
            blue: mk(COL_BLUE)?,
            alert: mk(COL_ALERT)?,
        };

        let mkf = |size: f32,
                   weight: DWRITE_FONT_WEIGHT,
                   align: DWRITE_TEXT_ALIGNMENT|
         -> windows::core::Result<IDWriteTextFormat> {
            let f = dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("es-ES"),
            )?;
            f.SetTextAlignment(align)?;
            Ok(f)
        };

        let f_title = mkf(13.0, DWRITE_FONT_WEIGHT_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let f_big = mkf(20.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
        let f11_l = mkf(11.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let f11_r = mkf(11.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
        let f10_l = mkf(10.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let f10_r = mkf(10.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
        let f10_b = mkf(10.0, DWRITE_FONT_WEIGHT_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let f9_l = mkf(9.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let f9_r = mkf(9.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
        let f_sym = mkf(12.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER)?;

        Ok(Gfx {
            rt,
            factory,
            dwrite,
            b,
            f_title,
            f_big,
            f11_l,
            f11_r,
            f10_l,
            f10_r,
            f10_b,
            f9_l,
            f9_r,
            f_sym,
        })
    }
}

impl Gfx {
    pub fn resize(&self, w: u32, h: u32) -> windows::core::Result<()> {
        unsafe { self.rt.Resize(&D2D_SIZE_U { width: w, height: h }) }
    }
}

// ---------------------------------------------------------------------------
// Estado de la UI (lo que dibuja y lo que registra para el hit-test)
// ---------------------------------------------------------------------------
pub struct UiState {
    pub mode: &'static str,
    pub pinned: bool,
    pub hover_btn: Option<Action>,
    pub hover_row: Option<usize>,
    pub history: History,
    pub hits: Vec<HitRect>,
    pub rows: Vec<RECT_F>, // filas del Top-5 (para tooltip/kill)
}

pub struct History {
    pub cpu: Vec<f64>,
    pub temp: Vec<f64>,
    pub ram: Vec<f64>,
    pub gpu: Vec<f64>,
    pub net: Vec<f64>,
    pub disk_r: Vec<f64>,
    pub disk_w: Vec<f64>,
}

pub const MAX_POINTS: usize = 20;

impl History {
    pub fn push(&mut self, cpu: f64, temp: f64, ram: f64, gpu: f64, net: f64, dr: f64, dw: f64) {
        push_cap(&mut self.cpu, cpu);
        push_cap(&mut self.temp, temp);
        push_cap(&mut self.ram, ram);
        push_cap(&mut self.gpu, gpu);
        push_cap(&mut self.net, net);
        push_cap(&mut self.disk_r, dr);
        push_cap(&mut self.disk_w, dw);
    }
}

fn push_cap(v: &mut Vec<f64>, x: f64) {
    v.push(x);
    if v.len() > MAX_POINTS {
        v.remove(0);
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            mode: "dev",
            pinned: true,
            hover_btn: None,
            hover_row: None,
            history: History {
                cpu: vec![],
                temp: vec![],
                ram: vec![],
                gpu: vec![],
                net: vec![],
                disk_r: vec![],
                disk_w: vec![],
            },
            hits: vec![],
            rows: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Primitivas de dibujo
// ---------------------------------------------------------------------------
fn rect_f(r: RECT_F) -> D2D_RECT_F {
    D2D_RECT_F { left: r.l, top: r.t, right: r.r, bottom: r.b }
}

impl Gfx {
    fn card(&self, r: RECT_F, hover: bool) {
        unsafe {
            let brush = if hover { &self.b.card_hover } else { &self.b.card };
            self.rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: 7.0, radiusY: 7.0 },
                brush,
            );
            self.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: 7.0, radiusY: 7.0 },
                &self.b.border,
                1.0,
                None,
            );
        }
    }

    fn text(&self, s: &str, f: &IDWriteTextFormat, r: RECT_F, brush: &ID2D1SolidColorBrush) {
        let wide: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.rt.DrawText(
                &wide,
                f,
                &rect_f(r),
                brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    fn bar(&self, x: f32, y: f32, w: f32, h: f32, pct: f32, fill: &ID2D1SolidColorBrush) {
        let track = RECT_F::new(x, y, x + w, y + h);
        unsafe {
            self.rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: rect_f(track), radiusX: h / 2.0, radiusY: h / 2.0 },
                &self.b.track,
            );
            let w = w * pct.clamp(0.0, 100.0) / 100.0;
            if w > 0.5 {
                let fillr = RECT_F::new(x, y, x + w, y + h);
                self.rt.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rect_f(fillr), radiusX: h / 2.0, radiusY: h / 2.0 },
                    fill,
                );
            }
        }
    }

    /// Gráfica de línea: polyline con HUECOS honestos — un NaN (sin dato)
    /// corta la línea, igual que spanGaps:false en Chart.js.
    fn chart(&self, r: RECT_F, data: &[f64], brush: &ID2D1SolidColorBrush, y_max: f32) {
        let n = MAX_POINTS as f32;
        let w = r.r - r.l;
        let hgt = r.b - r.t - 2.0;
        let mut prev: Option<(f32, f32)> = None;
        unsafe {
            // Línea base tenue.
            self.rt.DrawLine(
                Vector2 { X: r.l, Y: r.b },
                Vector2 { X: r.r, Y: r.b },
                &self.b.border,
                1.0,
                None,
            );
            for (i, v) in data.iter().enumerate() {
                if !v.is_finite() {
                    prev = None; // hueco: la línea se interrumpe
                    continue;
                }
                let x = r.l + (i as f32 + 0.5) * w / n;
                let y = r.b - (*v as f32 / y_max).clamp(0.0, 1.0) * hgt - 1.0;
                if let Some((px, py)) = prev {
                    self.rt.DrawLine(Vector2 { X: px, Y: py }, Vector2 { X: x, Y: y }, brush, 1.6, None);
                }
                prev = Some((x, y));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pase de dibujo principal
// ---------------------------------------------------------------------------
pub struct DrawCtx<'a> {
    pub app: &'a Arc<AppState>,
    pub ui: &'a mut UiState,
    pub scale: f32,
    pub w: f32,
    pub h: f32,
}

pub fn draw(gfx: &Gfx, ctx: &mut DrawCtx) {
    unsafe {
        gfx.rt.BeginDraw();
        gfx.rt.Clear(Some(&COL_BG));
    }
    ctx.ui.hits.clear();
    ctx.ui.rows.clear();

    let s = ctx.scale;
    let hdr_h = 36.0 * s;
    draw_header(gfx, ctx, hdr_h);

    match ctx.ui.mode {
        "mini" => draw_mini(gfx, ctx, hdr_h),
        "charts" => draw_charts(gfx, ctx, hdr_h),
        "procs" => draw_procs(gfx, ctx, hdr_h),
        _ => draw_dev(gfx, ctx, hdr_h),
    }

    unsafe {
        let _ = gfx.rt.EndDraw(None, None);
    }
}

fn draw_header(gfx: &Gfx, ctx: &mut DrawCtx, hdr_h: f32) {
    let s = ctx.scale;
    let w = ctx.w;
    // Línea inferior del header.
    unsafe {
        gfx.rt.DrawLine(
            Vector2 { X: 0.0, Y: hdr_h },
            Vector2 { X: w, Y: hdr_h },
            &gfx.b.border,
            1.0,
            None,
        );
    }
    // Dot + título.
    let dot_r = RECT_F::new(12.0 * s, hdr_h / 2.0 - 3.0 * s, 18.0 * s, hdr_h / 2.0 + 3.0 * s);
    unsafe {
        gfx.rt.FillEllipse(
            &D2D1_ELLIPSE {
                point: Vector2 { X: (dot_r.l + dot_r.r) / 2.0, Y: (dot_r.t + dot_r.b) / 2.0 },
                radiusX: 3.0 * s,
                radiusY: 3.0 * s,
            },
            &gfx.b.cyan,
        );
    }
    gfx.text(
        "SYS MONITOR",
        &gfx.f_title,
        RECT_F::new(24.0 * s, 6.0 * s, 130.0 * s, hdr_h - 4.0 * s),
        &gfx.b.txt,
    );

    // Botones de derecha a izquierda: close, procs, charts, dev, mini, pin.
    let btn = 26.0 * s;
    let gap = 2.0 * s;
    let mut x = w - 6.0 * s - btn;
    let buttons: [(Action, &str); 6] = [
        (Action::Close, "\u{2715}"),
        (Action::Mode("procs"), "\u{2630}"),
        (Action::Mode("charts"), "\u{25D4}"),
        (Action::Mode("dev"), "\u{25A6}"),
        (Action::Mode("mini"), "\u{25A1}"),
        (Action::Pin, "\u{1F4CC}"),
    ];
    for (action, glyph) in buttons.iter() {
        // El pin activo y el modo activo se pintan resaltados.
        let active = match action {
            Action::Pin => ctx.ui.pinned,
            Action::Mode(m) => *m == ctx.ui.mode,
            _ => false,
        };
        let hover = ctx.ui.hover_btn.map(|a| a == *action).unwrap_or(false);
        let r = RECT_F::new(x, (hdr_h - btn) / 2.0, x + btn, (hdr_h + btn) / 2.0);
        if active || hover {
            unsafe {
                gfx.rt.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: 5.0 * s, radiusY: 5.0 * s },
                    if active { &gfx.b.border } else { &gfx.b.track },
                );
            }
        }
        gfx.text(glyph, &gfx.f_sym, r, if active { &gfx.b.cyan } else { &gfx.b.dim });
        ctx.ui.hits.push(HitRect { rect: r, action: *action });
        x -= btn + gap;
    }
}

// ---------------------------------------------------------------------------
// MODO DEV
// ---------------------------------------------------------------------------
fn draw_dev(gfx: &Gfx, ctx: &mut DrawCtx, hdr_h: f32) {
    let s = ctx.scale;
    let snap = ctx.app.snapshot.lock().unwrap().clone();
    let x0 = 8.0 * s;
    let cw = ctx.w - 2.0 * x0;
    let mut y = hdr_h + 8.0 * s;

    // CPU
    let cpu = snap.cpu.clamp(0.0, 100.0);
    let alert = cpu >= 85.0;
    let r = RECT_F::new(x0, y, x0 + cw, y + 52.0 * s);
    gfx.card(r, false);
    gfx.text("CPU", &gfx.f10_l, RECT_F::new(x0 + 12.0 * s, y + 6.0 * s, x0 + 60.0 * s, y + 24.0 * s), &gfx.b.dim);
    gfx.text(&fmt::pct(snap.cpu), &gfx.f_big, RECT_F::new(x0 + cw - 110.0 * s, y + 4.0 * s, x0 + cw - 12.0 * s, y + 30.0 * s), if alert { &gfx.b.alert } else { &gfx.b.cyan });
    gfx.bar(x0 + 12.0 * s, y + 34.0 * s, cw - 24.0 * s, 6.0 * s, cpu as f32, &gfx.b.cyan);
    y += 60.0 * s;

    // TEMP
    let temp_ok = snap.cpu_temp > 0.0;
    let t_alert = temp_ok && snap.cpu_temp >= 80.0;
    let r = RECT_F::new(x0, y, x0 + cw, y + 52.0 * s);
    gfx.card(r, false);
    gfx.text("CPU TEMP", &gfx.f10_l, RECT_F::new(x0 + 12.0 * s, y + 6.0 * s, x0 + 90.0 * s, y + 24.0 * s), &gfx.b.dim);
    gfx.text(&fmt::temp(snap.cpu_temp), &gfx.f_big, RECT_F::new(x0 + cw - 110.0 * s, y + 4.0 * s, x0 + cw - 12.0 * s, y + 30.0 * s), if t_alert { &gfx.b.alert } else { &gfx.b.amber });
    if temp_ok {
        gfx.bar(x0 + 12.0 * s, y + 34.0 * s, cw - 24.0 * s, 6.0 * s, snap.cpu_temp as f32, &gfx.b.amber);
    } else {
        gfx.bar(x0 + 12.0 * s, y + 34.0 * s, cw - 24.0 * s, 6.0 * s, 0.0, &gfx.b.amber);
        let hint = match snap.temp_status.as_str() {
            "admin" => "\u{26A0} requiere ejecutar como administrador",
            _ => "sin sensor térmico accesible en este equipo",
        };
        gfx.text(hint, &gfx.f9_l, RECT_F::new(x0 + 12.0 * s, y + 42.0 * s, x0 + cw - 12.0 * s, y + 52.0 * s), &gfx.b.dim);
    }
    y += 60.0 * s;

    // GPU
    let gpu_ok = snap.gpu > -1.0;
    let r = RECT_F::new(x0, y, x0 + cw, y + 52.0 * s);
    gfx.card(r, false);
    gfx.text("GPU", &gfx.f10_l, RECT_F::new(x0 + 12.0 * s, y + 6.0 * s, x0 + 60.0 * s, y + 24.0 * s), &gfx.b.dim);
    gfx.text(&fmt::gpu(snap.gpu), &gfx.f_big, RECT_F::new(x0 + cw - 110.0 * s, y + 4.0 * s, x0 + cw - 12.0 * s, y + 30.0 * s), &gfx.b.violet);
    if gpu_ok {
        gfx.bar(x0 + 12.0 * s, y + 34.0 * s, cw - 24.0 * s, 6.0 * s, snap.gpu as f32, &gfx.b.violet);
    } else {
        gfx.bar(x0 + 12.0 * s, y + 34.0 * s, cw - 24.0 * s, 6.0 * s, 0.0, &gfx.b.violet);
    }
    let info = ctx.app.gpu_info.lock().unwrap();
    let detail = if info.info.ok {
        match info.info.vram_mb {
            Some(mb) => format!("{} · {} MB", info.info.model, mb),
            None => info.info.model.clone(),
        }
    } else {
        "--".into()
    };
    drop(info);
    gfx.text(&detail, &gfx.f9_l, RECT_F::new(x0 + 12.0 * s, y + 42.0 * s, x0 + cw - 12.0 * s, y + 52.0 * s), &gfx.b.dim);
    y += 60.0 * s;

    // RAM
    let ram = snap.memory.percent.clamp(0.0, 100.0);
    let r = RECT_F::new(x0, y, x0 + cw, y + 52.0 * s);
    gfx.card(r, false);
    gfx.text("RAM", &gfx.f10_l, RECT_F::new(x0 + 12.0 * s, y + 6.0 * s, x0 + 60.0 * s, y + 24.0 * s), &gfx.b.dim);
    gfx.text(&fmt::pct(snap.memory.percent), &gfx.f_big, RECT_F::new(x0 + cw - 110.0 * s, y + 4.0 * s, x0 + cw - 12.0 * s, y + 30.0 * s), &gfx.b.magenta);
    gfx.bar(x0 + 12.0 * s, y + 34.0 * s, cw - 24.0 * s, 6.0 * s, ram as f32, &gfx.b.magenta);
    gfx.text(
        &format!("{:.2} / {:.2} GB", snap.memory.used_gb, snap.memory.total_gb),
        &gfx.f9_l,
        RECT_F::new(x0 + 12.0 * s, y + 42.0 * s, x0 + cw - 12.0 * s, y + 52.0 * s),
        &gfx.b.dim,
    );
    y += 60.0 * s;

    // NET
    let r = RECT_F::new(x0, y, x0 + cw, y + 44.0 * s);
    gfx.card(r, false);
    gfx.text("NET", &gfx.f10_l, RECT_F::new(x0 + 12.0 * s, y + 6.0 * s, x0 + 60.0 * s, y + 24.0 * s), &gfx.b.dim);
    gfx.text(&snap.network.iface, &gfx.f9_l, RECT_F::new(x0 + 60.0 * s, y + 6.0 * s, x0 + cw - 150.0 * s, y + 24.0 * s), &gfx.b.dim);
    let net_row = format!(
        "\u{25BC}{}  |  \u{25B2}{}",
        fmt::speed(snap.network.rx_bytes_sec),
        fmt::speed(snap.network.tx_bytes_sec)
    );
    gfx.text(&net_row, &gfx.f10_r, RECT_F::new(x0 + cw - 170.0 * s, y + 5.0 * s, x0 + cw - 12.0 * s, y + 25.0 * s), &gfx.b.amber);
    y += 52.0 * s;

    // DISK
    let r = RECT_F::new(x0, y, x0 + cw, y + 44.0 * s);
    gfx.card(r, false);
    gfx.text("DISK", &gfx.f10_l, RECT_F::new(x0 + 12.0 * s, y + 6.0 * s, x0 + 60.0 * s, y + 24.0 * s), &gfx.b.dim);
    let disk_row = format!(
        "\u{25BC}{}  |  \u{25B2}{}",
        fmt::disk_speed(snap.disk.read_bytes_sec),
        fmt::disk_speed(snap.disk.write_bytes_sec)
    );
    gfx.text(&disk_row, &gfx.f10_r, RECT_F::new(x0 + cw - 170.0 * s, y + 5.0 * s, x0 + cw - 12.0 * s, y + 25.0 * s), &gfx.b.green);
}

// ---------------------------------------------------------------------------
// MODO MINI
// ---------------------------------------------------------------------------
fn draw_mini(gfx: &Gfx, ctx: &mut DrawCtx, hdr_h: f32) {
    let s = ctx.scale;
    let snap = ctx.app.snapshot.lock().unwrap().clone();
    let x0 = 8.0 * s;
    let cw = ctx.w - 2.0 * x0;
    let mut y = hdr_h + 8.0 * s;

    // CPU (clicable → expandir)
    let cpu = snap.cpu.clamp(0.0, 100.0);
    let r = RECT_F::new(x0, y, x0 + cw, y + 34.0 * s);
    let hover = ctx.ui.hover_btn.map(|a| a == Action::Expand).unwrap_or(false);
    gfx.card(r, hover);
    ctx.ui.hits.push(HitRect { rect: r, action: Action::Expand });
    gfx.text("CPU", &gfx.f10_l, RECT_F::new(x0 + 10.0 * s, y + 8.0 * s, x0 + 46.0 * s, y + 26.0 * s), &gfx.b.dim);
    gfx.bar(x0 + 48.0 * s, y + 14.0 * s, cw - 130.0 * s, 6.0 * s, cpu as f32, &gfx.b.cyan);
    gfx.text(&format!("{:.1}%", snap.cpu), &gfx.f11_r, RECT_F::new(x0 + cw - 74.0 * s, y + 7.0 * s, x0 + cw - 10.0 * s, y + 27.0 * s), if cpu >= 85.0 { &gfx.b.alert } else { &gfx.b.txt });
    y += 40.0 * s;

    // RAM (clicable → expandir)
    let ram = snap.memory.percent.clamp(0.0, 100.0);
    let r = RECT_F::new(x0, y, x0 + cw, y + 34.0 * s);
    let hover = ctx.ui.hover_btn.map(|a| a == Action::Expand).unwrap_or(false);
    gfx.card(r, hover);
    ctx.ui.hits.push(HitRect { rect: r, action: Action::Expand });
    gfx.text("RAM", &gfx.f10_l, RECT_F::new(x0 + 10.0 * s, y + 8.0 * s, x0 + 46.0 * s, y + 26.0 * s), &gfx.b.dim);
    gfx.bar(x0 + 48.0 * s, y + 14.0 * s, cw - 130.0 * s, 6.0 * s, ram as f32, &gfx.b.magenta);
    gfx.text(&format!("{:.1}%", snap.memory.percent), &gfx.f11_r, RECT_F::new(x0 + cw - 74.0 * s, y + 7.0 * s, x0 + cw - 10.0 * s, y + 27.0 * s), if ram >= 90.0 { &gfx.b.alert } else { &gfx.b.txt });
    y += 40.0 * s;

    // TEMP
    let temp_ok = snap.cpu_temp > 0.0;
    let r = RECT_F::new(x0, y, x0 + cw, y + 34.0 * s);
    gfx.card(r, false);
    gfx.text("TEMP", &gfx.f10_l, RECT_F::new(x0 + 10.0 * s, y + 8.0 * s, x0 + 46.0 * s, y + 26.0 * s), &gfx.b.dim);
    if temp_ok {
        gfx.bar(x0 + 48.0 * s, y + 14.0 * s, cw - 130.0 * s, 6.0 * s, snap.cpu_temp as f32, &gfx.b.amber);
    }
    gfx.text(&fmt::temp(snap.cpu_temp), &gfx.f11_r, RECT_F::new(x0 + cw - 74.0 * s, y + 7.0 * s, x0 + cw - 10.0 * s, y + 27.0 * s), &gfx.b.txt);
    y += 40.0 * s;

    // GPU
    let gpu_ok = snap.gpu > -1.0;
    let r = RECT_F::new(x0, y, x0 + cw, y + 34.0 * s);
    gfx.card(r, false);
    gfx.text("GPU", &gfx.f10_l, RECT_F::new(x0 + 10.0 * s, y + 8.0 * s, x0 + 46.0 * s, y + 26.0 * s), &gfx.b.dim);
    if gpu_ok {
        gfx.bar(x0 + 48.0 * s, y + 14.0 * s, cw - 130.0 * s, 6.0 * s, snap.gpu as f32, &gfx.b.violet);
    }
    gfx.text(&fmt::gpu(snap.gpu), &gfx.f11_r, RECT_F::new(x0 + cw - 74.0 * s, y + 7.0 * s, x0 + cw - 10.0 * s, y + 27.0 * s), &gfx.b.txt);
    y += 40.0 * s;

    // NET
    let r = RECT_F::new(x0, y, x0 + cw, y + 30.0 * s);
    gfx.card(r, false);
    gfx.text(&snap.network.iface, &gfx.f9_l, RECT_F::new(x0 + 10.0 * s, y + 7.0 * s, x0 + 120.0 * s, y + 24.0 * s), &gfx.b.dim);
    let net_row = format!(
        "\u{25BC}{} | \u{25B2}{}",
        fmt::speed(snap.network.rx_bytes_sec),
        fmt::speed(snap.network.tx_bytes_sec)
    );
    gfx.text(&net_row, &gfx.f10_r, RECT_F::new(x0 + 120.0 * s, y + 6.0 * s, x0 + cw - 10.0 * s, y + 25.0 * s), &gfx.b.amber);
}

// ---------------------------------------------------------------------------
// MODO CHARTS (6 gráficas + sesión)
// ---------------------------------------------------------------------------
fn draw_charts(gfx: &Gfx, ctx: &mut DrawCtx, hdr_h: f32) {
    let s = ctx.scale;
    let x0 = 8.0 * s;
    let cw = ctx.w - 2.0 * x0;
    let col_w = (cw - 8.0 * s) / 2.0;
    let ch_h = 92.0 * s;
    let mut y = hdr_h + 8.0 * s;

    let hist = &ctx.ui.history;
    let pairs: [(&str, &Vec<f64>, &ID2D1SolidColorBrush, f32, f32); 4] = [
        ("CPU %", &hist.cpu, &gfx.b.cyan, 100.0, 100.0),
        ("TEMP °C", &hist.temp, &gfx.b.amber, 100.0, 100.0),
        ("RAM %", &hist.ram, &gfx.b.magenta, 100.0, 100.0),
        ("GPU %", &hist.gpu, &gfx.b.violet, gpu_peak(hist), 100.0),
    ];
    let mut col = 0;
    for (label, data, brush, y_max, _fixed) in pairs.iter() {
        let cx = if col == 0 { x0 } else { x0 + col_w + 8.0 * s };
        let r = RECT_F::new(cx, y, cx + col_w, y + ch_h);
        gfx.card(r, false);
        gfx.text(label, &gfx.f9_l, RECT_F::new(cx + 8.0 * s, y + 4.0 * s, cx + col_w - 50.0 * s, y + 18.0 * s), &gfx.b.dim);
        let last = data.last().copied();
        let val = match *label {
            "CPU %" => last.map(|v| fmt::pct(v)),
            "TEMP °C" => last.map(fmt::temp),
            "RAM %" => last.map(|v| fmt::pct(v)),
            _ => last.map(fmt::gpu),
        };
        if let Some(v) = val {
            gfx.text(&v, &gfx.f10_r, RECT_F::new(cx + col_w - 60.0 * s, y + 3.0 * s, cx + col_w - 8.0 * s, y + 19.0 * s), brush);
        }
        let chart_r = RECT_F::new(cx + 8.0 * s, y + 20.0 * s, cx + col_w - 8.0 * s, y + ch_h - 6.0 * s);
        gfx.chart(chart_r, data, brush, *y_max);
        if col == 1 {
            y += ch_h + 8.0 * s;
        }
        col += 1;
    }

    // NET (ancho completo)
    let r = RECT_F::new(x0, y, x0 + cw, y + ch_h);
    gfx.card(r, false);
    gfx.text("NET KB/s", &gfx.f9_l, RECT_F::new(x0 + 8.0 * s, y + 4.0 * s, x0 + 90.0 * s, y + 18.0 * s), &gfx.b.dim);
    let last_net = hist.net.last().copied();
    if let Some(v) = last_net {
        gfx.text(&format!("{:.1}", v), &gfx.f10_r, RECT_F::new(x0 + cw - 70.0 * s, y + 3.0 * s, x0 + cw - 8.0 * s, y + 19.0 * s), &gfx.b.amber);
    }
    gfx.chart(RECT_F::new(x0 + 8.0 * s, y + 20.0 * s, x0 + cw - 8.0 * s, y + ch_h - 6.0 * s), &hist.net, &gfx.b.amber, net_peak(hist));
    y += ch_h + 8.0 * s;

    // DISK (ancho completo, 2 series)
    let r = RECT_F::new(x0, y, x0 + cw, y + ch_h);
    gfx.card(r, false);
    gfx.text("DISCO L/W", &gfx.f9_l, RECT_F::new(x0 + 8.0 * s, y + 4.0 * s, x0 + 110.0 * s, y + 18.0 * s), &gfx.b.dim);
    let snap = ctx.app.snapshot.lock().unwrap().clone();
    let disk_val = format!("{} / {}", fmt::disk_speed(snap.disk.read_bytes_sec), fmt::disk_speed(snap.disk.write_bytes_sec));
    gfx.text(&disk_val, &gfx.f10_r, RECT_F::new(x0 + cw - 150.0 * s, y + 3.0 * s, x0 + cw - 8.0 * s, y + 19.0 * s), &gfx.b.green);
    let chart_r = RECT_F::new(x0 + 8.0 * s, y + 20.0 * s, x0 + cw - 8.0 * s, y + ch_h - 6.0 * s);
    gfx.chart(chart_r, &hist.disk_r, &gfx.b.green, disk_peak(hist));
    gfx.chart(chart_r, &hist.disk_w, &gfx.b.blue, disk_peak(hist));
    y += ch_h + 8.0 * s;

    // SESIÓN
    let sess = ctx.app.session.lock().unwrap().clone();
    let r = RECT_F::new(x0, y, x0 + cw, y + 104.0 * s);
    gfx.card(r, false);
    gfx.text(
        &format!("SESIÓN — MÁX / PROM · {} muestras", sess.samples),
        &gfx.f9_l,
        RECT_F::new(x0 + 8.0 * s, y + 4.0 * s, x0 + cw - 8.0 * s, y + 18.0 * s),
        &gfx.b.dim,
    );
    let rows: [(&str, f64, f64, &ID2D1SolidColorBrush); 4] = [
        ("CPU", sess.cpu_max, sess.cpu_avg(), &gfx.b.cyan),
        ("RAM", sess.mem_max, sess.mem_avg(), &gfx.b.magenta),
        ("GPU", sess.gpu_max, sess.gpu_avg(), &gfx.b.violet),
        ("RED", sess.net_max, sess.net_avg(), &gfx.b.amber),
    ];
    let mut ry = y + 20.0 * s;
    for (label, max, avg, brush) in rows.iter() {
        gfx.text(label, &gfx.f9_l, RECT_F::new(x0 + 10.0 * s, ry, x0 + 50.0 * s, ry + 16.0 * s), &gfx.b.dim);
        let (max_s, avg_s) = if *label == "RED" {
            (fmt::speed(*max), format!("prom {}", fmt::speed(*avg)))
        } else {
            (format!("{:.1}%", max), format!("prom {:.1}%", avg))
        };
        gfx.text(&max_s, &gfx.f9_r, RECT_F::new(x0 + 50.0 * s, ry, x0 + 120.0 * s, ry + 16.0 * s), brush);
        gfx.text(&avg_s, &gfx.f9_l, RECT_F::new(x0 + 130.0 * s, ry, x0 + cw - 10.0 * s, ry + 16.0 * s), &gfx.b.dim);
        ry += 20.0 * s;
    }
}

fn gpu_peak(hist: &History) -> f32 {
    let peak = hist.gpu.iter().filter(|v| **v >= 0.0).fold(10.0f64, |a, b| a.max(*b));
    ((peak * 1.2) as f32).max(10.0)
}

fn net_peak(hist: &History) -> f32 {
    let peak = hist.net.iter().fold(64.0f64, |a, b| a.max(*b));
    peak as f32
}

fn disk_peak(hist: &History) -> f32 {
    let peak = hist
        .disk_r
        .iter()
        .chain(hist.disk_w.iter())
        .filter(|v| **v >= 0.0)
        .fold(64.0f64, |a, b| a.max(*b));
    peak as f32
}

// ---------------------------------------------------------------------------
// MODO PROCS
// ---------------------------------------------------------------------------
fn draw_procs(gfx: &Gfx, ctx: &mut DrawCtx, hdr_h: f32) {
    let s = ctx.scale;
    let x0 = 8.0 * s;
    let cw = ctx.w - 2.0 * x0;
    let y0 = hdr_h + 8.0 * s;
    let card_h = ctx.h - y0 - 8.0 * s;
    let r = RECT_F::new(x0, y0, x0 + cw, y0 + card_h);
    gfx.card(r, false);

    gfx.text("TOP 5 PROCESSES", &gfx.f10_b, RECT_F::new(x0 + 12.0 * s, y0 + 8.0 * s, x0 + 140.0 * s, y0 + 26.0 * s), &gfx.b.dim);

    let procs = ctx.app.procs.lock().unwrap().clone();
    let mut ry = y0 + 36.0 * s;
    let row_h = 44.0 * s;
    for (i, p) in procs.iter().enumerate() {
        let row_r = RECT_F::new(x0 + 8.0 * s, ry, x0 + cw - 8.0 * s, ry + row_h);
        let hover = ctx.ui.hover_row == Some(i);
        if hover {
            unsafe {
                gfx.rt.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rect_f(row_r), radiusX: 5.0 * s, radiusY: 5.0 * s },
                    &gfx.b.card_hover,
                );
            }
        }
        gfx.text(
            if p.name.is_empty() { "…" } else { &p.name },
            &gfx.f10_l,
            RECT_F::new(row_r.l + 8.0 * s, ry + 5.0 * s, row_r.r - 190.0 * s, ry + 22.0 * s),
            &gfx.b.txt,
        );
        gfx.text(
            &format!("#{} · {:.1}% cpu · {:.2}% ram", p.pid, p.cpu, p.mem),
            &gfx.f9_l,
            RECT_F::new(row_r.l + 8.0 * s, ry + 23.0 * s, row_r.r - 190.0 * s, ry + 38.0 * s),
            &gfx.b.dim,
        );
        gfx.text(
            &format!("{:.1}%", p.cpu),
            &gfx.f11_r,
            RECT_F::new(row_r.r - 185.0 * s, ry + 5.0 * s, row_r.r - 70.0 * s, ry + 25.0 * s),
            &gfx.b.cyan,
        );
        // Botón KILL.
        let kw = 56.0 * s;
        let kr = RECT_F::new(row_r.r - kw - 6.0 * s, ry + 8.0 * s, row_r.r - 6.0 * s, ry + 30.0 * s);
        unsafe {
            gfx.rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: rect_f(kr), radiusX: 4.0 * s, radiusY: 4.0 * s },
                &gfx.b.alert,
            );
        }
        gfx.text("KILL", &gfx.f9_l, RECT_F::new(kr.l + 8.0 * s, kr.t + 3.0 * s, kr.r, kr.b), &gfx.b.txt);
        ctx.ui.hits.push(HitRect { rect: kr, action: Action::Kill(p.pid) });
        ctx.ui.rows.push(row_r);
        ry += row_h + 4.0 * s;
    }
    if procs.is_empty() {
        gfx.text("muestreando procesos…", &gfx.f10_l, RECT_F::new(x0 + 12.0 * s, ry + 10.0 * s, x0 + cw - 12.0 * s, ry + 30.0 * s), &gfx.b.dim);
    } else {
        gfx.text(
            "muestreo cada 10 s · top 5 por CPU",
            &gfx.f9_l,
            RECT_F::new(x0 + 12.0 * s, ry + 4.0 * s, x0 + cw - 12.0 * s, ry + 20.0 * s),
            &gfx.b.dim,
        );
    }
}

// ---------------------------------------------------------------------------
// Tooltip del proceso bajo el cursor (dibujado encima de todo)
// ---------------------------------------------------------------------------
pub fn draw_tooltip(gfx: &Gfx, ctx: &DrawCtx, mouse: (f32, f32)) {
    let Some(idx) = ctx.ui.hover_row else { return };
    let procs = ctx.app.procs.lock().unwrap();
    let Some(p) = procs.get(idx) else { return };
    let s = ctx.scale;
    let exe = fmt::exe_path(p.exe.as_deref());
    let lines = [
        if p.name.is_empty() { format!("PID {}", p.pid) } else { p.name.clone() },
        format!("PID: {}", p.pid),
        exe,
        format!("CPU: {:.1}%  ·  RAM: {:.1}%", p.cpu, p.mem),
    ];
    let w = 230.0 * s;
    let h = 66.0 * s;
    let mut x = mouse.0 + 12.0 * s;
    let mut y = mouse.1 + 12.0 * s;
    if x + w > ctx.w {
        x = mouse.0 - w - 8.0 * s;
    }
    if y + h > ctx.h {
        y = mouse.1 - h - 8.0 * s;
    }
    let r = RECT_F::new(x, y, x + w, y + h);
    unsafe {
        gfx.rt.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: 6.0 * s, radiusY: 6.0 * s },
            &gfx.b.card_hover,
        );
        gfx.rt.DrawRoundedRectangle(
            &D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: 6.0 * s, radiusY: 6.0 * s },
            &gfx.b.border,
            1.0,
            None,
        );
    }
    let mut ty = y + 6.0 * s;
    for (i, line) in lines.iter().enumerate() {
        let f = if i == 0 { &gfx.f10_b } else { &gfx.f9_l };
        let brush = if i == 0 { &gfx.b.txt } else { &gfx.b.dim };
        gfx.text(line, f, RECT_F::new(x + 8.0 * s, ty, x + w - 6.0 * s, ty + 15.0 * s), brush);
        ty += 15.0 * s;
    }
}

/// Tamaño lógico de la ventana según el modo.
pub fn mode_size(mode: &str) -> (f64, f64) {
    if mode == "mini" {
        (WIDGET_W, MINI_H)
    } else {
        (WIDGET_W, WIDGET_H)
    }
}
