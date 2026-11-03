//! Eventos de Wayland: registro, xdg, asiento, teclado, puntero y salidas.

use super::*;

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "wl_subcompositor" => {
                state.subcompositor = Some(registry.bind(name, 1, qh, ()));
            }
            "xdg_wm_base" => state.wm = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(5), qh, ())),
            "wp_cursor_shape_manager_v1" => {
                state.cursor_mgr = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_output" => {
                let _: WlOutput = registry.bind(name, version.min(2), qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: wayland_client::protocol::wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSubcompositor, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSubcompositor,
        _: wayland_client::protocol::wl_subcompositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: wayland_client::protocol::wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: wayland_client::protocol::wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSubsurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSubsurface,
        _: wayland_client::protocol::wl_subsurface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: wayland_client::protocol::wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, u64> for App {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            state.release_buffer(*id);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
            state.need_paint = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.apply_size(width, height);
            }
            xdg_toplevel::Event::Close => state.exit = true,
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let WEnum::Value(caps) = capabilities else {
            return;
        };
        // Cuando el asiento pierde una capacidad (ratón o teclado
        // desconectado, un dispositivo virtual que entra o sale) el objeto
        // viejo queda inerte: hay que soltarlo y pedir otro cuando vuelva, o
        // la ventana deja de recibir clics o teclas.
        let has_keyboard = caps.contains(wl_seat::Capability::Keyboard);
        if !has_keyboard {
            if let Some(keyboard) = state.keyboard.take() {
                if keyboard.version() >= 3 {
                    keyboard.release();
                }
                state.reset_keys(&[]);
            }
        } else if state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
        let has_pointer = caps.contains(wl_seat::Capability::Pointer);
        if !has_pointer {
            if let Some(dev) = state.cursor_dev.take() {
                dev.destroy();
            }
            if let Some(pointer) = state.pointer.take() {
                if pointer.version() >= 3 {
                    pointer.release();
                }
            }
            state.pointer_serial = None;
            state.cursor_applied = None;
        } else if state.pointer.is_none() {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
                    state.xkb = keymap_from_fd(File::from(fd), size);
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat_rate = rate;
                state.repeat_delay_ms = delay;
            }
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                let down = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if let Some(xkb) = &mut state.xkb {
                    let dir = if down {
                        xkb::KeyDirection::Down
                    } else {
                        xkb::KeyDirection::Up
                    };
                    xkb.state.update_key(xkb::Keycode::new(key + 8), dir);
                }
                state.on_key(key, down);
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = &mut state.xkb {
                    xkb.state
                        .update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Enter { keys, .. } => {
                let pressed: Vec<u32> = keys
                    .chunks_exact(4)
                    .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                state.reset_keys(&pressed);
            }
            wl_keyboard::Event::Leave { .. } => state.reset_keys(&[]),
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_serial = Some(serial);
                state.cursor_applied = None;
                state.note_motion(surface_x, surface_y);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.note_motion(surface_x, surface_y),
            wl_pointer::Event::Button {
                button, state: st, ..
            } => {
                let down = matches!(st, WEnum::Value(wl_pointer::ButtonState::Pressed));
                state.on_pointer_button(button, down);
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let Some((px, py)) = state.page_px() else {
                    return;
                };
                let scale = state.scale as f64;
                let (dx, dy) = if matches!(axis, WEnum::Value(wl_pointer::Axis::VerticalScroll)) {
                    (0.0, -value * scale)
                } else {
                    (-value * scale, 0.0)
                };
                if let Some(session) = &state.session {
                    session.shell.wheel(px, py, dx, dy);
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_serial = None;
                state.cursor_applied = None;
                if state.on_page {
                    state.on_page = false;
                    if let Some(session) = &state.session {
                        session.shell.pointer_left();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_wait = None;
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeManagerV1,
        _: wp_cursor_shape_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeDeviceV1,
        _: wp_cursor_shape_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            let factor = factor.max(1);
            if factor != state.scale {
                state.scale = factor;
                state.apply_size(0, 0);
            }
        }
    }
}
