//! Tests de **imagen on-disk** (writer → reader) y benchmarks ligeros.
//!
//! ## Ejecutar
//! ```text
//! cargo test -p eclipsefs-lib --test eclipsefs_disk_tests
//! ```
//!
//! Benchmarks (no fallan por tiempo; imprimen métricas; omitidos en CI rápido):
//! ```text
//! cargo test -p eclipsefs-lib --test eclipsefs_disk_tests -- --ignored --nocapture
//! ```

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eclipsefs_lib::{
    constants, CacheType, EclipseFSError, EclipseFSNode, EclipseFSReader, EclipseFSWriter,
};

fn unique_tmp(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "eclipsefs_test_{}_{}_{}",
        label,
        std::process::id(),
        nanos
    ))
}

fn rm(p: &Path) {
    let _ = fs::remove_file(p);
}

/// Árbol mínimo: `/bin/<name>` con contenido `data`.
fn write_image_bin_file(name: &str, data: &[u8]) -> PathBuf {
    let path = unique_tmp("img");
    {
        let file = File::create(&path).expect("create temp image");
        let mut w = EclipseFSWriter::new(file);
        w.create_root().expect("root");
        let bin_inode = w
            .create_node(EclipseFSNode::new_dir())
            .expect("bin inode");
        w.get_root()
            .expect("root mut")
            .add_child("bin", bin_inode)
            .expect("add bin");
        let mut f = EclipseFSNode::new_file();
        f.set_data(data).expect("set_data");
        let fino = w.create_node(f).expect("file inode");
        w.get_node(bin_inode)
            .expect("bin mut")
            .add_child(name, fino)
            .expect("add file");
        w.write_image().expect("write_image");
    }
    path
}

// --- Fallos (siempre deben pasar) ---

#[test]
fn open_missing_image_fails() {
    let p = unique_tmp("nosuch");
    let _ = fs::remove_file(&p);
    let err = match EclipseFSReader::new(p.to_str().unwrap()) {
        Ok(_) => panic!("expected open to fail"),
        Err(e) => e,
    };
    assert!(
        matches!(err, EclipseFSError::IoError | EclipseFSError::PermissionDenied),
        "expected IoError or PermissionDenied, got {:?}",
        err
    );
}

#[test]
fn invalid_magic_rejected() {
    let path = unique_tmp("badmagic");
    {
        let mut f = File::create(&path).unwrap();
        let garbage = vec![0xFFu8; 4096];
        f.write_all(&garbage).unwrap();
    }
    let err = match EclipseFSReader::new(path.to_str().unwrap()) {
        Ok(_) => panic!("expected invalid magic to fail"),
        Err(e) => e,
    };
    assert_eq!(err, EclipseFSError::InvalidFormat);
    rm(&path);
}

#[test]
fn lookup_nonexistent_component() {
    let path = write_image_bin_file("hello", b"x");
    let mut r = EclipseFSReader::new(path.to_str().unwrap()).unwrap();
    let err = r.lookup_path("/bin/nope").unwrap_err();
    assert_eq!(err, EclipseFSError::NotFound);
    let err = r.lookup_path("/nope/foo").unwrap_err();
    assert_eq!(err, EclipseFSError::NotFound);
    rm(&path);
}

#[test]
fn lookup_file_as_directory_fails() {
    let path = write_image_bin_file("leaf", b"data");
    let mut r = EclipseFSReader::new(path.to_str().unwrap()).unwrap();
    let leaf = r.lookup_path("/bin/leaf").unwrap();
    let err = r.lookup_path("/bin/leaf/more").unwrap_err();
    assert_eq!(err, EclipseFSError::InvalidOperation);
    assert!(leaf > 0);
    rm(&path);
}

#[test]
fn root_and_deep_path_lookup() {
    let p = unique_tmp("deep");
    {
        let file = File::create(&p).unwrap();
        let mut w = EclipseFSWriter::new(file);
        w.create_root().unwrap();
        let a = w.create_node(EclipseFSNode::new_dir()).unwrap();
        w.get_root().unwrap().add_child("a", a).unwrap();
        let b = w.create_node(EclipseFSNode::new_dir()).unwrap();
        w.get_node(a).unwrap().add_child("b", b).unwrap();
        let mut f = EclipseFSNode::new_file();
        f.set_data(b"ok").unwrap();
        let fino = w.create_node(f).unwrap();
        w.get_node(b).unwrap().add_child("c.txt", fino).unwrap();
        w.write_image().unwrap();
    }
    let mut r = EclipseFSReader::new(p.to_str().unwrap()).unwrap();
    assert_eq!(r.lookup_path("/").unwrap(), constants::ROOT_INODE);
    let ino = r.lookup_path("/a/b/c.txt").unwrap();
    let data = r.read_file_content(ino).unwrap();
    assert_eq!(data, b"ok");
    rm(&p);
}

