/* Las cadenas de pantalla del instalador, en los dos idiomas.
 *
 * El instalador no enlaza ninguna biblioteca del sistema (se compila contra
 * musl y se mete en el initramfs), asi que aqui no hay gettext ni catalogos:
 * la tabla va dentro del propio binario, igual que el crate `eclipse-i18n`
 * hace con los clientes del escritorio.
 *
 * Una fila es `X(clave, castellano, ingles)` cuando el texto cambia de un
 * idioma al otro, y `XB(clave, texto)` cuando es el mismo en los dos
 * (`MBR/GPT`, `BIOS/Legacy`, las letras de las opciones). El test
 * `install_eclipse_i18n_table` del xtask lee este fichero y comprueba que
 * cada `X` tiene las dos columnas distintas y con las MISMAS conversiones de
 * `printf`: una columna con un `%s` de menos se lleva la pila por delante en
 * mitad de una instalacion.
 *
 * El color NO va en la tabla, va en la llamada: `TC(COLOR_RED, S_X)`. Asi la
 * fila se puede leer y traducir, y un `COLOR_RESET` que falte no se puede
 * colar en una sola de las dos columnas. Lo que si va dentro de la fila es un
 * color que cae EN MEDIO de la frase, y un `PRIu64` de la conversion: la fila
 * se concatena en tiempo de compilacion, asi que vale cualquier cosa que sea
 * un literal de cadena.
 *
 * Hay que incluirlo DESPUES de los `COLOR_*` y ANTES de `#define log`. */
