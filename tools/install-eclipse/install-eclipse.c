#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdint.h>
#include <inttypes.h>
#include <stdarg.h>
#include <time.h>
#include <sys/stat.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <linux/fs.h>
#include <errno.h>

#define SECTOR_SIZE 512ULL
#define PART1_START 2048ULL
#define PART1_SECTORS 204800ULL
#define MAX_DISKS 64
#define LINE_BUF 4096
#define SHA256_HEX_LEN 64
#define GIB (1024ULL * 1024ULL * 1024ULL)
#define MIB (1024ULL * 1024ULL)

// ANSI escape codes for styling
#define COLOR_RESET   "\x1b[0m"
#define COLOR_BOLD    "\x1b[1m"
#define COLOR_RED     "\x1b[31m"
#define COLOR_GREEN   "\x1b[32m"
#define COLOR_YELLOW  "\x1b[33m"
#define COLOR_BLUE    "\x1b[34m"
#define COLOR_CYAN    "\x1b[36m"
#define COLOR_WHITE   "\x1b[37m"

#define BG_BLUE       "\x1b[44m"

#define log installer_log

enum install_mode {
    MODE_UNSET = 0,
    MODE_NEW,
    MODE_UPGRADE
};

enum layout_mode {
    LAYOUT_UNSET = 0,
    LAYOUT_SIMPLE,
    LAYOUT_ADVANCED
};

enum table_mode {
    TABLE_UNSET = 0,
    TABLE_MBR,
    TABLE_GPT
};

struct partition_entry {
    uint8_t boot_indicator;
    uint8_t starting_chs[3];
    uint8_t type;
    uint8_t ending_chs[3];
    uint32_t starting_lba;
    uint32_t sectors_count;
} __attribute__((packed));

struct disk_info {
    char path[128];
    uint64_t size_bytes;
    int removable;
};

struct existing_install_info {
    int has_valid_mbr;
    int has_efi_partition;
    int has_gpt_header;
    int suggest_upgrade;
};

struct config {
    char disk_path[128];
    int disk_set;
    enum install_mode mode;
    int mode_set;
    int auto_yes;
    int dry_run;
    enum layout_mode layout;
    int layout_set;
    enum table_mode table;
    int table_set;
};

struct partition_plan {
    char name[16];
    uint64_t start_sector;
    uint64_t sectors;
    uint8_t mbr_type;
    const char *gpt_type;
    const char *image_path;
    int write_image;
    int verify_after_write;
};

static FILE *g_log_file = NULL;

int struct_file_exists(const char *path);
static void installer_log(const char *fmt, ...);
static void clear_screen(void);
static void print_header(void);
static uint64_t get_disk_size_bytes(const char *path);
static int scan_disks(struct disk_info disks[MAX_DISKS], char disk_list[MAX_DISKS][128]);
static int add_disk_if_present(struct disk_info disks[MAX_DISKS], char disk_list[MAX_DISKS][128], int found_disks, const char *path);
static void trim_newline(char *s);
static uint64_t align_up_u64(uint64_t value, uint64_t alignment);
static uint64_t align_down_u64(uint64_t value, uint64_t alignment);
static void format_size(char *buf, size_t buf_size, uint64_t size_bytes);
static const char *mode_label(enum install_mode mode);
static const char *layout_label(enum layout_mode layout);
static const char *table_label(enum table_mode table);
static const char *disk_basename(const char *disk_path);
static int is_removable_disk(const char *disk_path);
static int detect_existing_install(const char *disk_path, struct existing_install_info *info);
static int parse_args(int argc, char **argv, struct config *cfg);
static int prompt_disk_selection(struct config *cfg, struct disk_info disks[MAX_DISKS], char disk_list[MAX_DISKS][128], int found_disks);
static int prompt_install_mode(struct config *cfg, const struct existing_install_info *info);
static int prompt_layout(struct config *cfg);
static int prompt_table(struct config *cfg, int running_on_efi);
static int ask_yes_no(const char *prompt, int default_yes, int auto_yes);
static int command_exists(const char *name);
static int run_command_logged(const char *cmd, int dry_run);
static int run_command_capture_token(const char *cmd, char *out, size_t out_size);
static int verify_sha256_file(const char *image_path);
static int build_partition_plan(enum layout_mode layout, uint64_t disk_sectors,
                                struct partition_plan *parts, int *part_count,
                                char *summary, size_t summary_size);
static int prepare_gpt_or_fallback(enum table_mode *table);
static int write_mbr_partition_table(const char *disk_path, const struct partition_plan *parts, int part_count, int dry_run);
static int write_gpt_partition_table(const char *disk_path, const struct partition_plan *parts, int part_count, int dry_run);
static int write_partition_image_with_retry(const char *disk_path, const struct partition_plan *part, int dry_run);
static int verify_partition_write(const char *disk_path, const struct partition_plan *part, int dry_run);
static void close_log_file(void);

static void close_log_file(void) {
    if (g_log_file != NULL) {
        fclose(g_log_file);
        g_log_file = NULL;
    }
}

static void installer_log(const char *fmt, ...) {
    char message[LINE_BUF];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(message, sizeof(message), fmt, ap);
    va_end(ap);

    fprintf(stdout, "%s\n", message);
    fflush(stdout);

    if (g_log_file != NULL) {
        time_t now = time(NULL);
        struct tm tm_now;
        localtime_r(&now, &tm_now);
        char ts[32];
        strftime(ts, sizeof(ts), "%Y-%m-%d %H:%M:%S", &tm_now);
        fprintf(g_log_file, "[%s] %s\n", ts, message);
        fflush(g_log_file);
    }
}

static void clear_screen(void) {
    fprintf(stdout, "\x1b[2J\x1b[H");
    fflush(stdout);
}

