//! Manejo de puertos XHCI mejorado
//! Basado en el driver de Redox OS con mejores prácticas

use core::ptr::{read_volatile, write_volatile};
use bitflags::bitflags;

bitflags! {
    /// Flags del registro PORTSC (Port Status and Control)
    /// 
    /// Tipos de bits:
    /// - RO: Read-Only
    /// - ROS: Read-Only Sticky (preserva valor tras reset)
    /// - RW: Read/Write
    /// - RWS: Read/Write Sticky
    /// - RW1CS: Read/Write-1-to-Clear Sticky (escribir 1 limpia el bit)
    /// - RW1S: Read/Write-1-to-Set (escribir 1 setea el bit)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PortFlags: u32 {
        /// Current Connect Status (ROS)
        const CCS = 1 << 0;
        
        /// Port Enabled/Disabled (RW1CS)
        const PED = 1 << 1;
        
        /// Over-current Active (RO)
        const OCA = 1 << 3;
        
        /// Port Reset (RW1S)
        const PR = 1 << 4;
        
        /// Port Link State bits (RWS)
        const PLS_0 = 1 << 5;
        const PLS_1 = 1 << 6;
        const PLS_2 = 1 << 7;
        const PLS_3 = 1 << 8;
        
        /// Port Power (RWS)
        const PP = 1 << 9;
        
        /// Port Speed bits (ROS)
        const SPEED_0 = 1 << 10;
        const SPEED_1 = 1 << 11;
        const SPEED_2 = 1 << 12;
        const SPEED_3 = 1 << 13;
        
        /// Port Indicator Control bits (RWS)
        const PIC_AMB = 1 << 14;  // Amber
        const PIC_GRN = 1 << 15;  // Green
        
        /// Port Link State Write Strobe (RW)
        const LWS = 1 << 16;
        
        /// Connect Status Change (RW1CS)
        const CSC = 1 << 17;
        
        /// Port Enabled/Disabled Change (RW1CS)
        const PEC = 1 << 18;
        
        /// Warm Port Reset Change (RW1CS)
        const WRC = 1 << 19;
        
        /// Over-current Change (RW1CS)
        const OCC = 1 << 20;
        
        /// Port Reset Change (RW1CS)
        const PRC = 1 << 21;
        
        /// Port Link State Change (RW1CS)
        const PLC = 1 << 22;
        
        /// Port Config Error Change (RW1CS)
        const CEC = 1 << 23;
        
        /// Cold Attach Status (RO)
        const CAS = 1 << 24;
        
        /// Wake on Connect Enable (RWS)
        const WCE = 1 << 25;
        
        /// Wake on Disconnect Enable (RWS)
        const WDE = 1 << 26;
        
        /// Wake on Over-current Enable (RWS)
        const WOE = 1 << 27;
        
        /// Device Removable (RO)
        const DR = 1 << 30;
        
        /// Warm Port Reset (RW1S)
        const WPR = 1 << 31;
    }
}

impl PortFlags {
    /// Obtiene los flags que deben preservarse en escrituras
    /// 
    /// Al escribir al registro PORTSC, debemos preservar bits RO y RWS,
    /// pero NO los bits RW1CS o RW1S (ya que escribir 1 tiene efectos especiales)
    pub fn preserved(&self) -> Self {
        let preserved = Self::CCS
            | Self::OCA
            | Self::PLS_0
            | Self::PLS_1
            | Self::PLS_2
            | Self::PLS_3
            | Self::PP
            | Self::SPEED_0
            | Self::SPEED_1
            | Self::SPEED_2
            | Self::SPEED_3
            | Self::PIC_AMB
            | Self::PIC_GRN
            | Self::WCE
            | Self::WDE
            | Self::WOE
            | Self::DR;
        
        *self & preserved
    }
    
    /// Obtiene el Port Link State (PLS)
    pub fn port_link_state(&self) -> u8 {
        ((self.bits() >> 5) & 0xF) as u8
    }
    
    /// Obtiene la velocidad del puerto
    pub fn port_speed(&self) -> u8 {
        ((self.bits() >> 10) & 0xF) as u8
    }
    
    /// Verifica si un dispositivo está conectado
    pub fn is_connected(&self) -> bool {
        self.contains(Self::CCS)
    }
    
    /// Verifica si el puerto está habilitado
    pub fn is_enabled(&self) -> bool {
        self.contains(Self::PED)
    }
    
    /// Verifica si el puerto tiene energía
    pub fn has_power(&self) -> bool {
        self.contains(Self::PP)
    }
    
    /// Verifica si hay un reset en progreso
    pub fn is_resetting(&self) -> bool {
        self.contains(Self::PR)
    }
}

