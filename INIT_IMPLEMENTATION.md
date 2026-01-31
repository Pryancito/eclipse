# Implementación: Monte de EclipseFS y Lanzamiento de Init

## Objetivo
Hacer que el kernel monte el sistema de archivos eclipsefs y lance una instancia de /sbin/eclipse-systemd en vez de lanzar el proceso de prueba.

## Lo que se implementó

### 1. Proceso Init en Userspace
Se creó un nuevo proceso init (`eclipse-init`) en `eclipse_kernel/userspace/init/`:

- **Lenguaje**: Rust no_std (sin biblioteca estándar)
- **Tamaño**: 11 KB (binario estático ELF)
- **Características**:
  - Primer proceso de userspace que inicia el kernel
  - Imprime mensajes sobre el estado del sistema
  - Incluye TODOs para montaje de eclipsefs y gestión de servicios
  - Loop principal con heartbeat para demostrar que está operativo

### 2. Modificaciones al Kernel
El archivo `eclipse_kernel/src/main.rs` fue modificado para:

- **Eliminar** el proceso de prueba (`test_process`)
- **Embeber** el binario init usando `include_bytes!`
- **Cargar** el init usando el ELF loader existente
- **Agregar** mensajes y TODOs sobre montaje de eclipsefs

## Estado Actual

### ✅ Completado
- [x] Proceso init funcional en userspace
- [x] Kernel carga init en lugar del proceso de prueba
- [x] Sistema puede compilar y arrancar
- [x] Documentación clara de próximos pasos

### ⏳ Pendiente (TODOs marcados en el código)

#### Montaje de EclipseFS
Para implementar el montaje real de eclipsefs, se necesita:

1. **Driver de disco VirtIO**
   - Implementar driver VirtIO block device en el kernel
   - Permitir lectura/escritura de bloques del disco

2. **Integración de eclipsefs-lib**
   - La librería ya existe en modo no_std
   - Agregar al kernel como dependencia
   - Implementar interfaz de montaje

3. **Servidor de Filesystem**
   - Según arquitectura microkernel, el FS debería estar en userspace
   - Mover funcionalidad de FS del kernel al servidor

#### Lanzamiento de eclipse-systemd
El eclipse-systemd actual es un binario Linux completo con:
- Dependencias dinámicas (tokio, std, etc.)
- Necesita dynamic linker (/lib64/ld-linux-x86-64.so.2)
- Requiere syscalls de Linux

Para ejecutarlo se necesita:

1. **Opción A: Port a no_std**
   - Reescribir systemd sin std
   - Usar solo las syscalls del microkernel
   - Significativo trabajo de desarrollo

2. **Opción B: Capa de compatibilidad Linux**
   - Implementar dynamic linker
   - Implementar syscalls de Linux
   - Agregar libc
   - Proyecto muy grande

3. **Opción C: Init nativo (actual)**
   - Usar eclipse-init como init permanente
   - Expandirlo para gestionar servicios nativos
   - Más apropiado para microkernel

## Arquitectura Implementada

```
┌─────────────────────────────────────┐
│         Bootloader (UEFI)           │
│  - Carga kernel en memoria          │
│  - Inicia modo protegido            │
└────────────┬────────────────────────┘
             │
             ▼
┌─────────────────────────────────────┐
│      Eclipse Microkernel            │
│  - Gestión de memoria               │
│  - Interrupciones e IDT             │
│  - IPC entre procesos               │
│  - Scheduler                        │
│  - Syscalls básicos                 │
│  - ELF Loader                       │
│  - [TODO] Driver VirtIO Block       │
│  - [TODO] Montaje EclipseFS         │
└────────────┬────────────────────────┘
             │ Carga binario embebido
             ▼
┌─────────────────────────────────────┐
│       eclipse-init (Userspace)      │
│  - Primer proceso (PID 1)           │
│  - [TODO] Monta eclipsefs           │
│  - [TODO] Inicia servicios          │
│  - Loop principal del sistema       │
└─────────────────────────────────────┘
```

## Archivos Modificados/Creados

### Nuevos
- `eclipse_kernel/userspace/init/src/main.rs` - Código del init
- `eclipse_kernel/userspace/init/Cargo.toml` - Configuración del proyecto
- `eclipse_kernel/userspace/init/.cargo/config.toml` - Configuración de build
- `eclipse_kernel/userspace/init/linker.ld` - Script del linker
- `eclipse_kernel/userspace/init/userspace_linker.ld` - Script alternativo

### Modificados
- `eclipse_kernel/src/main.rs` - Cambio de test_process a init loader

## Próximos Pasos Recomendados

### Corto Plazo
1. Implementar driver VirtIO block device básico
2. Agregar eclipsefs-lib al kernel (ya disponible en no_std)
3. Implementar montaje básico del sistema de archivos raíz

### Medio Plazo
1. Mover funcionalidad de filesystem a servidor userspace
2. Expandir eclipse-init para gestionar servicios básicos
3. Implementar carga de binarios desde filesystem montado

### Largo Plazo
1. Decidir entre init nativo vs. compatibilidad Linux
2. Si se elige compatibilidad: implementar capa de syscalls Linux
3. Si se elige nativo: desarrollar gestor de servicios propio

## Cómo Probar

```bash
# Compilar el sistema completo
cd /home/runner/work/eclipse/eclipse
./build.sh

# O compilar solo las partes modificadas:

# 1. Compilar init
cd eclipse_kernel/userspace/init
cargo +nightly build --release

# 2. Compilar kernel (incluye init embebido)
cd ../../
cargo +nightly build --release --target x86_64-unknown-none

# 3. Compilar bootloader
cd ../bootloader-uefi
cargo +nightly build --release --target x86_64-unknown-uefi

# 4. Ejecutar en QEMU (requiere imagen de disco)
cd ..
./qemu.sh
```

## Notas Técnicas

### Por qué este enfoque
1. **Minimal changes**: No requiere reescribir todo el sistema
2. **Microkernel-friendly**: Filesystem puede ser userspace más tarde
3. **Incremental**: Cada paso es funcional y testeable
4. **Pragmático**: Reconoce limitaciones actuales

### Limitaciones Conocidas
1. Init está embebido en el kernel (temporal)
2. No hay driver de disco todavía
3. No puede leer archivos del filesystem todavía
4. eclipse-systemd requiere más trabajo para ejecutar

## Referencias
- Código del init: `eclipse_kernel/userspace/init/src/main.rs`
- Kernel modificado: `eclipse_kernel/src/main.rs` líneas 98-143
- ELF Loader: `eclipse_kernel/src/elf_loader.rs`
- EclipseFS lib: `eclipsefs-lib/` (no_std compatible)
