//! Root menu (`menu.xml` de labwc).
//!
//! Formato:
//! ```xml
//! <openbox_menu>
//!   <menu id="root-menu" label="Eclipse">
//!     <item label="Terminal"><action name="Execute" command="eclipse-terminal"/></item>
//!     <item label="Launcher"><action name="Execute" command="eclipse-launcher"/></item>
//!     <separator/>
//!     <menu id="apps" label="Apps">
//!       <item label="Files"><action name="Execute" command="eclipse-files"/></item>
//!     </menu>
//!     <separator/>
//!     <item label="Reconfigure"><action name="Reconfigure"/></item>
//!     <item label="Exit"><action name="Exit"/></item>
//!   </menu>
//! </openbox_menu>
//! ```

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::actions::Action;

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label:  String,
    pub kind:   MenuKind,
}

#[derive(Clone, Debug)]
pub enum MenuKind {
    Action(Action),
    Submenu(Menu),
    Separator,
}

#[derive(Clone, Debug, Default)]
pub struct Menu {
    pub id:    String,
    pub label: String,
    pub items: Vec<MenuItem>,
}

#[derive(Clone, Debug, Default)]
pub struct MenuRegistry {
    pub menus: Vec<Menu>,
}

impl MenuRegistry {
    pub fn load() -> Self {
        let paths = ["/etc/labwc/menu.xml", "/usr/share/labwc/menu.xml"];
        for p in &paths {
            if let Some(data) = read_file(p) {
                if let Some(reg) = parse_menu_xml(&data) {
                    return reg;
                }
            }
        }
        Self::default_root()
    }

    /// Menú raíz por defecto (cuando no hay `menu.xml`).
    pub fn default_root() -> Self {
        Self {
            menus: alloc::vec![Menu {
                id: "root-menu".into(),
                label: "Eclipse".into(),
                items: alloc::vec![
                    MenuItem { label: "Terminal".into(),     kind: MenuKind::Action(Action::Execute("eclipse-terminal".into())) },
                    MenuItem { label: "Launcher".into(),     kind: MenuKind::Action(Action::Execute("eclipse-launcher".into())) },
                    MenuItem { label: "Files".into(),        kind: MenuKind::Action(Action::Execute("eclipse-files".into())) },
                    MenuItem { label: "".into(),             kind: MenuKind::Separator },
                    MenuItem { label: "Reconfigure".into(),  kind: MenuKind::Action(Action::Reconfigure) },
                    MenuItem { label: "Exit".into(),         kind: MenuKind::Action(Action::Exit) },
                ],
            }],
        }
    }

    pub fn find(&self, id: &str) -> Option<&Menu> {
        self.menus.iter().find(|m| m.id == id)
    }
}

fn parse_menu_xml(bytes: &[u8]) -> Option<MenuRegistry> {
    let s = core::str::from_utf8(bytes).ok()?;
    if !s.contains("<openbox_menu") && !s.contains("<labwc_menu") { return None; }
    let mut reg = MenuRegistry { menus: Vec::new() };
    for (attrs, body) in crate::config::iter_tags_pub(s, "menu") {
        let id = crate::config::attr_pub(&attrs, "id").unwrap_or_else(|| "menu".to_string());
        let label = crate::config::attr_pub(&attrs, "label").unwrap_or_default();
        let mut items: Vec<MenuItem> = Vec::new();
        for (a_attrs, a_body) in crate::config::iter_tags_pub(&body, "item") {
            let label = crate::config::attr_pub(&a_attrs, "label").unwrap_or_default();
            let action = crate::config::iter_tags_pub(&a_body, "action")
                .into_iter().next()
                .and_then(|(at, _)| crate::config::action_from_attrs_pub(&at))
                .unwrap_or(Action::Nop);
            items.push(MenuItem { label, kind: MenuKind::Action(action) });
        }
        for _ in crate::config::iter_tags_pub(&body, "separator") {
            items.push(MenuItem { label: String::new(), kind: MenuKind::Separator });
        }
        reg.menus.push(Menu { id, label, items });
    }
    if reg.menus.is_empty() { return None; }
    Some(reg)
}

fn read_file(path: &str) -> Option<Vec<u8>> {
    use libc::{open, read, close, O_RDONLY};
    let mut cstr = path.as_bytes().to_vec();
    cstr.push(0);
    let fd = unsafe { open(cstr.as_ptr() as *const core::ffi::c_char, O_RDONLY, 0) };
    if fd < 0 { return None; }
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        let n = unsafe { read(fd, tmp.as_mut_ptr() as *mut core::ffi::c_void, tmp.len()) };
        if n <= 0 { break; }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    unsafe { close(fd); }
    Some(buf)
}

/// Estado del menú activo (lo que ve el usuario).
pub struct MenuOverlay {
    pub menu: Menu,
    pub x:    i32,
    pub y:    i32,
    pub hovered: Option<usize>,
}

impl MenuOverlay {
    pub fn open(menu: Menu, x: i32, y: i32) -> Self {
        Self { menu, x, y, hovered: None }
    }

    /// Bounding box (x, y, w, h). Cada item mide 24px de alto.
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        let w = 200;
        let h = self.menu.items.len() as i32 * 24 + 8;
        (self.x, self.y, w, h)
    }

    /// Hit-test → índice del item.
    pub fn hit(&self, cx: i32, cy: i32) -> Option<usize> {
        let (mx, my, mw, mh) = self.bounds();
        if cx < mx || cy < my || cx >= mx + mw || cy >= my + mh { return None; }
        let i = ((cy - my - 4) / 24).max(0) as usize;
        if i >= self.menu.items.len() { return None; }
        Some(i)
    }
}
