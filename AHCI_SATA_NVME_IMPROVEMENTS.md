# Mejoras en Soporte AHCI/SATA/NVMe

Este documento detalla las mejoras realizadas en los drivers de almacenamiento de Eclipse OS para mejorar el soporte de dispositivos AHCI/SATA y NVMe.

## Resumen

Se han implementado mejoras significativas en tres drivers principales:
1. **NVMe Driver** (`eclipse_kernel/src/drivers/nvme.rs`)
2. **AHCI Driver** (`eclipse_kernel/src/drivers/ahci.rs`)
3. **SATA/AHCI Driver** (`eclipse_kernel/src/drivers/sata_ahci.rs`)

## Mejoras Implementadas

### 1. Driver NVMe

#### Correcciones Críticas
- **Acceso Volátil a Registros**: Se corrigió el acceso directo a registros de memoria mapeada para usar `read_volatile()` y `write_volatile()`, garantizando que el compilador no optimice incorrectamente los accesos a hardware.
  ```rust
  // Antes (incorrecto):
  *addr
  
  // Después (correcto):
  core::ptr::read_volatile(addr)
  ```

- **Soporte de Registros de 64 bits**: Se agregaron funciones para leer/escribir registros de 64 bits, necesarios para direcciones de colas de administración.
  ```rust
  fn read_register_64(&self, offset: u32) -> u64
  fn write_register_64(&self, offset: u32, value: u64)
  ```

#### Mejoras en Configuración de Colas
- **Lectura de Capacidades del Controlador**: Se lee el registro CAP para obtener:
  - Máximo de entradas de cola soportadas (MQES)
  - Doorbell stride (DSTRD) para calcular offsets correctos
  
- **Verificación de Configuración**: Se verifica que las direcciones de colas se escribieron correctamente.

- **Doorbell Tracking**: Se almacena el doorbell stride para uso en envío de comandos.

#### Soporte de Namespaces
- **Estructura de Namespace**: Nueva estructura `NvmeNamespaceInfo` para almacenar información de cada namespace.
  ```rust
  pub struct NvmeNamespaceInfo {
      pub nsid: u32,
      pub size: u64,
      pub capacity: u64,
      pub block_size: u32,
      pub formatted_lba_size: u8,
  }
  ```

- **Enumeración de Namespaces**: Método `enumerate_namespaces()` para listar todos los namespaces activos.

#### Corrección de Problemas de Alineación
- Se solucionaron errores de referencias desalineadas en estructuras packed copiando campos a variables locales.

### 2. Driver AHCI

#### Estructuras de Datos AHCI
Se agregaron las estructuras fundamentales del protocolo AHCI:

1. **Command Header** (`AhciCommandHeader`):
   - Describe un comando en la Command List
   - Apunta a la Command Table que contiene el FIS y PRDs
   
2. **Physical Region Descriptor** (`AhciPrd`):
   - Describe regiones de memoria física para DMA
   - Permite transferencias de datos sin intervención de CPU

3. **Register FIS Host to Device** (`FisRegH2D`):
   - Estructura para enviar comandos ATA al dispositivo
   - Soporta direccionamiento LBA28 y LBA48

#### Gestión de Memoria
- **Tracking de Estructuras**: Se agregaron campos al driver para rastrear:
  - Command List Base Address (CLB)
  - FIS Base Address (FB)
  - Command Table Base Address
  
- **Configuración de Puerto**: Método `setup_port_structures()` que:
  1. Configura CLB y FB en los registros del puerto
  2. Habilita FIS Receive (FRE)
  3. Inicia el puerto (ST bit)
  4. Espera activación con timeouts apropiados

#### Utilidades de Comando
1. **Constructor de FIS** (`build_fis_reg_h2d()`):
   - Construye FIS de tipo Register H2D para comandos ATA
   - Soporta LBA28 y LBA48
   - Maneja sector count de 8 y 16 bits
   
2. **Búsqueda de Slot Libre** (`find_free_command_slot()`):
   - Encuentra un slot libre en la Command List
   - Verifica tanto PxSACT como PxCI
   
3. **Espera de Completación** (`wait_for_command_completion()`):
   - Espera que un comando complete
   - Verifica errores en PxIS
   - Limpia interrupciones automáticamente

