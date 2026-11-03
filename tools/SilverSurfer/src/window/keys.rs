//! Teclado: atajos, repetición, teclas muertas y traducción evdev → Servo.

use super::*;

impl App {
    /// Keyboard focus moved. Key-ups for keys pressed elsewhere (lunarfile,
    /// a new window) never reach us, so modifiers and repeat start over from
    /// what the compositor says is held now.
    pub(super) fn reset_keys(&mut self, pressed: &[u32]) {
        let held = |codes: &[u32]| pressed.iter().any(|k| codes.contains(k));
        self.ctrl = held(&[29, 97]);
        self.alt = held(&[56, 100]);
        self.shift = held(&[42, 54]);
        self.held = None;
        self.held_at = None;
        self.repeated_at = None;
        self.swallowed = None;
        if let Some(composer) = self.xkb.as_mut().and_then(|x| x.composer.as_mut()) {
            composer.reset();
        }
    }

    pub(super) fn on_key(&mut self, evdev: u32, down: bool) {
        match evdev {
            29 | 97 => self.ctrl = down,
            56 | 100 => self.alt = down,
            42 | 54 => self.shift = down,
            _ => {}
        }
        if !down {
            if self.held == Some(evdev) {
                self.held = None;
                self.held_at = None;
                self.repeated_at = None;
            }
            if self.swallowed == Some(evdev) {
                self.swallowed = None;
                return;
            }
            if !self.loc_focused && !is_modifier(evdev) {
                self.send_key(evdev, false, false);
            }
            return;
        }
        if self.handle_shortcut(evdev) {
            self.swallowed = Some(evdev);
            return;
        }
        if !is_modifier(evdev) && !self.ctrl {
            match self.compose_key(evdev) {
                Outcome::Pass => {}
                Outcome::Swallow => {
                    self.swallowed = Some(evdev);
                    return;
                }
                Outcome::Text(text) => {
                    self.swallowed = Some(evdev);
                    self.type_text(evdev, text);
                    return;
                }
            }
        }
        if !self.loc_focused && self.ctrl && !self.alt {
            let action = match evdev {
                46 => Some(ClipboardAction::Copy),
                45 => Some(ClipboardAction::Cut),
                47 => Some(ClipboardAction::Paste),
                _ => None,
            };
            if let Some(action) = action {
                if let Some(session) = &self.session {
                    session.shell.clipboard_action(action);
                }
                self.swallowed = Some(evdev);
                return;
            }
        }
        if self.loc_focused {
            if !is_modifier(evdev) {
                self.held = Some(evdev);
                self.held_at = Some(Instant::now());
                self.repeated_at = None;
            }
            self.edit_location(evdev);
            self.sync_primary();
            self.need_paint = true;
            return;
        }
        if !is_modifier(evdev) {
            self.held = Some(evdev);
            self.held_at = Some(Instant::now());
            self.repeated_at = None;
            self.send_key(evdev, true, false);
        }
    }

