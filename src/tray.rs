//! Значок в трее (справа внизу, у часов) — как у Discord.
//!
//! Окно сворачивается и закрывается в трей, а не висит на панели задач.
//! Клик по значку — открыть окно, правый клик — меню. Значок становится
//! коралловым, когда включён лаг.

use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum TrayAction {
    Show,
    ToggleLag,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    lag_item: MenuItem,
    rx: Receiver<TrayAction>,
    shown_lag: Option<bool>,
}

/// Фон значка: бирюзовый — всё спокойно, коралловый — лаг.
const CALM: [u8; 3] = [0x3C, 0xC8, 0xB4];
const LAG: [u8; 3] = [0xF0, 0x50, 0x6E];

/// Рисует значок «микрофон» размером `size`×`size` (RGBA).
pub fn icon_rgba(size: u32, lag: bool) -> Vec<u8> {
    let bg = if lag { LAG } else { CALM };
    let s = size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];

    // Расстояния до фигур (отрицательные — внутри), в пикселях.
    let rounded_square = |x: f32, y: f32| {
        let r = 0.24 * s;
        let half = s / 2.0 - 0.5;
        let qx = (x - s / 2.0).abs() - (half - r);
        let qy = (y - s / 2.0).abs() - (half - r);
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        outside + qx.max(qy).min(0.0) - r
    };
    let capsule = |x: f32, y: f32, ax: f32, ay: f32, by: f32, r: f32| {
        let py = y.clamp(ay, by);
        ((x - ax).powi(2) + (y - py).powi(2)).sqrt() - r
    };
    let segment_h = |x: f32, y: f32, x0: f32, x1: f32, yy: f32, r: f32| {
        let px = x.clamp(x0, x1);
        ((x - px).powi(2) + (y - yy).powi(2)).sqrt() - r
    };

    for py in 0..size {
        for px in 0..size {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let bg_cov = (0.5 - rounded_square(x, y)).clamp(0.0, 1.0);

            let cx = s / 2.0;
            let stroke = 0.045 * s;
            // Головка микрофона.
            let head = capsule(x, y, cx, 0.27 * s, 0.47 * s, 0.13 * s);
            // Дужка-держатель: нижняя половина окружности.
            let arc = {
                let (dx, dy) = (x - cx, y - 0.47 * s);
                let ring = ((dx * dx + dy * dy).sqrt() - 0.22 * s).abs() - stroke;
                if dy >= 0.0 { ring } else { f32::MAX }
            };
            // Ножка и подставка.
            let stem = capsule(x, y, cx, 0.69 * s, 0.79 * s, stroke);
            let base = segment_h(x, y, cx - 0.14 * s, cx + 0.14 * s, 0.79 * s, stroke);
            let glyph = head.min(arc).min(stem).min(base);
            let fg_cov = (0.5 - glyph).clamp(0.0, 1.0);

            let i = ((py * size + px) * 4) as usize;
            for c in 0..3 {
                out[i + c] = (bg[c] as f32 * (1.0 - fg_cov) + 255.0 * fg_cov) as u8;
            }
            out[i + 3] = (bg_cov * 255.0) as u8;
        }
    }
    out
}

fn make_icon(lag: bool) -> Result<Icon> {
    Ok(Icon::from_rgba(icon_rgba(32, lag), 32, 32)?)
}

impl Tray {
    pub fn new(ctx: &egui::Context) -> Result<Self> {
        let show = MenuItem::new("Открыть", true, None);
        let lag_item = MenuItem::new("Включить лаг", true, None);
        let quit = MenuItem::new("Выход", true, None);
        let menu = Menu::new();
        menu.append_items(&[&show, &lag_item, &PredefinedMenuItem::separator(), &quit])?;

        let icon = TrayIconBuilder::new()
            .with_icon(make_icon(false)?)
            .with_tooltip("Microphone Editor")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        // События трея приходят из цикла сообщений Windows; будим окно,
        // даже если оно скрыто.
        let (tx, rx) = mpsc::channel();
        let (tx_click, ctx_click) = (tx.clone(), ctx.clone());
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let open = matches!(
                e,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick { .. }
            );
            if open {
                let _ = tx_click.send(TrayAction::Show);
                ctx_click.request_repaint();
            }
        }));
        let (show_id, lag_id, quit_id) = (show.id().clone(), lag_item.id().clone(), quit.id().clone());
        let ctx_menu = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let action = if e.id == show_id {
                TrayAction::Show
            } else if e.id == lag_id {
                TrayAction::ToggleLag
            } else if e.id == quit_id {
                TrayAction::Quit
            } else {
                return;
            };
            let _ = tx.send(action);
            ctx_menu.request_repaint();
        }));

        Ok(Self {
            icon,
            lag_item,
            rx,
            shown_lag: None,
        })
    }

    pub fn poll(&self) -> Vec<TrayAction> {
        self.rx.try_iter().collect()
    }

    /// Обновляет значок и меню под состояние лага.
    pub fn set_lag(&mut self, on: bool) {
        if self.shown_lag == Some(on) {
            return;
        }
        self.shown_lag = Some(on);
        if let Ok(icon) = make_icon(on) {
            let _ = self.icon.set_icon(Some(icon));
        }
        self.lag_item.set_text(if on { "Выключить лаг" } else { "Включить лаг" });
        let _ = self.icon.set_tooltip(Some(if on {
            "Microphone Editor — лаг включён"
        } else {
            "Microphone Editor"
        }));
    }
}
