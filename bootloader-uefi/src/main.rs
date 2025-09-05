#![no_std]
#![no_main]

use core::fmt::Write;
use core::slice;
use uefi::prelude::*;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, Directory, RegularFile, FileAttribute, FileInfo, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{AllocateType, MemoryType};
use uefi::CString16;

// Global allocator simple
struct SimpleAllocator;

unsafe impl core::alloc::GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
        // No-op
    }
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

// Panic handler
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

const KERNEL_PHYS_LOAD_ADDR: u64 = 0x0010_0000;

#[inline(always)]
fn pages_for_size(size: usize) -> usize { (size + 0xFFF) / 0x1000 }

fn open_root_fs(bs: &BootServices) -> uefi::Result<Directory> {
    let image = bs.open_protocol_exclusive::<LoadedImage>(bs.image_handle())?;
    let device_handle = image.device().expect("LoadedImage without device handle");
    let mut fs = bs.open_protocol_exclusive::<SimpleFileSystem>(device_handle)?;
    fs.open_volume()
}

fn open_kernel_file(root: &mut Directory) -> uefi::Result<(RegularFile, CString16)> {
    let candidates = [
        "\\eclipse_kernel",
        "\\EFI\\BOOT\\eclipse_kernel",
        "\\boot\\eclipse_kernel",
    ];
    for path in candidates.iter() {
        if let Ok(p) = CString16::try_from(*path) {
            if let Ok(file) = root.open(&p, FileMode::Read, FileAttribute::READ_ONLY) {
                if let Some(reg) = file.into_regular_file() {
                    return Ok((reg, p));
                }
            }
        }
    }
    Err(uefi::Status::NOT_FOUND.into())
}

fn read_file_size(file: &mut RegularFile) -> Result<usize, Status> {
    let mut info_buf = [0u8; 1024];
    match file.get_info::<FileInfo>(&mut info_buf) {
        Ok(info) => Ok(info.file_size() as usize),
        Err(e) => Err(e.status()),
    }
}

unsafe fn jump_to_kernel(entry: u64) -> ! {
    let entry_fn: extern "sysv64" fn() -> ! = core::mem::transmute(entry as usize);
    entry_fn()
}

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Mensaje inicial
    {
        let mut out = system_table.stdout();
        let _ = out.write_str("Eclipse OS Bootloader UEFI\n");
        let _ = out.write_str("Cargando kernel raw...\n");
    }

    // Bloque sin usar stdout para evitar conflictos de préstamos
    {
        let bs = system_table.boot_services();

        // Abrir raíz del FS
        let mut root = match open_root_fs(bs) {
            Ok(v) => v,
            Err(e) => return e.status(),
        };

        // Abrir kernel
        let (mut kernel_file, _kernel_path) = match open_kernel_file(&mut root) {
            Ok(v) => v,
            Err(e) => return e.status(),
        };

        // Tamaño del kernel
        let kernel_size = match read_file_size(&mut kernel_file) {
            Ok(s) => s,
            Err(st) => return st,
        };

        // Reservar páginas en la dirección física fija (1 MiB)
        let num_pages = pages_for_size(kernel_size);
        if let Err(st) = bs.allocate_pages(AllocateType::Address(KERNEL_PHYS_LOAD_ADDR), MemoryType::LOADER_CODE, num_pages) {
            return st.status();
        }

        // Leer el fichero al buffer
        let dst_ptr = KERNEL_PHYS_LOAD_ADDR as *mut u8;
        let dst_slice = unsafe { slice::from_raw_parts_mut(dst_ptr, kernel_size) };
        if let Err(e) = kernel_file.read(dst_slice) {
            return e.status();
        }

        // ExitBootServices pendiente: se añadirá tras validar en HW
    }

    // Saltar al kernel
    unsafe { jump_to_kernel(KERNEL_PHYS_LOAD_ADDR) }
}