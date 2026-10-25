//! Lo que `populate` hace y solo se ve con un driver de mentira.
//!
//! El recorrido ya esta probado de punta a punta por los tests de
//! `btrfs_image` y `eclipsefs_image`, que construyen la imagen y la releen con
//! el driver del propio arbol: el orden alfabetico, los modos, los enlaces, el
//! anidamiento, una imagen demasiado pequena y el socket que se queda fuera
//! estan alli.
//!
//! Lo que esos no pueden montar es un driver que contesta MAL. Un `write` de
//! verdad escribe el trozo entero o se queda sin sitio; ninguno acepta siete
//! bytes de cien teniendo hueco para mas. Y ese es justamente el camino que
//! guarda contra la corrupcion silenciosa que describe el comentario de
//! `populate`: avanzar por `chunk.len()` en vez de por lo escrito deja el
//! fichero del tamano correcto con el contenido corrido, y el build no dice
//! nada. Para verlo hace falta un `TreeFs` que apunte lo que se le pide y
//! conteste lo que el test quiera.
//!
//! Dos ramas se quedan fuera a proposito: los nodos de dispositivo piden
//! `mknod` y root, y un FIFO pide `mkfifo`, que no esta en `std` y para el que
//! `xtask` no tiene `libc`. Son el unico trozo del recorrido sin probar.

use super::*;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// Lo que el recorrido pidio crear.
struct Apunte {
    ino: u64,
    dir: u64,
    nombre: String,
    tipo: &'static str,
    mode: u32,
}

fn tipo_de(kind: &Node) -> &'static str {
    match kind {
        Node::Dir => "dir",
        Node::File => "file",
        Node::Fifo => "fifo",
        Node::BlockDevice => "block",
        Node::CharDevice => "char",
    }
}

/// Un `TreeFs` que no escribe en ningun disco: apunta lo que se le pide y
/// contesta lo que el test le haya dicho.
#[derive(Default)]
struct Falso {
    creados: Vec<Apunte>,
    enlaces: Vec<(u64, String, Vec<u8>)>,
    /// ino -> lo que de verdad quedo escrito, cada byte en su sitio.
    datos: BTreeMap<u64, Vec<u8>>,
    /// ino, offset y cuantos bytes acepto cada `write`, en orden.
    escrituras: Vec<(u64, u64, usize)>,
    siguiente: u64,
    /// Cuanto acepta cada `write` como maximo. `None` acepta el trozo entero.
    tope: Option<usize>,
    /// La escritura numero N, contando desde cero, acepta cero bytes.
    se_llena_en: Option<usize>,
}

impl Falso {
    fn nuevo() -> Self {
        Falso {
            siguiente: 1,
            ..Default::default()
        }
    }

    fn con_tope(tope: usize) -> Self {
        Falso {
            tope: Some(tope),
            ..Falso::nuevo()
        }
    }

    fn creado(&self, nombre: &str) -> &Apunte {
        self.creados
            .iter()
            .find(|a| a.nombre == nombre)
            .unwrap_or_else(|| panic!("{nombre} no se creo"))
    }

