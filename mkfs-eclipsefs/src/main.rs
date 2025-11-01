/// mkfs.eclipsefs - Utilidad para formatear particiones con EclipseFS v2.0
/// 
/// Similar a mkfs.ext4, mkfs.fat32, etc.
/// 
/// Uso:
///   mkfs.eclipsefs /dev/sda2
///   mkfs.eclipsefs -L "Mi Disco" /dev/sda2
///   mkfs.eclipsefs --inodes 10000 /dev/sda2
///   mkfs.eclipsefs --block-size 8192 /dev/sda2

use std::fs::{File, OpenOptions};
use std::io::{self, Write, Seek, SeekFrom, Read};
use std::path::PathBuf;
use clap::Parser;
use uuid::Uuid;
use chrono::Utc;

/// Argumentos de línea de comandos
#[derive(Parser, Debug)]
#[command(name = "mkfs.eclipsefs")]
#[command(about = "Crear un filesystem EclipseFS v2.0", long_about = None)]
struct Args {
    /// Dispositivo o archivo de imagen a formatear
    #[arg(value_name = "DEVICE")]
    device: PathBuf,
    
    /// Label del filesystem (máx 100 caracteres)
    #[arg(short = 'L', long, default_value = "Eclipse OS")]
    label: String,
    
    /// Tamaño de bloque (512, 1024, 2048, 4096, 8192)
    #[arg(short = 'b', long, default_value = "4096")]
    block_size: u32,
    
    /// Número de inodes a crear
    #[arg(short = 'N', long, default_value = "10000")]
    inodes: u64,
    
    /// Forzar formateo sin confirmación
    #[arg(short = 'f', long)]
    force: bool,
    
    /// Modo verbose
    #[arg(short = 'v', long)]
    verbose: bool,
}

/// Header EclipseFS v2.0 (compatible con eclipsefs-lib)
#[repr(C)]
struct EclipseFSHeader {
    magic: [u8; 9],              // "ECLIPSEFS"
    version: u32,                // 0x00020000 para v2.0 (formato 16.16)
    inode_table_offset: u64,     // Offset de la tabla de inodos
    inode_table_size: u64,       // Tamaño de la tabla de inodos
    total_inodes: u32,           // Inodes totales
    header_checksum: u32,        // CRC32 del header
    metadata_checksum: u32,      // CRC32 de metadatos
    data_checksum: u32,          // CRC32 de datos
    creation_time: u64,          // Timestamp de creación
    last_check: u64,             // Última verificación
    flags: u32,                  // Flags del sistema
    reserved: [u8; 3998],        // Padding hasta 4096 bytes
}

impl EclipseFSHeader {
    fn new(label: &str, block_size: u32, total_blocks: u64, total_inodes: u64) -> Self {
        let mut header_label = [0u8; 100];
        let label_bytes = label.as_bytes();
        let copy_len = label_bytes.len().min(99);
        header_label[..copy_len].copy_from_slice(&label_bytes[..copy_len]);
        
        // Calcular offsets (compatible con eclipsefs-lib)
        let inode_table_offset = 4096; // Después del header
        let inode_table_entry_size = 16; // Tamaño de entrada en tabla de inodos
        let inode_table_size = total_inodes * inode_table_entry_size;
        
        let mut magic = [0u8; 9];
        magic.copy_from_slice(b"ECLIPSEFS");
        
        let now = Utc::now().timestamp() as u64;
        
        Self {
            magic,
            version: 0x00020000, // v2.0 en formato 16.16 (compatible con eclipsefs-lib)
            inode_table_offset,
            inode_table_size,
            total_inodes: total_inodes as u32,
            header_checksum: 0,      // Se calculará después
            metadata_checksum: 0,    // Se calculará después
            data_checksum: 0,        // Se calculará después
            creation_time: now,
            last_check: now,
            flags: 0,
            reserved: [0u8; 3998],
        }
    }
    
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4096);
        
        // Escribir en el mismo orden que eclipsefs-lib espera
        bytes.extend_from_slice(&self.magic);                      // 0-8: magic (9 bytes)
        bytes.extend_from_slice(&self.version.to_le_bytes());      // 9-12: version (4 bytes)
        bytes.extend_from_slice(&self.inode_table_offset.to_le_bytes()); // 13-20: inode_table_offset (8 bytes)
        bytes.extend_from_slice(&self.inode_table_size.to_le_bytes());   // 21-28: inode_table_size (8 bytes)
        bytes.extend_from_slice(&self.total_inodes.to_le_bytes());       // 29-32: total_inodes (4 bytes)
        bytes.extend_from_slice(&self.header_checksum.to_le_bytes());    // 33-36: header_checksum (4 bytes)
        bytes.extend_from_slice(&self.metadata_checksum.to_le_bytes());  // 37-40: metadata_checksum (4 bytes)
        bytes.extend_from_slice(&self.data_checksum.to_le_bytes());      // 41-44: data_checksum (4 bytes)
        bytes.extend_from_slice(&self.creation_time.to_le_bytes());      // 45-52: creation_time (8 bytes)
        bytes.extend_from_slice(&self.last_check.to_le_bytes());         // 53-60: last_check (8 bytes)
        bytes.extend_from_slice(&self.flags.to_le_bytes());              // 61-64: flags (4 bytes)
        bytes.extend_from_slice(&self.reserved);                         // 65-4095: reserved
        
        bytes
    }
}