#ifndef ECLIPSE_INSTALLER_I18N_H
#define ECLIPSE_INSTALLER_I18N_H

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define INSTALLER_STRINGS(X, XB) \
    X(S_ERR_DISK_NEEDS_PATH, "ERROR: --disk requiere una ruta.", \
        "ERROR: --disk needs a path.") \
    X(S_ERR_MODE_NEEDS_VALUE, "ERROR: --mode requiere new o upgrade.", \
        "ERROR: --mode needs new or upgrade.") \
    X(S_ERR_LANG_NEEDS_VALUE, "ERROR: --lang requiere es o us.", \
        "ERROR: --lang needs es or us.") \
    X(S_ERR_BAD_MODE, "ERROR: modo inválido: %s", \
        "ERROR: invalid mode: %s") \
    X(S_ERR_BAD_LANG, "ERROR: idioma inválido: %s", \
        "ERROR: invalid language: %s") \
    X(S_ERR_LAYOUT_EXCLUSIVE, "ERROR: --simple y --advanced son mutuamente excluyentes.", \
        "ERROR: --simple and --advanced are mutually exclusive.") \
    X(S_ERR_TABLE_EXCLUSIVE, "ERROR: --mbr y --gpt son mutuamente excluyentes.", \
        "ERROR: --mbr and --gpt are mutually exclusive.") \
    X(S_ERR_BAD_ARG, "ERROR: argumento no reconocido: %s", \
        "ERROR: unrecognised argument: %s") \
    X(S_MODE_NEW, "Nueva instalación", \
        "New install") \
    X(S_MODE_UPGRADE, "Actualización", \
        "Upgrade") \
    X(S_UNSET, "Sin definir", \
        "Not set") \
    X(S_LAYOUT_SIMPLE, "Simple (EFI %s + ROOT resto)", \
        "Simple (EFI %s + ROOT for the rest)") \
    X(S_LAYOUT_ADVANCED, "Avanzado (EFI %s + ROOT + HOME + SWAP)", \
        "Advanced (EFI %s + ROOT + HOME + SWAP)") \
    XB(S_TABLE_MBR, "MBR/BIOS") \
    XB(S_TABLE_GPT, "GPT/UEFI") \
    X(S_TABLE_UNCHANGED, "Sin cambios", \
        "Unchanged") \
    X(S_UPGRADE_SUMMARY, "Actualización selectiva (EFI: bootloader + kernel + initramfs)", \
        "Selective upgrade (EFI: bootloader + kernel + initramfs)") \
    X(S_WORD_YES, "Sí", \
        "Yes") \
    XB(S_WORD_NO, "No") \
    X(S_SUMMARY_TITLE, " RESUMEN DE INSTALACIÓN", \
        " INSTALL SUMMARY") \
    X(S_SUMMARY_DISK, " Disco:        %s  (%s)", \
        " Disk:         %s  (%s)") \
    X(S_SUMMARY_MODE, " Modo:         %s", \
        " Mode:         %s") \
    X(S_SUMMARY_TABLE, " Tabla:        %s", \
        " Table:        %s") \
    X(S_SUMMARY_PARTS, " Particiones:  %s", \
        " Partitions:   %s") \
    X(S_SUMMARY_DRYRUN, " Dry-run:      %s", \
        " Dry run:      %s") \
    X(S_DONE_INSTALL, " *          INSTALACIÓN COMPLETADA CON ÉXITO          *", \
        " *           INSTALL COMPLETED SUCCESSFULLY           *") \
    X(S_DONE_UPGRADE, " *         ACTUALIZACIÓN COMPLETADA CON ÉXITO         *", \
        " *           UPGRADE COMPLETED SUCCESSFULLY           *") \
    X(S_LOOKING_FOR_DISKS, "Buscando dispositivos de almacenamiento disponibles...\n", \
        "Looking for available storage devices...\n") \
    X(S_ERR_NO_DISKS, "ERROR: No se detectaron discos de almacenamiento.", \
        "ERROR: No storage disks were found.") \
    X(S_ATTACH_A_DISK, "Asegúrese de haber iniciado la máquina con un disco adjunto.", \
        "Make sure the machine was started with a disk attached.") \
    X(S_DISK_FROM_CLI, "Disco preseleccionado por CLI: " COLOR_YELLOW "%s" COLOR_RESET, \
        "Disk preselected on the command line: " COLOR_YELLOW "%s" COLOR_RESET) \
    X(S_DISK_CHOSEN, "Disco seleccionado: " COLOR_YELLOW "%s" COLOR_RESET " (%s)", \
        "Disk chosen: " COLOR_YELLOW "%s" COLOR_RESET " (%s)") \
    X(S_ASK_DISK, "Seleccione el disco de destino [por defecto: %s]:", \
        "Choose the target disk [default: %s]:") \
    X(S_YES_FIRST_DISK, "--yes activo: usando el primer disco detectado (%s).", \
        "--yes is on: using the first disk found (%s).") \
    X(S_EMPTY_INPUT_USING, "Entrada vacía: usando %s.", \
        "Empty answer: using %s.") \
    X(S_ERR_DISK_UNREACHABLE, "ERROR: No se pudo acceder al disco %s.", \
        "ERROR: Disk %s could not be reached.") \
    X(S_WARN_REMOVABLE, "ADVERTENCIA: %s es un dispositivo extraíble (/sys/block/.../removable=1).", \
        "WARNING: %s is a removable device (/sys/block/.../removable=1).") \
    X(S_WARN_REMOVABLE_HINT, "  Instalar aquí se lleva por delante la sesión live, y el medio deja de", \
        "  Installing here wipes the live session, and the medium stops being") \
    X(S_WARN_REMOVABLE_HINT2, "  servir para instalar. Lo normal es elegir el disco interno.", \
        "  usable for installing. The usual choice is the internal disk.") \
    X(S_ASK_REMOVABLE, "¿Desea continuar con este disco extraíble? (y/N):", \
        "Continue with this removable disk? (y/N):") \
    X(S_YES_REMOVABLE, "--yes activo: se continúa pese a la advertencia de disco extraíble.", \
        "--yes is on: carrying on despite the removable-disk warning.") \
    X(S_WARN_BOOT_MEDIUM, "ADVERTENCIA: %s es el medio del que ha arrancado: %s.", \
        "WARNING: %s is the medium you booted from: %s.") \
    X(S_WARN_PICK_OTHER, "  Indique otro disco con --disk.", \
        "  Pick another disk with --disk.") \
    X(S_ASK_OVER_BOOT_MEDIUM, "¿Instalar de todos modos sobre el medio de arranque? (y/N):", \
        "Install over the boot medium anyway? (y/N):") \
    X(S_ERR_YES_OVER_BOOT_MEDIUM, "ERROR: con --yes no se instala sobre el propio medio de arranque.", \
        "ERROR: --yes will not install over the boot medium itself.") \
    X(S_WARN_ROOT_IS_INSTALLED, "AVISO: la raíz (/) es %s, o sea un Eclipse YA INSTALADO, no el medio live.", \
        "NOTE: the root (/) is %s, that is an ALREADY INSTALLED Eclipse, not the live medium.") \
    X(S_WARN_IMAGES_LIVE_ONLY, "  Las imágenes de instalación viajan sólo dentro del medio de instalación;", \
        "  The install images travel only inside the install medium;") \
    X(S_WARN_IMAGES_LIVE_ONLY2, "  la raíz instalada se deja sin ellas a propósito. Arranque el USB o el ISO.", \
        "  the installed root is left without them on purpose. Boot the USB or the ISO.") \
    X(S_BOOT_ENV, "Entorno de arranque detectado: %s", \
        "Boot environment detected: %s") \
    XB(S_BOOT_EFI, "EFI/UEFI") \
    XB(S_BOOT_BIOS, "BIOS/Legacy") \
    X(S_WARN_UEFI_ONLY, "AVISO: Eclipse arranca únicamente por UEFI; no existe un cargador para BIOS.", \
        "NOTE: Eclipse boots through UEFI only; there is no BIOS loader.") \
    X(S_WARN_UEFI_ONLY2, "  Arranque el medio de instalación en modo UEFI (desactive CSM/Legacy en el", \
        "  Boot the install medium in UEFI mode (turn CSM/Legacy off in the") \
    X(S_WARN_UEFI_ONLY3, "  firmware) o el disco instalado no aparecerá como opción de arranque.", \
        "  firmware) or the installed disk will not show up as a boot option.") \
    X(S_WARN_MBR, "AVISO: con MBR el disco sólo arranca si el firmware está en modo UEFI y acepta", \
        "NOTE: with MBR the disk only boots if the firmware is in UEFI mode and accepts") \
    X(S_WARN_MBR2, "  una ESP en MBR. GPT/UEFI es lo seguro.", \
        "  an ESP on MBR. GPT/UEFI is the safe choice.") \
    X(S_TABLE_FROM_CLI, "Tabla de particiones preseleccionada por CLI: %s", \
        "Partition table preselected on the command line: %s") \
    X(S_ASK_TABLE, "Seleccione la tabla de particiones:", \
        "Choose the partition table:") \
    XB(S_TABLE_OPT_MBR, "  [1] MBR/BIOS") \
    XB(S_TABLE_OPT_GPT, "  [2] GPT/UEFI") \
    X(S_YES_TABLE, "--yes activo: usando tabla sugerida (%s).", \
        "--yes is on: using the suggested table (%s).") \
    X(S_MODE_FROM_CLI, "Modo preseleccionado por CLI: %s", \
        "Mode preselected on the command line: %s") \
    X(S_ASK_MODE, "Seleccione el modo de instalación:", \
        "Choose the install mode:") \
    X(S_MODE_OPT_NEW, "  [1] Nueva instalación (reparticiona y reescribe imágenes)", \
        "  [1] New install (repartitions and rewrites images)") \
    X(S_MODE_OPT_UPGRADE, "  [2] Actualización (Sin reparticionado ni reinstalacion de sistema.)", \
        "  [2] Upgrade (no repartitioning and no system reinstall.)") \
    X(S_YES_MODE, "--yes activo: usando modo sugerido (%s).", \
        "--yes is on: using the suggested mode (%s).") \
    X(S_LAYOUT_FROM_CLI, "Esquema de particiones preseleccionado por CLI: %s", \
        "Partition layout preselected on the command line: %s") \
    X(S_ASK_LAYOUT, "Seleccione el esquema de particiones:", \
        "Choose the partition layout:") \
    X(S_LAYOUT_OPT_SIMPLE, "  [1] Simple    -> EFI %s + ROOT resto", \
        "  [1] Simple    -> EFI %s + ROOT for the rest") \
    X(S_LAYOUT_OPT_ADVANCED, "  [2] Avanzado  -> EFI %s + ROOT + HOME + SWAP", \
        "  [2] Advanced  -> EFI %s + ROOT + HOME + SWAP") \
    X(S_YES_LAYOUT, "--yes activo: usando esquema simple por defecto.", \
        "--yes is on: using the simple layout by default.") \
    X(S_RECOMMENDED, "Se recomienda %s.", \
        "%s is recommended.") \
    X(S_ASK_RECOMMENDED_N, "Opción recomendada [%d]:", \
        "Recommended choice [%d]:") \
    X(S_ASK_RECOMMENDED_1, "Opción recomendada [1]:", \
        "Recommended choice [1]:") \
    X(S_BAD_CHOICE, "Opción numérica fuera de rango o inválida.", \
        "That number is out of range or not a number.") \
    X(S_FOUND_GPT, "Se detectó cabecera GPT existente en el disco.", \
        "An existing GPT header was found on the disk.") \
    X(S_FOUND_MBR_ESP, "Se detectó MBR válido con partición EFI en el disco.", \
        "A valid MBR with an EFI partition was found on the disk.") \
    X(S_FOUND_INSTALL, "Se detectó una instalación existente compatible; se recomienda ACTUALIZACIÓN.", \
        "A compatible existing install was found; UPGRADE is recommended.") \
    X(S_NO_INSTALL, "No se detectó instalación previa conocida; se recomienda NUEVA instalación.", \
        "No known previous install was found; a NEW install is recommended.") \
    X(S_ERR_OPEN_PREV, "ERROR: No se pudo abrir %s para detectar instalaciones previas (%s).", \
        "ERROR: Could not open %s to look for previous installs (%s).") \
    X(S_ASK_CONFIRM, "¿Confirmar y ejecutar? (y/N):", \
        "Confirm and run? (y/N):") \
    X(S_WARN_WIPE, "¡ADVERTENCIA! La nueva instalación reparticionará el disco y borrará datos existentes.", \
        "WARNING! A new install will repartition the disk and erase the data on it.") \
    X(S_CANCELLED, "Instalación cancelada por el usuario.", \
        "Install cancelled by the user.") \
    X(S_ERR_DISK_TOO_SMALL, "ERROR: El disco es demasiado pequeño para instalar Eclipse OS.", \
        "ERROR: The disk is too small to install Eclipse OS.") \
    X(S_ERR_DISK_TOO_SMALL_GPT, "ERROR: El disco es demasiado pequeño para GPT.", \
        "ERROR: The disk is too small for GPT.") \
    X(S_ERR_DISK_TOO_SMALL_GPT_TAIL, "ERROR: El disco es demasiado pequeño para GPT (se reservan %llu sectores al final).", \
        "ERROR: The disk is too small for GPT (%llu sectors are reserved at the end).") \
    X(S_ERR_DISK_TOO_SMALL_BACKUP, "ERROR: El disco es demasiado pequeño para la cabecera GPT de respaldo.", \
        "ERROR: The disk is too small for the backup GPT header.") \
    X(S_ERR_DISK_TOO_SMALL_ADV, "ERROR: El disco es demasiado pequeño para el esquema avanzado.", \
        "ERROR: The disk is too small for the advanced layout.") \
    X(S_ERR_NO_ROOM_ROOT, "ERROR: No hay espacio suficiente para ROOT.", \
        "ERROR: There is not enough room for ROOT.") \
    X(S_ERR_NO_ROOM_ROOT_ADV, "ERROR: No hay espacio suficiente para ROOT en el esquema avanzado.", \
        "ERROR: There is not enough room for ROOT in the advanced layout.") \
    X(S_ERR_NO_ROOM_HOME_ADV, "ERROR: No hay espacio suficiente para HOME en el esquema avanzado.", \
        "ERROR: There is not enough room for HOME in the advanced layout.") \
    X(S_ERR_PART_PATH, "ERROR: No se pudo construir la ruta de las particiones.", \
        "ERROR: The partition paths could not be built.") \
    X(S_ERR_PART_PATH_HOME, "ERROR: No se pudo construir la ruta de HOME/SWAP.", \
        "ERROR: The HOME/SWAP paths could not be built.") \
    X(S_ERR_PART_PATH_LONG, "ERROR: Ruta de partición demasiado larga.", \
        "ERROR: The partition path is too long.") \
    X(S_ERR_PART_OUT_MBR, "ERROR: La partición %s excede los límites de MBR.", \
        "ERROR: Partition %s goes past the MBR limits.") \
    X(S_ERR_PART_OUT_GPT, "ERROR: La partición %s queda fuera del rango GPT utilizable.", \
        "ERROR: Partition %s falls outside the usable GPT range.") \
    X(S_ERR_GPT_COUNT, "ERROR: Número de particiones GPT inválido.", \
        "ERROR: Invalid number of GPT partitions.") \
    X(S_ERR_GPT_GUID, "ERROR: GUID GPT inválido para %s.", \
        "ERROR: Invalid GPT GUID for %s.") \
    X(S_STEP_TABLE, "[1/3] Preparando tabla de particiones...", \
        "[1/3] Preparing the partition table...") \
    X(S_DRY_GPT, "[dry-run] Se escribiría tabla GPT en %s.", \
        "[dry-run] A GPT table would be written to %s.") \
    X(S_DRY_MBR, "[dry-run] Se escribiría tabla MBR en %s.", \
        "[dry-run] An MBR table would be written to %s.") \
    X(S_GPT_OK, "Tabla GPT escrita correctamente (escritor integrado).", \
        "GPT table written (built-in writer).") \
    X(S_MBR_OK, "Tabla MBR escrita correctamente.", \
        "MBR table written.") \
    X(S_GPT_RETRY_SGDISK, "Reintentando GPT con sgdisk externo...", \
        "Retrying GPT with the external sgdisk...") \
    X(S_GPT_RETRY_GDISK, "Reintentando GPT con gdisk externo...", \
        "Retrying GPT with the external gdisk...") \
    X(S_ERR_GPT_WRITE, "ERROR: No se pudo escribir la tabla GPT.", \
        "ERROR: The GPT table could not be written.") \
    X(S_ERR_GPT_WRITE_DEV, "ERROR: Falló la escritura de la tabla GPT en %s (%s).", \
        "ERROR: Writing the GPT table to %s failed (%s).") \
    X(S_ERR_MBR_WRITE_DEV, "ERROR: Falló la escritura del MBR en %s (%s).", \
        "ERROR: Writing the MBR to %s failed (%s).") \
    X(S_ERR_GPT_FSYNC, "ERROR: fsync falló tras escribir GPT (%s).", \
        "ERROR: fsync failed after writing the GPT (%s).") \
    X(S_ERR_MBR_FSYNC, "ERROR: fsync falló tras escribir MBR (%s).", \
        "ERROR: fsync failed after writing the MBR (%s).") \
    X(S_ERR_GPT_MEM, "ERROR: Sin memoria para leer entradas GPT.", \
        "ERROR: Out of memory while reading the GPT entries.") \
    X(S_ERR_READ_MBR, "ERROR: No se pudo leer el MBR de %s.", \
        "ERROR: The MBR of %s could not be read.") \
    X(S_ERR_READ_SECTOR0, "ERROR: No se pudo leer el sector 0 de %s.", \
        "ERROR: Sector 0 of %s could not be read.") \
    X(S_PARTPROBE_OK, "Solicitada recarga de tabla de particiones para %s.", \
        "Asked the kernel to reload the partition table of %s.") \
    X(S_WARN_PARTPROBE, "ADVERTENCIA: No se pudo solicitar recarga de tabla de particiones para %s (%s).", \
        "WARNING: Could not ask the kernel to reload the partition table of %s (%s).") \
    X(S_WARN_BLKFLSBUF, "ADVERTENCIA: BLKFLSBUF en %s: %s", \
        "WARNING: BLKFLSBUF on %s: %s") \
    X(S_WARN_FSYNC, "ADVERTENCIA: fsync en %s: %s", \
        "WARNING: fsync on %s: %s") \
    X(S_PART_EFI, "Partición EFI detectada:  %s", \
        "EFI partition found:  %s") \
    X(S_PART_ROOT, "Partición ROOT detectada: %s", \
        "ROOT partition found: %s") \
    X(S_PART_EFI_ASSUMED, "No se encontró partición EFI; se asume la partición %d.", \
        "No EFI partition found; assuming partition %d.") \
    X(S_PART_ROOT_ASSUMED, "No se encontró partición ROOT; se asume la partición %d.", \
        "No ROOT partition found; assuming partition %d.") \
    X(S_GPT_UNREADABLE, "No se pudieron leer entradas GPT; usando particiones 1/2 por defecto.", \
        "The GPT entries could not be read; using partitions 1/2 by default.") \
    X(S_GPT_INVALID, "Tabla GPT inválida; usando particiones 1 (EFI) y 2 (ROOT) por defecto.", \
        "Invalid GPT table; using partitions 1 (EFI) and 2 (ROOT) by default.") \
    X(S_ERR_OPEN_FOR_PARTS, "ERROR: No se pudo abrir %s para localizar particiones (%s).", \
        "ERROR: Could not open %s to locate the partitions (%s).") \
    X(S_STEP_EFI, "[2/%d] Escribiendo partición EFI (%s)...", \
        "[2/%d] Writing the EFI partition (%s)...") \
    X(S_STEP_ROOT, "[3/%d] Escribiendo partición ROOT...", \
        "[3/%d] Writing the ROOT partition...") \
    X(S_STEP_HOME, "[4/5] Formateando partición HOME (%s)...", \
        "[4/5] Formatting the HOME partition (%s)...") \
    X(S_STEP_SWAP, "[5/5] Inicializando partición SWAP...", \
        "[5/5] Setting up the SWAP partition...") \
    X(S_PROGRESS_WRITE, "\rEscribiendo %s en %s: %d%% (%llu MiB)...", \
        "\rWriting %s to %s: %d%% (%llu MiB)...") \
    X(S_PROGRESS_SYNC, "\rSincronizando %s (%llu MiB)...", \
        "\rFlushing %s (%llu MiB)...") \
    X(S_PROGRESS_GUNZIP, "\rDescomprimiendo %s: %d%%...", \
        "\rUnpacking %s: %d%%...") \
    X(S_PROGRESS_VERIFY, "\rVerificando %s: %d%%...", \
        "\rVerifying %s: %d%%...") \
    X(S_WROTE_BYTES, "Escritos %zu bytes en %s (sector inicial %" PRIu64 ").", \
        "Wrote %zu bytes to %s (first sector %" PRIu64 ").") \
    X(S_GUNZIPPED_BYTES, "Descomprimidos %zu bytes de %s a %s.", \
        "Unpacked %zu bytes from %s to %s.") \
    XB(S_DRY_GUNZIP_SECTOR, "[dry-run] gzip -dc %s -> %s @ sector %" PRIu64) \
    XB(S_DRY_GUNZIP, "[dry-run] gzip -dc %s -> %s") \
    X(S_ERR_WRITE_AT, "ERROR: Escritura en %s @ %" PRIu64 " falló (%s).", \
        "ERROR: Writing to %s @ %" PRIu64 " failed (%s).") \
    X(S_ERR_WRITE, "ERROR: Escritura en %s falló (%s).", \
        "ERROR: Writing to %s failed (%s).") \
    X(S_ERR_READ, "ERROR: Lectura de %s falló (%s).", \
        "ERROR: Reading %s failed (%s).") \
    X(S_ERR_CLOSE, "ERROR: Cierre de %s falló.", \
        "ERROR: Closing %s failed.") \
    X(S_ERR_CLOSE_ERRNO, "ERROR: No se pudo cerrar %s (%s).", \
        "ERROR: %s could not be closed (%s).") \
    X(S_ERR_OPEN, "ERROR: No se pudo abrir %s (%s).", \
        "ERROR: %s could not be opened (%s).") \
    X(S_ERR_OPEN_PLAIN, "ERROR: No se pudo abrir %s.", \
        "ERROR: %s could not be opened.") \
    X(S_ERR_OPEN_WRITE, "ERROR: No se pudo abrir %s para escritura (%s).", \
        "ERROR: %s could not be opened for writing (%s).") \
    X(S_ERR_OPEN_GUNZIP, "ERROR: No se pudo abrir %s para descompresión.", \
        "ERROR: %s could not be opened for unpacking.") \
    X(S_ERR_GUNZIP, "ERROR: Descompresión de %s falló (%s).", \
        "ERROR: Unpacking %s failed (%s).") \
    X(S_ERR_NOT_FOUND, "ERROR: No se encontró %s", \
        "ERROR: %s was not found") \
    X(S_RETRY_WRITE, "Intento %d/3 falló al escribir %s; reintentando...", \
        "Attempt %d/3 failed writing %s; retrying...") \
    X(S_WRITE_OK_ATTEMPT, "%s escrita correctamente en intento %d.", \
        "%s written on attempt %d.") \
    X(S_ERR_RETRIES, "ERROR: Se agotaron los reintentos al escribir %s.", \
        "ERROR: Ran out of retries writing %s.") \
    X(S_ERR_EFI_TOO_BIG, "ERROR: efi.img.gz descomprimido (%llu MiB) excede la partición EFI (%llu MiB).", \
        "ERROR: efi.img.gz unpacked (%llu MiB) does not fit the EFI partition (%llu MiB).") \
    X(S_REBUILD_IMAGE, "Reconstruya la imagen live (`make image`) con efi.img del mismo tamaño o aumente PART1_SIZE_MIB.", \
        "Rebuild the live image (`make image`) with an efi.img of the same size, or raise PART1_SIZE_MIB.") \
    X(S_WARN_NO_EFI_SIZE, "ADVERTENCIA: No se pudo leer el tamaño de %s; se omite comprobación de capacidad EFI.", \
        "WARNING: The size of %s could not be read; skipping the EFI capacity check.") \
    X(S_ERR_FORMAT_HOME, "ERROR: No se pudo formatear HOME.", \
        "ERROR: HOME could not be formatted.") \
    X(S_ERR_INIT_SWAP, "ERROR: No se pudo inicializar SWAP.", \
        "ERROR: SWAP could not be set up.") \
    X(S_WARN_NO_HOME_IMAGE, "ADVERTENCIA: falta %s; HOME quedará sin formatear.", \
        "WARNING: %s is missing; HOME will be left unformatted.") \
    X(S_ROOT_WILL_GROW, "ROOT (%s, %s) se expandirá a toda la partición en el primer arranque.", \
        "ROOT (%s, %s) will grow to the whole partition on the first boot.") \
    X(S_DRY_ROOT_WILL_GROW, "[dry-run] ROOT (%s) se expandirá en el primer montaje.", \
        "[dry-run] ROOT (%s) will grow on the first mount.") \
    X(S_SHA_OK, "SHA-256 verificado correctamente para %s.", \
        "SHA-256 checked out for %s.") \
    X(S_ERR_SHA_MISMATCH, "ERROR: SHA-256 no coincide para %s.", \
        "ERROR: SHA-256 does not match for %s.") \
    X(S_ERR_SHA_COMPUTE, "ERROR: No se pudo calcular SHA-256 de %s.", \
        "ERROR: The SHA-256 of %s could not be computed.") \
    X(S_ERR_SHA_EXPECTED, "ERROR: No se pudo leer el hash esperado de %s.", \
        "ERROR: The expected hash of %s could not be read.") \
    X(S_WARN_NO_SHA_FILE, "ADVERTENCIA: No existe %s; se omite verificación SHA-256 previa.", \
        "WARNING: %s does not exist; skipping the SHA-256 check beforehand.") \
    X(S_HASH_EXPECTED, "  Esperado: %s", \
        "  Expected: %s") \
    X(S_HASH_ACTUAL, "  Actual:   %s", \
        "  Got:      %s") \
    XB(S_HASH_DISK64K, "  disk64k:   %s") \
    XB(S_HASH_SOURCE64K, "  source64k: %s") \
    X(S_FILE_EMPTY, "  %s está vacío.", \
        "  %s is empty.") \
    X(S_FILE_UNREADABLE, "  %s no se puede leer (%s).", \
        "  %s cannot be read (%s).") \
    X(S_VERIFY_OK, "Verificación post-escritura correcta para %s.", \
        "The read-back check passed for %s.") \
    X(S_ERR_VERIFY, "ERROR: Verificación post-escritura falló para %s.", \
        "ERROR: The read-back check failed for %s.") \
    X(S_DRY_VERIFY, "[dry-run] Verificación posterior de %s (primeros %u bytes).", \
        "[dry-run] Read-back check of %s (first %u bytes).") \
    X(S_ERR_OPEN_VERIFY, "ERROR: No se pudo abrir %s para verificación (%s).", \
        "ERROR: %s could not be opened for the read-back check (%s).") \
    X(S_ERR_READ_VERIFY, "ERROR: No se pudo leer %s para verificación.", \
        "ERROR: %s could not be read for the read-back check.") \
    X(S_ERR_READ_AT_VERIFY, "ERROR: No se pudo leer %s @ %" PRIu64 " para verificación (%s).", \
        "ERROR: %s @ %" PRIu64 " could not be read for the read-back check (%s).") \
    X(S_FSTAB_UPDATING, "Actualizando /etc/fstab en la partición ROOT...", \
        "Updating /etc/fstab on the ROOT partition...") \
    X(S_FSTAB_LOOKING, "Buscando placeholders de fstab en %s...", \
        "Looking for the fstab placeholders in %s...") \
    X(S_ERR_FSTAB_NO_PLACEHOLDER, "ERROR: No se encontraron placeholders de fstab en %s (escaneados %lld bytes).", \
        "ERROR: No fstab placeholders were found in %s (%lld bytes scanned).") \
    X(S_FSTAB_REBUILD_MEDIUM, "Reconstruya el medio de instalación (cargo rootfs && cargo image) para incluir /etc/fstab con placeholders.", \
        "Rebuild the install medium (cargo rootfs && cargo image) so that /etc/fstab carries the placeholders.") \
    X(S_FSTAB_OK_RAW, "fstab actualizado y verificado (en disco) en %s (sin montar).", \
        "fstab updated and checked (on the disk) in %s (without mounting).") \
    X(S_FSTAB_OK_MOUNT, "fstab actualizado en %s (vía mount).", \
        "fstab updated in %s (through mount).") \
    X(S_WARN_FSTAB_RAW_FAILED, "ADVERTENCIA: parche en bruto falló; intentando escribir fstab vía mount.", \
        "WARNING: the raw patch failed; trying to write fstab through mount.") \
    X(S_ERR_FSTAB_WRITE, "ERROR: Escritura de fstab en %s falló (%s).", \
        "ERROR: Writing fstab to %s failed (%s).") \
    X(S_ERR_FSTAB_READBACK, "ERROR: lectura de verificación de fstab en %s falló (%s).", \
        "ERROR: Reading fstab back from %s failed (%s).") \
    X(S_ERR_FSTAB_NOT_REPLACED, "ERROR: verificación de fstab: %s no se sustituyó en %s.", \
        "ERROR: fstab check: %s was not replaced in %s.") \
    X(S_ERR_FSTAB_NOT_COMMENTED, "ERROR: verificación de fstab: la línea de %s no quedó comentada en %s.", \
        "ERROR: fstab check: the %s line was not commented out in %s.") \
    X(S_DRY_FSTAB_WRITE, "[dry-run] Escribiría fstab en %s.", \
        "[dry-run] Would write fstab to %s.") \
    X(S_DRY_FSTAB_PATCH, "[dry-run] Parchearía fstab en %s (ROOT=%s, EFI=%s).", \
        "[dry-run] Would patch fstab in %s (ROOT=%s, EFI=%s).") \
    X(S_RBOOT_OK, "rboot.conf: ROOT=%s fijado en %s (cmdline del kernel).", \
        "rboot.conf: ROOT=%s set in %s (kernel command line).") \
    X(S_DRY_RBOOT, "[dry-run] Fijaría ROOT=%s en rboot.conf de %s.", \
        "[dry-run] Would set ROOT=%s in the rboot.conf of %s.") \
    X(S_WARN_RBOOT_OPEN, "ADVERTENCIA: no se pudo abrir %s para fijar ROOT= en rboot.conf (%s); se usará auto-detección.", \
        "WARNING: %s could not be opened to set ROOT= in rboot.conf (%s); auto-detection will be used.") \
    X(S_WARN_RBOOT_WRITE, "ADVERTENCIA: no se pudo escribir ROOT= en rboot.conf de %s.", \
        "WARNING: ROOT= could not be written to the rboot.conf of %s.") \
    X(S_WARN_RBOOT_NO_PLACEHOLDER, "ADVERTENCIA: rboot.conf sin placeholder ROOT en %s; se usará auto-detección del root.", \
        "WARNING: rboot.conf has no ROOT placeholder in %s; the root will be auto-detected.") \
    X(S_MOUNTING, "Montando %s en %s (tipo: %s)...", \
        "Mounting %s on %s (type: %s)...") \
    X(S_UNMOUNTING, "Desmontando %s...", \
        "Unmounting %s...") \
    XB(S_DRY_MOUNT, "[dry-run] mount -t %s %s -> %s") \
    XB(S_DRY_UMOUNT, "[dry-run] umount %s") \
    X(S_ERR_MOUNT, "ERROR: mount(%s, %s, %s) falló: %s", \
        "ERROR: mount(%s, %s, %s) failed: %s") \
    X(S_WARN_UMOUNT, "ADVERTENCIA: umount(%s) falló: %s", \
        "WARNING: umount(%s) failed: %s") \
    X(S_ERR_MOUNT_PLAIN, "ERROR: No se pudo montar %s.", \
        "ERROR: %s could not be mounted.") \
    X(S_ERR_MOUNT_ROOT, "ERROR: No se pudo montar la partición ROOT (%s) para la descompresión.", \
        "ERROR: The ROOT partition (%s) could not be mounted for unpacking.") \
    X(S_ERR_MOUNT_EFI_IMG, "ERROR: No se pudo montar la imagen EFI extraída en disco.", \
        "ERROR: The EFI image unpacked to disk could not be mounted.") \
    X(S_ERR_EXTRACT_ROOT, "ERROR: No se pudo extraer %s a la partición ROOT.", \
        "ERROR: %s could not be unpacked onto the ROOT partition.") \
    X(S_UPGRADE_MODE, "Modo actualización: copia selectiva de archivos (sin dd ni reparticionado).", \
        "Upgrade mode: selective file copy (no dd and no repartitioning).") \
    X(S_UPGRADE_EFI, "Actualizando bootloader, kernel e initramfs en EFI (copia selectiva, sin dd)...", \
        "Updating the bootloader, the kernel and the initramfs on EFI (selective copy, no dd)...") \
    X(S_EFI_STAGING, "Se detectaron archivos de actualización pre-extraídos en /boot/efi-staging.", \
        "Pre-unpacked upgrade files were found in /boot/efi-staging.") \
    X(S_ESP_ALREADY_MOUNTED, "ESP ya montada en /boot/efi (origen %s); se escribe directamente sin re-montar.", \
        "The ESP is already mounted on /boot/efi (source %s); writing straight to it without remounting.") \
    X(S_UPDATED_FILE, "Actualizado: %s", \
        "Updated: %s") \
    X(S_SKIPPED_FILE, "Omitido (no presente en el medio de instalación): %s", \
        "Skipped (not on the install medium): %s") \
    X(S_ERR_COPY, "ERROR: No se pudo copiar %s -> %s.", \
        "ERROR: %s -> %s could not be copied.") \
    X(S_ERR_OPEN_SOURCE, "ERROR: No se pudo abrir origen %s (%s).", \
        "ERROR: The source %s could not be opened (%s).") \
    X(S_ERR_CREATE_DEST, "ERROR: No se pudo crear destino %s (%s).", \
        "ERROR: The target %s could not be created (%s).") \
    X(S_ERR_CREATE, "ERROR: No se pudo crear %s (%s).", \
        "ERROR: %s could not be created (%s).") \
    X(S_DRY_COPY, "[dry-run] copiar %s -> %s", \
        "[dry-run] copy %s -> %s") \
    X(S_NEED_LIVE_MEDIUM, "  Es la fuente del cargador y del kernel: sin ella no hay actualización.", \
        "  It is where the loader and the kernel come from: without it there is no upgrade.") \
    X(S_ERR_RUN, "ERROR: No se pudo ejecutar comando: %s", \
        "ERROR: Could not run command: %s") \
    X(S_ERR_CMD_FAILED, "ERROR: comando falló (%d): %s", \
        "ERROR: command failed (%d): %s") \
    X(S_ERR_PCLOSE, "ERROR: pclose() falló para: %s", \
        "ERROR: pclose() failed for: %s") \
    X(S_INSTALLED_OK, "Eclipse OS se ha instalado correctamente en %s.", \
        "Eclipse OS has been installed on %s.") \
    X(S_UPGRADED_OK, "Eclipse OS se ha actualizado correctamente en %s.", \
        "Eclipse OS has been upgraded on %s.") \
    X(S_APT_HINT, "El resto del sistema puede actualizarse con apt (p. ej. apt-get update && apt-get upgrade).", \
        "The rest of the system can be upgraded with apt (for example apt-get update && apt-get upgrade).") \
    X(S_ELAPSED, "Tiempo total transcurrido: %02d:%02d:%02d", \
        "Total time elapsed: %02d:%02d:%02d")

