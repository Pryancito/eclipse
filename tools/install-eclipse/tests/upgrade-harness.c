/* Arnés de la actualización del instalador: ejerce las funciones `static` de
 * `upgrade.h` contra directorios de verdad, sin arrancar nada.
 *
 * La receta es la del arnés del #1880: el mismo TU que el instalador
 * (`#define main` para apartar el suyo), compilado con el gcc del anfitrión
 * contra `tools/zlib`. Desde `tools/install-eclipse`:
 *
 *     gcc -O1 -D_LARGEFILE64_SOURCE=1 -DNO_GZCOMPRESS -I../zlib -I. \
 *         -o /tmp/h tests/upgrade-harness.c ../zlib/[a-z]*.c && /tmp/h
 *
 * Imprime `TODO BIEN` y sale con 0 cuando todo pasa. Lo corre el test
 * `el_arnes_de_la_actualizacion_pasa` del xtask, que es quien lo mete en la
 * puerta; los avisos de `-Wall -Wextra` del instalador son previos
 * (`unused variable 'i'`, `unused parameter 'parts'`,
 * `run_command_capture_token`).
 *
 * Lo que se mide aquí es la POLÍTICA de la actualización: qué ficheros de la
 * raíz instalada se pisan (los de Eclipse), cuáles no (los del usuario), que
 * el bit de ejecución de un envoltorio viaja, que no queda ningún `.new` a
 * medias, que correrlo dos veces deja lo mismo y que `--dry-run` no escribe.
 */
#define main installer_main_unused
#include "install-eclipse.c"
#undef main

#include <assert.h>

static void write_file(const char *path, const char *text, mode_t mode) {
    char dir[512];
    snprintf(dir, sizeof(dir), "%s", path);
    char *slash = strrchr(dir, '/');
    if (slash) { *slash = 0; char cmd[600]; snprintf(cmd, sizeof(cmd), "mkdir -p '%s'", dir); assert(system(cmd) == 0); }
    FILE *f = fopen(path, "w");
    assert(f != NULL);
    fputs(text, f);
    fclose(f);
    assert(chmod(path, mode) == 0);
}

static char *slurp(const char *path) {
    static char buf[8192];
    FILE *f = fopen(path, "r");
    if (!f) return NULL;
    size_t n = fread(buf, 1, sizeof(buf) - 1, f);
    buf[n] = 0;
    fclose(f);
    return buf;
}

static int exists(const char *p) { struct stat st; return stat(p, &st) == 0; }
static mode_t mode_of(const char *p) { struct stat st; assert(stat(p, &st) == 0); return st.st_mode & 07777; }