    pub(super) fn pump_key_repeat(&mut self) {
        let Some(evdev) = self.held else {
            return;
        };
        if self.repeat_rate <= 0 {
            return;
        }
        let Some(start) = self.held_at else {
            return;
        };
        let now = Instant::now();
        if now.duration_since(start) < Duration::from_millis(self.repeat_delay_ms.max(0) as u64) {
            return;
        }
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.repeat_rate));
        if self
            .repeated_at
            .is_some_and(|prev| now.duration_since(prev) < interval)
        {
            return;
        }
        self.repeated_at = Some(now);
        if self.loc_focused {
            self.edit_location(evdev);
            self.need_paint = true;
        } else {
            self.send_key(evdev, true, true);
        }
    }

    pub(super) fn handle_shortcut(&mut self, evdev: u32) -> bool {
        if self.alt && evdev == 105 {
            self.leave_location();
            if let Some(session) = &self.session {
                session.shell.go_back();
            }
            return true;
        }
        if self.alt && evdev == 106 {
            self.leave_location();
            if let Some(session) = &self.session {
                session.shell.go_forward();
            }
            return true;
        }
        // Ctrl+Shift+L — guardar usuario/contraseña del formulario visible.
        if self.ctrl && self.shift && evdev == 38 {
            self.leave_location();
            if let Some(session) = &self.session {
                session.shell.save_login_from_page();
            }
            return true;
        }
        if self.ctrl && !self.shift && evdev == 38 {
            self.find_mode = false;
            self.loc_focused = true;
            if let Some(session) = &self.session {
                session.shell.set_page_focused(false);
            }
            if let Some(session) = &self.session {
                self.loc.set(session.shell.location_text());
            }
            // Como en Firefox: Ctrl+L deja la dirección seleccionada.
            self.loc.select_all();
            self.need_paint = true;
            return true;
        }
        if self.ctrl && evdev == 20 {
            self.leave_location();
            if let Some(session) = &self.session {
                session.shell.new_tab(home::home_url());
            }
            return true;
        }
        if self.ctrl && evdev == 17 {
            self.leave_location();
            let keep = self
                .session
                .as_ref()
                .is_none_or(|session| session.shell.close_tab(session.shell.active_index()));
            if !keep {
                self.exit = true;
            }
            self.need_paint = true;
            return true;
        }
        // Ctrl+Tab / Ctrl+Shift+Tab — next / previous tab.
        if self.ctrl && evdev == 15 {
            if let Some(session) = &self.session {
                if self.shift {
                    session.shell.prev_tab();
                } else {
                    session.shell.next_tab();
                }
            }
            self.need_paint = true;
            return true;
        }
        // Ctrl+1..8 — jump to tab (1-based).
        if self.ctrl && (2..=9).contains(&evdev) {
            let index = (evdev - 2) as usize;
            if let Some(session) = &self.session {
                session.shell.select_tab(index);
            }
            self.need_paint = true;
            return true;
        }
        if self.ctrl && evdev == 24 {
            self.open_file();
            return true;
        }
        if evdev == 63 || (self.ctrl && evdev == 19) {
            if let Some(session) = &self.session {
                session.shell.reload();
            }
            return true;
        }
        // Ctrl+Shift+P — ventana privada (proceso nuevo).
        if self.ctrl && self.shift && evdev == 25 {
            if let Err(err) = spawn::private_window(None) {
                eprintln!("silversurfer: {err}");
            }
            return true;
        }
        // Ctrl++ / Ctrl+= / KP+ — zoom in.
        if self.ctrl && (evdev == 13 || evdev == 78) {
            if let Some(session) = &self.session {
                session.shell.zoom_by(1.1);
            }
            return true;
        }
        // Ctrl+- / KP- — zoom out.
        if self.ctrl && (evdev == 12 || evdev == 74) {
            if let Some(session) = &self.session {
                session.shell.zoom_by(1.0 / 1.1);
            }
            return true;
        }
        // Ctrl+0 — zoom reset.
        if self.ctrl && evdev == 11 {
            if let Some(session) = &self.session {
                session.shell.zoom_reset();
            }
            return true;
        }
        // Ctrl+D — toggle bookmark for the active tab.
        if self.ctrl && evdev == 32 {
            if let Some(session) = &self.session {
                let url = session.shell.location_text();
                let title = session.shell.active_title();
                let marked = bookmarks::toggle(&url, &title);
                eprintln!(
                    "silversurfer: marcador {} — {}",
                    if marked { "añadido" } else { "quitado" },
                    url
                );
            }
            return true;
        }
        // Ctrl+F — find in page (barra como campo de búsqueda).
        if self.ctrl && evdev == 33 {
            self.find_mode = true;
            self.loc_focused = true;
            self.loc.clear();
            if let Some(session) = &self.session {
                session.shell.set_page_focused(false);
            }
            self.need_paint = true;
            return true;
        }
        // Ctrl+H — about:history.
        if self.ctrl && evdev == 35 {
            self.leave_location();
            if let Some(session) = &self.session {
                if let Ok(url) = url::Url::parse("about:history") {
                    session.shell.load(url);
                }
            }
            return true;
        }
        // Ctrl+B — about:bookmarks.
        if self.ctrl && evdev == 48 {
            self.leave_location();
            if let Some(session) = &self.session {
                if let Ok(url) = url::Url::parse("about:bookmarks") {
                    session.shell.load(url);
                }
            }
            return true;
        }
        // Ctrl+J — about:downloads.
        if self.ctrl && evdev == 36 {
            self.leave_location();
            if let Some(session) = &self.session {
                if let Ok(url) = url::Url::parse("about:downloads") {
                    session.shell.load(url);
                }
            }
            return true;
        }
        // Ctrl+S — guardar / descargar la URL actual.
        if self.ctrl && evdev == 31 {
            self.leave_location();
            if let Some(session) = &self.session {
                if let Ok(url) = url::Url::parse(&session.shell.location_text()) {
                    let request = session.shell.download_request(&url, None);
                    if let Err(err) = downloads::save_url(&url, !self.persist, request) {
                        eprintln!("silversurfer: descarga: {err}");
                    }
                }
            }
            return true;
        }
        // Ctrl+N — ventana nueva (proceso).
        if self.ctrl && evdev == 49 {
            if let Err(err) = spawn::new_window(None) {
                eprintln!("silversurfer: {err}");
            }
            return true;
        }
        // Ctrl+Shift+Delete — borrar cookies y datos de sitio.
        if self.ctrl && self.shift && evdev == 111 {
            self.leave_location();
            if let Some(session) = &self.session {
                let summary = session.shell.clear_browsing_data();
                session
                    .shell
                    .load(crate::privacy::cleared_page_url(&summary));
            }
            return true;
        }
        // F11 — pantalla completa (también Escape para salir).
        if evdev == 87 {
            self.set_fullscreen(!self.fullscreen);
            return true;
        }
        if evdev == 1 && !self.loc_focused {
            if self
                .session
                .as_ref()
                .is_some_and(|s| s.shell.context_popup().is_some())
            {
                if let Some(session) = &self.session {
                    session.shell.dismiss_context_menu();
                }
                self.need_paint = true;
                return true;
            }
            if self.fullscreen {
                self.set_fullscreen(false);
                return true;
            }
        }
        false
    }

    /// Pasa la tecla por la tabla de composición (teclas muertas).
    pub(super) fn compose_key(&mut self, evdev: u32) -> Outcome {
        let Some(xkb) = self.xkb.as_mut() else {
            return Outcome::Pass;
        };
        let sym = xkb.state.key_get_one_sym(xkb::Keycode::new(evdev + 8));
        match xkb.composer.as_mut() {
            Some(composer) => composer.feed(sym),
            None => Outcome::Pass,
        }
    }

    /// Escribe el resultado de una secuencia compuesta en la barra o en la
    /// página (como pulsar y soltar una tecla que diera ese texto).
    pub(super) fn type_text(&mut self, evdev: u32, text: String) {
        if self.loc_focused {
            self.loc.insert(&text);
            self.need_paint = true;
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let code = evdev_code(evdev);
        let mods = self.modifiers();
        session
            .shell
            .key(true, Key::Character(text.clone().into()), code, mods, false);
        session
            .shell
            .key(false, Key::Character(text.into()), code, mods, false);
    }

    pub(super) fn send_key(&self, evdev: u32, down: bool, repeat: bool) {
        let Some(session) = &self.session else {
            return;
        };
        let (key, code) = self.key_and_code(evdev);
        session.shell.key(down, key, code, self.modifiers(), repeat);
    }

    pub(super) fn modifiers(&self) -> Modifiers {
        let mut mods = Modifiers::empty();
        if self.shift {
            mods |= Modifiers::SHIFT;
        }
        if self.ctrl {
            mods |= Modifiers::CONTROL;
        }
        if self.alt {
            mods |= Modifiers::ALT;
        }
        mods
    }

    pub(super) fn key_text(&self, evdev: u32) -> Option<String> {
        let code = evdev + 8;
        if let Some(xkb) = &self.xkb {
            let text = xkb.state.key_get_utf8(xkb::Keycode::new(code));
            if !text.is_empty() && !text.chars().all(char::is_control) {
                return Some(text);
            }
            return None;
        }
        if self.ctrl || self.alt {
            return None;
        }
        qwerty(evdev, self.shift)
    }

    pub(super) fn key_and_code(&self, evdev: u32) -> (Key, Code) {
        let code = evdev_code(evdev);
        let named = named_key(evdev);
        if let Some(named) = named {
            return (Key::Named(named), code);
        }
        let text = if self.ctrl || self.alt {
            self.shortcut_char(evdev)
        } else {
            self.key_text(evdev)
        };
        if let Some(text) = text {
            return (Key::Character(text.into()), code);
        }
        (Key::Named(NamedKey::Unidentified), code)
    }

    /// Carácter de la tecla con Ctrl/Alt pulsado. xkb aplica la transformación
    /// de control (Ctrl+V → U+0016), y Servo espera `Key::Character("v")` con
    /// `Modifiers::CONTROL` para copiar, pegar, cortar, deshacer y seleccionar
    /// todo en los campos de la página. El keysym no lleva esa transformación.
    pub(super) fn shortcut_char(&self, evdev: u32) -> Option<String> {
        if let Some(xkb) = &self.xkb {
            let sym = xkb.state.key_get_one_sym(xkb::Keycode::new(evdev + 8));
            let text = xkb::keysym_to_utf8(sym);
            let text = text.trim_end_matches('\0');
            if text.is_empty() || text.chars().any(char::is_control) {
                return None;
            }
            return Some(text.to_owned());
        }
        qwerty(evdev, self.shift)
    }
}

