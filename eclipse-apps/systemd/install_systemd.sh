#!/bin/bash

# Script de instalación de Eclipse SystemD
# Instala el sistema systemd en Eclipse OS

set -e

echo "Iniciando Instalando Eclipse SystemD v0.1.0"
echo "====================================="

# Verificar prerrequisitos
echo "Verificando Verificando prerrequisitos..."
for cmd in cargo rustc; do
    if ! command -v "$cmd" &> /dev/null; then
        echo "Error Error: '$cmd' no encontrado. Por favor, instala Rust primero."
        echo "       Visita: https://rustup.rs/"
        exit 1
    fi
done
echo "Completado Prerrequisitos verificados"

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    echo "Error Error: Ejecutar desde el directorio systemd/"
    exit 1
fi

# Compilar en modo release
echo "Dependencias Compilando Eclipse SystemD..."
cargo build --release

if [ $? -ne 0 ]; then
    echo "Error Error al compilar Eclipse SystemD"
    exit 1
fi

echo "Completado Compilación exitosa"

# Verificar que el binario fue creado
if [ ! -f "target/release/eclipse-systemd" ]; then
    echo "Error Error: El binario no fue creado correctamente"
    exit 1
fi
echo "Completado Binario verificado"

# Crear directorios del sistema
echo "Directorio Creando directorios del sistema..."
sudo mkdir -p /sbin
sudo mkdir -p /etc/eclipse/systemd/system
sudo mkdir -p /var/log/eclipse
sudo mkdir -p /var/lib/eclipse-systemd

# Instalar ejecutable
echo "Aplicando Instalando ejecutable..."
sudo cp target/release/eclipse-systemd /sbin/eclipse-systemd
sudo chmod +x /sbin/eclipse-systemd

# Instalar archivos de configuración
echo "Configuracion Instalando archivos de configuración..."
if [ -d "../etc/eclipse/systemd/system" ]; then
    sudo cp ../etc/eclipse/systemd/system/*.service /etc/eclipse/systemd/system/ 2>/dev/null || true
    sudo cp ../etc/eclipse/systemd/system/*.target /etc/eclipse/systemd/system/ 2>/dev/null || true
    echo "Completado Archivos de configuración instalados"
else
    echo "Advertencia  Directorio de configuración no encontrado, se omiten archivos .service y .target"
fi

# Crear enlace simbólico para /sbin/init
echo "Integrando Creando enlace simbólico para /sbin/init..."
if [ -e "/sbin/init" ] && [ ! -L "/sbin/init" ]; then
    echo "Advertencia  /sbin/init existe y no es un enlace simbólico, creando respaldo..."
    sudo mv /sbin/init /sbin/init.backup
fi
sudo ln -sf /sbin/eclipse-systemd /sbin/init

# Crear usuario del sistema
echo "👤 Creando usuario del sistema..."
if ! id "eclipse" >/dev/null 2>&1; then
    sudo useradd -r -s /bin/false -d /var/lib/eclipse-systemd eclipse
fi

# Configurar permisos
echo "🔐 Configurando permisos..."
sudo chown -R eclipse:eclipse /var/lib/eclipse-systemd
sudo chmod 755 /sbin/eclipse-systemd
sudo chmod 644 /etc/eclipse/systemd/system/*.service
sudo chmod 644 /etc/eclipse/systemd/system/*.target

# Crear script de inicio
echo "Creando Creando script de inicio..."
sudo tee /etc/init.d/eclipse-systemd > /dev/null << 'EOF'
#!/bin/bash
### BEGIN INIT INFO
# Provides:          eclipse-systemd
# Required-Start:    $local_fs $network
# Required-Stop:     $local_fs $network
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: Eclipse SystemD
# Description:       Sistema de inicialización moderno para Eclipse OS
### END INIT INFO

case "$1" in
    start)
        echo "Iniciando Eclipse SystemD..."
        /sbin/eclipse-systemd &
        ;;
    stop)
        echo "Deteniendo Eclipse SystemD..."
        pkill -f eclipse-systemd
        ;;
    restart)
        $0 stop
        sleep 2
        $0 start
        ;;
    status)
        if pgrep -f eclipse-systemd > /dev/null; then
            echo "Eclipse SystemD está ejecutándose"
        else
            echo "Eclipse SystemD no está ejecutándose"
        fi
        ;;
    *)
        echo "Uso: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac

exit 0
EOF

sudo chmod +x /etc/init.d/eclipse-systemd

# Habilitar servicio
echo "Configurando  Habilitando servicio..."
if command -v update-rc.d &> /dev/null; then
    sudo update-rc.d eclipse-systemd defaults
elif command -v systemctl &> /dev/null; then
    echo "Advertencia  Detectado systemd nativo, la integración puede variar"
else
    echo "Advertencia  No se detectó gestor de init, puede requerir configuración manual"
fi

# Crear archivo de configuración del sistema
echo "Configurando  Creando configuración del sistema..."
sudo tee /etc/eclipse/systemd.conf > /dev/null << 'EOF'
# Configuración de Eclipse SystemD
[systemd]
# Directorio de archivos .service
service_dir = /etc/eclipse/systemd/system

# Directorio de logs
log_dir = /var/log/eclipse

# Usuario del sistema
system_user = eclipse

# Nivel de log
log_level = info

# Timeout de inicio de servicios (segundos)
service_timeout = 30

# Reiniciar servicios fallidos
restart_failed_services = true

# Monitorear servicios
monitor_services = true
EOF

# Probar instalación
echo "Probando Probando instalación..."
if [ -x "/sbin/eclipse-systemd" ]; then
    echo "Completado Eclipse SystemD instalado correctamente"
else
    echo "Advertencia  Eclipse SystemD instalado pero el ejecutable puede tener problemas de permisos"
fi

# Mostrar información de instalación
echo ""
echo "Completado INSTALACIÓN COMPLETADA"
echo "========================="
echo "Directorio Ejecutable: /sbin/eclipse-systemd"
echo "Directorio Configuración: /etc/eclipse/systemd/"
echo "Directorio Logs: /var/log/eclipse/"
echo "Directorio Datos: /var/lib/eclipse-systemd/"
echo ""
echo "Aplicando COMANDOS ÚTILES:"
echo "  Iniciar:     sudo service eclipse-systemd start"
echo "  Detener:     sudo service eclipse-systemd stop"
echo "  Reiniciar:   sudo service eclipse-systemd restart"
echo "  Estado:      sudo service eclipse-systemd status"
echo "  Ejecutar:    /sbin/eclipse-systemd"
echo ""
echo "Iniciando Eclipse OS está listo para el arranque moderno con systemd!"
