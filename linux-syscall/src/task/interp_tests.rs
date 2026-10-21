//! The interpreter `execve` looks up, of a `#!` script and of a `PT_INTERP`,
//! is looked up as `open_exec` looks it up in Linux 6.18: from the working
//! directory when its path is relative, and asking the caller for search
//! permission on every directory on the way (`interp-survey`, measured there
//! with glibc and musl, 14 of 14 both).
//!
//! Both halves were missing. The loader looked every interpreter up from the
//! root and asked nothing, so a user ran an interpreter kept in another
//! user's 0700 directory, and `#!ld` ran `/ld` from anywhere.
//!
//! Fixed pids live in 44_20x here.

use super::*;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

#[cfg(target_arch = "x86_64")]
const EM_HOST: u16 = 62;
#[cfg(target_arch = "aarch64")]
const EM_HOST: u16 = 183;
#[cfg(target_arch = "riscv64")]
const EM_HOST: u16 = 243;

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;

/// An ELF64 image for this machine: a `PT_LOAD` over the whole file and, if
/// `interp` is given, a `PT_INTERP` naming it.
fn an_elf(interp: Option<&str>) -> Vec<u8> {
    let phnum = 1 + interp.is_some() as usize;
    let mut v = vec![0u8; 64 + 56 * phnum];
    v[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    v[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    v[18..20].copy_from_slice(&EM_HOST.to_le_bytes());
    v[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    v[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    v[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    v[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
    if let Some(interp) = interp {
        let at = 64 + 56;
        let offset = v.len() as u64;
        v.extend_from_slice(interp.as_bytes());
        v.push(0);
        v[at..at + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
        v[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
        v[at + 32..at + 40].copy_from_slice(&(interp.len() as u64 + 1).to_le_bytes());
    }
    let len = v.len() as u64;
    v[64..68].copy_from_slice(&PT_LOAD.to_le_bytes());
    v[96..104].copy_from_slice(&len.to_le_bytes()); // p_filesz
    v
}

/// A process with, in its root:
/// - `shut`, a directory 0700 of uid 2000, and `open`, 0711 of the same
///   uid, each holding `ld`, a static program anybody may run;
/// - `ld` itself nowhere else.
fn a_process(pid: KoID) -> Arc<Process> {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "interp",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let root = proc.linux().root_inode().clone();
    for (name, mode) in [("shut", 0o700), ("open", 0o711)] {
        let dir = root.create(name, FileType::Dir, mode).unwrap();
        let mut meta = dir.metadata().unwrap();
        (meta.mode, meta.uid, meta.gid) = (mode as u16, 2000, 2000);
        dir.set_metadata(&meta).unwrap();
        let ld = dir.create("ld", FileType::File, 0o755).unwrap();
        ld.write_at(0, &an_elf(None)).unwrap();
    }
    proc
}

/// `/name` in `proc`'s root, holding `bytes`, which anybody may run.
fn a_file(proc: &Arc<Process>, name: &str, bytes: &[u8]) -> Arc<dyn INode> {
    let file = proc
        .linux()
        .root_inode()
        .create(name, FileType::File, 0o755)
        .unwrap();
    file.write_at(0, bytes).unwrap();
    file
}

/// What `execve` answers before its line for a script `#!interp` and for a
/// program whose `PT_INTERP` is `interp`.
fn both(proc: &Arc<Process>, interp: &str) -> [LxResult<()>; 2] {
    let n = proc.linux().root_inode().list().unwrap().len();
    let script = a_file(
        proc,
        &alloc::format!("s{}", n),
        alloc::format!("#!{}\n", interp).as_bytes(),
    );
    let program = a_file(proc, &alloc::format!("p{}", n), &an_elf(Some(interp)));
    [&script, &program].map(|image| execve_preflight(image, 0, proc).map(|_| ()))
}

fn a_user(proc: &Arc<Process>) {
    proc.linux().set_resuid(1000, 1000, 1000).unwrap();
}

#[test]
fn an_interpreter_in_a_directory_the_caller_may_not_search_is_eacces() {
    let proc = a_process(44_201);
    a_user(&proc);
    assert_eq!(
        both(&proc, "/shut/ld"),
        [Err(LxError::EACCES), Err(LxError::EACCES)]
    );
    // The directory answers before its contents do.
    assert_eq!(
        both(&proc, "/shut/none"),
        [Err(LxError::EACCES), Err(LxError::EACCES)]
    );
}

#[test]
fn a_directory_searched_but_not_read_lets_its_interpreter_through() {
    let proc = a_process(44_202);
    a_user(&proc);
    assert_eq!(both(&proc, "/open/ld"), [Ok(()), Ok(())]);
    assert_eq!(
        both(&proc, "/open/none"),
        [Err(LxError::ENOENT), Err(LxError::ENOENT)]
    );
}

#[test]
fn root_searches_any_directory_for_its_interpreter() {
    let proc = a_process(44_203);
    assert_eq!(both(&proc, "/shut/ld"), [Ok(()), Ok(())]);
}

#[test]
fn a_relative_interpreter_is_looked_up_from_the_working_directory() {
    let proc = a_process(44_204);
    assert_eq!(
        both(&proc, "ld"),
        [Err(LxError::ENOENT), Err(LxError::ENOENT)]
    );
    proc.linux().change_directory("/open");
    assert_eq!(both(&proc, "ld"), [Ok(()), Ok(())]);
    assert_eq!(both(&proc, "./ld"), [Ok(()), Ok(())]);
    assert_eq!(both(&proc, "../shut/ld"), [Ok(()), Ok(())]);
}

#[test]
fn a_relative_interpreter_is_asked_about_from_the_working_directory_on() {
    // A user in `/open` may not walk on into `/shut` by `..`; the working
    // directory itself is not walked, so `ld` from it goes through.
    let proc = a_process(44_205);
    proc.linux().change_directory("/open");
    a_user(&proc);
    assert_eq!(both(&proc, "ld"), [Ok(()), Ok(())]);
    assert_eq!(
        both(&proc, "../shut/ld"),
        [Err(LxError::EACCES), Err(LxError::EACCES)]
    );
}

#[test]
fn an_interpreter_that_is_a_link_is_followed_and_asked_about_on_the_way() {
    // `open_exec` follows the last name like any open without `O_NOFOLLOW`:
    // a link to `ld` runs it, and a link into `/shut` is refused there.
    let proc = a_process(44_206);
    let open = proc.linux().root_inode().lookup("open").unwrap();
    for (name, target) in [("via", "ld"), ("into-shut", "/shut/ld")] {
        let link = open.create(name, FileType::SymLink, 0o777).unwrap();
        link.write_at(0, target.as_bytes()).unwrap();
    }
    a_user(&proc);
    assert_eq!(both(&proc, "/open/via"), [Ok(()), Ok(())]);
    assert_eq!(
        both(&proc, "/open/into-shut"),
        [Err(LxError::EACCES), Err(LxError::EACCES)]
    );
}
