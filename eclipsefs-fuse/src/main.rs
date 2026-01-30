//! EclipseFS FUSE Driver
//! Permite montar EclipseFS como un sistema de archivos Unix compatible

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fuse::{FileAttr, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyWrite, ReplyEmpty, ReplyCreate, Request};
use libc::ENOENT;
use nix::unistd::{Uid, Gid};
use time::Timespec;

use eclipsefs_lib::{EclipseFSReader, EclipseFSNode, NodeKind, constants};

struct EclipseFSFuse {
    reader: Arc<Mutex<EclipseFSReader>>,
}

impl EclipseFSFuse {
    fn new(device: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let reader = EclipseFSReader::new(device)?;
        Ok(EclipseFSFuse {
            reader: Arc::new(Mutex::new(reader)),
        })
    }
    
    fn node_to_attr(&self, node: &EclipseFSNode, inode: u64) -> FileAttr {
        let current_uid = Uid::current().as_raw();
        let current_gid = Gid::current().as_raw();
        
        FileAttr {
            ino: inode,
            size: node.size,
            blocks: (node.size + 511) / 512, // Redondear a bloques de 512 bytes
            atime: Timespec { sec: node.atime as i64, nsec: 0 },
            mtime: Timespec { sec: node.mtime as i64, nsec: 0 },
            ctime: Timespec { sec: node.ctime as i64, nsec: 0 },
            crtime: Timespec { sec: node.ctime as i64, nsec: 0 },
            kind: match node.kind {
                NodeKind::File => fuse::FileType::RegularFile,
                NodeKind::Directory => fuse::FileType::Directory,
                NodeKind::Symlink => fuse::FileType::Symlink,
            },
            perm: node.mode as u16,
            nlink: node.nlink,
            uid: current_uid,
            gid: current_gid,
            rdev: 0,
            flags: 0,
        }
    }

    fn lookup_node(&self, parent: u64, name: &str) -> Result<u32, eclipsefs_lib::EclipseFSError> {
        let mut reader = self.reader.lock().unwrap();
        
        // Obtener el nodo padre
        let parent_node = reader.read_node(parent as u32)?;
        
        // Buscar el hijo en el directorio
        parent_node.get_child_inode(name)
            .ok_or(eclipsefs_lib::EclipseFSError::NotFound)
    }
}

const TTL: Timespec = Timespec { sec: 1, nsec: 0 };

impl Filesystem for EclipseFSFuse {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy();
        