static void print_header(void) {
    clear_screen();
    log(COLOR_CYAN COLOR_BOLD "========================================================" COLOR_RESET);
    log(COLOR_BLUE COLOR_BOLD " *               ECLIPSE OS 2 INSTALLER               *" COLOR_RESET);
    log(COLOR_CYAN COLOR_BOLD "========================================================\n" COLOR_RESET);
}

static uint64_t get_disk_size_bytes(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return 0;
    }

    uint64_t size = 0;
    if (ioctl(fd, BLKGETSIZE64, &size) != 0) {
        off_t end = lseek(fd, 0, SEEK_END);
        if (end > 0) {
            size = (uint64_t)end;
        }
    }

    close(fd);
    return size;
}

static void trim_newline(char *s) {
    if (s == NULL) {
        return;
    }
    s[strcspn(s, "\r\n")] = 0;
}

static uint64_t align_up_u64(uint64_t value, uint64_t alignment) {
    if (alignment == 0) {
        return value;
    }
    uint64_t rem = value % alignment;
    if (rem == 0) {
        return value;
    }
    return value + (alignment - rem);
}

static uint64_t align_down_u64(uint64_t value, uint64_t alignment) {
    if (alignment == 0) {
        return value;
    }
    return value - (value % alignment);
}

static void format_size(char *buf, size_t buf_size, uint64_t size_bytes) {
    double gib = (double)size_bytes / (1024.0 * 1024.0 * 1024.0);
    double mib = (double)size_bytes / (1024.0 * 1024.0);
    if (gib >= 1.0) {
        snprintf(buf, buf_size, "%.2f GB", gib);
    } else {
        snprintf(buf, buf_size, "%.2f MB", mib);
    }
}

static const char *mode_label(enum install_mode mode) {
    switch (mode) {
        case MODE_NEW:
            return "Nueva instalación";
        case MODE_UPGRADE:
            return "Actualización";
        default:
            return "Sin definir";
    }
}

static const char *layout_label(enum layout_mode layout) {
    switch (layout) {
        case LAYOUT_SIMPLE:
            return "Simple (EFI 100MB + ROOT resto)";
        case LAYOUT_ADVANCED:
            return "Avanzado (EFI 100MB + ROOT + HOME + SWAP)";
        default:
            return "Sin definir";
    }
}

static const char *table_label(enum table_mode table) {
    switch (table) {
        case TABLE_MBR:
            return "MBR/BIOS";
        case TABLE_GPT:
            return "GPT/UEFI";
        default:
            return "Sin cambios";
    }
}

static const char *disk_basename(const char *disk_path) {
    const char *slash = strrchr(disk_path, '/');
    return (slash != NULL) ? slash + 1 : disk_path;
}

static int is_removable_disk(const char *disk_path) {
    char sysfs_path[256];
    snprintf(sysfs_path, sizeof(sysfs_path), "/sys/block/%s/removable", disk_basename(disk_path));
    FILE *fp = fopen(sysfs_path, "r");
    if (fp == NULL) {
        return 0;
    }

    char buf[32];
    if (fgets(buf, sizeof(buf), fp) == NULL) {
        fclose(fp);
        return 0;
    }
    fclose(fp);
    trim_newline(buf);
    return strcmp(buf, "1") == 0;
}

static int detect_existing_install(const char *disk_path, struct existing_install_info *info) {
    memset(info, 0, sizeof(*info));

    int fd = open(disk_path, O_RDONLY);
    if (fd < 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo abrir %s para detectar instalaciones previas (%s)." COLOR_RESET,
            disk_path, strerror(errno));
        return -1;
    }

    unsigned char sector0[SECTOR_SIZE];
    ssize_t nread = pread(fd, sector0, sizeof(sector0), 0);
    if (nread != (ssize_t)sizeof(sector0)) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo leer el sector 0 de %s." COLOR_RESET, disk_path);
        close(fd);
        return -1;
    }

    info->has_valid_mbr = (sector0[510] == 0x55 && sector0[511] == 0xAA);
    info->has_efi_partition = (sector0[446 + 4] == 0xEF);

    unsigned char sector1[SECTOR_SIZE];
    nread = pread(fd, sector1, sizeof(sector1), (off_t)SECTOR_SIZE);
    if (nread == (ssize_t)sizeof(sector1) && memcmp(sector1, "EFI PART", 8) == 0) {
        info->has_gpt_header = 1;
    }

    info->suggest_upgrade = (info->has_valid_mbr && info->has_efi_partition) || info->has_gpt_header;
    close(fd);
    return 0;
}

static int add_disk_if_present(struct disk_info disks[MAX_DISKS], char disk_list[MAX_DISKS][128], int found_disks, const char *path) {
    if (found_disks >= MAX_DISKS) {
        return found_disks;
    }

    struct stat st;
    if (stat(path, &st) != 0 || !S_ISBLK(st.st_mode)) {
        return found_disks;
    }

    uint64_t size_bytes = get_disk_size_bytes(path);
    char size_buf[64];
    format_size(size_buf, sizeof(size_buf), size_bytes);
    log("  [%d] " COLOR_YELLOW "%s" COLOR_RESET " (%s)", found_disks + 1, path, size_buf);

    strncpy(disks[found_disks].path, path, sizeof(disks[found_disks].path) - 1);
    disks[found_disks].path[sizeof(disks[found_disks].path) - 1] = 0;
    disks[found_disks].size_bytes = size_bytes;
    disks[found_disks].removable = is_removable_disk(path);

    strncpy(disk_list[found_disks], path, sizeof(disk_list[found_disks]) - 1);
    disk_list[found_disks][sizeof(disk_list[found_disks]) - 1] = 0;
    return found_disks + 1;
}