/// Link States del puerto
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortLinkState {
    U0 = 0,          // U0: Active/On
    U1 = 1,          // U1: Low power state
    U2 = 2,          // U2: Lower power state
    U3 = 3,          // U3: Suspended
    Disabled = 4,    // SS.Disabled
    RxDetect = 5,    // Rx.Detect
    Inactive = 6,    // SS.Inactive
    Polling = 7,     // Polling
    Recovery = 8,    // Recovery
    HotReset = 9,    // Hot Reset
    ComplianceMode = 10,  // Compliance Mode
    TestMode = 11,   // Test Mode
    Resume = 15,     // Resume
}

/// Registro de puerto XHCI
pub struct XhciPort {
    portsc_addr: u64,
    port_number: u8,
}

impl XhciPort {
    pub fn new(portsc_addr: u64, port_number: u8) -> Self {
        Self {
            portsc_addr,
            port_number,
        }
    }
    
    /// Lee el registro PORTSC
    pub fn read(&self) -> PortFlags {
        unsafe {
            let value = read_volatile(self.portsc_addr as *const u32);
            PortFlags::from_bits_truncate(value)
        }
    }
    
    /// Escribe al registro PORTSC preservando los bits correctos
    fn write(&self, flags: PortFlags) {
        unsafe {
            write_volatile(self.portsc_addr as *mut u32, flags.bits());
        }
    }
    
    /// Limpia el bit Connect Status Change (CSC)
    pub fn clear_csc(&self) {
        let flags = self.read().preserved() | PortFlags::CSC;
        self.write(flags);
    }
    
    /// Limpia el bit Port Reset Change (PRC)
    pub fn clear_prc(&self) {
        let flags = self.read().preserved() | PortFlags::PRC;
        self.write(flags);
    }
    
    /// Limpia el bit Port Enabled/Disabled Change (PEC)
    pub fn clear_pec(&self) {
        let flags = self.read().preserved() | PortFlags::PEC;
        self.write(flags);
    }
    
    /// Limpia todos los bits de cambio
    pub fn clear_all_change_bits(&self) {
        let flags = self.read().preserved() 
            | PortFlags::CSC 
            | PortFlags::PEC 
            | PortFlags::WRC 
            | PortFlags::OCC 
            | PortFlags::PRC 
            | PortFlags::PLC 
            | PortFlags::CEC;
        self.write(flags);
    }
    
    /// Inicia un reset de puerto
    pub fn set_reset(&self) {
        let flags = self.read().preserved() | PortFlags::PR;
        self.write(flags);
    }
    
    /// Verifica si un dispositivo está conectado
    pub fn is_connected(&self) -> bool {
        self.read().is_connected()
    }
    
    /// Verifica si el puerto está habilitado
    pub fn is_enabled(&self) -> bool {
        self.read().is_enabled()
    }
    
    /// Obtiene la velocidad del dispositivo conectado
    pub fn speed(&self) -> u8 {
        self.read().port_speed()
    }
    
    /// Obtiene el Port Link State
    pub fn link_state(&self) -> u8 {
        self.read().port_link_state()
    }
    
    /// Obtiene información completa del puerto
    pub fn get_info(&self) -> PortInfo {
        let flags = self.read();
        PortInfo {
            port_number: self.port_number,
            connected: flags.is_connected(),
            enabled: flags.is_enabled(),
            has_power: flags.has_power(),
            resetting: flags.is_resetting(),
            speed: flags.port_speed(),
            link_state: flags.port_link_state(),
        }
    }
}

/// Información de puerto USB
#[derive(Debug, Clone, Copy)]
pub struct PortInfo {
    pub port_number: u8,
    pub connected: bool,
    pub enabled: bool,
    pub has_power: bool,
    pub resetting: bool,
    pub speed: u8,
    pub link_state: u8,
}

impl PortInfo {
    /// Convierte velocidad XHCI a string
    pub fn speed_str(&self) -> &'static str {
        match self.speed {
            1 => "Full Speed (12 Mbps)",
            2 => "Low Speed (1.5 Mbps)",
            3 => "High Speed (480 Mbps)",
            4 => "SuperSpeed (5 Gbps)",
            5 => "SuperSpeed+ (10 Gbps)",
            6 => "SuperSpeed+ (20 Gbps)",
            _ => "Unknown",
        }
    }
    
    /// Convierte link state a string
    pub fn link_state_str(&self) -> &'static str {
        match self.link_state {
            0 => "U0 (Active)",
            1 => "U1 (Low Power)",
            2 => "U2 (Lower Power)",
            3 => "U3 (Suspended)",
            4 => "Disabled",
            5 => "Rx.Detect",
            6 => "Inactive",
            7 => "Polling",
            8 => "Recovery",
            9 => "Hot Reset",
            10 => "Compliance Mode",
            11 => "Test Mode",
            15 => "Resume",
            _ => "Reserved",
        }
    }
}

