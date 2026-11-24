/* La mitad de `--mode upgrade` que no vive en la partición EFI: la raíz ya
 * instalada.
 *
 * Es un fichero aparte de `install-eclipse.c` por tamaño, y una cabecera (y no
 * una unidad de traducción nueva) porque el instalador se compila de un solo
 * `.c`, igual que `i18n.h`. Se incluye DESPUÉS de las funciones de copia,
 * montaje y fstab que usa, justo antes de `run_upgrade`.
 *
 * Hay que tenerlo en la lista de fuentes de `install_eclipse` del xtask: un
 * cambio aquí con el binario ya construido lo dejaría con el código viejo y no
 * lo diría ni un aviso. */
#ifndef ECLIPSE_INSTALLER_UPGRADE_H
#define ECLIPSE_INSTALLER_UPGRADE_H

/* ------------------------------------------------------------------------
 * La mitad de la actualización que NO vive en la partición EFI.
 *
 * `--mode upgrade` copiaba tres ficheros a la ESP (cargador, kernel,
 * initramfs) y no tocaba la partición ROOT jamás, así que una máquina ya
 * instalada se quedaba para siempre con los envoltorios de
 * `/usr/local/bin`, las filas de `/etc/eclipse/services` y el `eclipse-init`
 * del día en que se instaló. Eso no es un detalle: es el bucle de
 * `error: dbus-system: /usr/local/bin/eclipse-dbus-system does not exist`
 * cada ocho segundos en la consola de una máquina que acaba de "actualizar",
 * y la única salida era reinstalar de cero.
 *
 * Esos ficheros los escribe el xtask dentro del rootfs y viajan EN EL MEDIO
 * (`LIVE_KEEP` copia `usr/local/bin`, `etc` y `sbin` al root live tal cual),
 * así que la fuente de la renovación es la propia raíz del instalador: no
 * hace falta descomprimir el payload de ROOT, que son varios GiB.
 *
 * Lo que NO se toca, a propósito: `/home`, `/root`, los paquetes de apt y la
 * configuración que el usuario pueda haber editado. De `/etc/eclipse` solo se
 * pisan las filas de servicios (describen programas que vienen en la imagen);
 * el resto de ese directorio es estado del usuario --el escritorio elegido, el
 * aspecto, el teclado, la zona horaria-- y solo se escribe lo que FALTE.
 * ------------------------------------------------------------------------ */

/* Qué hacer con cada ruta de `ECLIPSE_OWNED`. */
enum refresh_kind {
    REFRESH_FILE,       /* un fichero: se pisa siempre */
    REFRESH_TREE,       /* un árbol entero: se pisa siempre */
    REFRESH_SUFFIX,     /* los ficheros de un directorio con ese sufijo */
    REFRESH_IF_MISSING  /* los ficheros de un directorio, solo los que falten */
};

struct refresh_entry {
    const char *rel;    /* ruta relativa a la raíz, sin barra inicial */
    enum refresh_kind kind;
    const char *suffix; /* REFRESH_SUFFIX: el sufijo que exige */
};

/* Los ficheros de Eclipse en la raíz instalada, los que el kernel nuevo
 * espera encontrar. El test `la_actualizacion_solo_renueva_rutas_del_medio`
 * del xtask cruza esta tabla con `LIVE_KEEP`: una ruta que el medio no lleve
 * no se puede renovar, y el agujero sería mudo. */
static const struct refresh_entry ECLIPSE_OWNED[] = {
    /* Los envoltorios y ayudantes del escritorio y de los servicios. */
    { "usr/local/bin", REFRESH_TREE, NULL },
    /* PID 1: va emparejado con el kernel que se acaba de copiar a la ESP. */
    { "sbin/eclipse-init", REFRESH_FILE, NULL },
    /* Las filas de servicios: describen los programas de la imagen. */
    { "etc/eclipse/services", REFRESH_SUFFIX, ".service" },
    /* El resto de /etc/eclipse es del usuario: solo lo que falte. */
    { "etc/eclipse", REFRESH_IF_MISSING, NULL },
    /* La versión del sistema, para que la próxima actualización sepa cuál es. */
    { "etc/os-release", REFRESH_FILE, NULL },
};
#define ECLIPSE_OWNED_COUNT (sizeof(ECLIPSE_OWNED) / sizeof(ECLIPSE_OWNED[0]))