static int scan_disks(struct disk_info disks[MAX_DISKS], char disk_list[MAX_DISKS][128]) {
    int found_disks = 0;

    for (char c = 'a'; c <= 'z' && found_disks < MAX_DISKS; c++) {
        char path[128];
        snprintf(path, sizeof(path), "/dev/sd%c", c);
        found_disks = add_disk_if_present(disks, disk_list, found_disks, path);
    }

    for (int ctrl = 0; ctrl <= 9 && found_disks < MAX_DISKS; ctrl++) {
        for (int ns = 1; ns <= 9 && found_disks < MAX_DISKS; ns++) {
            char path[128];
            snprintf(path, sizeof(path), "/dev/nvme%dn%d", ctrl, ns);
            found_disks = add_disk_if_present(disks, disk_list, found_disks, path);
        }
    }

    for (char c = 'a'; c <= 'z' && found_disks < MAX_DISKS; c++) {
        char path[128];
        snprintf(path, sizeof(path), "/dev/vd%c", c);
        found_disks = add_disk_if_present(disks, disk_list, found_disks, path);
    }

    return found_disks;
}

static int parse_args(int argc, char **argv, struct config *cfg) {
    memset(cfg, 0, sizeof(*cfg));
    cfg->mode = MODE_UNSET;
    cfg->layout = LAYOUT_UNSET;
    cfg->table = TABLE_UNSET;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--disk") == 0) {
            if (i + 1 >= argc) {
                log(COLOR_RED COLOR_BOLD "ERROR: --disk requiere una ruta." COLOR_RESET);
                return -1;
            }
            strncpy(cfg->disk_path, argv[++i], sizeof(cfg->disk_path) - 1);
            cfg->disk_path[sizeof(cfg->disk_path) - 1] = 0;
            cfg->disk_set = 1;
        } else if (strcmp(argv[i], "--mode") == 0) {
            if (i + 1 >= argc) {
                log(COLOR_RED COLOR_BOLD "ERROR: --mode requiere new o upgrade." COLOR_RESET);
                return -1;
            }
            const char *value = argv[++i];
            if (strcmp(value, "new") == 0) {
                cfg->mode = MODE_NEW;
            } else if (strcmp(value, "upgrade") == 0) {
                cfg->mode = MODE_UPGRADE;
            } else {
                log(COLOR_RED COLOR_BOLD "ERROR: modo inválido: %s" COLOR_RESET, value);
                return -1;
            }
            cfg->mode_set = 1;
        } else if (strcmp(argv[i], "--yes") == 0) {
            cfg->auto_yes = 1;
        } else if (strcmp(argv[i], "--dry-run") == 0) {
            cfg->dry_run = 1;
        } else if (strcmp(argv[i], "--simple") == 0) {
            if (cfg->layout_set && cfg->layout != LAYOUT_SIMPLE) {
                log(COLOR_RED COLOR_BOLD "ERROR: --simple y --advanced son mutuamente excluyentes." COLOR_RESET);
                return -1;
            }
            cfg->layout = LAYOUT_SIMPLE;
            cfg->layout_set = 1;
        } else if (strcmp(argv[i], "--advanced") == 0) {
            if (cfg->layout_set && cfg->layout != LAYOUT_ADVANCED) {
                log(COLOR_RED COLOR_BOLD "ERROR: --simple y --advanced son mutuamente excluyentes." COLOR_RESET);
                return -1;
            }
            cfg->layout = LAYOUT_ADVANCED;
            cfg->layout_set = 1;
        } else if (strcmp(argv[i], "--mbr") == 0) {
            if (cfg->table_set && cfg->table != TABLE_MBR) {
                log(COLOR_RED COLOR_BOLD "ERROR: --mbr y --gpt son mutuamente excluyentes." COLOR_RESET);
                return -1;
            }
            cfg->table = TABLE_MBR;
            cfg->table_set = 1;
        } else if (strcmp(argv[i], "--gpt") == 0) {
            if (cfg->table_set && cfg->table != TABLE_GPT) {
                log(COLOR_RED COLOR_BOLD "ERROR: --mbr y --gpt son mutuamente excluyentes." COLOR_RESET);
                return -1;
            }
            cfg->table = TABLE_GPT;
            cfg->table_set = 1;
        } else {
            log(COLOR_RED COLOR_BOLD "ERROR: argumento no reconocido: %s" COLOR_RESET, argv[i]);
            return -1;
        }
    }

    return 0;
}

static int prompt_disk_selection(struct config *cfg, struct disk_info disks[MAX_DISKS], char disk_list[MAX_DISKS][128], int found_disks) {
    if (cfg->disk_set) {
        log("Disco preseleccionado por CLI: " COLOR_YELLOW "%s" COLOR_RESET, cfg->disk_path);
        return 0;
    }

    if (found_disks <= 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se detectaron discos de almacenamiento." COLOR_RESET);
        return -1;
    }

    strncpy(cfg->disk_path, disk_list[0], sizeof(cfg->disk_path) - 1);
    cfg->disk_path[sizeof(cfg->disk_path) - 1] = 0;

    if (cfg->auto_yes) {
        log("--yes activo: usando el primer disco detectado (%s).", cfg->disk_path);
        return 0;
    }

    char input[64];
    log("Seleccione el disco de destino [por defecto: %s]:", cfg->disk_path);
    if (fgets(input, sizeof(input), stdin) == NULL) {
        log("Entrada vacía: usando %s.", cfg->disk_path);
        return 0;
    }
    trim_newline(input);

    if (input[0] == 0) {
        return 0;
    }

    if (input[0] >= 1 && input[0] <= 9) {
        int idx = input[0] - 1;
        if (idx >= 0 && idx < found_disks) {
            strncpy(cfg->disk_path, disk_list[idx], sizeof(cfg->disk_path) - 1);
            cfg->disk_path[sizeof(cfg->disk_path) - 1] = 0;
            return 0;
        }
    }

    strncpy(cfg->disk_path, input, sizeof(cfg->disk_path) - 1);
    cfg->disk_path[sizeof(cfg->disk_path) - 1] = 0;
    (void)disks;
    return 0;
}

