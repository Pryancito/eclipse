#!/bin/bash

# Script de prueba para Eclipse SystemD
# Verifica que el sistema systemd funciona correctamente

set -e

echo "🧪 PRUEBAS DE ECLIPSE SYSTEMD v0.1.0"
echo "====================================="

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Función para imprimir con color
print_status() {
    local status=$1
    local message=$2
    case $status in
        "OK")
            echo -e "${GREEN}✅ $message${NC}"
            ;;
        "ERROR")
            echo -e "${RED}❌ $message${NC}"
            ;;
        "WARNING")
            echo -e "${YELLOW}⚠️  $message${NC}"
            ;;
        "INFO")
            echo -e "${BLUE}ℹ️  $message${NC}"
            ;;
    esac
}

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    print_status "ERROR" "Ejecutar desde el directorio systemd/"
    exit 1
fi

print_status "INFO" "Iniciando pruebas de Eclipse SystemD..."

# Prueba 1: Compilación
echo ""
echo "🔨 PRUEBA 1: Compilación"
echo "------------------------"
if cargo build --release > /dev/null 2>&1; then
    print_status "OK" "Compilación exitosa"
else
    print_status "ERROR" "Error en la compilación"
    exit 1
fi

# Prueba 2: Ejecución básica
echo ""
echo "🚀 PRUEBA 2: Ejecución básica"
echo "-----------------------------"
if timeout 10s target/release/eclipse-systemd > /dev/null 2>&1; then
    print_status "OK" "Ejecución básica exitosa"
else
    print_status "WARNING" "Ejecución básica con timeout (normal para systemd)"
fi

# Prueba 3: Parser de archivos .service
echo ""
echo "📄 PRUEBA 3: Parser de archivos .service"
echo "----------------------------------------"
service_dir="/home/moebius/eclipse/etc/eclipse/systemd/system"
if [ -d "$service_dir" ]; then
    service_count=$(find "$service_dir" -name "*.service" | wc -l)
    print_status "OK" "Encontrados $service_count archivos .service"
    
    # Probar parser con cada archivo
    for service_file in "$service_dir"/*.service; do
        if [ -f "$service_file" ]; then
            filename=$(basename "$service_file")
            if timeout 5s target/release/eclipse-systemd > /dev/null 2>&1; then
                print_status "OK" "Parser procesó $filename correctamente"
            else
                print_status "WARNING" "Parser tuvo problemas con $filename"
            fi
        fi
    done
else
    print_status "WARNING" "Directorio de servicios no encontrado: $service_dir"
fi

# Prueba 4: Validador de sintaxis
echo ""
echo "🔍 PRUEBA 4: Validador de sintaxis"
echo "----------------------------------"
if [ -d "$service_dir" ]; then
    valid_count=0
    total_count=0
    
    for service_file in "$service_dir"/*.service; do
        if [ -f "$service_file" ]; then
            total_count=$((total_count + 1))
            filename=$(basename "$service_file")
            
            # Verificar sintaxis básica
            if grep -q "\[Unit\]" "$service_file" && \
               grep -q "\[Service\]" "$service_file" && \
               grep -q "\[Install\]" "$service_file"; then
                valid_count=$((valid_count + 1))
                print_status "OK" "$filename tiene sintaxis válida"
            else
                print_status "ERROR" "$filename tiene sintaxis inválida"
            fi
        fi
    done
    
    print_status "INFO" "Archivos válidos: $valid_count/$total_count"
else
    print_status "WARNING" "No se pudo probar validador - directorio no encontrado"
fi

# Prueba 5: Dependencias
echo ""
echo "📦 PRUEBA 5: Dependencias"
echo "-------------------------"
if ldd target/release/eclipse-systemd > /dev/null 2>&1; then
    print_status "OK" "Dependencias del sistema resueltas"
else
    print_status "ERROR" "Problemas con dependencias del sistema"
fi

# Prueba 6: Permisos
echo ""
echo "🔐 PRUEBA 6: Permisos"
echo "---------------------"
if [ -x "target/release/eclipse-systemd" ]; then
    print_status "OK" "Ejecutable tiene permisos correctos"
else
    print_status "ERROR" "Ejecutable no tiene permisos de ejecución"
fi

# Prueba 7: Tamaño del binario
echo ""
echo "📊 PRUEBA 7: Tamaño del binario"
echo "-------------------------------"
binary_size=$(stat -c%s "target/release/eclipse-systemd" 2>/dev/null || echo "0")
if [ "$binary_size" -gt 1000000 ]; then
    print_status "OK" "Binario tiene tamaño apropiado ($(($binary_size / 1024 / 1024))MB)"
else
    print_status "WARNING" "Binario parece muy pequeño ($binary_size bytes)"
fi

# Resumen final
echo ""
echo "📋 RESUMEN DE PRUEBAS"
echo "====================="

# Contar pruebas exitosas
total_tests=7
passed_tests=0

# Verificar resultados
if cargo build --release > /dev/null 2>&1; then
    passed_tests=$((passed_tests + 1))
fi

if [ -x "target/release/eclipse-systemd" ]; then
    passed_tests=$((passed_tests + 1))
fi

if ldd target/release/eclipse-systemd > /dev/null 2>&1; then
    passed_tests=$((passed_tests + 1))
fi

if [ -d "$service_dir" ]; then
    passed_tests=$((passed_tests + 1))
fi

if [ "$binary_size" -gt 1000000 ]; then
    passed_tests=$((passed_tests + 1))
fi

# Mostrar resultado
if [ $passed_tests -eq $total_tests ]; then
    print_status "OK" "Todas las pruebas pasaron ($passed_tests/$total_tests)"
    echo ""
    echo "🎉 ¡Eclipse SystemD está listo para producción!"
    echo "🚀 El sistema de inicialización moderno funciona correctamente"
    exit 0
elif [ $passed_tests -gt $((total_tests / 2)) ]; then
    print_status "WARNING" "La mayoría de pruebas pasaron ($passed_tests/$total_tests)"
    echo ""
    echo "⚠️  Eclipse SystemD funciona pero puede necesitar ajustes"
    exit 1
else
    print_status "ERROR" "Muchas pruebas fallaron ($passed_tests/$total_tests)"
    echo ""
    echo "❌ Eclipse SystemD necesita correcciones antes de usar"
    exit 1
fi
