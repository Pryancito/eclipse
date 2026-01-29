# Changelog

Todos los cambios notables en Eclipse OS serán documentados en este archivo.

El formato está basado en [Keep a Changelog](https://keepachangelog.com/es-ES/1.0.0/),
y este proyecto adhiere a [Semantic Versioning](https://semver.org/lang/es/).

## [No Publicado]

### Añadido
- Validación de nombres de archivo para prevenir path traversal
- Validación de tamaño de datos para prevenir overflow de memoria
- Archivo SECURITY.md con guías de seguridad
- Archivo CONTRIBUTING.md con guías de contribución
- Archivo FAQ.md con preguntas frecuentes
- Archivo LICENSE (MIT)
- Archivo .editorconfig para consistencia de código
- Script check_build.sh para verificar estado de compilación
- Badges de estado en README
- Sección de Inicio Rápido en README
- Directorio de ejemplos con guías de uso
- Mejoras en .gitignore para excluir más archivos temporales

### Cambiado
- Mejorada la documentación del README con ejemplos más claros
- Optimizadas las condiciones de compilación para reducir warnings

### Corregido
- Eliminados imports no utilizados en eclipsefs-lib
- Eliminados imports no utilizados en mkfs-eclipsefs
- Corregidos warnings de variables no utilizadas con atributos condicionales
- Corregidos warnings de variables mutables innecesarias

## [0.1.0] - 2024-01-XX

### Añadido
- Kernel híbrido inicial con soporte x86_64
- Sistema de archivos EclipseFS con características avanzadas:
  - Journaling para integridad de datos
  - Encriptación transparente
  - Copy-on-Write (CoW)
  - Snapshots
  - Compresión de archivos
  - Sistema de caché inteligente
  - Defragmentación automática
- Soporte UEFI y Multiboot2
- Sistema DRM (Direct Rendering Manager)
- Integración con Wayland
- Driver FUSE para montar EclipseFS en Linux
- Herramientas CLI:
  - mkfs.eclipsefs - Formatear particiones
  - eclipsefs - Herramienta de gestión
- Sistema de userland básico
- Aplicaciones de ejemplo
- Scripts de construcción automatizados
- Soporte para QEMU

### Características del Kernel
- Gestión de memoria con paginación
- Sistema de interrupciones
- Drivers básicos (VGA, teclado, mouse)
- Soporte para FAT32 (lectura)
- Soporte inicial para NTFS

### Características de EclipseFS
- Estructura inspirada en RedoxFS
- Soporte para std y no_std
- Sistema de permisos Unix-like
- ACLs (Access Control Lists)
- Encriptación cuántica experimental
- Balanceo de carga inteligente
- Optimizaciones de IA para predicción de acceso

[No Publicado]: https://github.com/Pryancito/eclipse/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Pryancito/eclipse/releases/tag/v0.1.0
