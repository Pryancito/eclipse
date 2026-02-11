//! Ejemplo básico de uso de la librería EclipseFS

use eclipsefs_lib::{constants, EclipseFSNode, EclipseFSReader, EclipseFSResult, EclipseFSWriter};
use std::fs::File;

fn main() -> EclipseFSResult<()> {
    println!("=== Prueba básica de EclipseFS ===");

    // Crear un archivo temporal
    let test_file = "test_eclipsefs.img";

    // Crear y escribir una imagen EclipseFS
    {
        let file = File::create(test_file)?;
        let mut writer = EclipseFSWriter::new(file);

        // Crear el nodo raíz
        writer.create_root()?;

        // Crear directorio /bin
        let bin_node = EclipseFSNode::new_dir();
        let bin_inode = writer.create_node(bin_node)?;

        // Crear archivo /bin/hello
        let mut hello_node = EclipseFSNode::new_file();
        hello_node.set_data(b"Hello, EclipseFS!")?;
        let hello_inode = writer.create_node(hello_node)?;

        // Crear enlace simbólico /bin/sh -> hello
        let sh_link = EclipseFSNode::new_symlink("hello");
        let sh_inode = writer.create_node(sh_link)?;

        // Agregar hijos al directorio raíz
        let root = writer.get_root()?;
        root.add_child("bin", bin_inode)?;

        // Agregar hijos al directorio /bin
        let bin_dir = writer.get_node(bin_inode)?;
        bin_dir.add_child("hello", hello_inode)?;
        bin_dir.add_child("sh", sh_inode)?;

        // Escribir la imagen
        writer.write_image()?;
        println!("✅ Imagen EclipseFS creada exitosamente");
    }

    // Leer y verificar la imagen
    {
        let mut reader = EclipseFSReader::from_file(File::open(test_file).map_err(|e| {
            eprintln!("Error abriendo archivo: {}", e);
            eclipsefs_lib::EclipseFSError::IoError
        })?).inspect_err(|e| {
            eprintln!("Error creando reader: {:?}", e);
        })?;

        println!("\n=== Verificando imagen ===");

        // Verificar header
        let header = reader.get_header();
        println!("Magic: {}", String::from_utf8_lossy(&header.magic));
        println!("Versión: 0x{:08X}", header.version);
        println!("Total inodos: {}", header.total_inodes);

        // Verificar nodo raíz
        let root = reader.get_root()?;
        println!("Nodo raíz tiene {} hijos", root.children.len());

        // Verificar directorio /bin
        let bin_inode = reader.lookup(constants::ROOT_INODE as u64, "bin")?;
        let bin_node = reader.get_node(bin_inode)?;
        println!("Directorio /bin tiene {} hijos", bin_node.children.len());

        // Verificar archivo /bin/hello
        let hello_inode = reader.lookup(bin_inode, "hello")?;
        let hello_node = reader.get_node(hello_inode)?;
        let content = String::from_utf8_lossy(&hello_node.data);
        println!("Contenido de /bin/hello: {}", content);

        // Verificar enlace simbólico /bin/sh
        let sh_inode = reader.lookup(bin_inode, "sh")?;
        let sh_node = reader.get_node(sh_inode)?;
        let target = String::from_utf8_lossy(&sh_node.data);
        println!("Enlace simbólico /bin/sh -> {}", target);

        println!("✅ Verificación completada exitosamente");
    }

    // Limpiar archivo temporal
    std::fs::remove_file(test_file)?;
    println!("\n🧹 Archivo temporal eliminado");

    Ok(())
}