pub(super) fn is_modifier(evdev: u32) -> bool {
    matches!(evdev, 29 | 97 | 56 | 100 | 42 | 54)
}

pub(super) fn named_key(evdev: u32) -> Option<NamedKey> {
    Some(match evdev {
        1 => NamedKey::Escape,
        14 => NamedKey::Backspace,
        15 => NamedKey::Tab,
        28 => NamedKey::Enter,
        102 => NamedKey::Home,
        103 => NamedKey::ArrowUp,
        105 => NamedKey::ArrowLeft,
        106 => NamedKey::ArrowRight,
        107 => NamedKey::End,
        108 => NamedKey::ArrowDown,
        104 => NamedKey::PageUp,
        109 => NamedKey::PageDown,
        111 => NamedKey::Delete,
        59 => NamedKey::F1,
        60 => NamedKey::F2,
        61 => NamedKey::F3,
        62 => NamedKey::F4,
        63 => NamedKey::F5,
        64 => NamedKey::F6,
        65 => NamedKey::F7,
        66 => NamedKey::F8,
        67 => NamedKey::F9,
        68 => NamedKey::F10,
        87 => NamedKey::F11,
        88 => NamedKey::F12,
        _ => return None,
    })
}

pub(super) fn evdev_code(evdev: u32) -> Code {
    match evdev {
        1 => Code::Escape,
        14 => Code::Backspace,
        15 => Code::Tab,
        28 | 96 => Code::Enter,
        57 => Code::Space,
        102 => Code::Home,
        104 => Code::PageUp,
        107 => Code::End,
        109 => Code::PageDown,
        111 => Code::Delete,
        103 => Code::ArrowUp,
        105 => Code::ArrowLeft,
        106 => Code::ArrowRight,
        108 => Code::ArrowDown,
        59 => Code::F1,
        60 => Code::F2,
        61 => Code::F3,
        62 => Code::F4,
        63 => Code::F5,
        64 => Code::F6,
        65 => Code::F7,
        66 => Code::F8,
        67 => Code::F9,
        68 => Code::F10,
        87 => Code::F11,
        88 => Code::F12,
        16 => Code::KeyQ,
        17 => Code::KeyW,
        18 => Code::KeyE,
        19 => Code::KeyR,
        20 => Code::KeyT,
        24 => Code::KeyO,
        30 => Code::KeyA,
        31 => Code::KeyS,
        32 => Code::KeyD,
        33 => Code::KeyF,
        37 => Code::KeyK,
        38 => Code::KeyL,
        _ => Code::Unidentified,
    }
}

