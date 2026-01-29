//! COSMIC Panel/Taskbar Implementation

use crate::wayland_client::*;
use heapless::Vec;

/// Maximum number of panel items
pub const MAX_PANEL_ITEMS: usize = 32;

/// Panel position
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelPosition {
    Top,
    Bottom,
    Left,
    Right,
}

/// Panel item types
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelItemType {
    AppLauncher,
    Workspace,
    SystemTray,
    Clock,
    WindowList,
    Settings,
}

/// Panel item
pub struct PanelItem {
    pub item_type: PanelItemType,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
}

impl PanelItem {
    pub fn new(item_type: PanelItemType) -> Self {
        Self {
            item_type,
            x: 0,
            y: 0,
            width: 48,
            height: 48,
            visible: true,
        }
    }
}

/// COSMIC Panel
pub struct CosmicPanel {
    pub surface_id: u32,
    pub position: PanelPosition,
    pub width: u32,
    pub height: u32,
    pub items: Vec<PanelItem, MAX_PANEL_ITEMS>,
}

impl CosmicPanel {
    pub fn new(surface_id: u32, position: PanelPosition) -> Self {
        let (width, height) = match position {
            PanelPosition::Top | PanelPosition::Bottom => (1920, 48),
            PanelPosition::Left | PanelPosition::Right => (48, 1080),
        };

        let mut panel = Self {
            surface_id,
            position,
            width,
            height,
            items: Vec::new(),
        };

        // Add default panel items
        panel.add_item(PanelItemType::AppLauncher);
        panel.add_item(PanelItemType::Workspace);
        panel.add_item(PanelItemType::WindowList);
        panel.add_item(PanelItemType::SystemTray);
        panel.add_item(PanelItemType::Clock);
        panel.add_item(PanelItemType::Settings);

        panel
    }

    pub fn add_item(&mut self, item_type: PanelItemType) {
        let item = PanelItem::new(item_type);
        let _ = self.items.push(item);
    }

    pub fn layout_items(&mut self) {
        // Layout items based on panel position and size
        let mut x = 0;
        let y = 0;

        for item in self.items.iter_mut() {
            item.x = x;
            item.y = y;
            x += item.width as i32 + 4; // 4px spacing
        }
    }

    pub fn render(&self) {
        // In real implementation:
        // 1. Draw panel background
        // 2. Draw each panel item
        // 3. Commit surface
    }
}
