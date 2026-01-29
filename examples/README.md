# Ejemplos de Uso de Eclipse OS

Este directorio contiene ejemplos de cómo usar diferentes componentes de Eclipse OS.

## 📚 Ejemplos Disponibles

### EclipseFS

Para ejemplos de EclipseFS, consulta el directorio `eclipsefs-lib/examples/`:

- `test_basic.rs` - Operaciones básicas con EclipseFS
- `advanced_features.rs` - Características avanzadas
- `journal_demo.rs` - Uso del sistema de journaling
- `create_test_image.rs` - Creación de imágenes de sistema de archivos

## 🚀 Inicio Rápido con EclipseFS

### Operaciones Básicas

```rust
use eclipsefs_lib::{EclipseFS, NodeKind};

// Crear un nuevo sistema de archivos
let mut fs = EclipseFS::new();

// Crear un archivo
let file_inode = fs.create_file(1, "hello.txt")?;

// Escribir datos
fs.write_file(file_inode, b"Hello, Eclipse OS!")?;

// Leer datos
let data = fs.read_file(file_inode)?;
println!("Contenido: {}", String::from_utf8_lossy(&data));
```

### Directorios

```rust
// Crear un directorio
let dir_inode = fs.create_directory(1, "documents")?;

// Crear un archivo dentro del directorio
let file_inode = fs.create_file(dir_inode, "readme.txt")?;

// Listar contenido del directorio
let entries = fs.list_directory(dir_inode)?;
for entry in entries {
    println!("Archivo: {}", entry);
}
```

### Journaling (Características Avanzadas)

```rust
use eclipsefs_lib::JournalConfig;

// Habilitar journaling
fs.enable_journaling(JournalConfig::default())?;

// Las operaciones ahora son atómicas y recuperables
let file = fs.create_file(1, "important.txt")?;
fs.write_file(file, b"Datos importantes")?;

// Confirmar transacciones
fs.commit_journal()?;

// En caso de crash, se puede recuperar
let recovered = fs.recover_from_journal()?;
println!("Transacciones recuperadas: {}", recovered);
```

## 🔧 Ejecutar Ejemplos

Para ejecutar los ejemplos de EclipseFS:

```bash
# Navegar al directorio de EclipseFS
cd eclipsefs-lib

# Ejecutar ejemplo básico
cargo run --example test_basic --features std

# Ejecutar ejemplo avanzado
cargo run --example advanced_features --features std

# Ejecutar demo de journaling
cargo run --example journal_demo --features std
```

## 📖 Más Información

- [Documentación de EclipseFS](../eclipsefs-lib/README.md)
- [README principal](../README.md)
- [Guía de contribución](../CONTRIBUTING.md)
