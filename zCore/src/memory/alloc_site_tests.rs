//! The small-block call-site table.
//!
//! What these are about: the table exists to answer "who is holding the
//! heap", and the only answers worth printing are LIVE blocks. A table that
//! counted allocations would put the busiest cache at the top of the report
//! and the leak somewhere below it. So the tests that matter are the ones
//! about giving a block back -- on free, and on the eviction of a sampled
//! block from the direct-mapped table, which is the case nobody would think
//! to write and the one that silently inflates every row if it is wrong.

use super::alloc_sites::{key_of, slot_of, Table, FRAMES, SAMPLES, SAMPLE_SHIFT, SITES};

/// A heap-looking pointer. `distintos` below is what keeps a test from
/// accidentally testing eviction instead of what it says.
fn blk(i: usize) -> usize {
    0x10_0000 + i * 16
}

/// Whether these pointers all land in different slots.
fn distintos(ptrs: &[usize]) -> bool {
    ptrs.iter()
        .enumerate()
        .all(|(i, a)| ptrs[..i].iter().all(|b| slot_of(*a) != slot_of(*b)))
}

/// A pointer that shares `p`'s slot. There is no arithmetic for this once
/// the index is a hash, so it is searched for -- and not finding one is a
/// test failure, not a silent pass.
fn colision_con(p: usize) -> usize {
    (1..1_000_000)
        .map(|i| p + i * 16)
        .find(|q| slot_of(*q) == slot_of(p))
        .expect("ninguna colision en un millon de bloques")
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
    assert!(distintos(&[blk(1), blk(2), blk(3)]));
    t.record(blk(1), 512, a);
    t.record(blk(2), 600, a);
    t.record(blk(3), 64, b);
    assert_eq!(row_for(&t, a), (2, 1112));
    assert_eq!(row_for(&t, b), (1, 64));
}

#[test]
fn las_filas_salen_de_mas_bytes_a_menos() {
    let t = Table::new();
    assert!(distintos(&[blk(1), blk(2), blk(3)]));
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
    let ptrs: alloc::vec::Vec<usize> = (0..SITES).map(blk).collect();
    assert!(distintos(&ptrs));
    for (i, p) in ptrs.iter().enumerate() {
        t.record(*p, 8, chain(i));
    }
    let (_, missed) = t.rows();
    assert_eq!(missed, 0);
    // One chain too many: it must be counted, not silently dropped.
    t.record(blk(500), 8, chain(SITES));
    let (rows, missed) = t.rows();
    assert_eq!(missed, 1);
    assert_eq!(rows.iter().filter(|(live, _, _)| *live > 0).count(), SITES);
}

/// Found by running it: on an idle boot the table filled with churn within
/// seconds and from then on counted EVERY sample as missed. A leak that
/// starts ten minutes into a desktop session would never get a slot -- the
/// report would be a list of the busiest callers and the one it exists to
/// name would be in the "found no slot" line.
#[test]
fn un_sitio_sin_nada_vivo_le_cede_el_hueco_al_siguiente() {
    let t = Table::new();
    let ptrs: alloc::vec::Vec<usize> = (0..SITES).map(blk).collect();
    assert!(distintos(&ptrs));
    for (i, p) in ptrs.iter().enumerate() {
        t.record(*p, 8, chain(i));
    }
    // The first site gives its only block back: it holds nothing now.
    assert!(t.forget(blk(0)));
    t.record(blk(500), 64, chain(SITES));
    let (_, missed) = t.rows();
    assert_eq!(missed, 0, "la tabla llena no cedio el hueco muerto");
    assert_eq!(row_for(&t, chain(SITES)), (1, 64));
    assert_eq!(row_for(&t, chain(0)), (0, 0));
    // And the new owner starts from zero, not from what the dead site left.
    t.record(blk(501), 64, chain(SITES));
    assert_eq!(row_for(&t, chain(SITES)), (2, 128));
}

/// The other half: a site that still holds something is NOT evicted, however
/// full the table is. Counting a live site out would lose exactly the rows
/// worth printing.
#[test]
fn un_sitio_con_bloques_vivos_no_se_desaloja() {
    let t = Table::new();
    let ptrs: alloc::vec::Vec<usize> = (0..SITES).map(blk).collect();
    assert!(distintos(&ptrs));
    for (i, p) in ptrs.iter().enumerate() {
        t.record(*p, 8, chain(i));
    }
    t.record(blk(500), 64, chain(SITES));
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
    let colision = colision_con(p);
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
    assert!(distintos(&[blk(1), blk(2)]));
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

/// The failure this table would have had exactly where it is pointed: the
/// buddy hands out power-of-two blocks at their own alignment, and an index
/// taken from the low address bits gives every 1 KiB block one of 64 slots
/// and every 64 KiB block the SAME slot. The samples would evict each other
/// long before the table was full, and the report would be built on a
/// handful of blocks.
#[test]
fn los_bloques_alineados_usan_toda_la_tabla() {
    for align in [512usize, 1024, 4096, 65536] {
        let mut vistos = alloc::vec::Vec::new();
        for i in 0..SAMPLES {
            let s = slot_of(0x40_0000 + i * align);
            if !vistos.contains(&s) {
                vistos.push(s);
            }
        }
        assert!(
            vistos.len() > SAMPLES / 2,
            "bloques de {} B alcanzan solo {} de {} huecos",
            align,
            vistos.len(),
            SAMPLES
        );
    }
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

/// The one thing a single-threaded test cannot ask: that two CPUs landing on
/// the same slot do not both charge it. Before the slot had an owner, both
/// found it empty, both added a block, and only one pointer stayed findable
/// — so the other charge could never be freed and the row grew for ever.
///
/// The invariant is the one the report rests on: once every sampled block
/// has been given back, NOTHING is live. Drift is allowed downward (a
/// dropped sample is an under-count) and never upward.
#[test]
fn dos_cpus_en_el_mismo_hueco_no_dejan_un_bloque_cargado() {
    use std::sync::Arc as StdArc;

    let t = StdArc::new(Table::new());
    let p = blk(1);
    let q = colision_con(p);
    let hilos: std::vec::Vec<_> = [(p, 1usize), (q, 2usize)]
        .into_iter()
        .map(|(ptr, seed)| {
            let t = t.clone();
            std::thread::spawn(move || {
                for _ in 0..20_000 {
                    t.record(ptr, 512, chain(seed));
                    t.forget(ptr);
                }
            })
        })
        .collect();
    for h in hilos {
        h.join().unwrap();
    }
    // Whatever survived the races, free it, then nothing may be left.
    t.forget(p);
    t.forget(q);
    let (rows, _) = t.rows();
    let vivos: usize = rows.iter().map(|(live, _, _)| *live).sum();
    assert_eq!(vivos, 0, "quedo un bloque cargado a un sitio");
    let bytes: usize = rows.iter().map(|(_, b, _)| *b).sum();
    assert_eq!(bytes, 0);
}
