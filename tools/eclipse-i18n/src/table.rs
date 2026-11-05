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
//!
//! A row may carry `{}` where a name, a count or an error text goes, and the
//! caller fills it in with `str::replace`. That is for a value in the MIDDLE of
//! a sentence, where the two languages do not agree on the order: the English
//! of `nada abre un .{ext}` puts the extension in the same place, but
//! `{prog} no está instalado` and `{prog} is not installed` would not survive
//! being cut into "before" and "after" fragments in every language. A row with
//! a value at one END of it is still written as the fragment and formatted by
//! the caller. `every_row_fills_the_same_holes_in_both_languages` checks that
//! the two columns of a row hold the same number of `{}`.

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
    /// The calendar's today button, and lunarfile's date column (`hoy
    /// 10:40`). Shared for the reason `word_of` is: one word, one row.
    word_today => { es: "hoy", en: "today" },
    /// `día 203` / `day 203`, the day-of-year label.
    cal_day_of_year => { es: "día", en: "day" },

    // ── the lock screen ──────────────────────────────────────────────────
    lock_state => { es: "BLOQUEADO", en: "LOCKED" },
    lock_hint => {
        es: "pulsa una tecla o haz clic para volver",
        en: "press a key or click to return",
    },
    /// `desde las 10:24 · hace 12 min` / `since 10:24 · for 12 min`.
    lock_since => { es: "desde las {} · {}", en: "since {} · {}" },
    lock_just_now => { es: "ahora mismo", en: "just now" },
    /// `jueves 5 de noviembre de 2026` / `Thursday 5 November 2026`: el día,
    /// el mes y el año, que no van en el mismo orden en los dos idiomas.
    lock_date => { es: "{} de {} de {}", en: "{} {} {}" },
    /// `hace 1 h 05 min` / `for 1 h 05 min`.
    lock_for => { es: "hace {}", en: "for {}" },

    // ── screenshots ──────────────────────────────────────────────────────
    /// The pictures folder. This is a PATH component -- it has to match
    /// whatever `xdg-user-dirs` wrote for the language -- and it is also the
    /// label lunarfile's rail puts on that folder, which is why it is one row
    /// rather than two saying the same word.
    pictures_folder => { es: "Imágenes", en: "Pictures" },
    /// The filename stem: `captura-2026-11-05-053112.png`.
    shot_stem => { es: "captura", en: "screenshot" },
    shot_saved => { es: "Captura guardada", en: "Screenshot saved" },
    shot_open_folder => { es: "Abrir carpeta", en: "Open folder" },
    shot_copied => { es: "copiada al portapapeles", en: "copied to the clipboard" },

    // ── lunarclip: the clipboard picker ──────────────────────────────────
    clip_title => { es: "portapapeles", en: "clipboard" },
    /// `12 entradas` / `12 entries` -- the number goes before it.
    clip_entries => { es: "entradas", en: "entries" },
    /// `3 de 12` / `3 of 12`, when the filter is narrowing the list -- and
    /// `90 M de 200 M` in lunarfile's progress line. Shared, hence the name:
    /// it is one word, and two rows holding it would be two places to change.
    word_of => { es: "de", en: "of" },
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
    // ── lunarfile: the browser and the file chooser ──────────────────────
    file_preview => { es: "Vista previa", en: "Preview" },
    file_places => { es: "Lugares", en: "Places" },
    file_col_name => { es: "Nombre", en: "Name" },
    file_col_size => { es: "Tamaño", en: "Size" },
    /// Also `Modificado 3 oct` under the preview, with the date after it.
    file_col_modified => { es: "Modificado", en: "Modified" },
    /// The header chip of `--pick`. Capitals in both languages: it is a chip.
    file_chip_pick => { es: "SELECTOR", en: "CHOOSER" },
    file_chip_save => { es: "GUARDAR", en: "SAVE" },
    file_title_save => { es: "Guardar en", en: "Save in" },
    file_title_browse => { es: "Archivos", en: "Files" },
    file_title_pick => { es: "Elegir archivo", en: "Choose file" },
    file_label_rename => { es: "Renombrar:", en: "Rename:" },
    file_label_newdir => { es: "Carpeta:", en: "Folder:" },
    file_label_name => { es: "Nombre:", en: "Name:" },
    file_label_filter => { es: "Filtro:", en: "Filter:" },
    file_btn_cancel => { es: "Cancelar", en: "Cancel" },
    /// The primary button, and the menu's first action. `--multiple` puts the
    /// number of marked files after it.
    file_btn_open => { es: "Abrir", en: "Open" },
    file_btn_replace => { es: "Reemplazar", en: "Replace" },
    file_btn_save => { es: "Guardar", en: "Save" },

    // ── lunarfile: the preview column ────────────────────────────────────
    /// A folder with nothing in it, and one whose only entries are dotfiles.
    file_dir_empty => { es: "Carpeta vacía", en: "Empty folder" },
    file_dir_hidden_only => { es: "Sólo ocultos", en: "Hidden only" },
    file_unreadable => { es: "No se puede leer", en: "Cannot be read" },
    file_no_preview => { es: "Sin vista previa", en: "No preview" },
    /// `+12 más`: the count goes before it.
    file_more => { es: "más", en: "more" },
    /// What something is, as the preview names it. `file_folder` is also the
    /// rail's and the details' word for a directory.
    file_folder => { es: "Carpeta", en: "Folder" },
    file_kind_image => { es: "Imagen", en: "Image" },
    file_kind_document => { es: "Documento", en: "Document" },
    file_kind_archive => { es: "Archivo comprimido", en: "Compressed archive" },
    file_kind_disk => { es: "Imagen de disco", en: "Disk image" },
    file_kind_media => { es: "Multimedia", en: "Media" },
    file_kind_code => { es: "Código", en: "Code" },
    file_kind_other => { es: "Archivo", en: "File" },
    /// Counted: `3 carpetas · 12 archivos`.
    file_n_folder => { es: "carpeta", en: "folder" },
    file_n_folders => { es: "carpetas", en: "folders" },
    file_n_file => { es: "archivo", en: "file" },
    file_n_files => { es: "archivos", en: "files" },

    // ── lunarfile: the footer ────────────────────────────────────────────
    file_hint_save => {
        es: "Intro guardar  Esc cancelar  <- subir  . ocultos",
        en: "Enter save  Esc cancel  <- up  . hidden",
    },
    file_hint_many => {
        es: "Espacio marcar  Intro aceptar  Esc cancelar  <- subir",
        en: "Space mark  Enter accept  Esc cancel  <- up",
    },
    file_hint_open => {
        es: "Intro elegir  Esc cancelar  <- subir  . ocultos",
        en: "Enter choose  Esc cancel  <- up  . hidden",
    },
    file_hint_edit => {
        es: "Intro aceptar  Esc cancelar",
        en: "Enter accept  Esc cancel",
    },
    file_hint_browse => {
        es: "Intro abrir  F2 renombrar  F5/F6 copiar/cortar  Insert pegar  F7 carpeta  Supr papelera  Menú acciones",
        en: "Enter open  F2 rename  F5/F6 copy/cut  Insert paste  F7 folder  Del trash  Menu actions",
    },
    /// `3 marcados`, `12 elementos`: the count goes before them.
    file_marked => { es: "marcados", en: "marked" },
    file_items => { es: "elementos", en: "items" },
    file_cut => { es: "Cortado", en: "Cut" },
    file_copied => { es: "Copiado", en: "Copied" },
    file_paste_here => { es: "Insert pega aquí", en: "Insert pastes here" },

    // ── lunarfile: what it says went wrong ───────────────────────────────
    /// The filename goes before these three.
    file_exists_enter => {
        es: "ya existe: Intro para reemplazarlo",
        en: "already exists: Enter to replace it",
    },
    file_exists_enter_again => {
        es: "ya existe: Intro otra vez para reemplazarlo",
        en: "already exists: Enter again to replace it",
    },
    file_is_a_folder => {
        es: "es una carpeta: -> para entrar",
        en: "is a folder: -> to enter it",
    },
    file_type_a_name => { es: "escribe un nombre", en: "type a name" },
    /// `como mucho 64 archivos`: the count and `file_n_files` go after it.
    file_at_most => { es: "como mucho", en: "at most" },
    file_no_opener => {
        es: "nada abre un .{} (ver /etc/eclipse/openers)",
        en: "nothing opens a .{} (see /etc/eclipse/openers)",
    },
    file_no_extension => {
        es: "sin extensión: nada sabe abrirlo",
        en: "no extension: nothing knows how to open it",
    },
    file_not_installed => { es: "{} no está instalado", en: "{} is not installed" },

    // ── lunarfile: the action menu ───────────────────────────────────────
    file_op_rename => { es: "Renombrar", en: "Rename" },
    /// Also the browser's context menu, which Servo hands over in English.
    word_copy => { es: "Copiar", en: "Copy" },
    word_cut => { es: "Cortar", en: "Cut" },
    file_op_paste => { es: "Pegar aquí", en: "Paste here" },
    file_op_newdir => { es: "Nueva carpeta", en: "New folder" },
    /// Also `A la papelera: notas.txt` once it is gone.
    file_op_trash => { es: "A la papelera", en: "To the trash" },
    /// The keycaps the menu prints beside the actions. The function keys are
    /// named the same in both languages; these two are not.
    file_key_enter => { es: "Intro", en: "Enter" },
    file_key_del => { es: "Supr", en: "Del" },
    /// `No se pudo mover a la papelera: ...` / `No se pudo: ...`: what failed
    /// and the error go after it.
    file_could_not => { es: "No se pudo", en: "Could not" },
    file_what_trash => { es: "mover a la papelera", en: "move to the trash" },
    file_nothing_to_paste => {
        es: "Nada que pegar: copia o corta algo antes (F5, F6)",
        en: "Nothing to paste: copy or cut something first (F5, F6)",
    },
    file_pick_something => {
        es: "Elige un archivo o una carpeta primero",
        en: "Choose a file or a folder first",
    },
    file_rename_prompt => {
        es: "Escribe el nombre nuevo · Intro renombra · Esc cancela",
        en: "Type the new name · Enter renames · Esc cancels",
    },
    file_newdir_prompt => {
        es: "Nombre de la carpeta · Intro la crea · Esc cancela",
        en: "The folder's name · Enter creates it · Esc cancels",
    },
    /// The name the new-folder field is filled with, which the user then
    /// edits. A name on disk, so it is written in the session's language and
    /// not translated afterwards: the folder keeps the name it was made with.
    file_new_folder_name => { es: "Carpeta nueva", en: "New folder" },
    /// `Copiado: notas.txt · ve a otra carpeta e Insert lo pega`.
    file_held_tail => {
        es: "ve a otra carpeta e Insert lo pega",
        en: "go to another folder and Insert pastes it",
    },
    file_renamed => { es: "Renombrado", en: "Renamed" },
    file_dir_created => { es: "Carpeta creada", en: "Folder created" },

    // ── lunarfile: the paste that runs in the background ─────────────────
    file_job_cancelling => { es: "Cancelando", en: "Cancelling" },
    file_job_moving => { es: "Moviendo", en: "Moving" },
    file_job_copying => { es: "Copiando", en: "Copying" },
    file_job_esc => { es: "Esc cancela", en: "Esc cancels" },
    file_job_wait => {
        es: "Espera a que termine la copia en curso (Esc la cancela)",
        en: "Wait for the copy in progress to finish (Esc cancels it)",
    },
    /// The name goes after these two.
    file_pasted => { es: "Pegado", en: "Pasted" },
    file_cancelled => { es: "Cancelado", en: "Cancelled" },
    /// The error goes after it.
    file_paste_failed => { es: "No se pudo pegar", en: "Could not paste" },
    file_copy_error => {
        es: "La copia se detuvo por un error",
        en: "The copy stopped on an error",
    },

    // ── lunarfile: what `fileops` itself refuses ─────────────────────────
    // These are the errors fileops RAISES, as opposed to the system's. They
    // live here for the reason the `net_bad_*` rows do: the file operations
    // stay language-free, and `fileops::message` renders them in the
    // session's language.
    file_err_cancelled => { es: "cancelado", en: "cancelled" },
    file_err_bad_name => { es: "nombre no válido", en: "not a valid name" },
    file_err_self_nest => {
        es: "una carpeta no cabe dentro de sí misma",
        en: "a folder does not fit inside itself",
    },
    file_err_exists => {
        es: "ese nombre ya existe",
        en: "that name is already taken",
    },

    // ── lunarfile: the places rail ───────────────────────────────────────
    // The LABEL only. Which directory each one is stays a list of names in
    // `files::XDG_PLACES`, and that list holds both languages' spellings
    // whatever the session is in: an imported home has `Pictures` in it and a
    // Spanish session still has to find it.
    /// The rail's first entry, and the browser's link back to `about:home`.
    word_home => { es: "Inicio", en: "Home" },
    file_place_root => { es: "Raíz /", en: "Root /" },
    file_place_documents => { es: "Documentos", en: "Documents" },
    /// The folder in the rail, and the title of `about:downloads`.
    word_downloads => { es: "Descargas", en: "Downloads" },
    file_place_music => { es: "Música", en: "Music" },

    // ── lunarfile: the date column (`hoy 10:40` is `word_today`) ─────────
    word_yesterday => { es: "ayer", en: "yesterday" },
    // ── SilverSurfer: the chrome ─────────────────────────────────────────
    www_new_tab => { es: "Nueva pestaña", en: "New tab" },
    www_find_in_page => { es: "Buscar en la página", en: "Find in page" },
    www_search_or_url => {
        es: "Buscar o escribir una dirección",
        en: "Search or enter an address",
    },
    /// The xdg window title: `Privado — Eclipse OS`, `Cargando — SilverSurfer`.
    www_private => { es: "Privado", en: "Private" },
    www_loading => { es: "Cargando", en: "Loading" },

    // ── SilverSurfer: the context menu ───────────────────────────────────
    // Servo builds this menu itself and labels it in English (see
    // `vendor/servo-script/.../document_embedder_controls.rs`). Translating
    // it in the vendored copy would be a divergence to carry forever, so
    // `engine.rs` maps the label Servo hands over onto one of these rows on
    // its way into Eclipse's own menu widget.
    www_menu_back => { es: "Atrás", en: "Back" },
    www_menu_forward => { es: "Adelante", en: "Forward" },
    www_menu_reload => { es: "Recargar", en: "Reload" },
    www_menu_paste => { es: "Pegar", en: "Paste" },
    www_menu_select_all => { es: "Seleccionar todo", en: "Select All" },
    www_menu_copy_link => { es: "Copiar enlace", en: "Copy Link" },
    www_menu_copy_image_link => {
        es: "Copiar enlace de la imagen",
        en: "Copy Image Link",
    },
    www_menu_open_link_new => {
        es: "Abrir enlace en una vista nueva",
        en: "Open Link in New View",
    },
    www_menu_open_image_new => {
        es: "Abrir imagen en una vista nueva",
        en: "Open Image in New View",
    },

    // ── SilverSurfer: the about: pages ───────────────────────────────────
    www_history => { es: "Historial", en: "History" },
    www_history_empty => { es: "Aún no hay visitas.", en: "No visits yet." },
    www_bookmarks => { es: "Marcadores", en: "Bookmarks" },
    www_bookmarks_empty => {
        es: "No hay marcadores. Usa Ctrl+D en una página.",
        en: "No bookmarks yet. Use Ctrl+D on a page.",
    },
    www_downloads_empty => {
        es: "Aún no hay descargas. Navega a un PDF/ZIP o usa Ctrl+S sobre una URL.",
        en: "No downloads yet. Browse to a PDF/ZIP or use Ctrl+S on a URL.",
    },
    www_privacy => { es: "Privacidad", en: "Privacy" },
    www_privacy_window => {
        es: "La <strong>ventana privada</strong> (<code>--private</code> o Ctrl+Shift+P) no restaura pestañas ni escribe <code>session.json</code>. Al cerrarla se borran cookies, almacenamiento web y caché HTTP de esa instancia.",
        en: "The <strong>private window</strong> (<code>--private</code> or Ctrl+Shift+P) restores no tabs and writes no <code>session.json</code>. Closing it clears that instance's cookies, web storage and HTTP cache.",
    },
    www_privacy_servo => {
        es: "Servo 0.7 no aísla aún el almacenamiento privado por WebView; el proceso dedicado es el aislamiento práctico en Eclipse.",
        en: "Servo 0.7 does not isolate private storage per WebView yet; the separate process is what isolation means in Eclipse.",
    },
    www_privacy_clear_now => {
        es: "borrar cookies y datos de sitio ahora",
        en: "clear cookies and site data now",
    },
    www_privacy_same_via_page => {
        es: "misma acción vía página",
        en: "the same thing from a page",
    },
    www_privacy_site_list => {
        es: "lista de sitios con datos",
        en: "the list of sites holding data",
    },
    www_privacy_logins => {
        es: "logins guardados (no en privado)",
        en: "saved logins (not in a private window)",
    },
    www_cleared => { es: "Datos borrados", en: "Data cleared" },
    www_back_to_privacy => { es: "Volver a privacidad", en: "Back to privacy" },
    www_cookies_and_data => { es: "Cookies y datos", en: "Cookies and data" },
    www_cookies_title => {
        es: "Cookies y datos de sitio",
        en: "Cookies and site data",
    },
    www_cookies_empty => {
        es: "No hay sitios con cookies ni almacenamiento local.",
        en: "No sites with cookies or local storage.",
    },
    /// `12 sitios:` -- the count goes before it.
    www_sites => { es: "sitios:", en: "sites:" },
    www_clear_all => { es: "Borrar todo", en: "Clear all" },
    www_passwords => { es: "Contraseñas", en: "Passwords" },
    www_passwords_saved => {
        es: "Contraseñas guardadas",
        en: "Saved passwords",
    },
    www_passwords_how => {
        es: "Ctrl+Shift+L guarda el login visible. Autocompletado al cargar el origen. Cifrado con ChaCha20-Poly1305 en <code>passwords.json</code>; la clave está en <code>passwords.key</code>.",
        en: "Ctrl+Shift+L saves the login on screen. Filled in again when the origin loads. Encrypted with ChaCha20-Poly1305 in <code>passwords.json</code>; the key is in <code>passwords.key</code>.",
    },
    www_empty => { es: "Vacío.", en: "Empty." },
    www_filter => { es: "Filtro", en: "Filter" },
    www_filter_title => {
        es: "Bloqueo de anuncios (lite)",
        en: "Ad blocking (lite)",
    },
    www_filter_on => { es: "activo", en: "on" },
    www_filter_off => {
        es: "desactivado (SILVERSURFER_ADBLOCK=0)",
        en: "off (SILVERSURFER_ADBLOCK=0)",
    },
    /// `Estado: activo. Reglas embebidas: 412. Cancelaciones de red en esta
    /// sesión (aprox.): 7.` -- the three numbers fill the three holes.
    www_filter_state => {
        es: "Estado: <strong>{}</strong>. Reglas embebidas: {}. Cancelaciones de red en esta sesión (aprox.): {}.",
        en: "Status: <strong>{}</strong>. Rules built in: {}. Network cancellations this session (approx.): {}.",
    },
    www_filter_not_ublock => {
        es: "No es uBlock: solo hosts/paths frecuentes + CSS hide. Sin WebExtensions.",
        en: "This is not uBlock: common hosts and paths plus CSS hiding, and no WebExtensions.",
    },
    // ── lunarbg: the tecno wallpaper's readout ───────────────────────────
    // The eight phases of the Moon, as the instrument panel prints them.
    // Written in capitals in both columns because the face has no lower
    // case: it draws capitals the height of a 5x7 cell and nothing else.
    moon_new => { es: "LUNA NUEVA", en: "NEW MOON" },
    moon_waxing_crescent => { es: "LUNA CRECIENTE", en: "WAXING CRESCENT" },
    moon_first_quarter => { es: "CUARTO CRECIENTE", en: "FIRST QUARTER" },
    moon_waxing_gibbous => { es: "GIBOSA CRECIENTE", en: "WAXING GIBBOUS" },
    moon_full => { es: "LUNA LLENA", en: "FULL MOON" },
    moon_waning_gibbous => { es: "GIBOSA MENGUANTE", en: "WANING GIBBOUS" },
    moon_last_quarter => { es: "CUARTO MENGUANTE", en: "LAST QUARTER" },
    moon_waning_crescent => { es: "LUNA MENGUANTE", en: "WANING CRESCENT" },

    // ── lunardesk: the desktop switcher ──────────────────────────────────
    desk_desktops => { es: "ESCRITORIOS", en: "DESKTOPS" },
    desk_keys => {
        es: "1–9 o ←→ elegir · Intro ir · clic en una ventana · Esc cerrar",
        en: "1–9 or ←→ pick · Enter go · click a window · Esc close",
    },
    /// Un escritorio sin ninguna ventana.
    desk_empty => { es: "vacío", en: "empty" },

    // ── lunarnotes: the notification history ─────────────────────────────
    notes_notifications => { es: "AVISOS", en: "NOTIFICATIONS" },
    notes_no_notifications_yet => {
        es: "no ha llegado ningún aviso",
        en: "no notifications yet",
    },
    notes_keys => {
        es: "↑↓ recorrer · Supr borrar todo · Esc cerrar",
        en: "↑↓ scroll · Del clear all · Esc close",
    },
    /// `7 · 2 nuevos` / `7 · 2 new`: el número va delante.
    notes_new => { es: "nuevos", en: "new" },

    // ── lunarhunter: the security panel ──────────────────────────────────
    // Its tabs, the seven domains and their switches, the network tab, the
    // scheduled scans, and what each firewall mode does.
    // Los escaneos programados: el nombre del trabajo, lo que dio y de dónde
    // salió la ejecución.
    hunt_sched_quick => { es: "Rápido", en: "Quick" },
    hunt_sched_full => { es: "Completo", en: "Full" },
    hunt_sched_clean => { es: "limpio", en: "clean" },
    /// `amenazas: 3` / `threats: 3`: el número va detrás.
    hunt_sched_threats => { es: "amenazas:", en: "threats:" },
    hunt_sched_quarantined => { es: "aisladas:", en: "quarantined:" },
    hunt_sched_failed => { es: "falló", en: "failed" },
    /// Quién lanzó la ejecución.
    hunt_sched_manual => { es: "a mano", en: "manual" },
    hunt_sched_clock => { es: "reloj", en: "clock" },
    /// `1.204 arch.` / `1,204 files`: cortado porque la tarjeta no da más.
    hunt_sched_files => { es: "arch.", en: "files" },
    /// Los dos destinos del escaneo que no son una carpeta del usuario.
    hunt_target_all => { es: "Todo (/)", en: "All (/)" },
    /// `3 rutas` / `3 paths`: el número va delante.
    hunt_target_paths => { es: "rutas", en: "paths" },
    hunt_fw_off => { es: "Apagado: no se comprueba nada.", en: "Off: nothing is checked." },
    hunt_fw_report => {
        es: "Aviso: deja pasar y pregunta.",
        en: "Report: lets it through and asks.",
    },
    hunt_net_this_kernel_has_no_network_monitor => {
        es: "Este núcleo no tiene monitor de red",
        en: "This kernel has no network monitor",
    },
    hunt_net_no_proc_hunter_net_a_newer_hunter => {
        es: "No hay /proc/hunter_net: hace falta un hunter más nuevo.",
        en: "No /proc/hunter_net: a newer hunter is needed.",
    },
    hunt_net_listening => { es: "en escucha", en: "listening" },
    hunt_net_unexpected => { es: "inesperados", en: "unexpected" },
    hunt_net_connections => { es: "conexiones", en: "connections" },
    hunt_net_alerts_on => { es: "Avisos: activados", en: "Alerts: on" },
    hunt_net_alerts_off => { es: "Avisos: desactivados", en: "Alerts: off" },
    hunt_net_ports_on_127_0_0_1_only_do_not_ale => {
        es: "Los puertos solo de 127.0.0.1 no avisan: no se ven desde la red.",
        en: "Ports on 127.0.0.1 only do not alert: the network cannot reach them.",
    },
    hunt_net_ports_on_127_0_0_1_only_alert_too => {
        es: "También avisan los puertos solo de 127.0.0.1.",
        en: "Ports on 127.0.0.1 only alert too.",
    },
    hunt_net_connections_by_process => {
        es: "Conexiones por proceso",
        en: "Connections by process",
    },
    hunt_net_no_process_holds_a_network_socket => {
        es: "ningún proceso tiene sockets de red",
        en: "no process holds a network socket",
    },
    hunt_net_alert => { es: "¡ALERTA!", en: "ALERT!" },
    hunt_net_listen => { es: "escucha", en: "listen" },
    hunt_net_connected => { es: "conexión", en: "connected" },
    hunt_net_expected_ports_etc_hunter_ports => {
        es: "Puertos esperados (/etc/hunter/ports)",
        en: "Expected ports (/etc/hunter/ports)",
    },
    hunt_net_remove => { es: "Quitar", en: "Remove" },
    hunt_net_more_hunterctl_net => { es: "más (hunterctl net)", en: "more (hunterctl net)" },
    hunt_net_alerts_since_boot => { es: "avisos desde el arranque", en: "alerts since boot" },
    hunt_sched_schedule_saved => { es: "programación guardada", en: "schedule saved" },
    hunt_sched_no_etc_hunter_schedule => {
        es: "No hay /etc/hunter/schedule",
        en: "No /etc/hunter/schedule",
    },
    hunt_sched_this_image_predates_scheduled_scan => {
        es: "Esta imagen es anterior a los escaneos programados.",
        en: "This image predates scheduled scans.",
    },
    hunt_sched_the_hunter_scheduler_service_runs => {
        es: "Los lanza el servicio hunter-scheduler, sin esta ventana.",
        en: "The hunter-scheduler service runs them, window or not.",
    },
    hunt_sched_if_the_machine_is_off_then_they_ru => {
        es: "Si el equipo está apagado a esa hora, se hacen al volver.",
        en: "If the machine is off then, they run when it is back.",
    },
    hunt_sched_scheduled_scan_running => {
        es: "escaneo programado en curso",
        en: "scheduled scan running",
    },
    hunt_sched_none_has_run_yet => {
        es: "todavía no se ha hecho ninguno",
        en: "none has run yet",
    },
    hunt_sched_run_now => { es: "Ejecutar ahora", en: "Run now" },
    hunt_sched_disabled => { es: "desactivado", en: "disabled" },
    hunt_sched_next_now => { es: "próximo: ahora", en: "next: now" },
    hunt_sched_next => { es: "próximo:", en: "next:" },
    hunt_sched_off => { es: "no", en: "off" },
    hunt_sched_daily => { es: "diario", en: "daily" },
    hunt_sched_weekly => { es: "semanal", en: "weekly" },
    hunt_sched_on_a_finding => { es: "Si encuentra algo:", en: "On a finding:" },
    hunt_sched_alert => { es: "avisar", en: "alert" },
    hunt_sched_quarantine => { es: "cuarentena", en: "quarantine" },

    hunt_tab_overview => { es: "Resumen", en: "Overview" },
    hunt_tab_protection => { es: "Protección", en: "Protection" },
    hunt_tab_events => { es: "Eventos", en: "Events" },
    hunt_tab_scan => { es: "Escaneo", en: "Scan" },
    hunt_tab_quarantine => { es: "Cuarentena", en: "Quarantine" },
    hunt_tab_signatures => { es: "Firmas", en: "Signatures" },
    hunt_tab_programs => { es: "Programas", en: "Programs" },
    hunt_tab_scheduled => { es: "Programados", en: "Scheduled" },
    hunt_tab_network => { es: "Red", en: "Network" },
    hunt_tab_firewall => { es: "Cortafuegos", en: "Firewall" },
    hunt_dom_per_process_system_calls => {
        es: "llamadas al sistema por proceso",
        en: "per-process system calls",
    },
    hunt_dom_writable_executable_memory_w_x => {
        es: "memoria escribible y ejecutable (W^X)",
        en: "writable + executable memory (W^X)",
    },
    hunt_dom_which_programs_may_run => {
        es: "qué programas pueden ejecutarse",
        en: "which programs may run",
    },
    hunt_dom_heuristics_floods_fork_bombs => {
        es: "heurísticas: floods, fork-bombs",
        en: "heuristics: floods, fork bombs",
    },
    hunt_dom_sha_256_of_pinned_programs => {
        es: "SHA-256 de los programas fijados",
        en: "SHA-256 of pinned programs",
    },
    hunt_dom_signature_antivirus_at_exec => {
        es: "antivirus por firmas al ejecutar",
        en: "signature antivirus at exec",
    },
    hunt_dom_ransomware_and_process_injection => {
        es: "ransomware e inyección en procesos",
        en: "ransomware and process injection",
    },
    hunt_sw_learning => { es: "aprendizaje", en: "learning" },
    hunt_sw_trust_new_programs_on_first_use => {
        es: "confiar en lo nuevo al primer uso",
        en: "trust new programs on first use",
    },
    hunt_sw_anomaly_detection => { es: "detección de anomalías", en: "anomaly detection" },
    hunt_sw_flood_and_fork_bomb_counters => {
        es: "contadores de floods y fork-bombs",
        en: "flood and fork-bomb counters",
    },
    hunt_sw_privileged_deny => { es: "denegar privilegiadas", en: "privileged deny" },
    hunt_sw_modules_bpf_kexec_ptrace => {
        es: "módulos, bpf, kexec, ptrace",
        en: "modules, bpf, kexec, ptrace",
    },
    hunt_sw_require_hash => { es: "exigir hash", en: "require hash" },
    hunt_sw_an_unpinned_program_is_a_violation => {
        es: "un programa sin fijar es una violación",
        en: "an unpinned program is a violation",
    },
    hunt_sw_scan_usb => { es: "escanear USB", en: "scan USB" },
    hunt_sw_scan_every_stick_on_mount => {
        es: "escanear cada pendrive al montarlo",
        en: "scan every stick or disk when mounted",
    },
    hunt_sw_lock_usb => { es: "bloquear USB", en: "lock USB" },
    hunt_sw_nothing_runs_from_usb_unscanned => {
        es: "nada se ejecuta del USB sin escanear",
        en: "nothing runs from USB until scanned",
    },

    hunt_root_required => { es: "hace falta root", en: "root required" },
    hunt_proc_hunter_not_responding => {
        es: "/proc/hunter no responde",
        en: "/proc/hunter not responding",
    },
    hunt_sealed_cannot_relax => {
        es: "sellado: no se puede relajar",
        en: "sealed: cannot relax",
    },
    hunt_applied => { es: "aplicado", en: "applied" },
    hunt_scan_cancelled => { es: "escaneo cancelado", en: "scan cancelled" },
    hunt_no_threats_in => { es: "sin amenazas en", en: "no threats in" },
    hunt_threats_found => { es: "amenazas encontradas", en: "threats found" },
    hunt_click_again_to_confirm => {
        es: "pulsa otra vez para confirmar",
        en: "click again to confirm",
    },
    hunt_sealed_cannot_allow => {
        es: "sellado: no se puede permitir",
        en: "sealed: cannot allow",
    },
    hunt_there_is_no_hunter_to_read => {
        es: "no hay hunter que leer",
        en: "there is no hunter to read",
    },
    hunt_report_saved_to => { es: "informe guardado en", en: "report saved to" },
    hunt_sure => { es: "¿seguro?", en: "sure?" },
    hunt_exposed => { es: "Expuesto:", en: "Exposed:" },
    hunt_domain_s_off => { es: "dominio(s) apagado(s)", en: "domain(s) off" },
    hunt_protected_everything_enforces => {
        es: "Protegido: todo bloquea",
        en: "Protected: everything enforces",
    },
    hunt_watching => { es: "Vigilando:", en: "Watching:" },
    hunt_domain_s_only_report => { es: "dominio(s) solo avisan", en: "domain(s) only report" },
    hunt_sealed => { es: "sellado", en: "sealed" },
    hunt_read_only_not_root => {
        es: "solo lectura (no eres root)",
        en: "read only (not root)",
    },
    hunt_this_kernel_has_no_hunter => {
        es: "Este núcleo no tiene hunter",
        en: "This kernel has no hunter",
    },
    hunt_exec_allowlist => { es: "lista de ejecución", en: "exec allowlist" },
    hunt_active => { es: "activa", en: "active" },
    hunt_inactive => { es: "inactiva", en: "inactive" },
    hunt_seal => { es: "sello", en: "seal" },
    hunt_events => { es: "eventos", en: "events" },
    hunt_blocked => { es: "bloqueos", en: "blocked" },
    hunt_warnings => { es: "avisos", en: "warnings" },
    hunt_critical => { es: "críticos", en: "critical" },
    hunt_signatures => { es: "firmas", en: "signatures" },
    hunt_detections => { es: "detecciones", en: "detections" },
    hunt_pinned_programs => { es: "programas fijados", en: "pinned programs" },
    hunt_quarantined => { es: "en cuarentena", en: "quarantined" },
    hunt_quick_actions => { es: "Acciones rápidas", en: "Quick actions" },
    hunt_scan_home => { es: "Escanear inicio", en: "Scan home" },
    hunt_update_signatures => { es: "Actualizar firmas", en: "Update signatures" },
    hunt_lockdown => { es: "Blindar", en: "Lockdown" },
    hunt_export_report => { es: "Exportar informe", en: "Export report" },
    hunt_latest_alerts => { es: "Últimas alertas", en: "Latest alerts" },
    hunt_no_alerts => { es: "sin alertas", en: "no alerts" },
    hunt_removable_devices => { es: "Dispositivos extraíbles", en: "Removable devices" },
    hunt_scan => { es: "Escanear", en: "Scan" },
    hunt_domains => { es: "Dominios", en: "Domains" },
    hunt_switches => { es: "Interruptores", en: "Switches" },
    hunt_save_as_boot_policy => {
        es: "Guardar como política de arranque",
        en: "Save as boot policy",
    },
    hunt_apply_policy_conf => { es: "Aplicar policy.conf", en: "Apply policy.conf" },
    hunt_seal_2 => { es: "Sellar", en: "Seal" },
    hunt_all => { es: "Todos", en: "All" },
    hunt_alerts => { es: "Alertas", en: "Alerts" },
    hunt_critical_2 => { es: "Críticos", en: "Critical" },
    hunt_dropped => { es: "descartados", en: "dropped" },
    hunt_in_the_ring => { es: "en el anillo", en: "in the ring" },
    hunt_no_events => { es: "sin eventos", en: "no events" },
    hunt_what_to_scan => { es: "Qué escanear:", en: "What to scan:" },
    hunt_automatic_quarantine => { es: "Cuarentena automática", en: "Automatic quarantine" },
    hunt_listing_files => { es: "listando archivos...", en: "listing files..." },
    hunt_threats => { es: "amenazas", en: "threats" },
    hunt_unreadable => { es: "ilegibles", en: "unreadable" },
    hunt_quarantine_all => { es: "Cuarentena a todo", en: "Quarantine all" },
    hunt_the_quarantine_is_empty => {
        es: "la cuarentena está vacía",
        en: "the quarantine is empty",
    },
    hunt_restore => { es: "Restaurar", en: "Restore" },
    hunt_signatures_loaded => { es: "firmas cargadas", en: "signatures loaded" },
    hunt_by_sha_256_hash => { es: "por hash SHA-256", en: "by SHA-256 hash" },
    hunt_byte_patterns => { es: "patrones de bytes", en: "byte patterns" },
    hunt_update_now => { es: "Actualizar ahora", en: "Update now" },
    hunt_feeds_etc_hunter_feeds => {
        es: "Fuentes (/etc/hunter/feeds)",
        en: "Feeds (/etc/hunter/feeds)",
    },
    hunt_none => { es: "ninguna", en: "none" },
    hunt_trusted => { es: "de confianza", en: "trusted" },
    hunt_learned => { es: "aprendidos", en: "learned" },
    hunt_blacklisted => { es: "lista negra", en: "blacklisted" },
    hunt_pinned_kernel_disk => { es: "fijados núcleo/disco", en: "pinned kernel/disk" },
    hunt_verify_integrity => { es: "Verificar integridad", en: "Verify integrity" },
    hunt_enforce_waits_for_you => {
        es: "Bloqueo: espera tu respuesta",
        en: "Enforce: waits for you",
    },
    hunt_then_blocks => { es: ", si no, bloquea.", en: ", then blocks." },
    hunt_questions => { es: "Preguntas", en: "Questions" },
    hunt_allow => { es: "¿Permitir que", en: "Allow" },
    hunt_to_connect_to_the_network => {
        es: "se conecte a la red?",
        en: "to connect to the network?",
    },
    hunt_to_listen_on_port => { es: "escuche en el puerto", en: "to listen on port" },
    hunt_block => { es: "Bloquear", en: "Block" },
    hunt_allow_2 => { es: "Permitir", en: "Allow" },
    hunt_ignore => { es: "Ignorar", en: "Ignore" },
    hunt_attempt_s => { es: "intento(s)", en: "attempt(s)" },
    hunt_ago => { es: "hace", en: "ago" },
    hunt_and => { es: "y", en: "and" },
    hunt_rules => { es: "Reglas", en: "Rules" },
    hunt_allowed => { es: "permitido", en: "allowed" },
    hunt_blocked_2 => { es: "bloqueado", en: "blocked" },
    hunt_both => { es: "ambas", en: "both" },
    hunt_out => { es: "salida", en: "out" },
    hunt_in => { es: "entrada", en: "in" },
    hunt_forget => { es: "Olvidar", en: "Forget" },
    hunt_hunter_rejected_the_change_nothing => {
        es: "hunter rechazó el cambio; no se aplicó nada",
        en: "hunter rejected the change; nothing was applied",
    },
    hunt_the_command_failed_see_the_console => {
        es: "la orden falló (ver la consola)",
        en: "the command failed (see the console)",
    },
    hunt_applied_and_saved_to_etc_hunter_fi => {
        es: "aplicado y guardado en /etc/hunter/firewall",
        en: "applied and saved to /etc/hunter/firewall",
    },
    hunt_saved_to_etc_hunter_policy_conf_ap => {
        es: "guardado en /etc/hunter/policy.conf: se aplicará al arrancar",
        en: "saved to /etc/hunter/policy.conf: applied at boot",
    },
    hunt_no_proc_hunter_lunarhunter_needs_t => {
        es: "No hay /proc/hunter. lunarhunter necesita el núcleo de Eclipse OS.",
        en: "No /proc/hunter. lunarhunter needs the Eclipse OS kernel.",
    },
    hunt_no_device_mounted_they_are_scanned => {
        es: "ningún dispositivo montado: se escanean al conectarlos",
        en: "no device mounted: they are scanned when plugged in",
    },
    hunt_no_device_mounted_scan_usb_is_off => {
        es: "ningún dispositivo montado (escanear USB está apagado)",
        en: "no device mounted (scan USB is off)",
    },
    hunt_sealing_is_one_way_until_reboot_se => {
        es: "Sellar es de un solo sentido: hasta reiniciar solo se puede endurecer.",
        en: "Sealing is one way: until reboot, settings can only tighten.",
    },
    hunt_files_live_in_var_lib_hunter_quara => {
        es: "Los archivos están en /var/lib/hunter/quarantine, con permisos 000.",
        en: "Files live in /var/lib/hunter/quarantine, mode 000.",
    },
    hunt_restoring_does_not_lift_the_deny_o => {
        es: "Restaurar no quita la denegación de su ruta hasta reiniciar.",
        en: "Restoring does not lift the deny on its path until reboot.",
    },
    hunt_signature_files_loaded_at_boot => {
        es: "Archivos de firmas (se cargan al arrancar)",
        en: "Signature files (loaded at boot)",
    },
    hunt_save_learned_to_the_whitelist => {
        es: "Guardar aprendidos en la whitelist",
        en: "Save learned to the whitelist",
    },
    hunt_learned_programs_trusted_on_first => {
        es: "Programas aprendidos (confiados al primer uso)",
        en: "Learned programs (trusted on first use)",
    },
    hunt_this_kernel_has_no_firewall => {
        es: "Este núcleo no tiene cortafuegos",
        en: "This kernel has no firewall",
    },
    hunt_no_proc_hunter_firewall_a_newer_hu => {
        es: "No hay /proc/hunter_firewall: hace falta un hunter más nuevo.",
        en: "No /proc/hunter_firewall: a newer hunter is needed.",
    },
    hunt_no_program_is_waiting_for_an_answe => {
        es: "Ningún programa espera respuesta.",
        en: "No program is waiting for an answer.",
    },
    hunt_no_rules_yet_answering_a_question => {
        es: "Todavía no hay reglas: se crean al responder.",
        en: "No rules yet: answering a question makes one.",
    },


    // ── lunarmon: the system monitor ──────────────────────────────────────
    /// `firefox es parte del escritorio: pulsa otra vez para confirmar`. The
    /// name goes FIRST in both languages, so the row is the rest of the
    /// sentence and the caller puts the name in front of it.
    mon_part_of_the_desktop => {
        es: "es parte del escritorio: pulsa otra vez para confirmar",
        en: "is part of the desktop: press again to confirm",
    },
    /// Same shape: the service's name goes first.
    mon_runs_once_at_boot => {
        es: "se ejecuta una vez al arrancar: no se inicia ni se para",
        en: "runs once at boot: it is not started or stopped",
    },
    mon_applications_with_a_window => {
        es: "APLICACIONES · con ventana",
        en: "APPLICATIONS · with a window",
    },
    mon_system => { es: "SISTEMA", en: "SYSTEM" },
    mon_could_not => { es: "no se pudo", en: "could not" },
    mon_pick_a_process_from_the_list => {
        es: "elige un proceso de la lista",
        en: "pick a process from the list",
    },
    mon_eclipse_init_is_not_ended_from_her => {
        es: "eclipse-init no se termina desde aquí",
        en: "eclipse-init is not ended from here",
    },
    mon_disks => { es: "discos", en: "disks" },
    mon_asked_to_end => { es: "pedido que termine", en: "asked to end" },
    mon_killed => { es: "terminado a la fuerza", en: "killed" },
    mon_eclipse_init_asked_to_start_it => {
        es: "pedido a eclipse-init que lo inicie",
        en: "eclipse-init asked to start it",
    },
    mon_stopped_until_started_again_or_the => {
        es: "parado hasta que se inicie o se reinicie el equipo",
        en: "stopped until started again or the next boot",
    },
    mon_restarting => { es: "reiniciando", en: "restarting" },
    mon_processes => { es: "PROCESOS", en: "PROCESSES" },
    mon_services => { es: "SERVICIOS", en: "SERVICES" },
    mon_up => { es: "encendido", en: "up" },
    mon_load => { es: "carga", en: "load" },
    mon_services_2 => { es: "servicios", en: "services" },
    mon_running => { es: "en marcha", en: "running" },
    mon_in_trouble => { es: "con problemas", en: "in trouble" },
    mon_memory => { es: "MEMORIA", en: "MEMORY" },
    mon_cached => { es: "en caché", en: "cached" },
    mon_free => { es: "libre", en: "free" },
    mon_swap => { es: "intercambio", en: "swap" },
    mon_no_swap => { es: "sin intercambio", en: "no swap" },
    mon_in => { es: "▼ recibe", en: "▼ in" },
    mon_out => { es: "▲ envía", en: "▲ out" },
    mon_no_ipv4_address => { es: "sin dirección IPv4", en: "no IPv4 address" },
    mon_disk => { es: "DISCO", en: "DISK" },
    mon_the_kernel_gives_no_proc_diskstats => {
        es: "el kernel no da /proc/diskstats",
        en: "the kernel gives no /proc/diskstats",
    },
    mon_read => { es: "lee", en: "read" },
    mon_write => { es: "escribe", en: "write" },
    mon_search => { es: "Buscar", en: "Search" },
    mon_type_a_name_or_a_pid => {
        es: "escribe un nombre o un PID",
        en: "type a name or a PID",
    },
    mon_name => { es: "nombre", en: "name" },
    mon_memory_2 => { es: "memoria", en: "memory" },
    mon_sort => { es: "orden", en: "sort" },
    mon_processes_2 => { es: "procesos", en: "processes" },
    mon_with_a_window => { es: "con ventana", en: "with a window" },
    mon_name_2 => { es: "NOMBRE", en: "NAME" },
    mon_threads => { es: "HILOS", en: "THREADS" },
    mon_state => { es: "ESTADO", en: "STATE" },
    mon_time => { es: "TIEMPO", en: "TIME" },
    mon_no_process_matches => { es: "ningún proceso coincide", en: "no process matches" },
    mon_pick_del_end_shift_del_kill_tab_se => {
        es: "↑↓ elegir · Supr terminar · Mayús+Supr matar · Tab servicios",
        en: "↑↓ pick · Del end · Shift+Del kill · Tab services",
    },
    mon_end => { es: "TERMINAR", en: "END" },
    mon_kill => { es: "MATAR", en: "KILL" },
    mon_service => { es: "SERVICIO", en: "SERVICE" },
    mon_kind => { es: "TIPO", en: "KIND" },
    mon_starts => { es: "ARRANQUES", en: "STARTS" },
    mon_for => { es: "DESDE", en: "FOR" },
    mon_eclipse_init_has_not_written_run_e => {
        es: "eclipse-init no ha escrito /run/eclipse-init/services",
        en: "eclipse-init has not written /run/eclipse-init/services",
    },
    mon_respawn => { es: "continuo", en: "respawn" },
    mon_once => { es: "una vez", en: "once" },
    mon_respawn_restarted_when_it_ends => {
        es: "continuo: se reinicia si termina",
        en: "respawn: restarted when it ends",
    },
    mon_once_at_boot => { es: "una vez, al arrancar", en: "once, at boot" },
    mon_runs => { es: "ejecuta", en: "runs" },
    mon_kind_2 => { es: "tipo", en: "kind" },
    mon_log => { es: "registro", en: "log" },
    mon_cpu_memory => { es: "CPU · memoria", en: "CPU · memory" },
    mon_last_lines_of_the_log => {
        es: "ÚLTIMAS LÍNEAS DEL REGISTRO",
        en: "LAST LINES OF THE LOG",
    },
    mon_this_service_keeps_no_log => {
        es: "este servicio no guarda registro",
        en: "this service keeps no log",
    },
    mon_the_log_is_empty => { es: "el registro está vacío", en: "the log is empty" },
    mon_pick_i_start_r_restart_p_stop_unti => {
        es: "↑↓ elegir · I iniciar · R reiniciar · P parar (hasta el próximo arranque) · Tab procesos",
        en: "↑↓ pick · I start · R restart · P stop (until the next boot) · Tab processes",
    },
    mon_start => { es: "INICIAR", en: "START" },
    mon_restart => { es: "REINICIAR", en: "RESTART" },
    mon_stop => { es: "PARAR", en: "STOP" },
    mon_system_monitor => { es: "Monitor del sistema", en: "System monitor" },
}
