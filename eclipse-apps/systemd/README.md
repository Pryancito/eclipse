# Eclipse SystemD v0.6.0

Sistema de inicialización moderno y completo para Eclipse OS que implementa funcionalidades avanzadas similares a systemd.

## Características Principales

### Arquitectura del Sistema
- Sistema modular: Arquitectura completamente modular con separación de responsabilidades
- Integración completa: Integración nativa con el kernel Eclipse OS
- Gestión de procesos: Monitoreo avanzado de procesos usando `/proc`
- Gestión de recursos: Control de CPU, memoria e I/O por servicio
- Sistema de notificaciones: Notificaciones en tiempo real entre servicios

### Gestión de Servicios
- Parser completo: Parser robusto de archivos `.service` estándar
- Validador avanzado: Validación completa de sintaxis y dependencias
- Estados del servicio: Estados completos (inactive, activating, active, deactivating, failed)
- Control de ciclo de vida: Inicio, parada, reinicio y recarga de servicios
- Manejo de señales: SIGTERM graceful shutdown con fallback a SIGKILL

### Sistema de Dependencias
- Resolución inteligente: Resolución automática de dependencias con detección de ciclos
- Tipos de dependencia: `Requires`, `Wants`, `After`, `Before`, `Conflicts`
- Orden de inicio: Ordenamiento topológico para inicio correcto
- Validación: Verificación automática de dependencias faltantes

### Monitoreo y Logging
- Journal estructurado: Sistema de logging con JSON estructurado
- Rotación automática: Rotación de archivos con compresión gzip
- Niveles de prioridad: Emergencia, Alerta, Crítico, Error, Warning, Notice, Info, Debug
- Búsqueda avanzada: Búsqueda y filtrado de logs por servicio y prioridad
- Compresión: Compresión automática con niveles configurables

### Sistema de Notificaciones
- Canales broadcast: Comunicación en tiempo real entre servicios
- Tipos de notificación: Ready, Reloading, Stopping, Error, Custom
- Historial: Historial completo de notificaciones con límites configurables
- Suscripción: Sistema de suscripción/desuscripción a canales

### Gestión de Recursos
- Monitoreo de CPU: Uso de CPU por proceso y sistema
- Monitoreo de memoria: RAM, cache y buffers del sistema
- Monitoreo de I/O: Operaciones de lectura/escritura por proceso
- Límites configurables: Límites de CPU, memoria e I/O por servicio
- Historial de uso: Historial temporal de uso de recursos

### Gestión de Targets
- Sistema de targets: Equivalente moderno a los runlevels tradicionales
- Dependencias: Resolución de dependencias entre targets
- Estados: Estados completos para targets (active, inactive, failed)
- Transiciones: Transiciones suaves entre targets

### Interfaz de Control
- systemctl: Interfaz de línea de comandos completa
- Comandos principales: start, stop, restart, reload, status, enable, disable
- Gestión de targets: set-default, get-default, isolate
- Monitoreo: list-units, list-services, show
- Ayuda integrada: Sistema de ayuda completo

## Instalación

### Compilación desde fuente
```bash
# Clonar el repositorio
cd eclipse-apps/systemd

# Compilar en modo release
cargo build --release

# Ejecutar pruebas
cargo test

# Verificar compilación
ls -la target/release/eclipse-systemd
```

### Instalación del sistema
```bash
# Instalar (requiere sudo)
sudo ./install_systemd.sh

# Verificar instalación
sudo service eclipse-systemd status
```

### Integración con kernel
```bash
# Compilar kernel con integración systemd
cd ../..
./eclipse_kernel/build_with_systemd.sh

# Ejecutar pruebas de integración
./test_systemd_integration.sh
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

- Completado Parser de archivos .service
- Completado Validador de sintaxis
- Completado Gestión básica de servicios
- Completado Sistema de logging
- Completado Script de instalación
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
