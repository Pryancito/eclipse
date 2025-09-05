#!/bin/bash

# Script para limpiar warnings del kernel Eclipse
echo "🧹 Limpiando warnings del kernel Eclipse..."

# Agregar #[allow(dead_code)] a los módulos de aplicaciones
echo "📱 Limpiando módulos de aplicaciones..."

# Shell
sed -i '1i\#![allow(dead_code)]' src/apps/shell.rs

# File Manager
sed -i '1i\#![allow(dead_code)]' src/apps/file_manager.rs

# System Info
sed -i '1i\#![allow(dead_code)]' src/apps/system_info.rs

# Text Editor
sed -i '1i\#![allow(dead_code)]' src/apps/text_editor.rs

# Calculator
sed -i '1i\#![allow(dead_code)]' src/apps/calculator.rs

# Task Manager
sed -i '1i\#![allow(dead_code)]' src/apps/task_manager.rs

# Debug Hardware
sed -i '1i\#![allow(dead_code)]' src/debug_hardware.rs

# Hardware Safe
sed -i '1i\#![allow(dead_code)]' src/hardware_safe.rs

echo "✅ Warnings de aplicaciones limpiados"

# Agregar #[allow(dead_code)] a módulos de UI
echo "🖥️ Limpiando módulos de UI..."

# Window
sed -i '1i\#![allow(dead_code)]' src/ui/window.rs

# Event
sed -i '1i\#![allow(dead_code)]' src/ui/event.rs

# Graphics
sed -i '1i\#![allow(dead_code)]' src/ui/graphics.rs

# Terminal
sed -i '1i\#![allow(dead_code)]' src/ui/terminal.rs

# Compositor
sed -i '1i\#![allow(dead_code)]' src/ui/compositor.rs

# Widget
sed -i '1i\#![allow(dead_code)]' src/ui/widget.rs

echo "✅ Warnings de UI limpiados"

# Agregar #[allow(dead_code)] a módulos de seguridad
echo "🔒 Limpiando módulos de seguridad..."

# Permissions
sed -i '1i\#![allow(dead_code)]' src/security/permissions.rs

# Authentication
sed -i '1i\#![allow(dead_code)]' src/security/authentication.rs

# Encryption
sed -i '1i\#![allow(dead_code)]' src/security/encryption.rs

# Access Control
sed -i '1i\#![allow(dead_code)]' src/security/access_control.rs

# Audit
sed -i '1i\#![allow(dead_code)]' src/security/audit.rs

# Memory Protection
sed -i '1i\#![allow(dead_code)]' src/security/memory_protection.rs

# Sandbox
sed -i '1i\#![allow(dead_code)]' src/security/sandbox.rs

echo "✅ Warnings de seguridad limpiados"

# Agregar #[allow(dead_code)] a módulos de IA
echo "🤖 Limpiando módulos de IA..."

# AI Advanced
sed -i '1i\#![allow(dead_code)]' src/ai_advanced.rs

# AI Optimizer
sed -i '1i\#![allow(dead_code)]' src/ai_optimizer.rs

# AI Learning
sed -i '1i\#![allow(dead_code)]' src/ai_learning.rs

echo "✅ Warnings de IA limpiados"

# Agregar #[allow(dead_code)] a otros módulos
echo "🔧 Limpiando otros módulos..."

# Monitoring
sed -i '1i\#![allow(dead_code)]' src/monitoring.rs

# Customization
sed -i '1i\#![allow(dead_code)]' src/customization.rs

# Interrupts Manager
sed -i '1i\#![allow(dead_code)]' src/interrupts/manager.rs

echo "✅ Otros warnings limpiados"

echo "🎉 Limpieza de warnings completada!"
echo "🔍 Verificando compilación..."

# Verificar compilación
cargo check

echo "✅ Script de limpieza completado"