#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/stat.h>
#include <errno.h>

#define SECTOR_SIZE 512
#define PART1_START 2048
#define PART1_SECTORS 204800  // 100 MB

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

int struct_file_exists(const char *path);

void clear_screen() {
    printf("\x1b[2J\x1b[H");
}

void print_header() {
    clear_screen();
    printf(COLOR_CYAN COLOR_BOLD "========================================================\n" COLOR_RESET);
    printf(COLOR_BLUE COLOR_BOLD " *               ECLIPSE OS 2 INSTALLER               *\n" COLOR_RESET);
    printf(COLOR_CYAN COLOR_BOLD "========================================================\n\n" COLOR_RESET);
}

// MBR partition table entry structure
struct partition_entry {
    uint8_t boot_indicator;
    uint8_t starting_chs[3];
    uint8_t type;
    uint8_t ending_chs[3];
    uint32_t starting_lba;
    uint32_t sectors_count;
} __attribute__((packed));

int main() {
    char disk_path[128] = "/dev/sda";
    char input[64];
    
    print_header();
    
    printf(COLOR_WHITE "Buscando dispositivos de almacenamiento disponibles...\n\n" COLOR_RESET);
    
    // Scan for /dev/sd* devices
    int found_disks = 0;
    for (char c = 'a'; c <= 'z'; c++) {
        char path[128];
        snprintf(path, sizeof(path), "/dev/sd%c", c);
        
        struct stat st;
        if (stat(path, &st) == 0) {
            uint64_t size_bytes = st.st_size;
            double size_mb = (double)size_bytes / (1024.0 * 1024.0);
            printf("  [%d] " COLOR_YELLOW "%s" COLOR_RESET " (%.2f MB / %.2f GB)\n", 
                   found_disks + 1, path, size_mb, size_mb / 1024.0);
            
            if (found_disks == 0) {
                strncpy(disk_path, path, sizeof(disk_path));
            }
            found_disks++;
        }
    }
    
    if (found_disks == 0) {
        printf(COLOR_RED COLOR_BOLD "ERROR: No se detectaron discos de almacenamiento (/dev/sd*).\n" COLOR_RESET);
        printf("Asegúrese de haber iniciado la máquina virtual con un disco adjunto.\n");
        return 1;
    }
    
    printf("\nSeleccione el disco de destino [por defecto: %s]: ", disk_path);
    if (fgets(input, sizeof(input), stdin)) {
        // Strip newline
        input[strcspn(input, "\n")] = 0;
        if (strlen(input) > 0) {
            if (input[0] >= '1' && input[0] <= '9') {
                int idx = input[0] - '1';
                if (idx >= 0 && idx < found_disks) {
                    char c = 'a' + idx;
                    snprintf(disk_path, sizeof(disk_path), "/dev/sd%c", c);
                }
            } else {
                strncpy(disk_path, input, sizeof(disk_path));
            }
        }
    }
    
    print_header();
    printf("Disco seleccionado: " COLOR_YELLOW "%s" COLOR_RESET "\n\n", disk_path);
    
    // Get disk size
    struct stat st;
    if (stat(disk_path, &st) != 0) {
        printf(COLOR_RED COLOR_BOLD "ERROR: No se pudo acceder al disco %s\n" COLOR_RESET, disk_path);
        return 1;
    }
    uint64_t disk_sectors = st.st_size / SECTOR_SIZE;
    if (disk_sectors < PART1_START + PART1_SECTORS + 2048) {
        printf(COLOR_RED COLOR_BOLD "ERROR: El disco es demasiado pequeño (requiere al menos 200 MB).\n" COLOR_RESET);
        return 1;
    }
    
    printf(COLOR_RED COLOR_BOLD "¡ADVERTENCIA! Se borrarán todos los datos en %s.\n" COLOR_RESET, disk_path);
    printf("¿Está seguro de que desea continuar? (y/N): ");
    if (!fgets(input, sizeof(input), stdin) || (input[0] != 'y' && input[0] != 'Y')) {
        printf("\nInstalación cancelada por el usuario.\n");
        return 0;
    }
    
    print_header();
    printf(COLOR_GREEN "[1/3]" COLOR_WHITE " Escribiendo la tabla de particiones MBR...\n" COLOR_RESET);
    
    // Read current sector 0 to preserve anything if needed, or just write fresh MBR
    uint8_t mbr[SECTOR_SIZE];
    memset(mbr, 0, SECTOR_SIZE);
    
    // Partition 1: EFI System Partition (FAT32), 100MB
    struct partition_entry *pe1 = (struct partition_entry *)&mbr[446];
    pe1->boot_indicator = 0x80; // Active / Bootable
    pe1->starting_chs[0] = 0x00; pe1->starting_chs[1] = 0x02; pe1->starting_chs[2] = 0x00;
    pe1->type = 0xEF; // EFI System Partition
    pe1->ending_chs[0] = 0x00; pe1->ending_chs[1] = 0x02; pe1->ending_chs[2] = 0x00;
    pe1->starting_lba = PART1_START;
    pe1->sectors_count = PART1_SECTORS;
    
    // Partition 2: Linux native (ext2), rest of the disk
    struct partition_entry *pe2 = (struct partition_entry *)&mbr[462];
    pe2->boot_indicator = 0x00;
    pe2->starting_chs[0] = 0x00; pe2->starting_chs[1] = 0x02; pe2->starting_chs[2] = 0x00;
    pe2->type = 0x83; // Linux Native / ext2
    pe2->ending_chs[0] = 0x00; pe2->ending_chs[1] = 0x02; pe2->ending_chs[2] = 0x00;
    pe2->starting_lba = PART1_START + PART1_SECTORS;
    pe2->sectors_count = disk_sectors - pe2->starting_lba;
    
    // MBR signature
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    
    int fd = open(disk_path, O_RDWR);
    if (fd < 0) {
        printf(COLOR_RED COLOR_BOLD "ERROR: No se pudo abrir %s (errno=%d: %s).\n" COLOR_RESET, 
               disk_path, errno, strerror(errno));
        return 1;
    }
    
    ssize_t written = write(fd, mbr, SECTOR_SIZE);
    if (written != SECTOR_SIZE) {
        printf(COLOR_RED COLOR_BOLD "ERROR: Falló al escribir el sector MBR (escrito=%ld, errno=%d: %s).\n" COLOR_RESET,
               (long)written, errno, strerror(errno));
        close(fd);
        return 1;
    }
    close(fd);
    printf("  Tabla de particiones escrita correctamente.\n\n");
    
    // Step 2: Write EFI partition image
    printf(COLOR_GREEN "[2/3]" COLOR_WHITE " Escribiendo cargador, kernel y archivos EFI (FAT32) en la partición 1...\n" COLOR_RESET);
    if (!struct_file_exists("/boot/efi.img.gz")) {
        printf(COLOR_RED COLOR_BOLD "ERROR: No se encontró la imagen EFI en /boot/efi.img.gz\n" COLOR_RESET);
        return 1;
    }
    
    char cmd_efi[256];
    snprintf(cmd_efi, sizeof(cmd_efi), "zcat /boot/efi.img.gz | dd of=%s bs=512 seek=%d status=none 2>/dev/null", 
             disk_path, PART1_START);
    
    if (system(cmd_efi) != 0) {
        printf(COLOR_RED COLOR_BOLD "ERROR: Falló al copiar la partición EFI.\n" COLOR_RESET);
        return 1;
    }
    printf("  Partición EFI copiada correctamente.\n\n");
    
    // Step 3: Write Rootfs partition image
    printf(COLOR_GREEN "[3/3]" COLOR_WHITE " Escribiendo el sistema de archivos raíz (ext2) en la partición 2...\n" COLOR_RESET);
    if (!struct_file_exists("/boot/rootfs.ext2.gz")) {
        printf(COLOR_RED COLOR_BOLD "ERROR: No se encontró el rootfs en /boot/rootfs.ext2.gz\n" COLOR_RESET);
        return 1;
    }
    
    char cmd_rootfs[256];
    snprintf(cmd_rootfs, sizeof(cmd_rootfs), "zcat /boot/rootfs.ext2.gz | dd of=%s bs=512 seek=%d status=none 2>/dev/null", 
             disk_path, PART1_START + PART1_SECTORS);
    
    if (system(cmd_rootfs) != 0) {
        printf(COLOR_RED COLOR_BOLD "ERROR: Falló al copiar la partición raíz ext2.\n" COLOR_RESET);
        return 1;
    }
    printf("  Sistema de archivos raíz ext2 copiado correctamente.\n\n");
    
    printf(COLOR_GREEN COLOR_BOLD "========================================================\n" COLOR_RESET);
    printf(COLOR_GREEN COLOR_BOLD " *          INSTALACIÓN COMPLETADA CON ÉXITO          *\n" COLOR_RESET);
    printf(COLOR_GREEN COLOR_BOLD "========================================================\n\n" COLOR_RESET);
    printf("Eclipse OS se ha instalado correctamente en %s.\n", disk_path);
    printf("Puede retirar el medio de instalación y reiniciar el sistema.\n\n");
    
    return 0;
}

int struct_file_exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}