static int prompt_install_mode(struct config *cfg, const struct existing_install_info *info) {
    if (cfg->mode_set) {
        log("Modo preseleccionado por CLI: %s", mode_label(cfg->mode));
        return 0;
    }

    enum install_mode suggested = info->suggest_upgrade ? MODE_UPGRADE : MODE_NEW;
    if (info->suggest_upgrade) {
        log(COLOR_YELLOW COLOR_BOLD "Se detectó una instalación existente compatible; se recomienda ACTUALIZACIÓN." COLOR_RESET);
    } else {
        log("No se detectó instalación previa conocida; se recomienda NUEVA instalación.");
    }

    if (cfg->auto_yes) {
        cfg->mode = suggested;
        log("--yes activo: usando modo sugerido (%s).", mode_label(cfg->mode));
        return 0;
    }

    char input[32];
    log("Seleccione el modo de instalación:");
    log("  [1] Nueva instalación (reparticiona y reescribe imágenes)");
    log("  [2] Actualización (solo reescribe EFI y ROOT, sin reparticionar)");
    log("Opción recomendada [%d]:", suggested == MODE_NEW ? 1 : 2);
    if (fgets(input, sizeof(input), stdin) == NULL) {
        cfg->mode = suggested;
        return 0;
    }
    trim_newline(input);

    if (input[0] == '2') {
        cfg->mode = MODE_UPGRADE;
    } else if (input[0] == '1' || input[0] == '\0') {
        cfg->mode = suggested;
        if (input[0] == '1') {
            cfg->mode = MODE_NEW;
        }
    } else {
        cfg->mode = suggested;
    }

    return 0;
}

static int prompt_layout(struct config *cfg) {
    if (cfg->layout_set) {
        log("Esquema de particiones preseleccionado por CLI: %s", layout_label(cfg->layout));
        return 0;
    }

    if (cfg->auto_yes) {
        cfg->layout = LAYOUT_SIMPLE;
        log("--yes activo: usando esquema simple por defecto.");
        return 0;
    }

    char input[32];
    log("Seleccione el esquema de particiones:");
    log("  [1] Simple    -> EFI 100MB + ROOT resto");
    log("  [2] Avanzado  -> EFI 100MB + ROOT + HOME + SWAP");
    log("Opción recomendada [1]:");
    if (fgets(input, sizeof(input), stdin) == NULL) {
        cfg->layout = LAYOUT_SIMPLE;
        return 0;
    }
    trim_newline(input);
    cfg->layout = (input[0] == '2') ? LAYOUT_ADVANCED : LAYOUT_SIMPLE;
    return 0;
}

static int prompt_table(struct config *cfg, int running_on_efi) {
    if (cfg->table_set) {
        log("Tabla de particiones preseleccionada por CLI: %s", table_label(cfg->table));
        return 0;
    }

    enum table_mode suggested = running_on_efi ? TABLE_GPT : TABLE_MBR;
    log("Entorno de arranque detectado: %s", running_on_efi ? "EFI/UEFI" : "BIOS/Legacy");
    log("Se recomienda %s.", table_label(suggested));

    if (cfg->auto_yes) {
        cfg->table = suggested;
        log("--yes activo: usando tabla sugerida (%s).", table_label(cfg->table));
        return 0;
    }

    char input[32];
    log("Seleccione la tabla de particiones:");
    log("  [1] MBR/BIOS");
    log("  [2] GPT/UEFI");
    log("Opción recomendada [%d]:", suggested == TABLE_MBR ? 1 : 2);
    if (fgets(input, sizeof(input), stdin) == NULL) {
        cfg->table = suggested;
        return 0;
    }
    trim_newline(input);
    if (input[0] == '1') {
        cfg->table = TABLE_MBR;
    } else if (input[0] == '2') {
        cfg->table = TABLE_GPT;
    } else {
        cfg->table = suggested;
    }
    return 0;
}

static int ask_yes_no(const char *prompt, int default_yes, int auto_yes) {
    if (auto_yes) {
        log("%s %s (--yes)", prompt, default_yes ? "Sí" : "Sí");
        return 1;
    }

    char input[32];
    log("%s", prompt);
    if (fgets(input, sizeof(input), stdin) == NULL) {
        return default_yes;
    }
    trim_newline(input);
    if (input[0] == 0) {
        return default_yes;
    }
    return input[0] == 'y' || input[0] == 'Y' || input[0] == 's' || input[0] == 'S';
}

static int command_exists(const char *name) {
    const char *path = getenv("PATH");
    if (path == NULL || *path == 0) {
        return 0;
    }

    char path_copy[4096];
    strncpy(path_copy, path, sizeof(path_copy) - 1);
    path_copy[sizeof(path_copy) - 1] = 0;

    char *saveptr = NULL;
    for (char *dir = strtok_r(path_copy, ":", &saveptr);
         dir != NULL;
         dir = strtok_r(NULL, ":", &saveptr)) {
        char candidate[4096];
        snprintf(candidate, sizeof(candidate), "%s/%s", dir, name);
        if (access(candidate, X_OK) == 0) {
            return 1;
        }
    }

    return 0;
}

static int run_command_logged(const char *cmd, int dry_run) {
    if (dry_run) {
        log(COLOR_YELLOW "[dry-run] %s" COLOR_RESET, cmd);
        return 0;
    }

    char full_cmd[4096];
    snprintf(full_cmd, sizeof(full_cmd), "%s 2>&1", cmd);
    FILE *pipe = popen(full_cmd, "r");
    if (pipe == NULL) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo ejecutar comando: %s" COLOR_RESET, cmd);
        return -1;
    }

    char line[LINE_BUF];
    while (fgets(line, sizeof(line), pipe) != NULL) {
        trim_newline(line);
        if (line[0] != 0) {
            log("%s", line);
        }
    }

    int status = pclose(pipe);
    if (status == -1) {
        log(COLOR_RED COLOR_BOLD "ERROR: pclose() falló para: %s" COLOR_RESET, cmd);
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: comando falló (%d): %s" COLOR_RESET,
            WIFEXITED(status) ? WEXITSTATUS(status) : status, cmd);
        return -1;
    }

    return 0;
}

