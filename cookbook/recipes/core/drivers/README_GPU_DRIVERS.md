# 🎮 GPU Drivers - NVIDIA, AMD & Intel

## Drivers Incluidos

Este paquete incluye drivers de gráficos para las tres principales familias de GPUs:

### 1. nvidiad
**Driver para GPUs NVIDIA**
- **Ubicación**: `graphics/nvidiad/`
- **Binario**: `/usr/lib/drivers/nvidiad`
- **Config PCI**: `/etc/pcid.d/nvidiad.toml`
- **Display**: `display.nvidia`

### 2. amdd
**Driver para GPUs AMD**
- **Ubicación**: `graphics/amdd/`
- **Binario**: `/usr/lib/drivers/amdd`
- **Config PCI**: `/etc/pcid.d/amdd.toml`
- **Display**: `display.amd`

### 3. inteld
**Driver para GPUs Intel**
- **Ubicación**: `graphics/inteld/`
- **Binario**: `/usr/lib/drivers/inteld`
- **Config PCI**: `/etc/pcid.d/inteld.toml`
- **Display**: `display.intel`

### 4. multi-gpud
**Gestor Multi-GPU**
- **Ubicación**: `graphics/multi-gpud/`
- **Binario**: `/usr/bin/multi-gpud`
- **Script init**: `/usr/lib/init.d/01_multigpu`

## Compilación

Los drivers se compilan automáticamente con el paquete `drivers`:

```bash
cd cookbook
./cook.sh drivers
```

## Instalación

Después de compilar, los archivos se instalan en:

```
/usr/lib/drivers/
├── nvidiad          # Driver NVIDIA
├── amdd             # Driver AMD
└── inteld           # Driver Intel

/usr/bin/
└── multi-gpud       # Gestor multi-GPU

/etc/pcid.d/
├── nvidiad.toml     # Config detección NVIDIA
├── amdd.toml        # Config detección AMD
└── inteld.toml      # Config detección Intel

/usr/lib/init.d/
└── 01_multigpu      # Script de inicialización
```

## Funcionamiento

### 1. Arranque del Sistema
```
init.rc → pcid-spawner /etc/pcid.d/
```

### 2. Detección Automática
`pcid-spawner` lee las configuraciones en `/etc/pcid.d/` y lanza el driver apropiado cuando detecta una GPU compatible:

- **NVIDIA** (Vendor `0x10DE`) → lanza `nvidiad`
- **AMD** (Vendor `0x1002`) → lanza `amdd`
- **Intel** (Vendor `0x8086`) → lanza `inteld`

### 3. Inicialización Multi-GPU
Después de que los drivers individuales se hayan cargado, el script `01_multigpu` lanza el gestor:

```bash
multi-gpud &
```

### 4. Coordinación
`multi-gpud` detecta todas las GPUs activas y genera `/etc/multigpu.conf` con la configuración del sistema.

## Soporte Multi-GPU

El sistema soporta hasta **4 GPUs** funcionando simultáneamente de cualquier combinación:

### Ejemplos de Configuraciones

**Workstation**:
- 2x NVIDIA RTX 4090
- 1x AMD RX 7900 XTX
- 1x Intel UHD 770

**Gaming + Streaming**:
- 1x AMD RX 7900 XTX (gaming)
- 1x NVIDIA RTX 3060 (encoding)
- 1x Intel Arc A750 (display)

**Data Center**:
- 4x NVIDIA A100

## GPUs Soportadas

Ver `graphics/README_MULTI_GPU.md` para la lista completa de más de 100 modelos soportados.

### NVIDIA
Kepler, Maxwell, Pascal, Volta, Turing, Ampere, Ada Lovelace
(GTX 600+ hasta RTX 40 series)

### AMD
GCN, Polaris, Vega, RDNA 1/2/3
(R7/R9 hasta RX 7000 series)

### Intel
Gen7-12/Xe, Arc
(HD 4000 hasta Arc A770)

## Troubleshooting

### GPU no detectada
```bash
# Ver dispositivos PCI
lspci -nn | grep -i vga

# Ver logs
dmesg | grep -E "(nvidia|amd|intel).*"
```

### Driver no carga
```bash
# Verificar binario
ls -l /usr/lib/drivers/{nvidiad,amdd,inteld}

# Verificar configuración
cat /etc/pcid.d/nvidiad.toml
```

### Multi-GPU no funciona
```bash
# Ver estado del gestor
ps aux | grep multi-gpud

# Ver configuración generada
cat /etc/multigpu.conf
```

## Documentación Completa

- **Técnica**: `graphics/README_MULTI_GPU.md`
- **Resumen**: `/SISTEMA_MULTI_GPU.md`

## Desarrollo

Para agregar soporte para una nueva GPU, edita los archivos correspondientes en:
- `graphics/nvidiad/src/nvidia.rs` (NVIDIA)
- `graphics/amdd/src/amd.rs` (AMD)
- `graphics/inteld/src/intel.rs` (Intel)

Agrega el device ID en las funciones `get_gpu_name()` y `detect_architecture()`.

