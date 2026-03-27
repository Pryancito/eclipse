//! Tiling layout engine — master-stack layout for Lunas desktop.

use core::matches;
use super::ShellWindow;
use crate::compositor::WindowContent;
use crate::compositor::MAX_WINDOWS_COUNT;

pub struct TilingConfig {
    pub gap: i32,
    pub edge_margin: i32,
    pub master_ratio: i32,
}

impl Default for TilingConfig {
    fn default() -> Self {
        Self {
            gap: 12,
            edge_margin: 12,
            master_ratio: 55,
        }
    }
}

pub fn apply_master_stack(
    windows: &mut [ShellWindow],
    window_count: usize,
    fb_w: i32,
    fb_h: i32,
    focused_idx: Option<usize>,
    config: &TilingConfig,
) {
    let margin_top = ShellWindow::TITLE_H + 8;
    let margin_bottom = 44 + 8;
    let work_h = fb_h - margin_top - margin_bottom;
    if work_h <= 0 { return; }

    let mut visible: heapless::Vec<usize, MAX_WINDOWS_COUNT> = heapless::Vec::new();
    for i in 0..window_count {
        if !matches!(windows[i].content, WindowContent::None)
            && !windows[i].minimized
            && !windows[i].maximized
            && !windows[i].closing
        {
            let _ = visible.push(i);
        }
    }
    if visible.is_empty() { return; }

    let master_idx = focused_idx
        .filter(|&i| visible.contains(&i))
        .unwrap_or(*visible.last().expect("last"));
    let stack_count = visible.len().saturating_sub(1);

    let master_w = ((fb_w - (config.edge_margin * 2) - (if stack_count > 0 { config.gap } else { 0 })) * config.master_ratio) / 100;
    let stack_w = fb_w - (config.edge_margin * 2) - master_w - (if stack_count > 0 { config.gap } else { 0 });

    let total_stack_gap = if stack_count > 1 { (stack_count as i32 - 1) * config.gap } else { 0 };
    let stack_h = if stack_count > 0 { (work_h - (config.edge_margin * 2) - total_stack_gap) / stack_count as i32 } else { 0 };

    let mut stack_pos = 0usize;
    for &win_i in visible.iter() {
        let win = &mut windows[win_i];
        if win_i == master_idx {
            win.x = config.edge_margin;
            win.y = margin_top + config.edge_margin;
            win.w = master_w;
            win.h = work_h - (config.edge_margin * 2);
        } else {
            win.x = config.edge_margin + master_w + config.gap;
            win.y = margin_top + config.edge_margin + (stack_pos as i32 * (stack_h + config.gap));
            win.w = stack_w;
            win.h = stack_h;
            stack_pos += 1;
        }
        win.stored_rect = (win.x, win.y, win.w, win.h);
    }
}
