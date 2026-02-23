use crate::backend::Backend;
use crate::space::Space;
use crate::input::{InputState, CompositorEvent};
use crate::compositor::{ExternalSurface, MAX_EXTERNAL_SURFACES, ShellWindow};
use crate::render;
use eclipse_libc::send;
use sidewind_core::{SideWindEvent, SWND_EVENT_TYPE_RESIZE};

/// SmithayState is the central state of the compositor.
/// It orchestrates the Backend, Space, and Input.
pub struct SmithayState {
    pub backend: Backend,
    pub space: Space,
    pub input: InputState,
    pub surfaces: [ExternalSurface; MAX_EXTERNAL_SURFACES],
    pub counter: u64,
}

impl SmithayState {
    pub fn new() -> Option<Self> {
        let backend = Backend::new()?;
        let space = Space::new();
        let input = InputState::new(
            backend.fb.info.width as i32,
            backend.fb.info.height as i32,
        );
        let surfaces = [const { ExternalSurface {
            id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false
        } }; MAX_EXTERNAL_SURFACES];

        Some(Self {
            backend,
            space,
            input,
            surfaces,
            counter: 0,
        })
    }

    pub fn process_events(&mut self) {
        // Poll and process multiple events per frame to stay responsive
        const MAX_EVENTS: usize = 64;
        let mut count = 0;
        while count < MAX_EVENTS {
            if let Some(event) = self.backend.poll_event() {
                match event {
                    CompositorEvent::Input(ev) => {
                        self.input.apply_event(
                            &ev,
                            self.backend.fb.info.width as i32,
                            self.backend.fb.info.height as i32,
                            &mut self.space.windows,
                            &mut self.space.window_count,
                            &self.surfaces,
                        );
                    }
                    CompositorEvent::SideWind(sw, sender_pid) => {
                        crate::ipc::handle_sidewind_message(
                            sw, 
                            sender_pid, 
                            &mut self.surfaces, 
                            &mut self.space.windows, 
                            &mut self.space.window_count, 
                            &mut self.input
                        );
                    }
                    _ => {} // Handle Wayland/X11 if needed
                }
                count += 1;
            } else {
                break;
            }
        }
    }

    pub fn update(&mut self) {
        self.counter = self.counter.wrapping_add(1);
        self.handle_requests();
        self.space.update_animations(&mut self.surfaces);
        
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;

        let target_notif_x = if self.input.notifications_active { (fb_w - 300) as f32 } else { fb_w as f32 };
        self.input.notif_curr_x += (target_notif_x - self.input.notif_curr_x) * 0.2;

        let target_launcher_y = if self.input.launcher_active { (fb_h - 370) as f32 } else { fb_h as f32 };
        self.input.launcher_curr_y += (target_launcher_y - self.input.launcher_curr_y) * 0.2;

        let target_ws_offset = (self.input.current_workspace as f32) * (fb_w as f32);
        self.input.workspace_offset += (target_ws_offset - self.input.workspace_offset) * 0.15;

        let target_search_y = if self.input.search_active { 0.0 } else { -(fb_h as f32 / 2.0) };
        self.input.search_curr_y += (target_search_y - self.input.search_curr_y) * 0.15;
    }

