#!/bin/bash

# Script de instalación de Eclipse SystemD
# Instala el sistema systemd en Eclipse OS

set -e

echo "🚀 Instalando Eclipse SystemD v0.1.0"
echo "====================================="

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    echo "❌ Error: Ejecutar desde el directorio systemd/"
    exit 1
fi

# Compilar en modo release
echo "📦 Compilando Eclipse SystemD..."
cargo build --release

if [ $? -ne 0 ]; then
    echo "❌ Error al compilar Eclipse SystemD"
    exit 1
fi

echo "✅ Compilación exitosa"

# Crear directorios del sistema
echo "📁 Creando directorios del sistema..."
sudo mkdir -p /sbin
sudo mkdir -p /etc/eclipse/systemd/system
sudo mkdir -p /var/log/eclipse
sudo mkdir -p /var/lib/eclipse-systemd

# Instalar ejecutable
echo "🔧 Instalando ejecutable..."
sudo cp target/release/eclipse-systemd /sbin/eclipse-systemd
sudo chmod +x /sbin/eclipse-systemd

# Instalar archivos de configuración
echo "📋 Instalando archivos de configuración..."
sudo cp ../etc/eclipse/systemd/system/*.service /etc/eclipse/systemd/system/
sudo cp ../etc/eclipse/systemd/system/*.target /etc/eclipse/systemd/system/

# Crear enlace simbólico para /sbin/init
echo "🔗 Creando enlace simbólico para /sbin/init..."
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
echo "📜 Creando script de inicio..."
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
echo "⚙️  Habilitando servicio..."
sudo update-rc.d eclipse-systemd defaults

# Crear archivo de configuración del sistema
echo "⚙️  Creando configuración del sistema..."
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
echo "🧪 Probando instalación..."
if /sbin/eclipse-systemd --version > /dev/null 2>&1; then
    echo "✅ Eclipse SystemD instalado correctamente"
else
    echo "⚠️  Eclipse SystemD instalado pero no responde a --version"
fi

# Mostrar información de instalación
echo ""
echo "🎉 INSTALACIÓN COMPLETADA"
echo "========================="
echo "📁 Ejecutable: /sbin/eclipse-systemd"
echo "📁 Configuración: /etc/eclipse/systemd/"
echo "📁 Logs: /var/log/eclipse/"
echo "📁 Datos: /var/lib/eclipse-systemd/"
echo ""
echo "🔧 COMANDOS ÚTILES:"
echo "  Iniciar:     sudo service eclipse-systemd start"
echo "  Detener:     sudo service eclipse-systemd stop"
echo "  Reiniciar:   sudo service eclipse-systemd restart"
echo "  Estado:      sudo service eclipse-systemd status"
echo "  Ejecutar:    /sbin/eclipse-systemd"
echo ""
echo "🚀 Eclipse OS está listo para el arranque moderno con systemd!"