#[test]
fn empty_file_roundtrip() {
    let path = write_image_bin_file("empty", b"");
    let mut r = EclipseFSReader::new(path.to_str().unwrap()).unwrap();
    let ino = r.lookup_path("/bin/empty").unwrap();
    let data = r.read_file_content(ino).unwrap();
    assert!(data.is_empty());
    rm(&path);
}

#[test]
fn read_file_range_matches_full() {
    let payload: Vec<u8> = (0u8..=255).cycle().take(10_000).collect();
    let path = write_image_bin_file("blob", &payload);
    let mut r = EclipseFSReader::new(path.to_str().unwrap()).unwrap();
    let ino = r.lookup_path("/bin/blob").unwrap();
    let full = r.read_file_content(ino).unwrap();
    assert_eq!(full, payload);
    let slice = r
        .read_file_content_range(ino, 100, 500)
        .expect("range read");
    assert_eq!(slice, payload[100..600]);
    rm(&path);
}

#[test]
fn duplicate_child_rejected_at_write_time() {
    let p = unique_tmp("dup");
    let file = File::create(&p).unwrap();
    let mut w = EclipseFSWriter::new(file);
    w.create_root().unwrap();
    let d = w.create_node(EclipseFSNode::new_dir()).unwrap();
    w.get_root().unwrap().add_child("d", d).unwrap();
    let mut f1 = EclipseFSNode::new_file();
    f1.set_data(b"1").unwrap();
    let i1 = w.create_node(f1).unwrap();
    w.get_node(d).unwrap().add_child("x", i1).unwrap();
    let mut f2 = EclipseFSNode::new_file();
    f2.set_data(b"2").unwrap();
    let i2 = w.create_node(f2).unwrap();
    let err = w.get_node(d).unwrap().add_child("x", i2).unwrap_err();
    assert_eq!(err, EclipseFSError::DuplicateEntry);
    rm(&p);
}

// --- Benchmarks (#[ignore]: ejecutar manualmente) ---

#[test]
#[ignore = "benchmark: cargo test -p eclipsefs-lib --test eclipsefs_disk_tests bench_ -- --ignored --nocapture"]
fn bench_lookup_cold_vs_warm_paths() {
    let path = write_image_bin_file("x", b"hi");
    // Mismo path muchas veces: segunda tanda suele beneficiarse del caché de nodos en el reader.
    let mut r = EclipseFSReader::new(path.to_str().unwrap()).unwrap();
    let p = "/bin/x";

    let t0 = Instant::now();
    for _ in 0..500 {
        let _ = r.lookup_path(p).unwrap();
    }
    let cold_elapsed = t0.elapsed();

    let t1 = Instant::now();
    for _ in 0..500 {
        let _ = r.lookup_path(p).unwrap();
    }
    let warm_elapsed = t1.elapsed();

    eprintln!(
        "[eclipsefs bench] 500× lookup_path({}): first block {:?}, second block {:?}",
        p, cold_elapsed, warm_elapsed
    );
    rm(&path);
}

#[test]
#[ignore = "benchmark: large sequential read"]
fn bench_sequential_read_1mib() {
    const SIZE: usize = 1024 * 1024;
    let payload: Vec<u8> = (0u8..=255).cycle().take(SIZE).collect();
    let path = write_image_bin_file("big", &payload);

    let t_open = Instant::now();
    let mut r = EclipseFSReader::new(path.to_str().unwrap()).unwrap();
    let open_d = t_open.elapsed();
    let ino = r.lookup_path("/bin/big").unwrap();

    let t_read = Instant::now();
    let got = r.read_file_content(ino).unwrap();
    let read_d = t_read.elapsed();

    assert_eq!(got.len(), SIZE);
    let mib = SIZE as f64 / (1024.0 * 1024.0);
    let mb_s = mib / read_d.as_secs_f64().max(1e-9);
    eprintln!(
        "[eclipsefs bench] open {:?} | read {:.2} MiB in {:?} (~{:.1} MiB/s)",
        open_d,
        mib,
        read_d,
        mb_s
    );
    rm(&path);
}

#[test]
#[ignore = "benchmark: ARC vs LRU cache type"]
fn bench_reader_cache_arc_vs_lru() {
    let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
    let path = write_image_bin_file("cacheblob", &payload);

    for (label, cache) in [("LRU", CacheType::LRU), ("ARC", CacheType::ARC)] {
        let t = Instant::now();
        let mut r = EclipseFSReader::new_with_cache(path.to_str().unwrap(), cache).unwrap();
        let ino = r.lookup_path("/bin/cacheblob").unwrap();
        for _ in 0..20 {
            let _ = r.read_file_content(ino).unwrap();
        }
        eprintln!(
            "[eclipsefs bench] {} 20× read_file_content(64KiB): {:?}",
            label,
            t.elapsed()
        );
    }
    rm(&path);
}
