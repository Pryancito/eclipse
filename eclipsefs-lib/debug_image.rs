use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use byteorder::{LittleEndian, ReadBytesExt};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open("test_eclipsefs.img")?;
    
    println!("=== DEBUGGING ECLIPSEFS IMAGE ===");
    
    // Leer magic
    let mut magic = [0u8; 9];
    file.read_exact(&mut magic)?;
    println!("Magic: {}", String::from_utf8_lossy(&magic));
    
    // Leer versión
    let version = file.read_u32::<LittleEndian>()?;
    println!("Versión: 0x{:08X}", version);
    
    // Leer inode table offset
    let inode_table_offset = file.read_u64::<LittleEndian>()?;
    println!("Inode table offset: {}", inode_table_offset);
    
    // Leer inode table size
    let inode_table_size = file.read_u64::<LittleEndian>()?;
    println!("Inode table size: {}", inode_table_size);
    
    // Leer número de nodos
    let node_count = file.read_u32::<LittleEndian>()?;
    println!("Número de nodos: {}", node_count);
    
    // Mostrar posición actual
    let current_pos = file.seek(SeekFrom::Current(0))?;
    println!("Posición actual: {}", current_pos);
    
    // Saltar al inicio de la tabla de inodos
    file.seek(SeekFrom::Start(inode_table_offset))?;
    
    // Leer tabla de inodos
    println!("\n=== TABLA DE INODOS ===");
    for i in 0..node_count {
        let inode = file.read_u64::<LittleEndian>()?;
        let offset = file.read_u64::<LittleEndian>()?;
        println!("Inodo {}: offset {}", inode, offset);
    }
    
    Ok(())
}