/* Los nombres que el cargador puede tener en la ESP, en orden de preferencia.
 *
 * `BootX64.efi` estaba escrito a pelo en las tres rutas de la copia, y
 * `copy_upgrade_payload_if_exists` da 0 cuando el origen no está: en un medio
 * de arm64 (`bootaa64.efi`) la actualización decía "Omitido" dos veces y
 * terminaba con ACTUALIZACIÓN COMPLETADA CON ÉXITO sin haber copiado nada. */
static const char *const ESP_LOADER_NAMES[] = {
    "BootX64.efi", "bootx64.efi", "BootAA64.efi", "bootaa64.efi",
    "BootRiscv64.efi", "bootriscv64.efi",
};
#define ESP_LOADER_COUNT (sizeof(ESP_LOADER_NAMES) / sizeof(ESP_LOADER_NAMES[0]))

/* Si un directorio de la ESP ya extraída trae un cargador. Se prueban todos
 * los nombres, no solo `BootX64.efi`: en un medio que no sea de x86_64 ese
 * nombre no existe y la actualización se iba por el camino de descomprimir
 * `efi.img.gz` sin que hiciera falta. */
static int staging_dir_has_loader(const char *dir) {
    for (size_t i = 0; i < ESP_LOADER_COUNT; i++) {
        char path[512];
        snprintf(path, sizeof(path), "%s/EFI/Boot/%s", dir, ESP_LOADER_NAMES[i]);
        if (struct_file_exists(path)) {
            return 1;
        }
    }
    return 0;
}

/* Bytes libres de la montura en la que vive `path`, o 0 si no se sabe.
 *
 * Se usa para no empezar una copia que no cabe: un `BootX64.efi` a medio
 * escribir es una máquina que ya no arranca, y ese es el peor final posible
 * para un programa que solo tenía que poner un kernel nuevo. */
static uint64_t fs_available_bytes(const char *path) {
    struct statvfs vfs;
    uint64_t unit;

    if (path == NULL || statvfs(path, &vfs) != 0) {
        return 0;
    }
    unit = (vfs.f_frsize != 0) ? (uint64_t)vfs.f_frsize : (uint64_t)vfs.f_bsize;
    return (uint64_t)vfs.f_bavail * unit;
}

static uint64_t file_size_or_zero(const char *path) {
    struct stat st;
    if (path == NULL || stat(path, &st) != 0 || !S_ISREG(st.st_mode)) {
        return 0;
    }
    return (uint64_t)st.st_size;
}

/* `mkdir -p` del directorio que contiene `path`. */
static int mkdir_parent_of(const char *path, int dry_run) {
    char dir[640];
    char *slash;

    snprintf(dir, sizeof(dir), "%s", path);
    slash = strrchr(dir, '/');
    if (slash == NULL || slash == dir) {
        return 0;
    }
    *slash = '\0';
    return shell_mkdir_p(dir, dry_run);
}

/* Copia `src` encima de `dst` sin dejar un `dst` a medias: se escribe al lado
 * y se renombra, que es atómico. Si el sistema de ficheros no trae `rename`
 * (los de Eclipse los sirve un proceso, y uno puede no implementarlo), se
 * copia encima y se dice. */
static int copy_file_replace(const char *src, const char *dst, int dry_run) {
    char tmp[700];

    snprintf(tmp, sizeof(tmp), "%s.new", dst);
    if (shell_copy_file(src, tmp, dry_run) != 0) {
        if (!dry_run) {
            unlink(tmp);
        }
        return -1;
    }
    if (dry_run) {
        return 0;
    }
    if (rename(tmp, dst) != 0) {
        log(TC(COLOR_YELLOW, S_WARN_NO_RENAME), dst, strerror(errno));
        unlink(tmp);
        return shell_copy_file(src, dst, 0);
    }
    return 0;
}

/* Renueva un fichero de la raíz instalada. Un origen que no está en el medio
 * no es un error: se dice y se sigue. */
static int refresh_one_file(const char *src, const char *dst, int only_if_missing,
                            int dry_run, int *changed) {
    struct stat st;

    if (lstat(src, &st) != 0) {
        return 0;
    }
    if (S_ISLNK(st.st_mode)) {
        log(TC(COLOR_YELLOW, S_SKIPPED_SYMLINK), src);
        return 0;
    }
    if (!S_ISREG(st.st_mode)) {
        return 0;
    }
    if (only_if_missing && struct_file_exists(dst)) {
        return 0;
    }
    if (mkdir_parent_of(dst, dry_run) != 0) {
        return -1;
    }
    if (copy_file_replace(src, dst, dry_run) != 0) {
        log(TC(COLOR_RED COLOR_BOLD, S_ERR_COPY), src, dst);
        return -1;
    }
    if (!dry_run && chmod(dst, (mode_t)(st.st_mode & 07777)) != 0) {
        log(TC(COLOR_YELLOW, S_WARN_CHMOD), dst, strerror(errno));
    }
    log(TC(COLOR_GREEN, S_UPDATED_FILE), dst);
    (*changed)++;
    return 0;
}