/// Formatea un dispositivo con EclipseFS
fn format_device(args: &Args) -> io::Result<()> {
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║              mkfs.eclipsefs - EclipseFS v2.0 Formatter              ║");
    println!("╚══════════════════════════════════════════════════════════════════════╝");
    println!();
    
    // Abrir el dispositivo
    println!("📂 Abriendo dispositivo: {:?}", args.device);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.device)?;
    
    // Obtener tamaño del dispositivo
    let size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    
    let size_mb = size / (1024 * 1024);
    let size_gb = size / (1024 * 1024 * 1024);
    
    println!("💾 Tamaño del dispositivo: {} bytes ({} MB / {} GB)", size, size_mb, size_gb);
    
    // Calcular parámetros del filesystem
    let total_blocks = size / args.block_size as u64;
    let total_inodes = args.inodes;
    
    println!("🔧 Parámetros del filesystem:");
    println!("   Label:       {}", args.label);
    println!("   Block size:  {} bytes", args.block_size);
    println!("   Total blocks: {}", total_blocks);
    println!("   Total inodes: {}", total_inodes);
    
    // Confirmación
    if !args.force {
        println!();
        println!("⚠️  ADVERTENCIA: Esto borrará todos los datos en {:?}", args.device);
        print!("¿Continuar? (s/N): ");
        io::stdout().flush()?;
        
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        
        if !input.trim().eq_ignore_ascii_case("s") && !input.trim().eq_ignore_ascii_case("y") {
            println!("Operación cancelada.");
            return Ok(());
        }
    }
    
    println!();
    println!("🚀 Iniciando formateo...");
    
    // Crear header
    println!("📝 Creando header EclipseFS...");
    let header = EclipseFSHeader::new(&args.label, args.block_size, total_blocks, total_inodes);
    let header_bytes = header.to_bytes();
    
    if args.verbose {
        println!("   Magic: {}", String::from_utf8_lossy(&header.magic));
        println!("   Version: 0x{:08X}", header.version);
        println!("   Creation time: {}", header.creation_time);
        println!("   Total inodes: {}", header.total_inodes);
    }
    
    // Escribir header
    println!("💾 Escribiendo header (4KB)...");
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header_bytes)?;
    
    // Crear inode table vacío
    println!("📋 Inicializando tabla de inodos ({} inodes)...", total_inodes);
    file.seek(SeekFrom::Start(header.inode_table_offset))?;
    
    let inode_size = 128;
    let empty_inode = vec![0u8; inode_size];
    
    for i in 0..total_inodes {
        file.write_all(&empty_inode)?;
        
        if args.verbose && i % 1000 == 0 {
            println!("   Inodes escritos: {}/{}", i, total_inodes);
        }
    }
    
    // Crear bitmap de bloques libres (todos libres al inicio)
    println!("🗺️  Inicializando bitmaps...");
    let data_area_offset = header.inode_table_offset + header.inode_table_size;
    file.seek(SeekFrom::Start(data_area_offset))?;
    
    // Escribir algunos bloques de padding para bitmaps
    let bitmap_padding = vec![0xFF; 8192]; // 8KB de padding
    file.write_all(&bitmap_padding)?;
    
    // Crear directorio raíz (inode 0)
    println!("📁 Creando directorio raíz...");
    file.seek(SeekFrom::Start(header.inode_table_offset))?;
    
    // Inode del directorio raíz
    let mut root_inode = vec![0u8; 128];
    // Type: 1 = directory
    root_inode[0] = 1;
    // Permissions: 0755
    root_inode[1..3].copy_from_slice(&0o755u16.to_le_bytes());
    // Owner: 0 (root)
    root_inode[3..7].copy_from_slice(&0u32.to_le_bytes());
    // Size: 0
    root_inode[7..15].copy_from_slice(&0u64.to_le_bytes());
    // Created/Modified time
    let now = Utc::now().timestamp() as u64;
    root_inode[15..23].copy_from_slice(&now.to_le_bytes());
    root_inode[23..31].copy_from_slice(&now.to_le_bytes());
    
    file.write_all(&root_inode)?;
    
    // Sync
    file.sync_all()?;
    
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║                  ✅ FORMATEO COMPLETADO EXITOSAMENTE                  ║");
    println!("╚══════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("Filesystem EclipseFS v2.0 creado en: {:?}", args.device);
    println!("  Label:        {}", args.label);
    println!("  Tamaño:       {} MB", size_mb);
    println!("  Inodes:       {}", header.total_inodes);
    println!("  Versión:      0x{:08X}", header.version);
    println!();
    println!("Para montar: mount -t eclipsefs {:?} /mnt", args.device);
    println!();
    
    Ok(())
}

fn main() {
    let args = Args::parse();
    
    if let Err(e) = format_device(&args) {
        eprintln!("❌ Error: {}", e);
        std::process::exit(1);
    }
}