    fn contenido(&self, nombre: &str) -> &[u8] {
        let ino = self.creado(nombre).ino;
        self.datos.get(&ino).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

impl TreeFs for Falso {
    const NAME: &'static str = "de mentira";

    fn create(
        &mut self,
        dir: u64,
        name: &str,
        kind: Node,
        mode: u32,
        _rdev: u64,
    ) -> Result<u64, String> {
        self.siguiente += 1;
        let ino = self.siguiente;
        self.creados.push(Apunte {
            ino,
            dir,
            nombre: name.to_string(),
            tipo: tipo_de(&kind),
            mode,
        });
        Ok(ino)
    }

    fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<(), String> {
        self.enlaces.push((dir, name.to_string(), target.to_vec()));
        Ok(())
    }

    fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, String> {
        if self.se_llena_en == Some(self.escrituras.len()) {
            self.escrituras.push((ino, offset, 0));
            return Ok(0);
        }
        let acepta = self.tope.map_or(data.len(), |t| t.min(data.len()));
        let buf = self.datos.entry(ino).or_default();
        let fin = offset as usize + acepta;
        if buf.len() < fin {
            buf.resize(fin, 0);
        }
        buf[offset as usize..fin].copy_from_slice(&data[..acepta]);
        self.escrituras.push((ino, offset, acepta));
        Ok(acepta)
    }
}

/// Un directorio de escenario propio, uno por test.
fn escenario(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("eclipse-tree-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).expect("un directorio propio");
    p
}

/// Un `write` que acepta menos de lo que se le da se reintenta hasta meter el
/// trozo entero, y cada reintento sigue donde quedo el anterior.
#[test]
fn una_escritura_corta_se_reintenta_hasta_meter_todo_el_trozo() {
    let dir = escenario("corta");
    let patron: Vec<u8> = (0..100u32).map(|i| (i % 251) as u8).collect();
    fs::write(dir.join("fichero"), &patron).expect("un fichero con patron");

    let mut falso = Falso::con_tope(7);
    populate(&mut falso, 1, &dir);

    assert_eq!(
        falso.contenido("fichero"),
        &patron[..],
        "el contenido salio corrido"
    );
    assert_eq!(
        falso.escrituras.len(),
        15,
        "cien bytes de siete en siete son quince escrituras: {:?}",
        falso.escrituras
    );
    let mut esperado = 0u64;
    for (_, offset, n) in &falso.escrituras {
        assert_eq!(
            *offset, esperado,
            "una escritura empezo donde no tocaba: {:?}",
            falso.escrituras
        );
        esperado += *n as u64;
    }
    let _ = fs::remove_dir_all(&dir);
}

/// El extremo del mismo camino, de byte en byte.
#[test]
fn una_escritura_de_un_byte_cada_vez_no_desordena_nada() {
    let dir = escenario("un-byte");
    let patron = b"0123456789abcdef".to_vec();
    fs::write(dir.join("f"), &patron).expect("un fichero");

    let mut falso = Falso::con_tope(1);
    populate(&mut falso, 1, &dir);

    assert_eq!(falso.contenido("f"), &patron[..]);
    assert_eq!(falso.escrituras.len(), patron.len());
    let _ = fs::remove_dir_all(&dir);
}

/// Y una escritura que no avanza es el disco lleno, que se dice en vez de
/// girar para siempre. El mensaje se comprueba ENTERO porque es lo unico que
/// el build deja: que fichero, en que byte se quedo, que imagen se lleno y
/// cuanto faltaba. **Asi se encontro que el sitio de la llamada pasaba el
/// nombre de la imagen y el offset en el orden cambiado**, asi que decia
/// "@ de mentira" donde va el byte y "la imagen 0" donde va la imagen.
#[test]
fn una_escritura_que_no_avanza_es_disco_lleno_y_lo_dice() {
    let dir = escenario("llena");
    let f = dir.join("f");
    fs::write(&f, b"algo que no cabe").expect("un fichero de dieciseis bytes");

    // De cinco en cinco y la tercera no avanza: se queda en el byte diez con
    // seis sin escribir.
    let mut falso = Falso {
        se_llena_en: Some(2),
        ..Falso::con_tope(5)
    };
    let caido = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        populate(&mut falso, 1, &dir);
    }));
    let caida = caido.expect_err("una escritura que no avanza tiene que parar el build");
    let mensaje = caida
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| caida.downcast_ref::<&str>().copied())
        .unwrap_or("(el panico no llevaba texto)");

    assert_eq!(
        mensaje,
        format!(
            "write {:?} @ 10 no avanzo: la imagen de mentira es demasiado pequena \
             (6 bytes del fichero sin escribir)",
            f
        ),
        "el mensaje del disco lleno no dice lo que tiene que decir"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Los trozos son de un mega y cada uno va a su offset, que es lo que hace
/// que un fichero grande no se solape consigo mismo.
#[test]
fn los_trozos_son_de_un_mega_y_cada_uno_va_a_su_offset() {
    let dir = escenario("trozos");
    let mega = 1024 * 1024;
    let mut patron = vec![0xABu8; mega];
    patron.push(0xCD);
    fs::write(dir.join("grande"), &patron).expect("un fichero de un mega y un byte");

    let mut falso = Falso::nuevo();
    populate(&mut falso, 1, &dir);

    let ino = falso.creado("grande").ino;
    assert_eq!(
        falso.escrituras,
        vec![(ino, 0, mega), (ino, mega as u64, 1)],
        "un mega y un byte son dos trozos"
    );
    assert_eq!(falso.contenido("grande"), &patron[..]);
    let _ = fs::remove_dir_all(&dir);
}

/// Un nombre que no es UTF-8 para el recorrido y dice CUAL es el fichero. Sin
/// el nombre, el aviso llega en un build que ha andado decenas de miles.
#[test]
#[should_panic(expected = "nombre de archivo no UTF-8")]
fn un_nombre_que_no_es_utf8_dice_cual_es_el_fichero() {
    let dir = escenario("no-utf8");
    let nombre = std::ffi::OsStr::from_bytes(b"malo\xff\xfe");
    fs::write(dir.join(nombre), b"x").expect("un fichero de nombre no UTF-8");

    let mut falso = Falso::nuevo();
    populate(&mut falso, 1, &dir);
}

/// Se copia el CONTENIDO de `dir` y no `dir`: lo de arriba cuelga del inodo
/// que se pasa, que es la raiz que ya creo quien llama.
#[test]
fn se_copia_el_contenido_del_directorio_y_no_el_directorio() {
    let dir = escenario("contenido");
    fs::write(dir.join("solo-este"), b"x").expect("un fichero");

    let mut falso = Falso::nuevo();
    populate(&mut falso, 7, &dir);

    assert_eq!(falso.creados.len(), 1, "se creo algo mas que el fichero");
    assert_eq!(falso.creados[0].nombre, "solo-este");
    assert_eq!(
        falso.creados[0].dir, 7,
        "cuelga del inodo que se paso, no de uno inventado"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Al bajar un nivel se baja con el inodo que devolvio `create`, no con el de
/// arriba: con el de arriba el arbol sale plano y todo acaba en la raiz.
#[test]
fn el_recorrido_baja_con_el_inodo_que_devolvio_create() {
    let dir = escenario("anidado");
    fs::create_dir_all(dir.join("uno/dos")).expect("dos niveles");
    fs::write(dir.join("uno/dos/hoja"), b"x").expect("una hoja");

    let mut falso = Falso::nuevo();
    populate(&mut falso, 1, &dir);

    let raiz = falso.creado("uno").ino;
    let medio = falso.creado("dos").ino;
    assert_eq!(falso.creado("uno").dir, 1);
    assert_eq!(falso.creado("dos").dir, raiz, "dos cuelga de uno");
    assert_eq!(falso.creado("hoja").dir, medio, "la hoja cuelga de dos");
    assert_eq!(falso.creado("uno").tipo, "dir");
    assert_eq!(falso.creado("hoja").tipo, "file");
    let _ = fs::remove_dir_all(&dir);
}

/// Del modo se queda solo con los doce bits de permiso. `st_mode` trae
/// tambien el tipo, y un `0o100644` pasado tal cual a la imagen no es el modo
/// de nadie.
#[test]
fn del_modo_se_queda_solo_los_doce_bits_de_permiso() {
    let dir = escenario("modos");
    let f = dir.join("ejecutable");
    fs::write(&f, b"x").expect("un fichero");
    fs::set_permissions(&f, fs::Permissions::from_mode(0o4755)).expect("setuid y 755");

    let mut falso = Falso::nuevo();
    populate(&mut falso, 1, &dir);

    assert_eq!(
        falso.creado("ejecutable").mode,
        0o4755,
        "el setuid es parte del modo y tiene que llegar"
    );
    assert_eq!(
        falso.creado("ejecutable").mode & 0o170000,
        0,
        "el tipo de fichero no es parte del modo"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Un enlace va por `symlink` con su destino en bytes, y no se crea ni se
/// escribe nada: en la imagen un enlace no tiene contenido.
#[test]
fn un_enlace_va_por_symlink_y_no_se_crea_ni_se_escribe() {
    let dir = escenario("enlace");
    fs::write(dir.join("destino"), b"x").expect("un destino");
    std::os::unix::fs::symlink("destino", dir.join("enlace")).expect("un enlace");

    let mut falso = Falso::nuevo();
    populate(&mut falso, 1, &dir);

    assert_eq!(
        falso.enlaces,
        vec![(1u64, String::from("enlace"), b"destino".to_vec())]
    );
    assert_eq!(
        falso
            .creados
            .iter()
            .filter(|a| a.nombre == "enlace")
            .count(),
        0,
        "un enlace no se crea como nodo"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Y un enlace colgado entra igual, porque el recorrido mira el enlace y no
/// su destino. Seguirlo seria copiar el destino dos veces, o fallar cuando el
/// destino solo existira dentro de la imagen.
#[test]
fn un_enlace_colgado_entra_igual_y_no_se_sigue() {
    let dir = escenario("colgado");
    std::os::unix::fs::symlink("/no/existe", dir.join("colgado")).expect("un enlace colgado");

    let mut falso = Falso::nuevo();
    populate(&mut falso, 1, &dir);

    assert_eq!(falso.enlaces.len(), 1);
    assert_eq!(falso.enlaces[0].2, b"/no/existe".to_vec());
    assert!(
        falso.creados.is_empty(),
        "no se creo nada por seguir el enlace"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Un fichero vacio se crea y no se escribe NUNCA: `chunks` de nada no da ni
/// una vuelta. El driver de este test contesta cero a la primera escritura,
/// asi que si hubiera una, seria el disco lleno y este test panicaria.
#[test]
fn un_fichero_vacio_se_crea_y_no_se_escribe_nunca() {
    let dir = escenario("vacio");
    fs::write(dir.join("vacio"), b"").expect("un fichero vacio");

    let mut falso = Falso {
        se_llena_en: Some(0),
        ..Falso::nuevo()
    };
    populate(&mut falso, 1, &dir);

    assert_eq!(falso.creado("vacio").tipo, "file");
    assert!(
        falso.escrituras.is_empty(),
        "se escribio algo de un fichero vacio: {:?}",
        falso.escrituras
    );
    let _ = fs::remove_dir_all(&dir);
}
