//! Benchmark comparando LRU vs ARC (Arquera)
//! Demuestra la ventaja del algoritmo adaptativo

use eclipsefs_lib::{EclipseFSNode, EclipseFSReader, EclipseFSWriter, CacheType};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Comparación: LRU vs ARC (Algoritmo Arquera) ===\n");

    // Create test filesystem
    let test_image = "/tmp/arc_vs_lru_test.eclipsefs";
    println!("Creando filesystem de prueba con 1000 archivos...");
    create_test_filesystem(test_image, 1000)?;
    println!("✅ Filesystem creado\n");

    // Benchmark 1: LRU Cache
    println!("=== Test 1: Cache LRU (Simple) ===");
    let lru_time = benchmark_cache(test_image, CacheType::LRU)?;
    println!();

    // Benchmark 2: ARC Cache
    println!("=== Test 2: Cache ARC (Algoritmo Arquera) ===");
    let arc_time = benchmark_cache(test_image, CacheType::ARC)?;
    println!();

    // Comparison
    println!("=== Comparación de Resultados ===");
    println!("LRU:  {:.2}ms", lru_time * 1000.0);
    println!("ARC:  {:.2}ms", arc_time * 1000.0);
    
    let improvement = ((lru_time - arc_time) / lru_time * 100.0).abs();
    if arc_time < lru_time {
        println!("\n✅ ARC es {:.1}% más rápido que LRU", improvement);
        println!("   El algoritmo Arquera se adapta mejor al patrón de acceso!");
    } else {
        println!("\n✅ LRU es {:.1}% más rápido que ARC", improvement);
        println!("   Para este patrón simple, LRU es suficiente");
    }

    Ok(())
}

fn create_test_filesystem(path: &str, num_files: usize) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut writer = EclipseFSWriter::new(file);
    
    writer.create_root()?;
    
    // Create multiple directories with files
    for dir_idx in 0..10 {
        let mut dir_node = EclipseFSNode::new_dir();
        
        for file_idx in 0..(num_files / 10) {
            let file_name = format!("file_{:04}.txt", file_idx);
            let mut file_node = EclipseFSNode::new_file();
            let data = format!("Content of file {} in dir {}", file_idx, dir_idx);
            file_node.data = data.into_bytes();
            file_node.size = file_node.data.len() as u64;
            
            let file_inode = writer.allocate_inode();
            writer.add_node(file_inode, file_node)?;
            dir_node.add_child(&file_name, file_inode)?;
        }
        
        let dir_inode = writer.allocate_inode();
        writer.add_node(dir_inode, dir_node)?;
        writer.get_root()?.add_child(&format!("dir_{:02}", dir_idx), dir_inode)?;
    }
    
    writer.write_image()?;
    
    Ok(())
}

fn benchmark_cache(path: &str, cache_type: CacheType) -> Result<f64, Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new_with_cache(path, cache_type)?;
    
    println!("Patrón de acceso mixto (reciente + frecuente):");
    
    let start = Instant::now();
    
    let root = reader.get_root()?;
    
    // Phase 1: Sequential scan (favorece LRU)
    for i in 0..10 {
        let dir_name = format!("dir_{:02}", i);
        if let Some(dir_inode) = root.get_child_inode(&dir_name) {
            let dir_node = reader.read_node(dir_inode)?;
            
            // Read some files from each directory
            for (_name, child_inode) in dir_node.get_children().iter().take(10) {
                let _ = reader.read_node(*child_inode)?;
            }
        }
    }
    
    // Phase 2: Repeated access to hot files (favorece ARC - detection de frecuencia)
    for _ in 0..5 {
        for i in 0..5 {
            let dir_name = format!("dir_{:02}", i);
            if let Some(dir_inode) = root.get_child_inode(&dir_name) {
                let _ = reader.read_node(dir_inode)?;
            }
        }
    }
    
    // Phase 3: Mixed pattern (ARC adapts better)
    for i in 0..10 {
        let dir_name = format!("dir_{:02}", i);
        if let Some(dir_inode) = root.get_child_inode(&dir_name) {
            let dir_node = reader.read_node(dir_inode)?;
            
            // Repeatedly access first few files (hot data)
            for (_name, child_inode) in dir_node.get_children().iter().take(3) {
                for _ in 0..3 {
                    let _ = reader.read_node(*child_inode)?;
                }
            }
            
            // Sequential scan of remaining files (scan pattern)
            for (_name, child_inode) in dir_node.get_children().iter().skip(3).take(7) {
                let _ = reader.read_node(*child_inode)?;
            }
        }
    }
    
    let elapsed = start.elapsed();
    
    // Print stats
    let stats = reader.get_cache_stats();
    stats.print();
    println!("Tiempo total: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    
    Ok(elapsed.as_secs_f64())
}
