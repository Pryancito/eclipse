// GPU Detection Module for Redox OS Installer
// Detects GPUs and generates appropriate init_graphics.rc

use std::fs;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Bochs,  // QEMU
    VirtIO, // QEMU virtio
    Unknown,
}

#[derive(Debug)]
pub struct GpuInfo {
    pub vendor: GpuVendor,
    pub vendor_id: u16,
    pub device_id: u16,
    pub name: String,
}

impl GpuInfo {
    pub fn driver_name(&self) -> &'static str {
        match self.vendor {
            GpuVendor::Nvidia => "nvidiad",
            GpuVendor::Amd => "amdd",
            GpuVendor::Intel => "inteld",
            GpuVendor::Bochs | GpuVendor::VirtIO => "bgad",
            GpuVendor::Unknown => "vesad",
        }
    }
    
    pub fn scheme_name(&self) -> &'static str {
        match self.vendor {
            GpuVendor::Nvidia => "nvidia",
            GpuVendor::Amd => "amd",
            GpuVendor::Intel => "intel",
            GpuVendor::Bochs | GpuVendor::VirtIO => "bga",
            GpuVendor::Unknown => "vesa",
        }
    }
}

/// Detect GPU from /sys/class or similar (when running on Linux during installation)
pub fn detect_gpu() -> Option<GpuInfo> {
    // Try lspci first (if available on host system)
    if let Ok(output) = Command::new("lspci").arg("-nn").output() {
        if let Ok(text) = String::from_utf8(output.stdout) {
            for line in text.lines() {
                if line.contains("VGA") || line.contains("3D") || line.contains("Display") {
                    return parse_lspci_line(line);
                }
            }
        }
    }
    
    // Fallback: try reading PCI devices from sysfs
    if let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") {
        for entry in entries.flatten() {
            if let Some(gpu) = check_pci_device(&entry.path()) {
                return Some(gpu);
            }
        }
    }
    
    None
}

fn parse_lspci_line(line: &str) -> Option<GpuInfo> {
    // Example: "01:00.0 VGA compatible controller [0300]: NVIDIA Corporation ... [10de:1f06]"
    
    if line.contains("NVIDIA") || line.contains("10de") {
        let (vendor_id, device_id) = extract_ids(line)?;
        return Some(GpuInfo {
            vendor: GpuVendor::Nvidia,
            vendor_id,
            device_id,
            name: extract_name(line),
        });
    }
    
    if line.contains("AMD") || line.contains("ATI") || line.contains("1002") {
        let (vendor_id, device_id) = extract_ids(line)?;
        return Some(GpuInfo {
            vendor: GpuVendor::Amd,
            vendor_id,
            device_id,
            name: extract_name(line),
        });
    }
    
    if line.contains("Intel") || line.contains("8086") {
        let (vendor_id, device_id) = extract_ids(line)?;
        return Some(GpuInfo {
            vendor: GpuVendor::Intel,
            vendor_id,
            device_id,
            name: extract_name(line),
        });
    }
    
    if line.contains("Bochs") || line.contains("1234:1111") {
        return Some(GpuInfo {
            vendor: GpuVendor::Bochs,
            vendor_id: 0x1234,
            device_id: 0x1111,
            name: "Bochs Graphics Adapter (QEMU)".to_string(),
        });
    }
    
    if line.contains("virtio") && (line.contains("VGA") || line.contains("GPU")) {
        return Some(GpuInfo {
            vendor: GpuVendor::VirtIO,
            vendor_id: 0x1AF4,
            device_id: 0x1050,
            name: "VirtIO GPU (QEMU)".to_string(),
        });
    }
    
    None
}

fn extract_ids(line: &str) -> Option<(u16, u16)> {
    // Look for pattern [1234:5678]
    if let Some(start) = line.rfind('[') {
        if let Some(end) = line[start..].find(']') {
            let ids = &line[start + 1..start + end];
            let parts: Vec<&str> = ids.split(':').collect();
            if parts.len() == 2 {
                let vendor = u16::from_str_radix(parts[0], 16).ok()?;
                let device = u16::from_str_radix(parts[1], 16).ok()?;
                return Some((vendor, device));
            }
        }
    }
    None
}

fn extract_name(line: &str) -> String {
    // Extract everything after "controller: " and before " [" or end of line
    if let Some(start) = line.find("controller: ") {
        let name_start = start + "controller: ".len();
        let name_end = line[name_start..].find(" [").unwrap_or(line[name_start..].len());
        return line[name_start..name_start + name_end].trim().to_string();
    }
    "Unknown GPU".to_string()
}