#define REFRESH_MAX_DEPTH 8

/* Renueva los ficheros de un directorio del medio en la raíz instalada. Nada
 * se borra nunca: lo que el usuario tenga ahí y no venga en el medio se queda.
 *
 * `suffix` limita a los ficheros que acaban en él (las filas `.service`);
 * `recurse` baja a los subdirectorios (el árbol de `/usr/local/bin`). */
static int refresh_dir(const char *src_dir, const char *dst_dir, const char *suffix,
                       int only_if_missing, int recurse, int depth,
                       int dry_run, int *changed) {
    DIR *d;
    struct dirent *e;
    int rc = 0;

    d = opendir(src_dir);
    if (d == NULL) {
        return 0; /* el medio no lo trae: no hay nada que renovar */
    }
    if (shell_mkdir_p(dst_dir, dry_run) != 0) {
        closedir(d);
        return -1;
    }

    while ((e = readdir(d)) != NULL) {
        char src[640], dst[640];
        struct stat st;

        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) {
            continue;
        }
        snprintf(src, sizeof(src), "%s/%s", src_dir, e->d_name);
        snprintf(dst, sizeof(dst), "%s/%s", dst_dir, e->d_name);
        if (lstat(src, &st) != 0) {
            continue;
        }
        if (S_ISDIR(st.st_mode)) {
            if (!recurse || depth + 1 >= REFRESH_MAX_DEPTH) {
                continue;
            }
            if (refresh_dir(src, dst, suffix, only_if_missing, recurse, depth + 1,
                            dry_run, changed) != 0) {
                rc = -1;
            }
            continue;
        }
        if (suffix != NULL) {
            size_t name_len = strlen(e->d_name);
            size_t suf_len = strlen(suffix);
            if (name_len <= suf_len ||
                strcmp(e->d_name + (name_len - suf_len), suffix) != 0) {
                continue;
            }
        }
        if (refresh_one_file(src, dst, only_if_missing, dry_run, changed) != 0) {
            rc = -1;
        }
    }
    closedir(d);
    return rc;
}

/* El valor de una clave de un os-release, sin comillas. */
static int os_release_field(const char *path, const char *key, char *out, size_t out_size) {
    FILE *fp;
    char line[512];
    size_t key_len = strlen(key);
    int found = -1;

    if (out == NULL || out_size == 0) {
        return -1;
    }
    out[0] = '\0';
    fp = fopen(path, "r");
    if (fp == NULL) {
        return -1;
    }
    while (fgets(line, sizeof(line), fp) != NULL) {
        char *value;
        size_t len;

        if (strncmp(line, key, key_len) != 0 || line[key_len] != '=') {
            continue;
        }
        value = line + key_len + 1;
        trim_newline(value);
        len = strlen(value);
        if (len >= 2 && value[0] == '"' && value[len - 1] == '"') {
            value[len - 1] = '\0';
            value++;
        }
        snprintf(out, out_size, "%s", value);
        found = 0;
        break;
    }
    fclose(fp);
    return found;
}

/* «Qué hay puesto y qué voy a poner». Sin esto la actualización no dice en
 * ningún momento de qué versión viene ni a cuál va, y un medio viejo pisa un
 * kernel nuevo sin que nada lo mencione. */
static void describe_version(const char *os_release_path, int installed) {
    char version[64];
    char build[64];
    char texto[160];

    if (os_release_field(os_release_path, "VERSION_ID", version, sizeof(version)) != 0 ||
        version[0] == '\0') {
        if (installed) {
            log(TC(COLOR_YELLOW, S_VERSION_UNKNOWN));
        }
        return;
    }
    if (os_release_field(os_release_path, "BUILD_ID", build, sizeof(build)) == 0 &&
        build[0] != '\0') {
        snprintf(texto, sizeof(texto), "%s (%s)", version, build);
    } else {
        snprintf(texto, sizeof(texto), "%s", version);
    }
    log(T(installed ? S_VERSION_INSTALLED : S_VERSION_MEDIUM), texto);
}