pub(super) fn qwerty(evdev: u32, shift: bool) -> Option<String> {
    let (low, high) = match evdev {
        16 => ('q', 'Q'),
        17 => ('w', 'W'),
        18 => ('e', 'E'),
        19 => ('r', 'R'),
        20 => ('t', 'T'),
        21 => ('y', 'Y'),
        22 => ('u', 'U'),
        23 => ('i', 'I'),
        24 => ('o', 'O'),
        25 => ('p', 'P'),
        30 => ('a', 'A'),
        31 => ('s', 'S'),
        32 => ('d', 'D'),
        33 => ('f', 'F'),
        34 => ('g', 'G'),
        35 => ('h', 'H'),
        36 => ('j', 'J'),
        37 => ('k', 'K'),
        38 => ('l', 'L'),
        44 => ('z', 'Z'),
        45 => ('x', 'X'),
        46 => ('c', 'C'),
        47 => ('v', 'V'),
        48 => ('b', 'B'),
        49 => ('n', 'N'),
        50 => ('m', 'M'),
        2 => ('1', '!'),
        3 => ('2', '@'),
        4 => ('3', '#'),
        5 => ('4', '$'),
        6 => ('5', '%'),
        7 => ('6', '^'),
        8 => ('7', '&'),
        9 => ('8', '*'),
        10 => ('9', '('),
        11 => ('0', ')'),
        12 => ('-', '_'),
        13 => ('=', '+'),
        26 => ('[', '{'),
        27 => (']', '}'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        43 => ('\\', '|'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        57 => (' ', ' '),
        _ => return None,
    };
    Some(if shift { high } else { low }.to_string())
}

pub(super) fn keymap_from_fd(fd: File, size: u32) -> Option<Xkb> {
    let mut file = fd;
    let mut buf = vec![0u8; size as usize];
    file.read_exact(&mut buf).ok()?;
    // wlroots includes the terminating NUL in `size`. CString rejects it.
    if buf.last() == Some(&0) {
        buf.pop();
    }
    let text = String::from_utf8(buf).ok()?;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = Keymap::new_from_string(
        &context,
        text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    let state = XkbState::new(&keymap);
    Some(Xkb {
        _keymap: keymap,
        state,
        composer: Composer::new(),
    })
}