fn check_pci_device(path: &std::path::Path) -> Option<GpuInfo> {
    // Read vendor and device from sysfs
    let vendor_path = path.join("vendor");
    let device_path = path.join("device");
    let class_path = path.join("class");
    
    let vendor = fs::read_to_string(vendor_path).ok()?;
    let device = fs::read_to_string(device_path).ok()?;
    let class = fs::read_to_string(class_path).ok()?;
    
    // Check if it's a display controller (class 0x03xxxx)
    if !class.trim().starts_with("0x03") {
        return None;
    }
    
    let vendor_id = u16::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).ok()?;
    let device_id = u16::from_str_radix(device.trim().trim_start_matches("0x"), 16).ok()?;
    
    let vendor_type = match vendor_id {
        0x10DE => GpuVendor::Nvidia,
        0x1002 => GpuVendor::Amd,
        0x8086 => GpuVendor::Intel,
        0x1234 if device_id == 0x1111 => GpuVendor::Bochs,
        0x1AF4 if device_id == 0x1050 => GpuVendor::VirtIO,
        _ => GpuVendor::Unknown,
    };
    
    Some(GpuInfo {
        vendor: vendor_type,
        vendor_id,
        device_id,
        name: format!("{:04X}:{:04X}", vendor_id, device_id),
    })
}

/// Generate init_graphics.rc content based on detected GPU
pub fn generate_init_graphics_rc(gpu: Option<&GpuInfo>) -> String {
    match gpu {
        Some(gpu) => {
            // GPU específica detectada - NO usar vesad
            let vendor_name = match gpu.vendor {
                GpuVendor::Nvidia => "NVIDIA",
                GpuVendor::Amd => "AMD",
                GpuVendor::Intel => "Intel",
                GpuVendor::Bochs => "Bochs (QEMU)",
                GpuVendor::VirtIO => "VirtIO (QEMU)",
                GpuVendor::Unknown => "Unknown",
            };
            
            format!(r#"# Graphics drivers initialization script
# Auto-generated by Redox installer
# Detected GPU: {} ({:04X}:{:04X})
# Driver: {}

echo "[init_graphics] Iniciando controlador PCI..."

# Start PS/2 and ACPI first
ps2d us
acpid

# Start pcid (required for pcid-spawner)
pcid

echo "[init_graphics] Cargando driver de gráficos: {}"

# Load detected GPU driver via pcid-spawner
pcid-spawner /etc/pcid/initfs_graphics.toml

echo "[init_graphics] Sistema gráfico inicializado"

# Load remaining drivers (storage, network, etc.)
pcid-spawner /etc/pcid/initfs.toml
"#, vendor_name, gpu.vendor_id, gpu.device_id, gpu.driver_name(), gpu.driver_name())
        }
        None => {
            // No GPU detectada - usar vesad como único driver
            format!(r#"# Graphics drivers initialization script
# Auto-generated by Redox installer
# No specific GPU detected - using VESA fallback

echo "[init_graphics] Iniciando controlador PCI..."

# Start PS/2 and ACPI first
ps2d us
acpid

# Start pcid (required for pcid-spawner)
pcid

echo "[init_graphics] Cargando driver de gráficos: vesad (VESA fallback)"

# Try pcid-spawner first (might detect something we missed)
pcid-spawner /etc/pcid/initfs_graphics.toml

# Load vesad as primary driver
vesad

echo "[init_graphics] Sistema gráfico inicializado"

# Load remaining drivers (storage, network, etc.)
pcid-spawner /etc/pcid/initfs.toml
"#)
        }
    }
}
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_nvidia() {
        let line = "01:00.0 VGA compatible controller [0300]: NVIDIA Corporation TU104 [GeForce RTX 2060 SUPER] [10de:1f06]";
        let gpu = parse_lspci_line(line).unwrap();
        assert_eq!(gpu.vendor, GpuVendor::Nvidia);
        assert_eq!(gpu.vendor_id, 0x10de);
        assert_eq!(gpu.device_id, 0x1f06);
    }
    
    #[test]
    fn test_parse_amd() {
        let line = "01:00.0 VGA compatible controller [0300]: Advanced Micro Devices [AMD/ATI] Navi 21 [1002:73bf]";
        let gpu = parse_lspci_line(line).unwrap();
        assert_eq!(gpu.vendor, GpuVendor::Amd);
    }
}

