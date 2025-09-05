#!/bin/bash

# Script para reemplazar format! con strings estáticos

# Reemplazar en nvidia_control.rs
sed -i 's/format!("[^"]*{[^}]*}[^"]*")/"[STATIC_TEXT]"/g' src/gui/nvidia_control.rs
sed -i 's/&format!("[^"]*{[^}]*}[^"]*")/&"[STATIC_TEXT]"/g' src/gui/nvidia_control.rs

# Reemplazar en nvidia_benchmark.rs
sed -i 's/format!("[^"]*{[^}]*}[^"]*")/"[STATIC_TEXT]"/g' src/gui/nvidia_benchmark.rs
sed -i 's/&format!("[^"]*{[^}]*}[^"]*")/&"[STATIC_TEXT]"/g' src/gui/nvidia_benchmark.rs

echo "Reemplazos completados"