#define INSTALLER_I18N_ENUM(key, es, en) key,
#define INSTALLER_I18N_ENUM_BOTH(key, text) key,
enum installer_string {
    INSTALLER_STRINGS(INSTALLER_I18N_ENUM, INSTALLER_I18N_ENUM_BOTH)
    S_COUNT
};

#define INSTALLER_I18N_ROW(key, es, en) { es, en },
#define INSTALLER_I18N_ROW_BOTH(key, text) { text, text },
static const char *const INSTALLER_TABLE[S_COUNT][2] = {
    INSTALLER_STRINGS(INSTALLER_I18N_ROW, INSTALLER_I18N_ROW_BOTH)
};

#define INSTALLER_LANG_ES 0
#define INSTALLER_LANG_EN 1

static int g_installer_lang = INSTALLER_LANG_ES;

/* El texto de una clave en el idioma de la sesion. */
#define T(key) INSTALLER_TABLE[key][g_installer_lang]

/* Lo mismo, envuelto en un color y cerrado con COLOR_RESET. Devuelve un
 * buffer estatico: vale para el formato de UNA llamada a `log`, que es como
 * se usa, y no para dos textos de la misma. */
static const char *installer_colored(const char *color, const char *text) {
    static char buf[4096];
    snprintf(buf, sizeof(buf), "%s%s" COLOR_RESET, color, text);
    return buf;
}

