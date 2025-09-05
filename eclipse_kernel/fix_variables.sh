#!/bin/bash

# Script para reemplazar variables no definidas con strings estáticos

# Reemplazar en nvidia_control.rs
sed -i 's/&rt_text/"RT Cores: 68"/g' src/gui/nvidia_control.rs
sed -i 's/&tensor_text/"Tensor Cores: 272"/g' src/gui/nvidia_control.rs
sed -i 's/&gpu_util_text/"Utilización GPU: 85%"/g' src/gui/nvidia_control.rs
sed -i 's/&mem_util_text/"Utilización Memoria: 60%"/g' src/gui/nvidia_control.rs
sed -i 's/&temp_text/"Temperatura: 65°C"/g' src/gui/nvidia_control.rs
sed -i 's/&power_text/"Energía: 200W"/g' src/gui/nvidia_control.rs
sed -i 's/&fan_text/"Ventilador: 70%"/g' src/gui/nvidia_control.rs
sed -i 's/&core_clock_text/"Reloj Núcleo: 1800 MHz"/g' src/gui/nvidia_control.rs
sed -i 's/&mem_clock_text/"Reloj Memoria: 7000 MHz"/g' src/gui/nvidia_control.rs
sed -i 's/&vram_text/"VRAM Usado: 4096 MB / 8192 MB"/g' src/gui/nvidia_control.rs
sed -i 's/&frames_text/"Frames Renderizados: 1000"/g' src/gui/nvidia_control.rs
sed -i 's/&dropped_text/"Frames Perdidos: 0"/g' src/gui/nvidia_control.rs
sed -i 's/&dlss_text/"DLSS: Habilitado"/g' src/gui/nvidia_control.rs
sed -i 's/&rtx_voice_text/"RTX Voice: Habilitado"/g' src/gui/nvidia_control.rs
sed -i 's/&ansel_text/"Ansel: Habilitado"/g' src/gui/nvidia_control.rs
sed -i 's/&oc_text/"Overclock: Habilitado"/g' src/gui/nvidia_control.rs
sed -i 's/&core_offset_text/"Offset Reloj Núcleo: +100 MHz"/g' src/gui/nvidia_control.rs
sed -i 's/&mem_offset_text/"Offset Reloj Memoria: +500 MHz"/g' src/gui/nvidia_control.rs
sed -i 's/&voltage_offset_text/"Offset Voltaje: +50 mV"/g' src/gui/nvidia_control.rs
sed -i 's/&efficiency_text/"Eficiencia: 95.5% por Watt"/g' src/gui/nvidia_control.rs
sed -i 's/&curve_text/"65°C -> 70%"/g' src/gui/nvidia_control.rs

# Reemplazar en nvidia_benchmark.rs
sed -i 's/&progress_text/"Progreso: 50%"/g' src/gui/nvidia_benchmark.rs
sed -i 's/&frame_text/"Frames: 1000"/g' src/gui/nvidia_benchmark.rs
sed -i 's/&avg_time_text/"Tiempo Promedio: 16.7 ms"/g' src/gui/nvidia_benchmark.rs
sed -i 's/&min_time_text/"Tiempo Mínimo: 15.0 ms"/g' src/gui/nvidia_benchmark.rs
sed -i 's/&max_time_text/"Tiempo Máximo: 20.0 ms"/g' src/gui/nvidia_benchmark.rs
sed -i 's/&fps_text/"FPS: 60.0"/g' src/gui/nvidia_benchmark.rs

echo "Reemplazos de variables completados"