static int run_command_capture_token(const char *cmd, char *out, size_t out_size) {
    FILE *pipe = popen(cmd, "r");
    if (pipe == NULL) {
        return -1;
    }

    char token[SHA256_HEX_LEN + 2];
    int scanned = fscanf(pipe, "%64s", token);
    int status = pclose(pipe);
    if (scanned != 1 || status == -1 || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return -1;
    }

    strncpy(out, token, out_size - 1);
    out[out_size - 1] = 0;
    return 0;
}

static int verify_sha256_file(const char *image_path) {
    char sha_path[256];
    snprintf(sha_path, sizeof(sha_path), "%s.sha256", image_path);

    if (!struct_file_exists(sha_path)) {
        log(COLOR_YELLOW "ADVERTENCIA: No existe %s; se omite verificación SHA-256 previa." COLOR_RESET, sha_path);
        return 0;
    }

    FILE *fp = fopen(sha_path, "r");
    if (fp == NULL) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo abrir %s." COLOR_RESET, sha_path);
        return -1;
    }

    char expected[SHA256_HEX_LEN + 1];
    if (fscanf(fp, "%64s", expected) != 1) {
        fclose(fp);
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo leer el hash esperado de %s." COLOR_RESET, sha_path);
        return -1;
    }
    fclose(fp);

    char cmd[512];
    snprintf(cmd, sizeof(cmd), "sha256sum %s", image_path);
    char actual[SHA256_HEX_LEN + 1];
    if (run_command_capture_token(cmd, actual, sizeof(actual)) != 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo calcular sha256sum de %s." COLOR_RESET, image_path);
        return -1;
    }

    if (strcasecmp(expected, actual) != 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: SHA-256 no coincide para %s." COLOR_RESET, image_path);
        log("  Esperado: %s", expected);
        log("  Actual:   %s", actual);
        return -1;
    }

    log(COLOR_GREEN "SHA-256 verificado correctamente para %s." COLOR_RESET, image_path);
    return 0;
}

static int build_partition_plan(enum layout_mode layout, uint64_t disk_sectors,
                                struct partition_plan *parts, int *part_count,
                                char *summary, size_t summary_size) {
    memset(parts, 0, sizeof(struct partition_plan) * 4);

    if (disk_sectors < PART1_START + PART1_SECTORS + 4096ULL) {
        log(COLOR_RED COLOR_BOLD "ERROR: El disco es demasiado pequeño para instalar Eclipse OS." COLOR_RESET);
        return -1;
    }

    strcpy(parts[0].name, "EFI");
    parts[0].start_sector = PART1_START;
    parts[0].sectors = PART1_SECTORS;
    parts[0].mbr_type = 0xEF;
    parts[0].gpt_type = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    parts[0].image_path = "/boot/efi.img.gz";
    parts[0].write_image = 1;
    parts[0].verify_after_write = 1;

    uint64_t next_start = align_up_u64(parts[0].start_sector + parts[0].sectors, 2048ULL);

    if (layout == LAYOUT_SIMPLE) {
        strcpy(parts[1].name, "ROOT");
        parts[1].start_sector = next_start;
        parts[1].sectors = disk_sectors - parts[1].start_sector;
        parts[1].mbr_type = 0x83;
        parts[1].gpt_type = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
        parts[1].image_path = "/boot/rootfs.ext2.gz";
        parts[1].write_image = 1;
        parts[1].verify_after_write = 1;
        *part_count = 2;
        snprintf(summary, summary_size, "Simple (EFI 100MB + ROOT resto)");
        return 0;
    }

    uint64_t swap_target = (disk_sectors * 5ULL) / 100ULL;
    uint64_t max_swap = (2ULL * GIB) / SECTOR_SIZE;
    if (swap_target > max_swap) {
        swap_target = max_swap;
    }
    swap_target = align_up_u64(swap_target, 2048ULL);

    uint64_t root_target;
    if (disk_sectors * SECTOR_SIZE >= 30ULL * GIB) {
        root_target = (10ULL * GIB) / SECTOR_SIZE;
    } else {
        uint64_t remaining = disk_sectors - next_start;
        root_target = remaining / 2ULL;
    }
    root_target = align_up_u64(root_target, 2048ULL);

    uint64_t swap_start = align_down_u64(disk_sectors - swap_target, 2048ULL);
    if (swap_start <= next_start + 4096ULL) {
        log(COLOR_RED COLOR_BOLD "ERROR: El disco es demasiado pequeño para el esquema avanzado." COLOR_RESET);
        return -1;
    }

    if (next_start + root_target + 2048ULL >= swap_start) {
        root_target = align_down_u64((swap_start - next_start) / 2ULL, 2048ULL);
    }
    if (root_target < 2048ULL) {
        log(COLOR_RED COLOR_BOLD "ERROR: No hay espacio suficiente para ROOT en el esquema avanzado." COLOR_RESET);
        return -1;
    }

    strcpy(parts[1].name, "ROOT");
    parts[1].start_sector = next_start;
    parts[1].sectors = root_target;
    parts[1].mbr_type = 0x83;
    parts[1].gpt_type = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
    parts[1].image_path = "/boot/rootfs.ext2.gz";
    parts[1].write_image = 1;
    parts[1].verify_after_write = 1;

    strcpy(parts[2].name, "HOME");
    parts[2].start_sector = align_up_u64(parts[1].start_sector + parts[1].sectors, 2048ULL);
    if (parts[2].start_sector >= swap_start) {
        log(COLOR_RED COLOR_BOLD "ERROR: No hay espacio suficiente para HOME en el esquema avanzado." COLOR_RESET);
        return -1;
    }
    parts[2].sectors = swap_start - parts[2].start_sector;
    parts[2].mbr_type = 0x83;
    parts[2].gpt_type = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
    parts[2].image_path = NULL;
    parts[2].write_image = 0;
    parts[2].verify_after_write = 0;

    strcpy(parts[3].name, "SWAP");
    parts[3].start_sector = swap_start;
    parts[3].sectors = disk_sectors - swap_start;
    parts[3].mbr_type = 0x82;
    parts[3].gpt_type = "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F";
    parts[3].image_path = NULL;
    parts[3].write_image = 0;
    parts[3].verify_after_write = 0;

    *part_count = 4;
    snprintf(summary, summary_size, "Avanzado (EFI 100MB + ROOT + HOME + SWAP)");
    return 0;
}

