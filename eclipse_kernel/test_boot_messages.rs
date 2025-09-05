// Test program for boot messages system
// This file demonstrates the boot message functionality

use core::sync::atomic::{AtomicU32, Ordering};

// Import the boot message functions
mod boot_messages {
    use core::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum BootColor {
        White,
        Green,
        Yellow,
        Red,
        Cyan,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum BootLevel {
        Info,
        Success,
        Warning,
        Error,
        Debug,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct BootMessage {
        pub level: BootLevel,
        pub component: &'static str,
        pub message: &'static str,
        pub timestamp: u64,
    }

    pub struct BootMessenger {
        pub messages: [Option<BootMessage>; 256],
        pub message_count: AtomicU32,
        pub current_step: AtomicU32,
        pub total_steps: u32,
    }

    impl BootMessenger {
        pub const fn new() -> Self {
            Self {
                messages: [None; 256],
                message_count: AtomicU32::new(0),
                current_step: AtomicU32::new(0),
                total_steps: 10,
            }
        }

        pub fn add_message(&mut self, level: BootLevel, component: &'static str, message: &'static str) {
            let count = self.message_count.load(Ordering::Relaxed) as usize;
            if count < 256 {
                self.messages[count] = Some(BootMessage {
                    level,
                    component,
                    message,
                    timestamp: 0, // Simplified for testing
                });
                self.message_count.store(count as u32 + 1, Ordering::Relaxed);
            }
        }

        pub fn show_info(&mut self, component: &'static str, message: &'static str) {
            self.add_message(BootLevel::Info, component, message);
            self.display_message(BootLevel::Info, component, message);
        }

        pub fn show_success(&mut self, component: &'static str, message: &'static str) {
            self.add_message(BootLevel::Success, component, message);
            self.display_message(BootLevel::Success, component, message);
        }

        pub fn show_warning(&mut self, component: &'static str, message: &'static str) {
            self.add_message(BootLevel::Warning, component, message);
            self.display_message(BootLevel::Warning, component, message);
        }

        pub fn show_error(&mut self, component: &'static str, message: &'static str) {
            self.add_message(BootLevel::Error, component, message);
            self.display_message(BootLevel::Error, component, message);
        }

        pub fn show_progress(&mut self, step: u32, component: &'static str, message: &'static str) {
            self.current_step.store(step, Ordering::Relaxed);
            self.add_message(BootLevel::Info, component, message);
            self.display_progress_bar();
            self.display_message(BootLevel::Info, component, message);
        }

        fn display_progress_bar(&self) {
            let current = self.current_step.load(Ordering::Relaxed);
            let total = self.total_steps;
            let progress = (current * 100) / total;
            
            self.print_text("[");
            for i in 0..20 {
                if (i * 5) < progress {
                    self.print_text("=");
                } else {
                    self.print_text(" ");
                }
            }
            self.print_text("] ");
            self.print_number(progress);
            self.print_text("%\n");
        }

        fn display_message(&self, level: BootLevel, component: &str, message: &str) {
            match level {
                BootLevel::Info => self.print_colored(BootColor::Cyan, "[INFO]"),
                BootLevel::Success => self.print_colored(BootColor::Green, "[OK]"),
                BootLevel::Warning => self.print_colored(BootColor::Yellow, "[WARN]"),
                BootLevel::Error => self.print_colored(BootColor::Red, "[ERROR]"),
                BootLevel::Debug => self.print_colored(BootColor::White, "[DEBUG]"),
            }
            
            self.print_text(" ");
            self.print_text(component);
            self.print_text(": ");
            self.print_text(message);
            self.print_text("\n");
        }

        fn print_colored(&self, _color: BootColor, text: &str) {
            self.print_text(text);
        }

        fn print_text(&self, text: &str) {
            // In a real kernel, this would write to VGA or serial
            // For testing, we'll just simulate the output
            unsafe {
                core::arch::asm!(
                    "mov $0x0e, %ah",
                    "mov {0}, %al",
                    "int $0x10",
                    in(reg) text.as_bytes()[0] as u32,
                    options(nostack)
                );
            }
        }

        fn print_number(&self, n: u32) {
            if n == 0 {
                self.print_text("0");
                return;
            }

            let mut digits = [0u8; 10];
            let mut i = 0;
            let mut num = n;

            while num > 0 && i < 10 {
                digits[i] = (num % 10) as u8 + b'0';
                num /= 10;
                i += 1;
            }

            // Print digits in reverse order
            for j in (0..i).rev() {
                let digit_bytes = [digits[j]];
                let digit_str = core::str::from_utf8(&digit_bytes).unwrap_or("0");
                self.print_text(digit_str);
            }
        }

        pub fn show_banner(&self) {
            self.print_text("\n");
            self.print_text("========================================\n");
            self.print_text("    ECLIPSE KERNEL - BOOT MESSAGES\n");
            self.print_text("========================================\n");
            self.print_text("\n");
        }

        pub fn show_summary(&self) {
            let count = self.message_count.load(Ordering::Relaxed);
            self.print_text("\n");
            self.print_text("========================================\n");
            self.print_text("    BOOT SUMMARY\n");
            self.print_text("========================================\n");
            self.print_text("Total messages: ");
            self.print_number(count);
            self.print_text("\n");
            self.print_text("Boot completed successfully!\n");
            self.print_text("========================================\n");
        }
    }

    pub static mut BOOT_MESSENGER: BootMessenger = BootMessenger::new();

    pub fn boot_info(component: &'static str, message: &'static str) {
        unsafe {
            BOOT_MESSENGER.add_message(BootLevel::Info, component, message);
        }
    }

    pub fn boot_success(component: &'static str, message: &'static str) {
        unsafe {
            BOOT_MESSENGER.show_success(component, message);
        }
    }

    pub fn boot_warning(component: &'static str, message: &'static str) {
        unsafe {
            BOOT_MESSENGER.show_warning(component, message);
        }
    }

    pub fn boot_error(component: &'static str, message: &'static str) {
        unsafe {
            BOOT_MESSENGER.show_error(component, message);
        }
    }

    pub fn boot_progress(step: u32, component: &'static str, message: &'static str) {
        unsafe {
            BOOT_MESSENGER.show_progress(step, component, message);
        }
    }

    pub fn boot_banner() {
        unsafe {
            BOOT_MESSENGER.show_banner();
        }
    }

    pub fn boot_summary() {
        unsafe {
            BOOT_MESSENGER.show_summary();
        }
    }
}

// Test function to demonstrate boot messages
pub fn test_boot_messages() {
    use boot_messages::*;
    
    // Show banner
    boot_banner();
    
    // Simulate kernel initialization steps
    boot_progress(1, "KERNEL", "Initializing core systems...");
    boot_info("MEMORY", "Setting up memory management");
    boot_success("MEMORY", "Memory manager initialized");
    
    boot_progress(2, "KERNEL", "Loading device drivers...");
    boot_info("DRIVERS", "Loading VGA driver");
    boot_success("DRIVERS", "VGA driver loaded");
    
    boot_progress(3, "KERNEL", "Initializing filesystem...");
    boot_info("FS", "Mounting root filesystem");
    boot_success("FS", "Root filesystem mounted");
    
    boot_progress(4, "KERNEL", "Starting system services...");
    boot_info("SERVICES", "Starting process manager");
    boot_success("SERVICES", "Process manager started");
    
    boot_progress(5, "KERNEL", "Loading user interface...");
    boot_info("UI", "Initializing graphics system");
    boot_success("UI", "Graphics system ready");
    
    boot_progress(6, "KERNEL", "Running system tests...");
    boot_info("TESTS", "Running memory tests");
    boot_success("TESTS", "Memory tests passed");
    
    boot_progress(7, "KERNEL", "Finalizing initialization...");
    boot_info("KERNEL", "All systems operational");
    boot_success("KERNEL", "Kernel initialization complete");
    
    // Show summary
    boot_summary();
}

// Entry point for testing
#[no_mangle]
pub extern "C" fn test_entry() {
    test_boot_messages();
    
    // Infinite loop to keep the test running
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
