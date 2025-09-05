#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::fmt::Write;

// VGA Driver simplificado
pub struct VgaWriter {
    buffer: *mut u16,
    position: usize,
}

impl VgaWriter {
    pub const fn new() -> Self {
        Self {
            buffer: 0xb8000 as *mut u16,
            position: 0,
        }
    }

    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.position = (self.position / 80 + 1) * 80;
            } else {
                if self.position < 2000 {
                    unsafe {
                        *self.buffer.add(self.position) = 0x0F00 | byte as u16;
                    }
                    self.position += 1;
                }
            }
        }
    }

    pub fn set_color(&mut self, _fg: u8, _bg: u8) {
        // Simplificado - solo blanco sobre negro
    }

    pub fn clear_screen(&mut self) {
        for i in 0..2000 {
            unsafe {
                *self.buffer.add(i) = 0x0F00 | b' ' as u16;
            }
        }
        self.position = 0;
    }
}

static mut VGA: VgaWriter = VgaWriter::new();

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe {
        VGA.clear_screen();
        VGA.write_string("Eclipse OS Kernel v0.4.0\n");
        VGA.write_string("============================\n\n");
        
        VGA.write_string("✓ Kernel inicializado correctamente\n");
        VGA.write_string("✓ VGA Driver funcionando\n");
        VGA.write_string("✓ Sistema en modo texto\n\n");
        
        VGA.write_string("Características:\n");
        VGA.write_string("- Kernel Rust no_std\n");
        VGA.write_string("- Drivers modulares\n");
        VGA.write_string("- Userland con std\n");
        VGA.write_string("- IPC entre kernel y userland\n\n");
        
        VGA.write_string("Sistema funcionando correctamente!\n");
        VGA.write_string("Presiona cualquier tecla para continuar...\n");
    }

    loop {
        // Bucle infinito del kernel
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe {
        VGA.write_string("\n\nKERNEL PANIC!\n");
        VGA.write_string("==============\n");
        
        if let Some(location) = info.location() {
            VGA.write_string("Ubicación: ");
            VGA.write_string(location.file());
            VGA.write_string(":");
            VGA.write_string(&int_to_string(location.line()));
            VGA.write_string("\n");
        }
        
        VGA.write_string("Mensaje: Kernel panic detectado\n");
        VGA.write_string("Reinicia el sistema para continuar.\n");
    }

    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

fn int_to_string(mut num: u32) -> heapless::String<32> {
    let mut result = heapless::String::<32>::new();
    if num == 0 {
        let _ = result.push_str("0");
        return result;
    }
    while num > 0 {
        let digit = (num % 10) as u8;
        let _ = result.push((digit + b'0') as char);
        num /= 10;
    }
    let mut reversed = heapless::String::<32>::new();
    for &byte in result.as_bytes().iter().rev() {
        let _ = reversed.push(byte as char);
    }
    reversed
}