static int prepare_gpt_or_fallback(enum table_mode *table) {
    if (*table != TABLE_GPT) {
        return 0;
    }

    if (command_exists("sgdisk") || command_exists("gdisk")) {
        return 0;
    }

    log(COLOR_YELLOW COLOR_BOLD "ADVERTENCIA: ni sgdisk ni gdisk están disponibles; se usará MBR como alternativa." COLOR_RESET);
    *table = TABLE_MBR;
    return 0;
}

static int write_mbr_partition_table(const char *disk_path, const struct partition_plan *parts, int part_count, int dry_run) {
    if (dry_run) {
        log(COLOR_YELLOW "[dry-run] Se escribiría tabla MBR en %s." COLOR_RESET, disk_path);
        return 0;
    }

    uint8_t mbr[SECTOR_SIZE];
    memset(mbr, 0, sizeof(mbr));

    for (int i = 0; i < part_count && i < 4; i++) {
        if (parts[i].start_sector > UINT32_MAX || parts[i].sectors > UINT32_MAX) {
            log(COLOR_RED COLOR_BOLD "ERROR: La partición %s excede los límites de MBR." COLOR_RESET, parts[i].name);
            return -1;
        }

        struct partition_entry *pe = (struct partition_entry *)&mbr[446 + (i * 16)];
        pe->boot_indicator = (i == 0) ? 0x80 : 0x00;
        pe->starting_chs[0] = 0x00;
        pe->starting_chs[1] = 0x02;
        pe->starting_chs[2] = 0x00;
        pe->type = parts[i].mbr_type;
        pe->ending_chs[0] = 0x00;
        pe->ending_chs[1] = 0x02;
        pe->ending_chs[2] = 0x00;
        pe->starting_lba = (uint32_t)parts[i].start_sector;
        pe->sectors_count = (uint32_t)parts[i].sectors;
    }

    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    int fd = open(disk_path, O_RDWR);
    if (fd < 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo abrir %s (%s)." COLOR_RESET, disk_path, strerror(errno));
        return -1;
    }

    ssize_t written = write(fd, mbr, sizeof(mbr));
    if (written != (ssize_t)sizeof(mbr)) {
        log(COLOR_RED COLOR_BOLD "ERROR: Falló la escritura del MBR en %s (%s)." COLOR_RESET,
            disk_path, strerror(errno));
        close(fd);
        return -1;
    }

    if (fsync(fd) != 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: fsync falló tras escribir MBR (%s)." COLOR_RESET, strerror(errno));
        close(fd);
        return -1;
    }

    close(fd);
    log(COLOR_GREEN "Tabla MBR escrita correctamente." COLOR_RESET);
    return 0;
}

static int write_gpt_partition_table(const char *disk_path, const struct partition_plan *parts, int part_count, int dry_run) {
    char cmd[4096];

    if (command_exists("sgdisk")) {
        int pos = snprintf(cmd, sizeof(cmd), "sgdisk -og");
        for (int i = 0; i < part_count && pos > 0 && pos < (int)sizeof(cmd); i++) {
            uint64_t end_sector = parts[i].start_sector + parts[i].sectors - 1ULL;
            pos += snprintf(cmd + pos, sizeof(cmd) - (size_t)pos,
                            " -n %d:%" PRIu64 ":%" PRIu64 " -t %d:%s -c %d:%s",
                            i + 1, parts[i].start_sector, end_sector,
                            i + 1, parts[i].gpt_type,
                            i + 1, parts[i].name);
        }
        snprintf(cmd + strlen(cmd), sizeof(cmd) - strlen(cmd), " %s", disk_path);
        return run_command_logged(cmd, dry_run);
    }

    if (!command_exists("gdisk")) {
        log(COLOR_RED COLOR_BOLD "ERROR: GPT solicitado pero no hay sgdisk/gdisk disponible." COLOR_RESET);
        return -1;
    }

    char script[2048];
    int spos = snprintf(script, sizeof(script), "o\\ny\\n");
    for (int i = 0; i < part_count && spos > 0 && spos < (int)sizeof(script); i++) {
        uint64_t end_sector = parts[i].start_sector + parts[i].sectors - 1ULL;
        const char *type_code = (parts[i].mbr_type == 0xEF) ? "ef00" :
                                (parts[i].mbr_type == 0x82 ? "8200" : "8300");
        spos += snprintf(script + spos, sizeof(script) - (size_t)spos,
                         "n\\n%d\\n%" PRIu64 "\\n%" PRIu64 "\\n%s\\n",
                         i + 1, parts[i].start_sector, end_sector, type_code);
    }
    snprintf(script + strlen(script), sizeof(script) - strlen(script), "w\\ny\\n");
    snprintf(cmd, sizeof(cmd), "bash -lc \"printf %%b %s | gdisk %s\"", script, disk_path);
    return run_command_logged(cmd, dry_run);
}

