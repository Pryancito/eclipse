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
            ext_workspace_manager_v1::Event::Done => state.render_task_bars(),
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
    fn chips_are_two_digits_from_one() {
        assert_eq!(chip_label(0), "01");
        assert_eq!(chip_label(3), "04");
        assert_eq!(chip_label(11), "12");
    }
}