        match self.lookup_node(parent, &name_str) {
            Ok(child_inode) => {
                let mut reader = self.reader.lock().unwrap();
                match reader.read_node(child_inode) {
                    Ok(node) => {
                        let attr = self.node_to_attr(&node, child_inode as u64);
                        reply.entry(&TTL, &attr, 0);
                    }
                    Err(_) => {
                        reply.error(ENOENT);
                    }
                }
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let mut reader = self.reader.lock().unwrap();
        
        match reader.read_node(ino as u32) {
            Ok(node) => {
                let attr = self.node_to_attr(&node, ino);
                reply.attr(&TTL, &attr);
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn readdir(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let mut reader = self.reader.lock().unwrap();
        
        match reader.read_node(ino as u32) {
            Ok(node) => {
                if node.kind != NodeKind::Directory {
                    reply.error(ENOENT);
                    return;
                }
                
                let mut entries = Vec::new();
                
                // Agregar entradas especiales
                entries.push((".", ino, fuse::FileType::Directory));
                entries.push(("..", if ino == constants::ROOT_INODE as u64 { ino } else { 1 }, fuse::FileType::Directory));
                
                // Agregar entradas del directorio
                for (name, child_inode) in node.get_children().iter() {
                    let file_type = match reader.read_node(*child_inode) {
                        Ok(child_node) => match child_node.kind {
                            NodeKind::File => fuse::FileType::RegularFile,
                            NodeKind::Directory => fuse::FileType::Directory,
                            NodeKind::Symlink => fuse::FileType::Symlink,
                        },
                        Err(_) => fuse::FileType::RegularFile, // Valor por defecto
                    };
                    
                    entries.push((name.as_str(), *child_inode as u64, file_type));
                }
                
                // Enviar entradas desde el offset
                for (i, (name, inode, file_type)) in entries.iter().enumerate() {
                    if i as i64 >= offset {
                        if reply.add(*inode, (i + 1) as i64, *file_type, name) {
                            break;
                        }
                    }
                }
                
                reply.ok();
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn read(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32, reply: ReplyData) {
        let mut reader = self.reader.lock().unwrap();
        
        match reader.read_node(ino as u32) {
            Ok(node) => {
                if node.kind == NodeKind::File || node.kind == NodeKind::Symlink {
                    let data = node.get_data();
                    let start = offset as usize;
                    let end = std::cmp::min(start + size as usize, data.len());
                    
                    if start < data.len() {
                        reply.data(&data[start..end]);
                    } else {
                        reply.data(&[]);
                    }
                } else {
                    reply.error(ENOENT);
                }
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        let mut reader = self.reader.lock().unwrap();
        
        match reader.read_node(ino as u32) {
            Ok(node) => {
                if node.kind == NodeKind::Symlink {
                    let data = node.get_data();
                    reply.data(data);
                } else {
                    reply.error(ENOENT);
                }
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: u32, reply: ReplyOpen) {
        let mut reader = self.reader.lock().unwrap();
        
        match reader.read_node(ino as u32) {
            Ok(node) => {
                if node.kind == NodeKind::File {
                    reply.opened(ino, 0);
                } else {
                    reply.error(ENOENT);
                }
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn write(&mut self, _req: &Request, _ino: u64, _fh: u64, _offset: i64, _data: &[u8], _flags: u32, reply: ReplyWrite) {
        // EclipseFS no soporta escritura por ahora - devolver operación no soportada
        reply.error(libc::ENOTSUP);
    }

    fn unlink(&mut self, _req: &Request, _parent: u64, _name: &OsStr, reply: ReplyEmpty) {
        // EclipseFS no soporta escritura por ahora - devolver operación no soportada
        reply.error(libc::ENOTSUP);
    }

    fn rmdir(&mut self, _req: &Request, _parent: u64, _name: &OsStr, reply: ReplyEmpty) {
        // EclipseFS no soporta escritura por ahora - devolver operación no soportada
        reply.error(libc::ENOTSUP);
    }

    fn mkdir(&mut self, _req: &Request, _parent: u64, _name: &OsStr, _mode: u32, reply: ReplyEntry) {
        // EclipseFS no soporta escritura por ahora - devolver operación no soportada
        reply.error(libc::ENOTSUP);
    }

    fn create(&mut self, _req: &Request, _parent: u64, _name: &OsStr, _mode: u32, _flags: u32, reply: ReplyCreate) {
        // EclipseFS no soporta escritura por ahora - devolver operación no soportada
        reply.error(libc::ENOTSUP);
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    
    if args.len() != 3 {
        eprintln!("Uso: {} <dispositivo> <punto_montaje>", args[0]);
        eprintln!("Ejemplo: {} /dev/sda2 /mnt/eclipse", args[0]);
        std::process::exit(1);
    }
    
    let device = &args[1];
    let mount_point = &args[2];
    
    println!("Montando EclipseFS desde {} en {}", device, mount_point);
    
    // Verificar que el dispositivo existe
    if !Path::new(device).exists() {
        eprintln!("Error: El dispositivo {} no existe", device);
        std::process::exit(1);
    }
    
    // Crear el driver FUSE
    let fuse_fs = match EclipseFSFuse::new(device) {
        Ok(fs) => fs,
        Err(e) => {
            eprintln!("Error inicializando EclipseFS: {}", e);
            eprintln!("\nPosibles causas:");
            eprintln!("  1. El dispositivo {} requiere permisos de root. Intenta con 'sudo'", device);
            eprintln!("  2. El dispositivo no contiene un sistema de archivos EclipseFS válido");
            eprintln!("  3. El sistema de archivos está corrupto");
            eprintln!("\nPara diagnóstico detallado, ejecuta:");
            eprintln!("  sudo eclipsefs info {}", device);
            std::process::exit(1);
        }
    };
    
    println!("EclipseFS inicializado correctamente");
    
    // Montar el sistema de archivos en modo read-write para permitir instalación
    let options = vec!["-o", "rw", "-o", "allow_other"];
    let fuse_args: Vec<&OsStr> = options.iter().map(|s| OsStr::new(s)).collect();
    
    fuse::mount(fuse_fs, &mount_point, &fuse_args).unwrap();
}