int main(void) {
    char tmpl[] = "/tmp/upgh-XXXXXX";
    assert(mkdtemp(tmpl) != NULL);
    char live[512], root[512], path[700], path2[700];
    snprintf(live, sizeof(live), "%s/live", tmpl);
    snprintf(root, sizeof(root), "%s/root", tmpl);

    /* === 1. refresh_one_file: pisa, preserva el modo y es idempotente === */
    snprintf(path, sizeof(path), "%s/wrapper", live);
    write_file(path, "#!/bin/sh\nnuevo\n", 0755);
    snprintf(path2, sizeof(path2), "%s/sub/wrapper", root);
    write_file(path2, "viejo\n", 0644);
    int changed = 0;
    assert(refresh_one_file(path, path2, 0, 0, &changed) == 0);
    assert(changed == 1);
    assert(strcmp(slurp(path2), "#!/bin/sh\nnuevo\n") == 0);
    assert(mode_of(path2) == 0755);   /* el bit x del envoltorio viaja */
    snprintf(path, sizeof(path), "%s/sub/wrapper.new", root);
    assert(!exists(path));            /* no deja el temporal */

    /* only_if_missing: no pisa lo que ya hay */
    snprintf(path, sizeof(path), "%s/wrapper", live);
    changed = 0;
    assert(refresh_one_file(path, path2, 1, 0, &changed) == 0);
    assert(changed == 0);

    /* un origen que el medio no trae no es un error */
    snprintf(path, sizeof(path), "%s/no-existe", live);
    snprintf(path2, sizeof(path2), "%s/no-existe", root);
    changed = 0;
    assert(refresh_one_file(path, path2, 0, 0, &changed) == 0);
    assert(changed == 0 && !exists(path2));

    /* un enlace simbólico se omite, no se copia su contenido */
    snprintf(path, sizeof(path), "%s/enlace", live);
    assert(symlink("/bin/busybox", path) == 0);
    snprintf(path2, sizeof(path2), "%s/enlace", root);
    changed = 0;
    assert(refresh_one_file(path, path2, 0, 0, &changed) == 0);
    assert(changed == 0 && !exists(path2));

    /* === 2. refresh_dir con sufijo: solo los .service === */
    snprintf(path, sizeof(path), "%s/etc/eclipse/services/dbus.service", live);
    write_file(path, "exec = /usr/local/bin/eclipse-dbus-system\n", 0644);
    snprintf(path, sizeof(path), "%s/etc/eclipse/services/LEEME", live);
    write_file(path, "no soy una fila\n", 0644);
    char src_dir[600], dst_dir[600];
    snprintf(src_dir, sizeof(src_dir), "%s/etc/eclipse/services", live);
    snprintf(dst_dir, sizeof(dst_dir), "%s/etc/eclipse/services", root);
    changed = 0;
    assert(refresh_dir(src_dir, dst_dir, ".service", 0, 0, 0, 0, &changed) == 0);
    assert(changed == 1);
    snprintf(path, sizeof(path), "%s/etc/eclipse/services/dbus.service", root);
    assert(exists(path));
    snprintf(path, sizeof(path), "%s/etc/eclipse/services/LEEME", root);
    assert(!exists(path));

    /* === 3. REFRESH_IF_MISSING no pisa la configuración del usuario === */
    snprintf(path, sizeof(path), "%s/etc/eclipse/desktop", live);
    write_file(path, "abyss\n", 0644);
    snprintf(path, sizeof(path), "%s/etc/eclipse/look", live);
    write_file(path, "tecno\n", 0644);
    snprintf(path, sizeof(path), "%s/etc/eclipse/desktop", root);
    write_file(path, "labwc\n", 0644);   /* lo que el usuario eligió */
    snprintf(src_dir, sizeof(src_dir), "%s/etc/eclipse", live);
    snprintf(dst_dir, sizeof(dst_dir), "%s/etc/eclipse", root);
    changed = 0;
    assert(refresh_dir(src_dir, dst_dir, NULL, 1, 0, 0, 0, &changed) == 0);
    assert(changed == 1);                 /* solo `look`, que faltaba */
    assert(strcmp(slurp(path), "labwc\n") == 0);  /* su elección intacta */

    /* === 4. refresh_dir recursivo: el árbol de /usr/local/bin === */
    snprintf(path, sizeof(path), "%s/usr/local/bin/eclipse-terminal", live);
    write_file(path, "term\n", 0755);
    snprintf(path, sizeof(path), "%s/usr/local/bin/hondo/profundo", live);
    write_file(path, "p\n", 0755);
    snprintf(src_dir, sizeof(src_dir), "%s/usr/local/bin", live);
    snprintf(dst_dir, sizeof(dst_dir), "%s/usr/local/bin", root);
    /* algo del usuario que no viene en el medio: no se borra */
    snprintf(path, sizeof(path), "%s/usr/local/bin/mio", root);
    write_file(path, "mio\n", 0755);
    changed = 0;
    assert(refresh_dir(src_dir, dst_dir, NULL, 0, 1, 0, 0, &changed) == 0);
    assert(changed == 2);
    snprintf(path2, sizeof(path2), "%s/usr/local/bin/hondo/profundo", root);
    assert(exists(path2) && mode_of(path2) == 0755);
    assert(exists(path));   /* lo del usuario sigue ahí */

    /* idempotencia: una segunda pasada copia lo mismo y no rompe nada */
    changed = 0;
    assert(refresh_dir(src_dir, dst_dir, NULL, 0, 1, 0, 0, &changed) == 0);
    assert(changed == 2);
    assert(strcmp(slurp(path2), "p\n") == 0);

    /* === 5. os_release_field === */
    snprintf(path, sizeof(path), "%s/etc/os-release", live);
    write_file(path, "NAME=\"Eclipse OS\"\nID=eclipse\nVERSION_ID=\"0.7.1\"\nBUILD_ID=abc1234\n", 0644);
    char v[64];
    assert(os_release_field(path, "VERSION_ID", v, sizeof(v)) == 0 && strcmp(v, "0.7.1") == 0);
    assert(os_release_field(path, "BUILD_ID", v, sizeof(v)) == 0 && strcmp(v, "abc1234") == 0);
    assert(os_release_field(path, "VARIANT_ID", v, sizeof(v)) != 0);
    assert(os_release_field("/no/existe", "VERSION_ID", v, sizeof(v)) != 0);

    /* === 6. repair_fstab_placeholders === */
    snprintf(path, sizeof(path), "%s/etc/fstab", root);
    write_file(path,
        "# /etc/fstab - generado por install-eclipse\n"
        "__ECLIPSE_ROOT_DEV__  /                  eclipsefs defaults        0  1\n"
        "__ECLIPSE_EFI_DEV___  /boot/efi          vfat    defaults,noatime  0  0\n"
        "__ECLIPSE_HOME_DEV__  /home              eclipsefs defaults        0  0\n"
        "servidor:/export  /mnt/nfs  nfs  defaults  0  0\n", 0644);
    assert(repair_fstab_placeholders(root, "/dev/sda2", "/dev/sda1", 0) == 0);
    char *fstab = slurp(path);
    assert(strstr(fstab, "__ECLIPSE_ROOT_DEV__") == NULL);
    assert(strstr(fstab, "__ECLIPSE_EFI_DEV___") == NULL);
    /* columnas intactas: el campo de dispositivo mide 20 y el punto de
     * montaje sigue empezando en la columna 23 */
    assert(strncmp(fstab + strlen("# /etc/fstab - generado por install-eclipse\n"),
                   "/dev/sda2             /                  eclipsefs", 50) == 0);
    assert(strstr(fstab, "/dev/sda1             /boot/efi          vfat") != NULL);
    assert(strstr(fstab, "#__ECLIPSE_HOME_DEV__  /home") != NULL);      /* comentada */
    assert(strstr(fstab, "servidor:/export") != NULL);           /* su línea intacta */
    /* idempotente: una segunda pasada no toca nada */
    char copia[8192];
    snprintf(copia, sizeof(copia), "%s", fstab);
    assert(repair_fstab_placeholders(root, "/dev/sda2", "/dev/sda1", 0) == 0);
    assert(strcmp(slurp(path), copia) == 0);

    /* un fstab sano no se reescribe */
    snprintf(path2, sizeof(path2), "%s/etc/fstab", live);
    write_file(path2, "/dev/sda2  /  eclipsefs  defaults  0  1\n", 0644);
    assert(repair_fstab_placeholders(live, "/dev/sdb2", "/dev/sdb1", 0) == 0);
    assert(strcmp(slurp(path2), "/dev/sda2  /  eclipsefs  defaults  0  1\n") == 0);

    /* === 7. fs_available_bytes y file_size_or_zero === */
    assert(fs_available_bytes("/tmp") > 0);
    assert(fs_available_bytes("/no/existe") == 0);
    assert(file_size_or_zero(path2) == strlen("/dev/sda2  /  eclipsefs  defaults  0  1\n"));
    assert(file_size_or_zero("/no/existe") == 0);

    /* === 8. refresh_installed_root: la tabla entera y su política ===
     * Raíces nuevas, para que lo que se mide sea esta pasada. */
    char live2[512], root2[512];
    snprintf(live2, sizeof(live2), "%s/live2", tmpl);
    snprintf(root2, sizeof(root2), "%s/root2", tmpl);
    snprintf(path, sizeof(path), "%s/usr/local/bin/eclipse-dbus-system", live2);
    write_file(path, "nuevo\n", 0755);
    snprintf(path, sizeof(path), "%s/sbin/eclipse-init", live2);
    write_file(path, "init nuevo\n", 0755);
    snprintf(path, sizeof(path), "%s/etc/eclipse/services/dbus-system.service", live2);
    write_file(path, "exec = /usr/local/bin/eclipse-dbus-system\n", 0644);
    snprintf(path, sizeof(path), "%s/etc/eclipse/desktop", live2);
    write_file(path, "abyss\n", 0644);
    snprintf(path, sizeof(path), "%s/etc/os-release", live2);
    write_file(path, "VERSION_ID=\"0.7.2\"\n", 0644);
    /* la raíz instalada: una versión vieja, su escritorio elegido y su /home */
    snprintf(path, sizeof(path), "%s/etc/eclipse/desktop", root2);
    write_file(path, "labwc\n", 0644);
    snprintf(path, sizeof(path), "%s/etc/os-release", root2);
    write_file(path, "VERSION_ID=\"0.6.9\"\n", 0644);
    snprintf(path, sizeof(path), "%s/home/moebius/apuntes.txt", root2);
    write_file(path, "mios\n", 0644);
    snprintf(path, sizeof(path), "%s/sbin/eclipse-init", root2);
    write_file(path, "init viejo\n", 0755);

    assert(refresh_installed_root(live2, root2, 0) == 0);
    /* el envoltorio que faltaba llega, con su bit x */
    snprintf(path, sizeof(path), "%s/usr/local/bin/eclipse-dbus-system", root2);
    assert(exists(path) && mode_of(path) == 0755);
    /* la fila del servicio llega */
    snprintf(path, sizeof(path), "%s/etc/eclipse/services/dbus-system.service", root2);
    assert(exists(path));
    /* PID 1 se renueva */
    snprintf(path, sizeof(path), "%s/sbin/eclipse-init", root2);
    assert(strcmp(slurp(path), "init nuevo\n") == 0);
    /* la versión instalada pasa a ser la del medio */
    snprintf(path, sizeof(path), "%s/etc/os-release", root2);
    assert(os_release_field(path, "VERSION_ID", v, sizeof(v)) == 0 && strcmp(v, "0.7.2") == 0);
    /* y lo que es del usuario NO se pisa */
    snprintf(path, sizeof(path), "%s/etc/eclipse/desktop", root2);
    assert(strcmp(slurp(path), "labwc\n") == 0);
    snprintf(path, sizeof(path), "%s/home/moebius/apuntes.txt", root2);
    assert(strcmp(slurp(path), "mios\n") == 0);
    /* dos veces seguidas deja lo mismo */
    assert(refresh_installed_root(live2, root2, 0) == 0);
    snprintf(path, sizeof(path), "%s/etc/eclipse/desktop", root2);
    assert(strcmp(slurp(path), "labwc\n") == 0);
    /* y en dry-run no escribe nada */
    snprintf(path, sizeof(path), "%s/root3", tmpl);
    assert(refresh_installed_root(live2, path, 1) == 0);
    snprintf(path2, sizeof(path2), "%s/root3/sbin/eclipse-init", tmpl);
    assert(!exists(path2));

    /* === 9. la tabla de rutas propias === */
    assert(ECLIPSE_OWNED_COUNT == 5);
    assert(ESP_LOADER_COUNT == 6);

    printf("TODO BIEN\n");
    return 0;
}