#define TC(color, key) installer_colored(color, T(key))

/* El ajuste se escribe como ETIQUETA (`lang=es` / `lang=en`), que es lo que
 * deja `eclipse-locale` en /etc/eclipse/locale y en la linea de ordenes del
 * kernel; `--lang` acepta ademas los nombres de pais de `eclipse-kbd`
 * (`es`/`us`), y $LANG llega como `es_ES.UTF-8` o `en_US.UTF-8`.
 *
 * ATENCION: este es el CUARTO sitio que interpreta el ajuste. Los otros tres
 * son el `normalize` de xtask/src/linux/desktop/locale.rs, el `ui_lang_token`
 * de tools/eclipse-init/src/main.rs y el `Lang::from_name` de eclipse-i18n.
 * Un nombre nuevo hay que añadirlo en los cuatro. */
static int installer_lang_from_name(const char *name) {
    if (name == NULL) {
        return -1;
    }
    while (*name == ' ' || *name == '\t' || *name == '"') {
        name++;
    }
    if (strncmp(name, "es", 2) == 0) {
        return INSTALLER_LANG_ES;
    }
    if (strncmp(name, "en", 2) == 0 || strncmp(name, "us", 2) == 0) {
        return INSTALLER_LANG_EN;
    }
    return -1;
}

