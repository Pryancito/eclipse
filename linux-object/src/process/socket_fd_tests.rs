//! Which descriptor a new socket gets, and why it is not always 1000.
//!
//! Sockets are handed out from [`SOCKET_FD`] up because poll, select and
//! epoll used to recognise a socket by its NUMBER. A descriptor at 1000 is
//! one `insert_file` refuses outright when the process's soft
//! `RLIMIT_NOFILE` does not reach that far -- and PulseAudio drops its own
//! to 256 (`rlimit-nofile = 256`, its built-in default) before it does
//! anything else. So the first socket musl opened inside `initgroups()`,
//! asking nscd about the `pulse` user, came back `EMFILE` with three
//! descriptors open:
//!
//! ```text
//! E: [pulseaudio] main.c: Failed to change group list: No file descriptors available
//! ```
//!
//! which is musl's wording for `EMFILE`. init respawned it twenty times and
//! gave up; the machine booted with no sound server, and Firefox's cubeb
//! failed to open a stream and took its whole media decode down with it.

use super::*;

/// The ordinary process: the preferred descriptor is below the limit, so
/// the socket base is honoured and nothing about the numbering changes.
#[test]
fn con_el_limite_normal_el_socket_sigue_saliendo_en_el_mil() {
    let fd = socket_fd(FileDesc::from(SOCKET_FD as i32), 1024, || {
        panic!("no debia hacer falta el hueco bajo")
    });
    assert_eq!(usize::from(fd), SOCKET_FD);
}

/// PulseAudio's process: a soft limit of 256 cannot hold a descriptor at
/// 1000, so the socket takes the lowest free one instead of being refused.
#[test]
fn con_un_limite_por_debajo_del_mil_el_socket_cae_al_hueco_bajo() {
    let fd = socket_fd(FileDesc::from(SOCKET_FD as i32), 256, || FileDesc::from(3));
    assert_eq!(
        usize::from(fd),
        3,
        "un limite de 256 dejaba al proceso sin un solo socket: EMFILE con tres fds abiertos"
    );
}

/// The ceiling the base imposed on everybody else: with the default soft
/// limit of 1024 only 1000..=1023 were sockets, so the 25th was `EMFILE`
/// however empty the rest of the table was.
#[test]
fn el_socket_numero_25_del_limite_por_defecto_ya_no_es_emfile() {
    // Every slot from 1000 to 1023 taken: the search lands on 1024, which is
    // exactly the limit.
    let fd = socket_fd(FileDesc::from(1024), 1024, || FileDesc::from(7));
    assert_eq!(usize::from(fd), 7);
}

/// The boundary itself: a descriptor EQUAL to the limit is one too many
/// (`insert_file` refuses `fd >= nofile`), and one below it is fine.
#[test]
fn el_limite_es_exclusivo_por_abajo_y_por_arriba() {
    assert_eq!(
        usize::from(socket_fd(FileDesc::from(999), 1000, || FileDesc::from(4))),
        999
    );
    assert_eq!(
        usize::from(socket_fd(FileDesc::from(1000), 1000, || FileDesc::from(4))),
        4
    );
}

/// The whole path, on a real fd table: a process whose soft limit is 256
/// asks for a socket and gets one. This is the bug as PulseAudio hit it —
/// before the fallback, `add_socket` picked 1000 and `insert_file` turned it
/// into `EMFILE` with three descriptors open.
#[test]
fn un_proceso_con_limite_de_256_consigue_su_socket() {
    use super::dup_fd_tests::{a_process, an_open_log, Log};

    let proc = a_process();
    {
        let mut inner = proc.inner.lock();
        inner.limits.0[RLIMIT_NOFILE] = RLimit { cur: 256, max: 256 };
    }
    // stdin/stdout/stderr, the way a service starts.
    for _ in 0..3 {
        proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
            .unwrap();
    }

    let fd = proc
        .add_socket(an_open_log(Log::new(), OpenFlags::RDWR))
        .expect("EMFILE con la tabla practicamente vacia: el fallo de pulseaudio");
    assert!(
        usize::from(fd) < 256,
        "el socket salio en el {}, que es un descriptor que este proceso no puede tener",
        usize::from(fd)
    );
}

/// And the default process is unchanged: three open files, and the socket
/// still lands on the socket base.
#[test]
fn un_proceso_con_el_limite_de_fabrica_sigue_viendo_sus_sockets_en_el_mil() {
    use super::dup_fd_tests::{a_process, an_open_log, Log};

    let proc = a_process();
    for _ in 0..3 {
        proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
            .unwrap();
    }
    let fd = proc
        .add_socket(an_open_log(Log::new(), OpenFlags::RDWR))
        .unwrap();
    assert_eq!(usize::from(fd), SOCKET_FD);
}