static int write_partition_image_with_retry(const char *disk_path, const struct partition_plan *part, int dry_run) {
    char cmd[1024];
    for (int attempt = 1; attempt <= 3; attempt++) {
        snprintf(cmd, sizeof(cmd),
                 "bash -lc \"set -o pipefail; zcat %s | dd of=%s bs=512 seek=%" PRIu64 " status=progress\"",
                 part->image_path, disk_path, part->start_sector);

        if (run_command_logged(cmd, dry_run) == 0) {
            log(COLOR_GREEN "%s escrita correctamente en intento %d." COLOR_RESET, part->name, attempt);
            return 0;
        }
        log(COLOR_YELLOW "Intento %d/3 falló al escribir %s; reintentando..." COLOR_RESET, attempt, part->name);
    }

    log(COLOR_RED COLOR_BOLD "ERROR: Se agotaron los reintentos al escribir %s." COLOR_RESET, part->name);
    return -1;
}

static int verify_partition_write(const char *disk_path, const struct partition_plan *part, int dry_run) {
    uint64_t skip_blocks = (part->start_sector * SECTOR_SIZE) / 4096ULL;
    char disk_cmd[1024];
    char src_cmd[1024];

    snprintf(disk_cmd, sizeof(disk_cmd),
             "bash -lc \"dd if=%s bs=4096 skip=%" PRIu64 " count=16 2>/dev/null | sha256sum\"",
             disk_path, skip_blocks);
    snprintf(src_cmd, sizeof(src_cmd),
             "bash -lc \"zcat %s | dd bs=4096 count=16 2>/dev/null | sha256sum\"",
             part->image_path);

    if (dry_run) {
        log(COLOR_YELLOW "[dry-run] Verificación posterior: %s" COLOR_RESET, disk_cmd);
        log(COLOR_YELLOW "[dry-run] Verificación posterior: %s" COLOR_RESET, src_cmd);
        return 0;
    }

    char disk_sha[SHA256_HEX_LEN + 1];
    char src_sha[SHA256_HEX_LEN + 1];
    if (run_command_capture_token(disk_cmd, disk_sha, sizeof(disk_sha)) != 0 ||
        run_command_capture_token(src_cmd, src_sha, sizeof(src_sha)) != 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo verificar %s tras la escritura." COLOR_RESET, part->name);
        return -1;
    }

    if (strcasecmp(disk_sha, src_sha) != 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: Verificación post-escritura falló para %s." COLOR_RESET, part->name);
        log("  source64k: %s", src_sha);
        log("  disk64k:   %s", disk_sha);
        return -1;
    }

    log(COLOR_GREEN "Verificación post-escritura correcta para %s." COLOR_RESET, part->name);
    return 0;
}

