//! The small-block call-site table.
//!
//! What these are about: the table exists to answer "who is holding the
//! heap", and the only answers worth printing are LIVE blocks. A table that
//! counted allocations would put the busiest cache at the top of the report
//! and the leak somewhere below it. So the tests that matter are the ones
//! about giving a block back -- on free, and on the eviction of a sampled
//! block from the direct-mapped table, which is the case nobody would think
//! to write and the one that silently inflates every row if it is wrong.

use super::alloc_sites::{key_of, Table, FRAMES, SAMPLES, SAMPLE_SHIFT, SITES};

/// A heap-looking pointer whose direct-mapped slot is its own: the table
/// keys on `ptr >> 4`, so pointers a page apart would all land in slot 0 and
/// every test would be accidentally testing eviction.
fn blk(i: usize) -> usize {
    0x10_0000 + i * 16
}

fn chain(seed: usize) -> [usize; FRAMES] {
    let mut f = [0usize; FRAMES];
    for (i, slot) in f.iter_mut().enumerate() {
        *slot = 0xffff_ff00_0000_0000 + seed * 0x100 + i;
    }
    f
}

/// Live blocks and bytes for the row whose chain is `frames`.
fn row_for(t: &Table, frames: [usize; FRAMES]) -> (usize, usize) {
    let (rows, _) = t.rows();
    rows.iter()
        .find(|(_, _, f)| *f == frames)
        .map(|(live, bytes, _)| (*live, *bytes))
        .unwrap_or((0, 0))
}

#[test]
fn una_asignacion_se_carga_a_su_sitio_y_el_free_la_devuelve() {
    let t = Table::new();
    let c = chain(1);
    t.record(blk(1), 512, c);
    assert_eq!(row_for(&t, c), (1, 512));
    assert!(t.tracks(blk(1)));
    assert!(t.forget(blk(1)));
    assert_eq!(row_for(&t, c), (0, 0));
    assert!(!t.tracks(blk(1)));
}

#[test]
fn el_mismo_sitio_suma_y_sitios_distintos_no_se_mezclan() {
    let t = Table::new();
    let (a, b) = (chain(1), chain(2));
    t.record(blk(1), 512, a);
    t.record(blk(2), 600, a);
    t.record(blk(3), 64, b);
    assert_eq!(row_for(&t, a), (2, 1112));
    assert_eq!(row_for(&t, b), (1, 64));
}

#[test]
fn las_filas_salen_de_mas_bytes_a_menos() {
    let t = Table::new();
    t.record(blk(1), 64, chain(1));
    t.record(blk(2), 4096, chain(2));
    t.record(blk(3), 512, chain(3));
    let (rows, _) = t.rows();
    assert_eq!(rows[0].1, 4096);
    assert_eq!(rows[1].1, 512);
    assert_eq!(rows[2].1, 64);
}

#[test]
fn pasados_los_huecos_de_sitio_se_cuentan_las_muestras_perdidas() {
    let t = Table::new();
    for i in 0..SITES {
        t.record(blk(i), 8, chain(i));
    }
    let (_, missed) = t.rows();
    assert_eq!(missed, 0);
    // One chain too many: it must be counted, not silently dropped.
    t.record(blk(500), 8, chain(SITES));
    let (rows, missed) = t.rows();
    assert_eq!(missed, 1);
    assert_eq!(rows.iter().filter(|(live, _, _)| *live > 0).count(), SITES);
}

#[test]
fn un_free_de_algo_no_muestreado_no_toca_nada() {
    let t = Table::new();
    let c = chain(1);
    t.record(blk(1), 512, c);
    assert!(!t.forget(blk(900)));
    assert!(!t.forget(0));
    assert_eq!(row_for(&t, c), (1, 512));
}

/// The case the report lives or dies on: two sampled blocks land in the same
/// direct-mapped slot, so the first one's free can never find it again. Its
/// site has to be paid back when it is evicted, or every row drifts upward
/// for as long as the machine is up and the biggest number on the screen is
/// the one that churns, not the one that leaks.
#[test]
fn una_muestra_desalojada_se_le_devuelve_a_su_sitio() {
    let t = Table::new();
    let (a, b) = (chain(1), chain(2));
    let p = blk(1);
    let colision = p + (SAMPLES << 4);
    t.record(p, 512, a);
    t.record(colision, 64, b);
    assert_eq!(row_for(&t, a), (0, 0), "el desalojado sigue cargado");
    assert_eq!(row_for(&t, b), (1, 64));
    // And the evicted pointer is no longer claimed, so its free is a no-op
    // instead of subtracting a second time.
    assert!(!t.tracks(p));
    assert!(!t.forget(p));
    assert_eq!(row_for(&t, b), (1, 64));
}

#[test]
fn dos_frees_del_mismo_bloque_no_restan_dos_veces() {
    let t = Table::new();
    let c = chain(1);
    t.record(blk(1), 512, c);
    t.record(blk(2), 512, c);
    assert_eq!(row_for(&t, c), (2, 1024));
    assert!(t.forget(blk(1)));
    assert!(!t.forget(blk(1)));
    assert_eq!(row_for(&t, c), (1, 512));
}

#[test]
fn el_muestreo_coge_uno_de_cada_mil_veinticuatro() {
    let t = Table::new();
    let n = 1 << SAMPLE_SHIFT;
    let cogidos = (0..4 * n).filter(|_| t.sampled()).count();
    assert_eq!(cogidos, 4);
}

#[test]
fn la_clave_de_una_cadena_nunca_es_cero() {
    // Zero is "free slot": a chain that hashed to it would make every site
    // after it unclaimable and the whole report empty.
    assert_ne!(key_of(&[0; FRAMES]), 0);
    for i in 0..256 {
        assert_ne!(key_of(&chain(i)), 0);
    }
    assert_ne!(key_of(&chain(1)), key_of(&chain(2)));
}

#[test]
fn un_puntero_nulo_no_se_registra() {
    let t = Table::new();
    t.record(0, 512, chain(1));
    let (rows, _) = t.rows();
    assert!(rows.iter().all(|(live, _, _)| *live == 0));
}
