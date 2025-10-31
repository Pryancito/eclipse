#!/bin/bash

# Script de compilación del kernel Eclipse OS con integración systemd
# Compila el kernel y prepara la integración con eclipse-systemd

set -e

echo "🔗 COMPILANDO KERNEL ECLIPSE OS CON INTEGRACIÓN SYSTEMD"
echo "======================================================"
echo ""

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    echo "❌ Error: Ejecutar desde el directorio eclipse_kernel/"
    exit 1
fi

# Verificar que eclipse-systemd existe
if [ ! -f "../eclipse-apps/systemd/target/release/eclipse-systemd" ]; then
    echo "❌ Error: eclipse-systemd no encontrado"
    echo "   Compila primero: cd ../eclipse-apps/systemd && cargo build --release"
    exit 1
fi

echo "✅ eclipse-systemd encontrado"

# Crear directorio de salida
mkdir -p target/systemd-integration

echo "🔧 Compilando kernel con integración systemd..."

# Compilar el kernel
cargo build --release --target x86_64-unknown-none

if [ $? -ne 0 ]; then
    echo "❌ Error al compilar el kernel"
    exit 1
fi

echo "✅ Kernel compilado correctamente"

# Copiar eclipse-systemd al directorio de salida
echo "📋 Preparando integración con systemd..."
cp ../eclipse-apps/systemd/target/release/eclipse-systemd target/systemd-integration/

# Crear enlace simbólico para /sbin/init
echo "🔗 Creando enlace simbólico /sbin/init..."
ln -sf eclipse-systemd target/systemd-integration/init

# Crear directorio de configuración
mkdir -p target/systemd-integration/etc/eclipse/systemd/system

# Copiar archivos de configuración
echo "📋 Copiando archivos de configuración..."
cp -r ../etc/eclipse/systemd/system/* target/systemd-integration/etc/eclipse/systemd/system/ 2>/dev/null || true

# Crear script de arranque
echo "📜 Creando script de arranque..."
cat > target/systemd-integration/start_eclipse_os.sh << 'EOF'
#!/bin/bash

# Script de arranque de Eclipse OS con systemd
echo "🚀 Iniciando Eclipse OS con systemd..."

# Verificar que eclipse-systemd existe
if [ ! -f "./eclipse-systemd" ]; then
    echo "❌ Error: eclipse-systemd no encontrado"
    exit 1
fi

# Crear directorios necesarios
mkdir -p /sbin /bin /etc /var/log

# Crear enlace simbólico
ln -sf $(pwd)/eclipse-systemd /sbin/init
ln -sf $(pwd)/eclipse-systemd /sbin/eclipse-systemd

# Configurar variables de entorno
export PATH="/sbin:/bin"
export HOME="/root"
export USER="root"
export SHELL="/bin/eclipse-shell"
export TERM="xterm-256color"
export DISPLAY=":0"
export XDG_SESSION_TYPE="wayland"
export XDG_SESSION_DESKTOP="eclipse"
export XDG_CURRENT_DESKTOP="Eclipse:GNOME"

# Ejecutar eclipse-systemd como PID 1
echo "🔗 Ejecutando eclipse-systemd como PID 1..."
exec ./eclipse-systemd
EOF

chmod +x target/systemd-integration/start_eclipse_os.sh

# Crear README de integración
echo "📚 Creando documentación de integración..."
cat > target/systemd-integration/README.md << 'EOF'
# Eclipse OS con Integración SystemD

Este directorio contiene la integración completa del kernel Eclipse OS con eclipse-systemd.

## Archivos incluidos

- `eclipse-systemd`: Ejecutable de eclipse-systemd
- `init`: Enlace simbólico a eclipse-systemd (para /sbin/init)
- `start_eclipse_os.sh`: Script de arranque
- `etc/eclipse/systemd/system/`: Archivos de configuración .service

## Uso

1. Ejecutar el script de arranque:
   ```bash
   ./start_eclipse_os.sh
   ```

2. O ejecutar directamente:
   ```bash
   ./eclipse-systemd
   ```

## Integración con el kernel

El kernel Eclipse OS está configurado para:
1. Inicializar el sistema básico
2. Crear el enlace simbólico /sbin/init -> eclipse-systemd
3. Transferir control a eclipse-systemd como PID 1
4. eclipse-systemd maneja todos los servicios del sistema

## Servicios disponibles

- eclipse-gui.service: Interfaz gráfica
- network.service: Gestión de red
- syslog.service: Sistema de logging
- eclipse-shell.service: Terminal

## Configuración

Los archivos de configuración están en `etc/eclipse/systemd/system/`.
Puedes modificar los servicios editando estos archivos .service.
EOF

echo ""
echo "🎉 INTEGRACIÓN COMPLETADA"
echo "========================"
echo ""
echo "📁 Archivos generados en: target/systemd-integration/"
echo "   - eclipse-systemd: Ejecutable principal"
echo "   - init: Enlace simbólico para /sbin/init"
echo "   - start_eclipse_os.sh: Script de arranque"
echo "   - etc/: Archivos de configuración"
echo "   - README.md: Documentación"
echo ""
echo "🚀 Para probar la integración:"
echo "   cd target/systemd-integration"
echo "   ./start_eclipse_os.sh"
echo ""
echo "✅ Eclipse OS con systemd está listo para usar!"
