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
    lock_just_now => { es: "ahora mismo", en: "just now" },

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