/* El valor de `clave=` en un fichero de ajustes o en /proc/cmdline. Mira que
 * la clave empiece palabra para que `ui_lang=` no pase por `lang=`. */
static int installer_lang_from_file(const char *path) {
    FILE *fp = fopen(path, "r");
    if (fp == NULL) {
        return -1;
    }

    char buf[4096];
    size_t got = fread(buf, 1, sizeof(buf) - 1, fp);
    fclose(fp);
    buf[got] = '\0';

    for (char *at = buf; (at = strstr(at, "lang=")) != NULL; at += 5) {
        if (at != buf) {
            char before = at[-1];
            if (before != ' ' && before != '\t' && before != '\n' && before != '\r') {
                continue;
            }
        }
        int lang = installer_lang_from_name(at + 5);
        if (lang >= 0) {
            return lang;
        }
    }
    return -1;
}

/* De mas fuerte a mas debil: `--lang`, la linea de ordenes del kernel, el
 * ajuste del escritorio y por ultimo el entorno. Sin nada de eso, castellano.
 *
 * Se resuelve ANTES de `parse_args` porque los errores de `parse_args` ya se
 * imprimen traducidos. */
static void installer_lang_init(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--lang") == 0 && i + 1 < argc) {
            int lang = installer_lang_from_name(argv[i + 1]);
            if (lang >= 0) {
                g_installer_lang = lang;
                return;
            }
        }
    }

    const char *const files[] = { "/proc/cmdline", "/etc/eclipse/locale" };
    for (size_t i = 0; i < sizeof(files) / sizeof(files[0]); i++) {
        int lang = installer_lang_from_file(files[i]);
        if (lang >= 0) {
            g_installer_lang = lang;
            return;
        }
    }

    const char *const vars[] = { "LC_ALL", "LC_MESSAGES", "LANG" };
    for (size_t i = 0; i < sizeof(vars) / sizeof(vars[0]); i++) {
        int lang = installer_lang_from_name(getenv(vars[i]));
        if (lang >= 0) {
            g_installer_lang = lang;
            return;
        }
    }
}

#endif /* ECLIPSE_INSTALLER_I18N_H */
