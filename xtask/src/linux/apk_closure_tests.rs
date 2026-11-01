use super::apk_closure_fits_image;

/// El arco que rompio: su imagen va de `-initrd` en un invitado de 1 GiB.
#[test]
fn riscv64_no_carga_el_cierre_de_apk() {
    assert!(
        !apk_closure_fits_image("riscv64"),
        "la imagen de riscv64 la carga QEMU en la RAM del invitado (-initrd, -m 1G); \
         con el cierre del escritorio dentro sale de 1,3 GiB y no arranca"
    );
}

/// Y el que NO hay que volver a dejar sin paquetes: el #1758 existe por eso.
#[test]
fn aarch64_si_lo_carga() {
    assert!(
        apk_closure_fits_image("aarch64"),
        "aarch64 monta su imagen por virtio-blk, fuera de la RAM, y su ISO de \
         escritorio necesita el cierre: dejarla sin el es el bug que arreglo el #1758"
    );
}

#[test]
fn x86_64_si_lo_carga() {
    assert!(apk_closure_fits_image("x86_64"));
}

/// El techo es de UN arco, no «todo lo que no sea x86_64».
#[test]
fn no_es_un_corte_por_todo_lo_que_no_sea_x86() {
    let fuera: Vec<&str> = ["x86_64", "aarch64", "riscv64"]
        .into_iter()
        .filter(|a| !apk_closure_fits_image(a))
        .collect();
    assert_eq!(
        fuera,
        vec!["riscv64"],
        "solo riscv64 tiene techo de RAM; si aparece otro, que sea con su motivo escrito"
    );
}
