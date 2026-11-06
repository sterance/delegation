//! The tray's context menu: one row per saved server, then "Add server…"
//! and "Quit". Only the active row shows a status dot. Status changes swap
//! that row's icon in place; the menu is rebuilt only when a server is added.

use crate::store::Store;
use tray_icon::menu::{Icon, IconMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};

pub enum MenuAction {
    Server(usize),
    AddServer,
    Quit,
}

pub struct TrayMenu {
    pub menu: Menu,
    rows: Vec<IconMenuItem>,
    add: MenuItem,
    quit: MenuItem,
}

impl TrayMenu {
    pub fn build(store: &Store, connected: bool) -> TrayMenu {
        let menu = Menu::new();
        let rows: Vec<IconMenuItem> = store
            .servers
            .iter()
            // '&' marks a keyboard mnemonic in menu text.
            .map(|s| IconMenuItem::new(s.url.replace('&', "&&"), true, None, None))
            .collect();
        let add = MenuItem::new("Add server…", true, None);
        let quit = MenuItem::new("Quit", true, None);
        for row in &rows {
            let _ = menu.append(row);
        }
        if !rows.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let _ = menu.append_items(&[&add, &quit]);
        let tray_menu = TrayMenu { menu, rows, add, quit };
        tray_menu.set_status(store.active, connected);
        tray_menu
    }

    pub fn action(&self, id: &MenuId) -> Option<MenuAction> {
        if id == self.add.id() {
            return Some(MenuAction::AddServer);
        }
        if id == self.quit.id() {
            return Some(MenuAction::Quit);
        }
        self.rows.iter().position(|r| r.id() == id).map(MenuAction::Server)
    }

    pub fn set_status(&self, active: usize, connected: bool) {
        for (i, row) in self.rows.iter().enumerate() {
            let color = match (i == active, connected) {
                (false, _) => None,
                (true, true) => Some(GREEN),
                (true, false) => Some(RED),
            };
            row.set_icon(Some(dot(color)));
        }
    }
}

// The dashboard's success/error colors (MUI defaults).
const GREEN: [u8; 3] = [0x2e, 0x7d, 0x32];
const RED: [u8; 3] = [0xd3, 0x2f, 0x2f];

/// A 16×16 anti-aliased dot, or a transparent square that keeps inactive
/// rows' text aligned with the active one.
fn dot(color: Option<[u8; 3]>) -> Icon {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 + 0.5 - 8.0, y as f32 + 0.5 - 8.0);
            let coverage = (5.0 - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            let [r, g, b] = color.unwrap_or([0, 0, 0]);
            let alpha = if color.is_some() { (coverage * 255.0) as u8 } else { 0 };
            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("16x16 RGBA is a valid icon")
}

/// The tray icon itself: the exe's embedded icon, or a plain dot if the
/// resource is missing (e.g. a build without build.rs's resource step).
pub fn tray_icon() -> tray_icon::Icon {
    tray_icon::Icon::from_resource(1, None).unwrap_or_else(|_| {
        const SIZE: u32 = 32;
        let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
                let coverage = (14.0 - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
                rgba.extend_from_slice(&[0x25, 0x63, 0xeb, (coverage * 255.0) as u8]);
            }
        }
        tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("32x32 RGBA is a valid icon")
    })
}
