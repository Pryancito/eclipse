//! Every UI string, one row each.
//!
//! A row is either a PAIR (`es:` and `en:`, which must differ) or `both:` for a
//! string that is deliberately the same in both languages -- a protocol name
//! like `DHCP`, or a label that is a word Spanish borrowed whole. Writing those
//! two cases differently is what lets
//! `every_pair_differs_in_the_two_languages` be strict: the alternative is an
//! allow-list kept somewhere else, which is one more thing to forget, and a row
//! whose English column was copied from the Spanish one is invisible until
//! somebody runs the client in English.

macro_rules! strings {
    ($(
        $(#[$doc:meta])*
        $key:ident => { $($row:tt)* }
    ),* $(,)?) => {
        /// Every string an Eclipse client can draw.
        ///
        /// A value rather than a method per string: the set of keys has to be
        /// walkable for the tests to check every row, and at this many strings
        /// a method each is a file nobody reads.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[allow(non_camel_case_types)]
        pub enum Key {
            $($(#[$doc])* $key,)*
        }

        impl Key {
            /// Every key, in the order the table declares them.
            pub const ALL: &'static [Key] = &[$(Key::$key,)*];

            /// The key's own name, for a test that has to say which row.
            pub fn name(self) -> &'static str {
                match self {
                    $(Key::$key => stringify!($key),)*
                }
            }

            /// `(Spanish, English)`.
            pub(crate) fn pair(self) -> (&'static str, &'static str) {
                match self {
                    $(Key::$key => strings!(@pair $($row)*),)*
                }
            }

            /// Whether the row is deliberately one string for both languages.
            pub fn is_shared(self) -> bool {
                match self {
                    $(Key::$key => strings!(@shared $($row)*),)*
                }
            }
        }
    };

    (@pair es: $es:expr, en: $en:expr $(,)?) => { ($es, $en) };
    (@pair both: $both:expr $(,)?) => { ($both, $both) };
    (@shared es: $es:expr, en: $en:expr $(,)?) => { false };
    (@shared both: $both:expr $(,)?) => { true };
}

strings! {
    // ── the application menu and the runner ──────────────────────────────
    apps_title => { es: "aplicaciones", en: "applications" },
    apps_search => { es: "buscar aplicaciones", en: "search apps" },
    apps_other => { es: "Otros", en: "Other" },
    apps_empty => { es: "Sin aplicaciones", en: "No applications" },
    apps_back => { es: "Volver", en: "Back" },
    run_title => { es: "ejecutar", en: "run" },
    /// The runner's shortcut, as labwc's rc.xml binds it (`A-space`).
    run_key => { es: "ALT+ESPACIO", en: "ALT+SPACE" },
    /// The key the selected launcher row answers to, in caps (tecno).
    enter_key => { es: "INTRO", en: "ENTER" },
    run_hint => { es: "escribe para buscar o ejecutar", en: "type to search or run" },
    run_command => { es: "ejecutar orden", en: "run command" },
    run_empty => { es: "sin resultados", en: "no results" },
    run_keys => { es: "Intro ejecuta   Esc cierra", en: "Enter runs   Esc closes" },
    run_reload => { es: "recargar labwc", en: "reload labwc" },
    run_copies => { es: "Intro copia", en: "Enter copies" },

    // ── the power menu ───────────────────────────────────────────────────
    power_lock => { es: "bloquear", en: "lock" },
    power_logout => { es: "cerrar sesión", en: "log out" },
    power_reboot => { es: "reiniciar", en: "reboot" },
    power_shutdown => { es: "apagar", en: "shut down" },

    // ── the calendar ─────────────────────────────────────────────────────
    /// Header of the week-number column, one letter.
    cal_week => { es: "s", en: "w" },
    cal_today => { es: "hoy", en: "today" },
    /// `día 203` / `day 203`, the day-of-year label.
    cal_day_of_year => { es: "día", en: "day" },

    // ── the lock screen ──────────────────────────────────────────────────
    lock_state => { es: "BLOQUEADO", en: "LOCKED" },
    lock_hint => {
        es: "pulsa una tecla o haz clic para volver",
        en: "press a key or click to return",
    },
    lock_just_now => { es: "ahora mismo", en: "just now" },

    // ── screenshots ──────────────────────────────────────────────────────
    /// The directory, as XDG names it; this is a PATH component, so it has to
    /// match whatever `xdg-user-dirs` wrote for the language.
    shot_pictures_dir => { es: "Imágenes", en: "Pictures" },
    /// The filename stem: `captura-2026-11-05-053112.png`.
    shot_stem => { es: "captura", en: "screenshot" },
    shot_saved => { es: "Captura guardada", en: "Screenshot saved" },
    shot_open_folder => { es: "Abrir carpeta", en: "Open folder" },
    shot_copied => { es: "copiada al portapapeles", en: "copied to the clipboard" },

    // ── lunarclip: the clipboard picker ──────────────────────────────────
    clip_title => { es: "portapapeles", en: "clipboard" },
    /// `12 entradas` / `12 entries` -- the number goes before it.
    clip_entries => { es: "entradas", en: "entries" },
    /// `3 de 12` / `3 of 12`, when the filter is narrowing the list.
    clip_of => { es: "de", en: "of" },
    clip_filter => { es: "filtrar...", en: "filter..." },
    clip_empty => { es: "el historial está vacío", en: "the history is empty" },
    clip_no_match => { es: "nada coincide", en: "nothing matches" },
    clip_keys => {
        es: "Intro copia  ^/v elige  Esc cierra",
        en: "Enter copies  ^/v picks  Esc closes",
    },
    clip_read_only => {
        es: "sin lunarclip --watch: solo lectura",
        en: "no lunarclip --watch: read only",
    },
    clip_unreadable => { es: "no se puede leer", en: "cannot be read" },
    clip_image => { es: "imagen", en: "image" },

    // ── lunarclip: what it says in a terminal ────────────────────────────
    // The `OK` and `ERR` that start a daemon answer are PROTOCOL -- the other
    // end matches on them with `starts_with(b"OK")` -- so they stay as they
    // are. What follows an `ERR ` is the reason, which is printed to whoever
    // ran the command, so that part is translated.
    clip_one_mode => { es: "solo un modo a la vez", en: "only one mode at a time" },
    clip_dump_needs => {
        es: "--dump necesita RUTA[:ANCHOxALTO]",
        en: "--dump needs PATH[:WIDTHxHEIGHT]",
    },
    /// The option goes after it.
    clip_unknown_option => { es: "opción desconocida", en: "unknown option" },
    /// The words go before it.
    clip_no_bare_args => {
        es: "no toma argumentos sueltos",
        en: "takes no bare arguments",
    },
    clip_source_silent => { es: "la fuente no contestó", en: "the source did not answer" },
    clip_no_seat => {
        es: "no hay wl_seat; sin asiento no hay selección",
        en: "no wl_seat; without a seat there is no selection",
    },
    clip_out_of_range => { es: "fuera de rango", en: "out of range" },
    clip_nothing_to_copy => { es: "nada que copiar", en: "nothing to copy" },
    clip_already_watching => {
        es: "ya hay un lunarclip --watch escuchando",
        en: "a lunarclip --watch is already listening",
    },
    clip_no_compositor => { es: "no encuentro el compositor", en: "cannot find the compositor" },
    /// Added to [`Key::clip_no_compositor`] when the session may simply be down.
    clip_session_up => { es: "(¿está la sesión arriba?)", en: "(is the session up?)" },
    /// `vigilando (socket X, historial Y con N entradas)`.
    clip_watching => { es: "vigilando", en: "watching" },
    clip_history => { es: "historial", en: "history" },
    clip_control_withdrawn => {
        es: "el compositor retiró el control de datos; saliendo",
        en: "the compositor withdrew data control; exiting",
    },
    clip_connection_lost => {
        es: "se perdió la conexión con el compositor",
        en: "lost the connection to the compositor",
    },
    clip_stdin_failed => {
        es: "no se pudo leer la entrada estándar",
        en: "could not read standard input",
    },
    /// What the daemon answered, when it was not an `OK`.
    clip_daemon_said => { es: "el demonio dijo", en: "the daemon said" },

    // ── lunarnet: the window ─────────────────────────────────────────────
    net_title => { es: "RED", en: "NETWORK" },
    /// The three things the window edits. The same in both: these are
    /// protocol names.
    net_subtitle => { both: "IPv4 · IPv6 · DNS" },
    net_keys => {
        es: "Tab campo · ←→ modo · Intro aplicar · Esc cerrar",
        en: "Tab field · ←→ mode · Enter applies · Esc closes",
    },
    net_no_ifaces => { es: "no hay interfaces de red", en: "no network interfaces" },

    // ── lunarnet: what an interface is doing now ─────────────────────────
    net_state_up => { es: "conectada", en: "connected" },
    net_state_dormant => { es: "en espera", en: "dormant" },
    net_state_down => { es: "sin conexión", en: "no link" },
    net_state_unknown => { es: "sin estado", en: "no state" },
    net_unapplied => { es: "sin aplicar", en: "not applied" },
    net_no_address => { es: "sin dirección", en: "no address" },
    /// ` · vía 192.168.1.1`, the gateway an address goes out through.
    net_via => { es: "vía", en: "via" },
    /// Prefix of the read-only value beside a section's title, with its
    /// trailing space: `ahora 192.168.1.20/24`.
    net_now => { es: "ahora ", en: "now " },

    // ── lunarnet: the modes ──────────────────────────────────────────────
    net_mode => { es: "Modo", en: "Mode" },
    /// Protocol names, the same in both languages.
    net_mode_dhcp => { both: "DHCP" },
    net_mode_dhcpv6 => { both: "DHCPV6" },
    /// Spanish borrowed this one whole, and it is a mode name in caps.
    net_mode_manual => { both: "MANUAL" },
    net_mode_off => { es: "APAGADO", en: "OFF" },
    net_mode_auto => { es: "AUTOMÁTICO", en: "AUTOMATIC" },

    // ── lunarnet: the fields ─────────────────────────────────────────────
    net_addr_prefix => { es: "Dirección / prefijo", en: "Address / prefix" },
    net_gateway => { es: "Puerta de enlace", en: "Gateway" },
    net_ph_dhcp => { es: "la da el servidor DHCP", en: "the DHCP server gives it" },
    net_ph_no_v4 => { es: "sin IPv4", en: "no IPv4" },
    net_ph_slaac => { es: "automática (SLAAC)", en: "automatic (SLAAC)" },
    net_ph_router => { es: "la anuncia el router", en: "the router announces it" },
    net_ph_dhcpv6 => { es: "la da el servidor DHCPv6", en: "the DHCPv6 server gives it" },
    net_dns_none => { es: "ninguno", en: "none" },
    net_dns_servers => { es: "Servidores", en: "Servers" },
    net_dns_ph => {
        es: "vacío: los que dé DHCP  (1.1.1.1 2606:4700::1111)",
        en: "empty: whatever DHCP gives  (1.1.1.1 2606:4700::1111)",
    },
    /// `192.168.1.1  (opcional)` -- the parenthesis beside an example
    /// address, which is the only part of the placeholder that is a word.
    net_optional => { es: "opcional", en: "optional" },
    net_dns_hint => {
        es: "separados por espacios; IPv4 e IPv6 mezclados",
        en: "space-separated; IPv4 and IPv6 mixed",
    },

    // ── lunarnet: the footer ─────────────────────────────────────────────
    net_apply => { es: "Aplicar", en: "Apply" },
    net_apply_caps => { es: "APLICAR", en: "APPLY" },
    net_undo_caps => { es: "DESHACER", en: "UNDO" },
    net_saved_in => {
        es: "los cambios se guardan en /etc/eclipse/network",
        en: "changes are saved in /etc/eclipse/network",
    },
    net_saved_applied => { es: "guardado y aplicado", en: "saved and applied" },
    net_saved_no_service => {
        es: "guardado; el servicio de red no está en marcha, se aplicará al arrancar",
        en: "saved; the network service is not running, it will be applied at boot",
    },
    /// `wlan0: vuelve a lo que dice el archivo` -- the interface name goes
    /// before it.
    net_reverted => {
        es: "vuelve a lo que dice el archivo",
        en: "back to what the file says",
    },
    /// The error text goes after it.
    net_save_failed => {
        es: "no se pudo guardar /etc/eclipse/network",
        en: "could not save /etc/eclipse/network",
    },

    // ── lunarnet: why a field does not make sense ────────────────────────
    // These are `netconf::check`'s answers. They live here and not there so
    // that the validation stays language-free: it says WHICH field and WHY by
    // key, and the window renders it in the language the session is in.
    net_bad_v4_cidr => {
        es: "no es una dirección IPv4 con prefijo (192.168.1.20/24)",
        en: "not an IPv4 address with a prefix (192.168.1.20/24)",
    },
    net_bad_v6_cidr => {
        es: "no es una dirección IPv6 con prefijo (2001:db8::20/64)",
        en: "not an IPv6 address with a prefix (2001:db8::20/64)",
    },
    net_bad_addr_reserved => {
        es: "esa dirección no se puede dar a una interfaz",
        en: "that address cannot be given to an interface",
    },
    net_bad_prefix4 => {
        es: "el prefijo tiene que estar entre 1 y 32",
        en: "the prefix has to be between 1 and 32",
    },
    net_bad_prefix6 => {
        es: "el prefijo tiene que estar entre 1 y 128",
        en: "the prefix has to be between 1 and 128",
    },
    net_bad_gw4 => { es: "no es una dirección IPv4", en: "not an IPv4 address" },
    net_bad_gw6 => { es: "no es una dirección IPv6", en: "not an IPv6 address" },
    net_bad_gw_is_self => {
        es: "la puerta de enlace no puede ser la propia dirección",
        en: "the gateway cannot be the address itself",
    },
    net_bad_gw_offnet => {
        es: "la puerta de enlace no está en la red de la dirección",
        en: "the gateway is not on the address's network",
    },
    net_bad_dns => {
        es: "hay un servidor DNS que no es una dirección",
        en: "one of the DNS servers is not an address",
    },
}
