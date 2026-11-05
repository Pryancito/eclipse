//! labwc's virtual desktops over `ext-workspace-v1`, for the `01 02 03 04`
//! chips of the tecno top bar (mockup A). labwc announces one group with the
//! `<desktops number=.../>` of rc.xml in it and marks the current one
//! `active`; a click asks for `activate` and commits.
//!
//! Its own file because `main.rs` is at its size ceiling (see
//! `xtask/src/tamano.rs`). A compositor without the protocol simply never
//! announces the global, and then there are no chips -- nothing is faked.

use super::*;
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

/// More than this many and the bar would be all chips; the rest are ignored.
const MAX_WORKSPACES: usize = 16;

pub(crate) struct Ws {
    handle: ExtWorkspaceHandleV1,
    coords: Vec<u32>,
    active: bool,
    hidden: bool,
}

#[derive(Default)]
pub(crate) struct Workspaces {
    pub(crate) mgr: Option<ExtWorkspaceManagerV1>,
    list: Vec<Ws>,
    /// Which desktop (chip order) each window is on, by its protocol id.
    desk: Vec<(u32, usize)>,
    /// What was last written to [`DESKTOPS_FILE`], to skip identical writes.
    written: String,
}

impl Workspaces {
    /// The order chips are drawn (and clicked) in.
    fn order(&self) -> Vec<usize> {
        let keys: Vec<_> = self.list.iter().map(|w| (w.hidden, w.coords.clone())).collect();
        ordered(&keys)
    }

    /// One `(label, active)` per visible workspace, in bar order.
    pub(crate) fn chips(&self) -> Vec<(String, bool)> {
        self.order()
            .into_iter()
            .enumerate()
            .map(|(n, i)| (chip_label(n), self.list[i].active))
            .collect()
    }

    /// Switch to the `n`th chip.
    pub(crate) fn activate(&self, n: usize) {
        let (Some(mgr), Some(&i)) = (&self.mgr, self.order().get(n)) else { return };
        self.list[i].handle.activate();
        mgr.commit();
    }
}

/// Where the bar tells `lunardesk` (Super+Tab) which window is on which
/// desktop, under `$XDG_RUNTIME_DIR`: neither wlr-foreign-toplevel nor
/// ext-workspace says it, so the bar, which watches both all session long,
/// keeps the answer. One `desktop<TAB>app_id<TAB>title` line per window.
pub const DESKTOPS_FILE: &str = "lunar/desktops";

/// The desktop a window is on, by what has been seen of it: a window that
/// is active is on the current desktop (labwc focuses only windows there),
/// and a window seen for the first time opened there. Otherwise it stays
/// where it was last seen.
pub(crate) fn place(known: Option<usize>, activated: bool, current: usize) -> usize {
    match known {
        Some(d) if !activated => d,
        _ => current,
    }
}

/// The file's text for `(desktop, app_id, title)` rows. Tabs and newlines in
/// a title would break the line, so they become spaces.
pub(crate) fn desktops_text(rows: &[(usize, &str, &str)]) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    rows.iter().map(|(d, a, t)| format!("{d}\t{}\t{}\n", clean(a), clean(t))).collect()
}

impl State {
    pub(crate) fn track_desktops(&mut self) {
        self.track_desktops_with(true);
    }

    /// Bring the window-to-desktop map up to date and publish it.
    /// `follow_active` is false right after a desktop switch, when the
    /// window that was active a moment ago may still say so.
    pub(crate) fn track_desktops_with(&mut self, follow_active: bool) {
        let Some(current) = self.ws.order().iter().position(|&i| self.ws.list[i].active) else {
            return;
        };
        let mut desk = Vec::with_capacity(self.toplevels.len());
        for t in &self.toplevels {
            let id = t.handle.id().protocol_id();
            let known = self.ws.desk.iter().find(|(i, _)| *i == id).map(|(_, d)| *d);
            desk.push((id, place(known, follow_active && t.activated, current)));
        }
        self.ws.desk = desk;
        let rows: Vec<(usize, &str, &str)> = self
            .toplevels
            .iter()
            .zip(&self.ws.desk)
            .map(|(t, (_, d))| (*d, t.app_id.as_str(), t.title.as_str()))
            .collect();
        let text = desktops_text(&rows);
        if text == self.ws.written {
            return;
        }
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/0".into());
        let path = std::path::Path::new(&dir).join(DESKTOPS_FILE);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &path)).is_ok() {
            self.ws.written = text;
        }
    }
}

/// Visible workspaces sorted by the compositor's coordinates, ties (and a
/// compositor that sends none) in announcement order.
fn ordered(keys: &[(bool, Vec<u32>)]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..keys.len()).filter(|&i| !keys[i].0).collect();
    idx.sort_by(|&a, &b| (&keys[a].1, a).cmp(&(&keys[b].1, b)));
    idx
}

/// Two digits, from 01: the mockup's chips.
fn chip_label(n: usize) -> String {
    format!("{:02}", n + 1)
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                if state.ws.list.len() >= MAX_WORKSPACES {
                    workspace.destroy();
                    return;
                }
                state.ws.list.push(Ws {
                    handle: workspace,
                    coords: Vec::new(),
                    active: false,
                    hidden: false,
                });
            }
            ext_workspace_manager_v1::Event::Done => {
                state.track_desktops_with(false);
                state.render_task_bars();
            }
            ext_workspace_manager_v1::Event::Finished => {
                state.ws = Workspaces::default();
                state.render_task_bars();
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        _: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let ext_workspace_group_handle_v1::Event::Removed = event {
            group.destroy();
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let Some(pos) = state.ws.list.iter().position(|w| &w.handle == handle) else { return };
        if let ext_workspace_handle_v1::Event::Removed = event {
            state.ws.list.remove(pos).handle.destroy();
            return;
        }
        let ws = &mut state.ws.list[pos];
        match event {
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                ws.coords = coordinates
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state: WEnum::Value(s) } => {
                ws.active = s.contains(ext_workspace_handle_v1::State::Active);
                ws.hidden = s.contains(ext_workspace_handle_v1::State::Hidden);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_follow_the_coordinates_and_skip_hidden_desktops() {
        let k = |hidden: bool, c: &[u32]| (hidden, c.to_vec());
        let keys = [k(false, &[2]), k(false, &[0]), k(true, &[1]), k(false, &[1])];
        assert_eq!(ordered(&keys), vec![1, 3, 0]);
        // No coordinates at all: announcement order.
        let flat = [k(false, &[]), k(false, &[])];
        assert_eq!(ordered(&flat), vec![0, 1]);
    }

    #[test]
    fn a_window_is_where_it_was_last_active_or_where_it_opened() {
        assert_eq!(place(None, false, 2), 2, "a new window opened on the current desktop");
        assert_eq!(place(Some(0), false, 2), 0, "an idle window stays where it was");
        assert_eq!(place(Some(0), true, 2), 2, "an active one is on the current desktop");
        assert_eq!(desktops_text(&[(1, "foot", "a\tb\nc")]), "1\tfoot\ta b c\n");
    }

    #[test]
    fn chips_are_two_digits_from_one() {
        assert_eq!(chip_label(0), "01");
        assert_eq!(chip_label(3), "04");
        assert_eq!(chip_label(11), "12");
    }
}