/* Igual de versión no significa igual de ficheros (el medio puede ser una
 * reconstrucción de la misma versión), así que esto informa y no decide. */
static void report_versions(const char *root_mnt) {
    char installed_path[640];

    snprintf(installed_path, sizeof(installed_path), "%s/etc/os-release", root_mnt);
    describe_version(installed_path, 1);
    describe_version("/etc/os-release", 0);
}

/* Renueva en la raíz instalada los ficheros que el xtask escribe y el
 * instalador antes solo ponía en una instalación de cero.
 *
 * `src_root` es la raíz de la que se copia: en el instalador es `""`, o sea
 * la raíz del propio medio live. Es un parámetro y no una constante para que
 * el arnés (`tests/upgrade-harness.c`) pueda ejercer la tabla entera, con su
 * política de qué se pisa y qué no, contra dos directorios de verdad. */
static int refresh_installed_root(const char *src_root, const char *root_mnt, int dry_run) {
    int changed = 0;
    int rc = 0;

    log(TC(COLOR_GREEN, S_REFRESH_ROOT));
    log(T(S_REFRESH_ROOT_KEEPS));

    for (size_t i = 0; i < ECLIPSE_OWNED_COUNT; i++) {
        const struct refresh_entry *entry = &ECLIPSE_OWNED[i];
        char src[640], dst[640];
        int step;

        snprintf(src, sizeof(src), "%s/%s", src_root, entry->rel);
        snprintf(dst, sizeof(dst), "%s/%s", root_mnt, entry->rel);

        switch (entry->kind) {
            case REFRESH_FILE:
                step = refresh_one_file(src, dst, 0, dry_run, &changed);
                break;
            case REFRESH_TREE:
                step = refresh_dir(src, dst, NULL, 0, 1, 0, dry_run, &changed);
                break;
            case REFRESH_SUFFIX:
                step = refresh_dir(src, dst, entry->suffix, 0, 0, 0, dry_run, &changed);
                break;
            case REFRESH_IF_MISSING:
            default:
                step = refresh_dir(src, dst, NULL, 1, 0, 0, dry_run, &changed);
                break;
        }
        if (step != 0) {
            rc = -1;
        }
    }

    log(T(S_REFRESH_ROOT_DONE), changed);
    return rc;
}

/* El marcador `needle` dentro de los `len` primeros bytes de `hay`, sin salir
 * de la línea. (No se usa `strstr` porque la línea no acaba en cero.) */
static const char *memmem_placeholder(const char *hay, size_t len, const char *needle) {
    size_t n = strlen(needle);

    if (n == 0 || len < n) {
        return NULL;
    }
    for (size_t i = 0; i + n <= len; i++) {
        if (memcmp(hay + i, needle, n) == 0) {
            return hay + i;
        }
    }
    return NULL;
}

/* Arregla los marcadores que una instalación anterior pudo dejar sin
 * sustituir en el `/etc/fstab` de la raíz. Con `__ECLIPSE_ROOT_DEV__` en la
 * primera columna no monta nada, y hasta ahora ninguna actualización lo
 * miraba: la única salida era editarlo a mano desde otro sistema.
 *
 * Solo se tocan los veinte caracteres del marcador, que es exactamente lo que
 * mide un campo de dispositivo: las columnas no se mueven y el resto del
 * fichero (lo que el usuario haya añadido) se queda como está. Las líneas de
 * HOME y SWAP se comentan, porque su partición no se conoce desde aquí y una
 * línea con marcador solo puede fallar. */
