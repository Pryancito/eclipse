# Guía de Contribución a Eclipse OS

¡Gracias por tu interés en contribuir a Eclipse OS! Este documento proporciona pautas para contribuir al proyecto.

## 📋 Tabla de Contenidos

- [Código de Conducta](#código-de-conducta)
- [Cómo Contribuir](#cómo-contribuir)
- [Proceso de Desarrollo](#proceso-de-desarrollo)
- [Estándares de Código](#estándares-de-código)
- [Pruebas](#pruebas)
- [Envío de Cambios](#envío-de-cambios)
- [Reporte de Bugs](#reporte-de-bugs)
- [Solicitud de Características](#solicitud-de-características)

## Código de Conducta

Al participar en este proyecto, te comprometes a mantener un ambiente respetuoso y colaborativo. Esperamos que todos los contribuyentes:

- Sean respetuosos con otros contribuyentes
- Acepten críticas constructivas
- Se enfoquen en lo que es mejor para la comunidad
- Muestren empatía hacia otros miembros de la comunidad

## Cómo Contribuir

### Áreas de Contribución

Puedes contribuir en varias áreas:

1. **Desarrollo del Kernel**: Mejoras en el kernel, drivers, gestión de memoria
2. **Sistema Userland**: Aplicaciones, herramientas del sistema
3. **Documentación**: Mejoras en README, guías, tutoriales
4. **Pruebas**: Añadir tests, reportar bugs, verificar funcionalidades
5. **Sistema de Archivos**: Mejoras en EclipseFS
6. **Gráficos**: Sistema DRM, Wayland, compositor

### Prerequisitos

Antes de contribuir, asegúrate de tener:

- Rust 1.70 o superior instalado
- Conocimientos básicos de Rust
- Git instalado
- (Opcional) QEMU para pruebas

## Proceso de Desarrollo

### 1. Fork y Clone

```bash
# Fork el repositorio en GitHub
# Luego clona tu fork
git clone https://github.com/TU_USUARIO/eclipse.git
cd eclipse

# Agrega el repositorio original como upstream
git remote add upstream https://github.com/Pryancito/eclipse.git
```

### 2. Crear una Rama

```bash
# Actualiza tu rama main
git checkout main
git pull upstream main

# Crea una nueva rama para tu característica
git checkout -b feature/nombre-descriptivo
# o para corrección de bugs
git checkout -b fix/descripcion-del-bug
```

### 3. Hacer Cambios

- Realiza tus cambios siguiendo los estándares de código
- Añade tests si es aplicable
- Actualiza la documentación según sea necesario
- Asegúrate de que el código compile sin warnings

### 4. Commit

```bash
# Añade tus cambios
git add .

# Commit con un mensaje descriptivo
git commit -m "feat: Añadir soporte para X característica"
```

**Formato de mensajes de commit:**
- `feat:` - Nueva característica
- `fix:` - Corrección de bug
- `docs:` - Cambios en documentación
- `style:` - Cambios de formato (sin cambios en código)
- `refactor:` - Refactorización de código
- `test:` - Añadir o modificar tests
- `chore:` - Cambios en herramientas, configuración, etc.

### 5. Push y Pull Request

```bash
# Push a tu fork
git push origin feature/nombre-descriptivo

# Crea un Pull Request en GitHub
```

## Estándares de Código

### Rust

- Sigue las convenciones estándar de Rust
- Usa `cargo fmt` antes de hacer commit
- Ejecuta `cargo clippy` y resuelve los warnings
- Documenta funciones públicas con comentarios `///`
- Mantén las funciones pequeñas y enfocadas
- Usa nombres descriptivos para variables y funciones

**Ejemplo de documentación:**

```rust
/// Crea un nuevo nodo en el sistema de archivos
///
/// # Argumentos
///
/// * `name` - Nombre del nodo
/// * `kind` - Tipo de nodo (Archivo, Directorio, etc.)
///
/// # Ejemplo
///
/// ```
/// let node = create_node("test.txt", NodeKind::File)?;
/// ```
///
/// # Errores
///
/// Retorna `EclipseFSError` si el nodo ya existe
pub fn create_node(name: &str, kind: NodeKind) -> Result<Node, EclipseFSError> {
    // implementación
}
```

### Organización del Código

- Un módulo por archivo cuando sea posible
- Agrupa funciones relacionadas
- Mantén las dependencias al mínimo
- Usa características (features) para código condicional

## Pruebas

### Ejecutar Tests

```bash
# Tests del kernel
cd eclipse_kernel
cargo test

# Tests de userland
cd userland
cargo test

# Tests de EclipseFS
cd eclipsefs-lib
cargo test
```

### Añadir Tests

- Añade tests unitarios para nuevas funcionalidades
- Añade tests de integración cuando sea apropiado
- Asegúrate de que todos los tests pasen antes de hacer commit

**Ejemplo de test:**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_node() {
        let fs = EclipseFS::new();
        let result = fs.create_node("test", NodeKind::File);
        assert!(result.is_ok());
    }
}
```

## Envío de Cambios

### Checklist antes de hacer PR

- [ ] El código compila sin errores
- [ ] No hay warnings de compilación
- [ ] Todos los tests pasan
- [ ] Se ha ejecutado `cargo fmt`
- [ ] Se ha ejecutado `cargo clippy` y se han resuelto los warnings
- [ ] La documentación está actualizada
- [ ] Los mensajes de commit son claros y descriptivos

### Descripción del PR

Incluye en la descripción del PR:

1. **Resumen**: Descripción breve de los cambios
2. **Motivación**: Por qué es necesario este cambio
3. **Cambios**: Lista de cambios realizados
4. **Tests**: Cómo se probaron los cambios
5. **Screenshots**: Si hay cambios visuales

## Reporte de Bugs

Usa la plantilla de issues de GitHub e incluye:

- **Descripción**: Descripción clara del bug
- **Pasos para reproducir**: Pasos detallados
- **Comportamiento esperado**: Qué debería pasar
- **Comportamiento actual**: Qué está pasando
- **Entorno**: SO, versión de Rust, etc.
- **Logs**: Salida de error, logs relevantes

## Solicitud de Características

Para solicitar una nueva característica:

- Verifica que no exista ya como issue
- Describe claramente la característica
- Explica por qué sería útil
- Proporciona ejemplos de uso si es posible

## Preguntas y Soporte

Si tienes preguntas:

- Revisa la documentación primero
- Busca en issues existentes
- Crea un nuevo issue con la etiqueta "question"
- Participa en las discusiones de GitHub

## Agradecimientos

¡Gracias por contribuir a Eclipse OS! Cada contribución, grande o pequeña, es valiosa y apreciada.

---

**Eclipse OS** - Construyendo el futuro juntos 🚀