#### Soporte de Comandos Extendidos
- Se agregaron constantes para comandos LBA48:
  - `ATA_CMD_READ_DMA_EXT` (0x25)
  - `ATA_CMD_WRITE_DMA_EXT` (0x35)

### 3. Driver SATA/AHCI

#### Mejoras en Operaciones de Bloque
- **Validación de Buffer**: Se verifica que los buffers sean múltiplos de 512 bytes.
  
- **Mejor Simulación**: 
  - Sector 0 incluye firma MBR (0x55AA)
  - Datos más realistas basados en número de sector
  
- **Soporte de Escritura**: Se implementó `write_blocks()` con estructura apropiada (aunque aún simulado).

- **Comentarios TODO**: Se agregaron comentarios detallados explicando cómo implementar DMA real:
  1. Configurar Command List y Command Table
  2. Construir FIS con comando READ/WRITE DMA EXT
  3. Configurar PRDs apuntando al buffer
  4. Escribir en PxCI para iniciar comando
  5. Esperar completación verificando PxCI y PxIS

## Arquitectura Mejorada

### Flujo de Lectura/Escritura AHCI (Preparado para implementación real)

```
1. Aplicación solicita lectura
   ↓
2. find_free_command_slot() → encuentra slot libre
   ↓
3. build_fis_reg_h2d() → construye comando ATA
   ↓
4. Configurar Command Header
   ↓
5. Configurar PRDs en Command Table
   ↓
6. Copiar FIS a Command Table
   ↓
7. Escribir PxCI para iniciar comando
   ↓
8. wait_for_command_completion() → espera y verifica
   ↓
9. Datos disponibles en buffer de aplicación
```

### Flujo de Comandos NVMe (Mejorado)

```
1. Leer CAP para obtener capacidades
   ↓
2. Configurar colas con tamaños apropiados
   ↓
3. Escribir ASQ/ACQ con direcciones de 64 bits
   ↓
4. Verificar escritura correcta
   ↓
5. Construir comando NVMe
   ↓
6. submit_command() → escribe doorbell
   ↓
7. wait_for_completion() → verifica CQ
   ↓
8. Procesar resultado
```

## Beneficios de las Mejoras

### Corrección
- ✅ Acceso volátil garantiza comportamiento correcto con hardware real
- ✅ Estructuras alineadas evitan comportamiento indefinido
- ✅ Verificación de escrituras detecta problemas de configuración

### Robustez
- ✅ Mejor manejo de errores en cada paso
- ✅ Timeouts apropiados evitan cuelgues infinitos
- ✅ Limpieza de errores antes de operaciones

### Preparación para Hardware Real
- ✅ Estructuras AHCI listas para DMA
- ✅ Doorbell stride correcto para NVMe
- ✅ Soporte de LBA48 para discos grandes (> 128 GB)

### Escalabilidad
- ✅ Soporte de múltiples namespaces NVMe
- ✅ Gestión de múltiples comandos simultáneos
- ✅ Preparado para colas de I/O NVMe

## Próximos Pasos

### Implementación Completa de DMA
1. **AHCI**:
   - Asignar memoria física para Command List, FIS, Command Tables
   - Implementar construcción completa de Command Headers
   - Configurar PRDs con direcciones físicas de buffers
   - Manejar interrupciones de completación

2. **NVMe**:
   - Implementar colas de I/O (separadas de admin queue)
   - Manejar phase bit en Completion Queue
   - Configurar PRPs (Physical Region Pages) para transferencias
   - Soporte de múltiples colas para rendimiento

### Soporte de Hardware Avanzado
- Native Command Queuing (NCQ) para SATA
- Multi-queue NVMe para paralelismo
- Soporte de TRIM/UNMAP
- Gestión de energía (Link Power Management)

### Testing
- Validación en QEMU con diferentes configuraciones
- Pruebas con discos reales vía USB pass-through
- Benchmarks de rendimiento
- Pruebas de estabilidad bajo carga

## Conclusión

Estas mejoras establecen una base sólida para soporte completo de almacenamiento en Eclipse OS. Los drivers ahora:
- Usan correctamente accesos volátiles para hardware
- Tienen estructuras completas del protocolo AHCI/NVMe
- Incluyen utilidades para gestión de comandos
- Están preparados para implementación de DMA real

La arquitectura mejorada facilita la transición de simulación a hardware real, manteniendo el código limpio y bien documentado.