    fn handle_requests(&mut self) {
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;

        // Create new window
        if self.input.request_new_window && self.space.window_count < crate::compositor::MAX_WINDOWS_COUNT {
            let idx = self.space.window_count;
            let win = crate::compositor::ShellWindow {
                x: 60 + (idx as i32) * 20,
                y: 160 + (idx as i32) * 15,
                w: 600,
                h: 380,
                curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
                minimized: false, maximized: false, closing: false,
                stored_rect: (0, 0, 0, 0),
                workspace: self.input.current_workspace,
                content: crate::compositor::WindowContent::InternalDemo,
            };
            self.space.map_window(win);
            self.input.focused_window = Some(idx);
            self.input.request_new_window = false;
        } else if self.input.request_new_window {
            self.input.request_new_window = false;
        }

        // Close window
        if self.input.request_close_window {
            if let Some(idx) = self.input.focused_window {
                if idx < self.space.window_count {
                    self.space.windows[idx].closing = true;
                }
            }
            self.input.focused_window = None;
            self.input.dragging_window = None;
            self.input.request_close_window = false;
        }

        // Minimize
        if self.input.request_minimize {
            if let Some(idx) = self.input.focused_window {
                if idx < self.space.window_count && !self.space.windows[idx].minimized {
                    self.space.windows[idx].minimized = true;
                    self.input.focused_window = None;
                    self.input.dragging_window = None;
                }
            }
            self.input.request_minimize = false;
        }

        // Maximize
        if self.input.request_maximize {
            if let Some(idx) = self.input.focused_window {
                if idx < self.space.window_count {
                    let win = &mut self.space.windows[idx];
                    if win.maximized {
                        let (x, y, w, h) = win.stored_rect;
                        win.x = x; win.y = y; win.w = w; win.h = h;
                        win.maximized = false;
                    } else {
                        win.stored_rect = (win.x, win.y, win.w, win.h);
                        win.x = 0; win.y = 0;
                        win.w = fb_w;
                        win.h = fb_h - 45;
                        win.maximized = true;
                    }
                    // Notify client if external
                    if let crate::compositor::WindowContent::External(s_idx) = win.content {
                        if (s_idx as usize) < self.surfaces.len() {
                            let pid = self.surfaces[s_idx as usize].pid;
                            let se = SideWindEvent { 
                                event_type: SWND_EVENT_TYPE_RESIZE, 
                                data1: win.w, data2: win.h - ShellWindow::TITLE_H, data3: 0 
                            };
                            let _ = send(pid, 0x00000040, unsafe { core::slice::from_raw_parts(&se as *const _ as *const u8, core::mem::size_of::<SideWindEvent>()) });
                        }
                    }
                }
            }
            self.input.request_maximize = false;
        }

        // Restore
        if self.input.request_restore {
            if let Some(idx) = (0..self.space.window_count).rev().find(|&i| self.space.windows[i].content != crate::compositor::WindowContent::None && self.space.windows[i].minimized) {
                self.space.windows[idx].minimized = false;
                self.space.raise_window(idx);
                self.input.focused_window = Some(self.space.window_count - 1);
            }
            self.input.request_restore = false;
        }

        // Cycle Focus
        if self.input.request_cycle_forward {
            if self.space.window_count > 1 {
                let current = self.input.focused_window.unwrap_or(0);
                if let Some(next) = crate::compositor::next_visible(current, true, &self.space.windows, self.space.window_count) {
                    self.space.raise_window(next);
                    self.input.focused_window = Some(self.space.window_count - 1);
                }
            }
            self.input.request_cycle_forward = false;
        }
        if self.input.request_cycle_backward {
            if self.space.window_count > 1 {
                let current = self.input.focused_window.unwrap_or(self.space.window_count - 1);
                if let Some(prev) = crate::compositor::next_visible(current, false, &self.space.windows, self.space.window_count) {
                    self.space.raise_window(prev);
                    self.input.focused_window = Some(self.space.window_count - 1);
                }
            }
            self.input.request_cycle_backward = false;
        }

        // Dashboard
        if self.input.request_dashboard {
            self.input.dashboard_active = !self.input.dashboard_active;
            self.input.request_dashboard = false;
        }

        // Center Cursor
        if self.input.request_center_cursor {
            self.input.cursor_x = fb_w / 2;
            self.input.cursor_y = fb_h / 2;
            self.input.request_center_cursor = false;
        }
    }

    pub fn render(&mut self) {
        if !self.input.lock_active {
            render::draw_static_ui(
                &mut self.backend.fb, 
                &self.space.windows, 
                self.space.window_count, 
                self.counter, 
                self.input.cursor_x, 
                self.input.cursor_y
            );
            
            if !self.input.dashboard_active {
                render::draw_shell_windows(
                    &mut self.backend.fb, 
                    &self.space.windows, 
                    self.space.window_count, 
                    self.input.focused_window, 
                    &self.surfaces, 
                    self.input.workspace_offset, 
                    self.input.current_workspace,
                    self.input.cursor_x, 
                    self.input.cursor_y
                );
            } else {
                render::draw_dashboard(&mut self.backend.fb, self.counter);
            }

            if self.input.quick_settings_active { render::draw_quick_settings(&mut self.backend.fb); }
            if self.input.context_menu_active { render::draw_context_menu(&mut self.backend.fb, self.input.context_menu_pos); }
            
            render::draw_launcher(&mut self.backend.fb, self.input.launcher_curr_y);
            render::draw_notifications(&mut self.backend.fb, &self.input.notifications, self.input.notif_curr_x);
            
            if self.input.alt_tab_active { 
                render::draw_alt_tab_hud(&mut self.backend.fb, &self.space.windows, self.space.window_count, self.input.focused_window); 
            }
            
            if self.input.search_active || self.input.search_curr_y > -(self.backend.fb.info.height as f32 / 2.0) + 5.0 {
                render::draw_search_hud(
                    &mut self.backend.fb, 
                    &self.input.search_query, 
                    self.input.search_selected_idx, 
                    self.counter, 
                    self.input.search_curr_y
                );
            }

            // Desktop "Stroke" drawing
            if self.input.mouse_buttons & 1 != 0 && self.input.dragging_window.is_none() {
                render::draw_stroke(&mut self.backend.fb, self.input.cursor_x, self.input.cursor_y, self.input.stroke_color);
            }
        } else {
            render::draw_lock_screen(&mut self.backend.fb, self.counter);
        }

        render::draw_cursor(&mut self.backend.fb, embedded_graphics::prelude::Point::new(self.input.cursor_x, self.input.cursor_y));
    }
}