static int repair_fstab_placeholders(const char *root_mnt, const char *root_dev,
                                     const char *efi_dev, int dry_run) {
    char path[640];
    char *text;
    long size;
    size_t len;
    FILE *fp;
    int changed = 0;
    int rc = 0;

    snprintf(path, sizeof(path), "%s/etc/fstab", root_mnt);
    fp = fopen(path, "rb");
    if (fp == NULL) {
        return 0;
    }
    if (fseek(fp, 0, SEEK_END) != 0 || (size = ftell(fp)) < 0 || size > 256 * 1024) {
        fclose(fp);
        return 0;
    }
    rewind(fp);
    /* Con holgura: comentar una línea mete un byte por delante. */
    text = calloc(1, (size_t)size + 8);
    if (text == NULL) {
        fclose(fp);
        return 0;
    }
    if (fread(text, 1, (size_t)size, fp) != (size_t)size) {
        free(text);
        fclose(fp);
        return 0;
    }
    fclose(fp);
    len = (size_t)size;

    for (int i = 0; i < 2; i++) {
        const char *placeholder = (i == 0) ? FSTAB_PLACEHOLDER_ROOT : FSTAB_PLACEHOLDER_EFI;
        const char *device = (i == 0) ? root_dev : efi_dev;
        char field[FSTAB_PLACEHOLDER_LEN + 1];
        char *at;

        format_fstab_device_field(device, field);
        while ((at = strstr(text, placeholder)) != NULL) {
            memcpy(at, field, FSTAB_PLACEHOLDER_LEN);
            changed++;
        }
    }
    /* HOME y SWAP se comentan línea a línea, y una línea ya comentada se deja
     * en paz: así una segunda pasada no añade una segunda almohadilla. */
    for (char *line = text; line != NULL && *line != '\0';) {
        char *next = strchr(line, '\n');
        size_t line_len = (next != NULL) ? (size_t)(next - line) : strlen(line);
        const char *marcador = NULL;

        if (memmem_placeholder(line, line_len, FSTAB_PLACEHOLDER_HOME) != NULL) {
            marcador = FSTAB_PLACEHOLDER_HOME;
        } else if (memmem_placeholder(line, line_len, FSTAB_PLACEHOLDER_SWAP) != NULL) {
            marcador = FSTAB_PLACEHOLDER_SWAP;
        }

        if (marcador != NULL && line[0] != '#') {
            memmove(line + 1, line, len - (size_t)(line - text) + 1);
            *line = '#';
            len++;
            line_len++;
            log(TC(COLOR_YELLOW, S_FSTAB_COMMENTED), marcador);
            changed++;
            next = strchr(line, '\n');
        }
        line = (next != NULL) ? next + 1 : NULL;
    }

    if (changed == 0) {
        free(text);
        return 0;
    }

    log(TC(COLOR_YELLOW, S_FSTAB_PLACEHOLDERS_FOUND), path);
    if (dry_run) {
        log(TC(COLOR_YELLOW, S_DRY_FSTAB_WRITE), path);
        free(text);
        return 0;
    }

    fp = fopen(path, "wb");
    if (fp == NULL) {
        log(TC(COLOR_YELLOW, S_WARN_FSTAB_REPAIR), path, strerror(errno));
        free(text);
        return 0;
    }
    if (fwrite(text, 1, len, fp) != len || fclose(fp) != 0) {
        log(TC(COLOR_YELLOW, S_WARN_FSTAB_REPAIR), path, strerror(errno));
        rc = 0;
    } else {
        log(TC(COLOR_GREEN, S_FSTAB_REPAIRED), root_dev, efi_dev);
    }
    free(text);
    return rc;
}

/* El montaje de la raíz instalada para la actualización: si ya está montada
 * (porque el instalador corre desde ella, o porque alguien la montó a mano),
 * se usa ese montaje. Dos monturas del mismo dispositivo son dos cachés
 * independientes sobre los mismos bloques, que es la misma trampa que la ESP
 * de `/boot/efi` ya evitaba. */
static int mount_installed_root(const char *root_dev, char *mnt_out, size_t mnt_size,
                                int *we_mounted, int dry_run) {
    FILE *fp;
    char line[1024];

    *we_mounted = 0;
    fp = fopen("/proc/mounts", "r");
    if (fp != NULL) {
        while (fgets(line, sizeof(line), fp) != NULL) {
            char src[256], tgt[256];
            if (sscanf(line, "%255s %255s", src, tgt) == 2 && strcmp(src, root_dev) == 0) {
                fclose(fp);
                snprintf(mnt_out, mnt_size, "%s", tgt);
                log(TC(COLOR_GREEN, S_ROOT_ALREADY_MOUNTED), root_dev, tgt);
                return 0;
            }
        }
        fclose(fp);
    }

    if (shell_mkdir_p(UPD_ROOT_MNT, dry_run) != 0) {
        return -1;
    }
    /* El ROOT ya instalado puede ser EclipseFS o btrfs: se prueban ambos. */
    if (shell_mount(root_dev, UPD_ROOT_MNT, "eclipsefs", 0, dry_run) != 0 &&
        shell_mount(root_dev, UPD_ROOT_MNT, "btrfs", 0, dry_run) != 0) {
        return -1;
    }
    snprintf(mnt_out, mnt_size, "%s", UPD_ROOT_MNT);
    *we_mounted = 1;
    return 0;
}

#endif /* ECLIPSE_INSTALLER_UPGRADE_H */
