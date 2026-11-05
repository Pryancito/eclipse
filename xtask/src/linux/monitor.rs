//! "Monitor del sistema" in the launcher: `eclipse-monitor` and its entry.

use std::fs;
use std::path::Path;

/// The wrapper, the `.desktop` row and nothing else: the keybind lives in
/// the labwc `rc.xml` next to the others.
///
/// Through a wrapper like every other entry: when the image has no lunarmon
/// the row still opens something useful, `top` in a terminal.
pub(super) fn write_monitor_desktop_entry(rootfs: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let wrapper = localbin.join("eclipse-monitor");
    fs::write(
        &wrapper,
        b"#!/bin/sh\n\
          # Eclipse OS: the system monitor (processes, load, init services).\n\
          # See tools/lunarbar/src/bin/lunarmon.rs.\n\
          if command -v lunarmon >/dev/null 2>&1; then\n\
          \x20 exec lunarmon \"$@\"\n\
          fi\n\
          exec /usr/local/bin/eclipse-terminal top\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();

    let dir = rootfs.join("root/.local/share/applications");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("lunarmon.desktop"),
        "[Desktop Entry]\n\
         Version=1.0\n\
         Type=Application\n\
         Name=System monitor\n\
         Name[es]=Monitor del sistema\n\
         GenericName=System Monitor\n\
         Comment=Applications, processes, CPU, memory, network, disk and the boot services\n\
         Comment[es]=Aplicaciones, procesos, CPU, memoria, red, disco y servicios del arranque\n\
         Exec=/usr/local/bin/eclipse-monitor\n\
         TryExec=/usr/local/bin/eclipse-monitor\n\
         Terminal=false\n\
         Icon=utilities-system-monitor\n\
         Categories=System;Monitor;\n\
         Keywords=monitor;procesos;process;cpu;memoria;memory;servicios;services;tareas;task;top;\n\
         StartupNotify=false\n",
    )
    .unwrap();
}