int main(int argc, char **argv) {
    struct timespec start_time;
    clock_gettime(CLOCK_MONOTONIC, &start_time);

    g_log_file = fopen("/tmp/eclipse-install.log", "a");
    if (g_log_file != NULL) {
        setvbuf(g_log_file, NULL, _IOLBF, 0);
    }
    atexit(close_log_file);

    struct config cfg;
    if (parse_args(argc, argv, &cfg) != 0) {
        return 1;
    }

    print_header();
    log(COLOR_WHITE "Buscando dispositivos de almacenamiento disponibles...\n" COLOR_RESET);

    struct disk_info disks[MAX_DISKS];
    memset(disks, 0, sizeof(disks));
    char disk_list[MAX_DISKS][128];
    memset(disk_list, 0, sizeof(disk_list));
    int found_disks = scan_disks(disks, disk_list);

    if (found_disks == 0 && !cfg.disk_set) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se detectaron discos de almacenamiento." COLOR_RESET);
        log("Asegúrese de haber iniciado la máquina con un disco adjunto.");
        return 1;
    }

    if (prompt_disk_selection(&cfg, disks, disk_list, found_disks) != 0) {
        return 1;
    }

    uint64_t disk_size_bytes = get_disk_size_bytes(cfg.disk_path);
    if (disk_size_bytes == 0) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se pudo acceder al disco %s." COLOR_RESET, cfg.disk_path);
        return 1;
    }
    uint64_t disk_sectors = disk_size_bytes / SECTOR_SIZE;

    print_header();
    char disk_size_str[64];
    format_size(disk_size_str, sizeof(disk_size_str), disk_size_bytes);
    log("Disco seleccionado: " COLOR_YELLOW "%s" COLOR_RESET " (%s)", cfg.disk_path, disk_size_str);

    struct existing_install_info existing;
    if (detect_existing_install(cfg.disk_path, &existing) != 0) {
        return 1;
    }

    if (existing.has_valid_mbr && existing.has_efi_partition) {
        log(COLOR_YELLOW "Se detectó MBR válido con partición EFI en el disco." COLOR_RESET);
    }
    if (existing.has_gpt_header) {
        log(COLOR_YELLOW "Se detectó cabecera GPT existente en el disco." COLOR_RESET);
    }

    if (prompt_install_mode(&cfg, &existing) != 0) {
        return 1;
    }

    if (cfg.mode == MODE_NEW) {
        log(COLOR_RED COLOR_BOLD "¡ADVERTENCIA! La nueva instalación reparticionará el disco y borrará datos existentes." COLOR_RESET);
        if (prompt_layout(&cfg) != 0) {
            return 1;
        }
        if (prompt_table(&cfg, struct_file_exists("/sys/firmware/efi")) != 0) {
            return 1;
        }
        if (prepare_gpt_or_fallback(&cfg.table) != 0) {
            return 1;
        }
    } else {
        log(COLOR_GREEN "Modo actualización: se omite el reparticionado." COLOR_RESET);
    }

    int removable = is_removable_disk(cfg.disk_path);
    if (removable) {
        log(COLOR_YELLOW COLOR_BOLD "ADVERTENCIA: %s es un dispositivo extraíble (/sys/block/.../removable=1)." COLOR_RESET,
            cfg.disk_path);
        if (!cfg.auto_yes) {
            if (!ask_yes_no("¿Desea continuar con este disco extraíble? (y/N):", 0, 0)) {
                log("Instalación cancelada por el usuario.");
                return 2;
            }
        } else {
            log(COLOR_YELLOW "--yes activo: se continúa pese a la advertencia de disco extraíble." COLOR_RESET);
        }
    }

    if (!struct_file_exists("/boot/efi.img.gz")) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se encontró /boot/efi.img.gz" COLOR_RESET);
        return 1;
    }
    if (!struct_file_exists("/boot/rootfs.ext2.gz")) {
        log(COLOR_RED COLOR_BOLD "ERROR: No se encontró /boot/rootfs.ext2.gz" COLOR_RESET);
        return 1;
    }

    if (verify_sha256_file("/boot/efi.img.gz") != 0) {
        return 1;
    }
    if (verify_sha256_file("/boot/rootfs.ext2.gz") != 0) {
        return 1;
    }

    struct partition_plan parts[4];
    int part_count = 0;
    char partition_summary[128] = "Existentes (sin reparticionar)";
    if (cfg.mode == MODE_NEW) {
        if (build_partition_plan(cfg.layout, disk_sectors, parts, &part_count,
                                 partition_summary, sizeof(partition_summary)) != 0) {
            return 1;
        }
    } else {
        memset(parts, 0, sizeof(parts));
        strcpy(parts[0].name, "EFI");
        parts[0].start_sector = PART1_START;
        parts[0].sectors = PART1_SECTORS;
        parts[0].mbr_type = 0xEF;
        parts[0].gpt_type = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
        parts[0].image_path = "/boot/efi.img.gz";
        parts[0].write_image = 1;
        parts[0].verify_after_write = 1;

        strcpy(parts[1].name, "ROOT");
        parts[1].start_sector = align_up_u64(PART1_START + PART1_SECTORS, 2048ULL);
        parts[1].sectors = (disk_sectors > parts[1].start_sector) ? (disk_sectors - parts[1].start_sector) : 0;
        parts[1].mbr_type = 0x83;
        parts[1].gpt_type = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
        parts[1].image_path = "/boot/rootfs.ext2.gz";
        parts[1].write_image = 1;
        parts[1].verify_after_write = 1;
        part_count = 2;
    }

    log("========================================");
    log(" RESUMEN DE INSTALACIÓN");
    log("========================================");
    log(" Disco:        %s  (%s)", cfg.disk_path, disk_size_str);
    log(" Modo:         %s", mode_label(cfg.mode));
    log(" Tabla:        %s", cfg.mode == MODE_NEW ? table_label(cfg.table) : "Sin cambios");
    log(" Particiones:  %s", partition_summary);
    log(" Dry-run:      %s", cfg.dry_run ? "Sí" : "No");
    log("========================================");

    if (cfg.auto_yes) {
        log("¿Confirmar y ejecutar? (y/N): y (--yes)");
    } else if (!ask_yes_no("¿Confirmar y ejecutar? (y/N):", 0, 0)) {
        log("Instalación cancelada por el usuario.");
        return 2;
    }

    if (cfg.mode == MODE_NEW) {
        log(COLOR_GREEN "[1/3] Preparando tabla de particiones..." COLOR_RESET);
        int table_rc = (cfg.table == TABLE_GPT)
                       ? write_gpt_partition_table(cfg.disk_path, parts, part_count, cfg.dry_run)
                       : write_mbr_partition_table(cfg.disk_path, parts, part_count, cfg.dry_run);
        if (table_rc != 0) {
            return 1;
        }
    } else {
        log(COLOR_GREEN "[1/3] Actualización seleccionada: se omite la tabla de particiones." COLOR_RESET);
    }

    log(COLOR_GREEN "[2/3] Escribiendo partición EFI..." COLOR_RESET);
    if (write_partition_image_with_retry(cfg.disk_path, &parts[0], cfg.dry_run) != 0) {
        return 1;
    }
    if (verify_partition_write(cfg.disk_path, &parts[0], cfg.dry_run) != 0) {
        return 1;
    }

    log(COLOR_GREEN "[3/3] Escribiendo partición ROOT..." COLOR_RESET);
    if (write_partition_image_with_retry(cfg.disk_path, &parts[1], cfg.dry_run) != 0) {
        return 1;
    }
    if (verify_partition_write(cfg.disk_path, &parts[1], cfg.dry_run) != 0) {
        return 1;
    }

    if (cfg.mode == MODE_NEW && cfg.layout == LAYOUT_ADVANCED) {
        log(COLOR_GREEN "Información: las particiones HOME y SWAP fueron creadas sin sobrescribir contenido adicional." COLOR_RESET);
    }

    sync();

    struct timespec end_time;
    clock_gettime(CLOCK_MONOTONIC, &end_time);
    time_t elapsed = (time_t)(end_time.tv_sec - start_time.tv_sec);
    int hours = (int)(elapsed / 3600);
    int minutes = (int)((elapsed % 3600) / 60);
    int seconds = (int)(elapsed % 60);

    log(COLOR_GREEN COLOR_BOLD "========================================================" COLOR_RESET);
    log(COLOR_GREEN COLOR_BOLD " *          INSTALACIÓN COMPLETADA CON ÉXITO          *" COLOR_RESET);
    log(COLOR_GREEN COLOR_BOLD "========================================================" COLOR_RESET);
    log("Eclipse OS se ha instalado correctamente en %s.", cfg.disk_path);
    log("Tiempo total transcurrido: %02d:%02d:%02d", hours, minutes, seconds);

    return 0;
}

int struct_file_exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}
