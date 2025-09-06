# Eclipse SystemD v0.1.0

Sistema de inicialización moderno para Eclipse OS que implementa funcionalidades similares a systemd.

## Características

- ✅ **Parser de archivos .service**: Parsea archivos de configuración .service estándar
- ✅ **Validador de sintaxis**: Verifica la validez de los archivos .service
- ✅ **Gestión de servicios**: Inicia, detiene y monitorea servicios
- ✅ **Gestión de targets**: Maneja targets (equivalente a runlevels)
- ✅ **Sistema de logging**: Journal integrado para logs del sistema
- ✅ **Gestión de dependencias**: Resuelve dependencias entre servicios
- ✅ **Configuración flexible**: Archivos .service estándar de systemd

## Instalación

```bash
# Compilar
cargo build --release

# Instalar (requiere sudo)
./install_systemd.sh
```

## Uso

### Ejecutar Eclipse SystemD
```bash
# Ejecutar directamente
/sbin/eclipse-systemd

# Como servicio del sistema
sudo service eclipse-systemd start
sudo service eclipse-systemd stop
sudo service eclipse-systemd restart
sudo service eclipse-systemd status
```

### Archivos de configuración

Los archivos .service se encuentran en `/etc/eclipse/systemd/system/`:

- `eclipse-gui.service` - Interfaz gráfica de Eclipse OS
- `network.service` - Gestión de red
- `syslog.service` - Sistema de logging
- `eclipse-shell.service` - Terminal de Eclipse OS

### Targets disponibles

- `basic.target` - Sistema básico
- `multi-user.target` - Sistema multi-usuario
- `graphical.target` - Interfaz gráfica

## Estructura del proyecto

```
eclipse-apps/systemd/
├── src/
│   ├── main.rs              # Aplicación principal
│   └── service_parser.rs    # Parser de archivos .service
├── Cargo.toml               # Configuración del proyecto
├── install_systemd.sh       # Script de instalación
└── README.md               # Este archivo
```

## Archivos .service

Eclipse SystemD soporta archivos .service estándar con las siguientes secciones:

### [Unit]
- `Description` - Descripción del servicio
- `After` - Servicios que deben iniciarse antes
- `Requires` - Dependencias obligatorias
- `Wants` - Dependencias opcionales
- `Conflicts` - Servicios incompatibles

### [Service]
- `Type` - Tipo de servicio (simple, forking, oneshot, dbus, notify, idle)
- `ExecStart` - Comando de inicio
- `ExecReload` - Comando de recarga
- `Restart` - Política de reinicio
- `RestartSec` - Tiempo de espera antes de reiniciar
- `User` - Usuario del servicio
- `Group` - Grupo del servicio
- `WorkingDirectory` - Directorio de trabajo
- `Environment` - Variables de entorno

### [Install]
- `WantedBy` - Target que quiere este servicio
- `RequiredBy` - Target que requiere este servicio

## Ejemplo de archivo .service

```ini
[Unit]
Description=Eclipse OS Graphical User Interface
Documentation=https://eclipse-os.dev/gui
After=network.service
Wants=network.service
Requires=basic.target

[Service]
Type=notify
ExecStart=/sbin/eclipse-gui
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5
User=root
Group=root
WorkingDirectory=/
Environment=DISPLAY=:0
Environment=XDG_SESSION_TYPE=wayland

[Install]
WantedBy=graphical.target
```

## Desarrollo

### Compilar
```bash
cargo build
cargo build --release
```

### Ejecutar
```bash
cargo run
```

### Tests
```bash
cargo test
```

## Dependencias

- `anyhow` - Manejo de errores
- `log` - Sistema de logging
- `env_logger` - Logger para entorno
- `serde` - Serialización
- `tokio` - Runtime asíncrono
- `chrono` - Manejo de fechas y tiempos
- `uuid` - Generación de UUIDs

## Integración con Eclipse OS

Eclipse SystemD está diseñado para integrarse con el kernel Eclipse:

1. **Arranque**: El kernel ejecuta `/sbin/init` (enlace a eclipse-systemd)
2. **Servicios**: Carga y ejecuta servicios desde archivos .service
3. **Targets**: Inicia el target apropiado (multi-user o graphical)
4. **Monitoreo**: Monitorea servicios y los reinicia si fallan

## Estado del proyecto

- ✅ Parser de archivos .service
- ✅ Validador de sintaxis
- ✅ Gestión básica de servicios
- ✅ Sistema de logging
- ✅ Script de instalación
- 🚧 Integración con kernel (en progreso)
- ⏳ Gestión avanzada de dependencias
- ⏳ Sistema de notificaciones
- ⏳ Gestión de recursos

## Contribuir

1. Fork el proyecto
2. Crea una rama para tu feature
3. Commit tus cambios
4. Push a la rama
5. Abre un Pull Request

## Licencia

Este proyecto está bajo la licencia MIT. Ver `LICENSE` para más detalles.

## Soporte

Para soporte y preguntas:
- GitHub Issues: [eclipse-os/issues](https://github.com/eclipse-os/issues)
- Documentación: [eclipse-os.dev](https://eclipse-os.dev)
- Email: support@eclipse-os.dev
