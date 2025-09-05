//! Sistema de manejo de excepciones para Eclipse OS
//! 
//! Maneja excepciones del procesador y errores del sistema

use core::fmt;

/// Tipos de excepciones del procesador
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExceptionType {
    /// División por cero
    DivisionByZero = 0,
    /// Debug
    Debug = 1,
    /// Interrupción no enmascarable
    NonMaskableInterrupt = 2,
    /// Breakpoint
    Breakpoint = 3,
    /// Overflow
    Overflow = 4,
    /// Bound Range Exceeded
    BoundRangeExceeded = 5,
    /// Invalid Opcode
    InvalidOpcode = 6,
    /// Device Not Available
    DeviceNotAvailable = 7,
    /// Double Fault
    DoubleFault = 8,
    /// Coprocessor Segment Overrun
    CoprocessorSegmentOverrun = 9,
    /// Invalid TSS
    InvalidTSS = 10,
    /// Segment Not Present
    SegmentNotPresent = 11,
    /// Stack Fault
    StackFault = 12,
    /// General Protection Fault
    GeneralProtectionFault = 13,
    /// Page Fault
    PageFault = 14,
    /// Reserved
    Reserved = 15,
    /// x87 Floating Point Exception
    X87FloatingPointException = 16,
    /// Alignment Check
    AlignmentCheck = 17,
    /// Machine Check
    MachineCheck = 18,
    /// SIMD Floating Point Exception
    SIMDFloatingPointException = 19,
    /// Virtualization Exception
    VirtualizationException = 20,
    /// Control Protection Exception
    ControlProtectionException = 21,
    /// Hypervisor Injection Exception
    HypervisorInjectionException = 28,
    /// VMM Communication Exception
    VMMCommunicationException = 29,
    /// Security Exception
    SecurityException = 30,
    /// Unknown
    Unknown = 255,
}

impl ExceptionType {
    /// Crear ExceptionType desde un número
    pub fn from_number(num: u8) -> Self {
        match num {
            0 => ExceptionType::DivisionByZero,
            1 => ExceptionType::Debug,
            2 => ExceptionType::NonMaskableInterrupt,
            3 => ExceptionType::Breakpoint,
            4 => ExceptionType::Overflow,
            5 => ExceptionType::BoundRangeExceeded,
            6 => ExceptionType::InvalidOpcode,
            7 => ExceptionType::DeviceNotAvailable,
            8 => ExceptionType::DoubleFault,
            9 => ExceptionType::CoprocessorSegmentOverrun,
            10 => ExceptionType::InvalidTSS,
            11 => ExceptionType::SegmentNotPresent,
            12 => ExceptionType::StackFault,
            13 => ExceptionType::GeneralProtectionFault,
            14 => ExceptionType::PageFault,
            15 => ExceptionType::Reserved,
            16 => ExceptionType::X87FloatingPointException,
            17 => ExceptionType::AlignmentCheck,
            18 => ExceptionType::MachineCheck,
            19 => ExceptionType::SIMDFloatingPointException,
            20 => ExceptionType::VirtualizationException,
            21 => ExceptionType::ControlProtectionException,
            28 => ExceptionType::HypervisorInjectionException,
            29 => ExceptionType::VMMCommunicationException,
            30 => ExceptionType::SecurityException,
            _ => ExceptionType::Unknown,
        }
    }
    
    /// Obtener descripción de la excepción
    pub fn description(&self) -> &'static str {
        match self {
            ExceptionType::DivisionByZero => "División por cero",
            ExceptionType::Debug => "Debug",
            ExceptionType::NonMaskableInterrupt => "Interrupción no enmascarable",
            ExceptionType::Breakpoint => "Breakpoint",
            ExceptionType::Overflow => "Overflow",
            ExceptionType::BoundRangeExceeded => "Rango de límites excedido",
            ExceptionType::InvalidOpcode => "Opcode inválido",
            ExceptionType::DeviceNotAvailable => "Dispositivo no disponible",
            ExceptionType::DoubleFault => "Doble fallo",
            ExceptionType::CoprocessorSegmentOverrun => "Desbordamiento de segmento del coprocesador",
            ExceptionType::InvalidTSS => "TSS inválido",
            ExceptionType::SegmentNotPresent => "Segmento no presente",
            ExceptionType::StackFault => "Fallo de pila",
            ExceptionType::GeneralProtectionFault => "Fallo de protección general",
            ExceptionType::PageFault => "Fallo de página",
            ExceptionType::Reserved => "Reservado",
            ExceptionType::X87FloatingPointException => "Excepción de punto flotante x87",
            ExceptionType::AlignmentCheck => "Verificación de alineación",
            ExceptionType::MachineCheck => "Verificación de máquina",
            ExceptionType::SIMDFloatingPointException => "Excepción de punto flotante SIMD",
            ExceptionType::VirtualizationException => "Excepción de virtualización",
            ExceptionType::ControlProtectionException => "Excepción de protección de control",
            ExceptionType::HypervisorInjectionException => "Excepción de inyección de hipervisor",
            ExceptionType::VMMCommunicationException => "Excepción de comunicación VMM",
            ExceptionType::SecurityException => "Excepción de seguridad",
            ExceptionType::Unknown => "Excepción desconocida",
        }
    }
    
    /// Verificar si la excepción es crítica
    pub fn is_critical(&self) -> bool {
        matches!(self, 
            ExceptionType::DoubleFault |
            ExceptionType::MachineCheck |
            ExceptionType::SecurityException
        )
    }
    
    /// Verificar si la excepción es recuperable
    pub fn is_recoverable(&self) -> bool {
        matches!(self,
            ExceptionType::DivisionByZero |
            ExceptionType::Breakpoint |
            ExceptionType::Overflow |
            ExceptionType::BoundRangeExceeded |
            ExceptionType::InvalidOpcode |
            ExceptionType::DeviceNotAvailable |
            ExceptionType::PageFault
        )
    }
}

impl fmt::Display for ExceptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Información de una excepción
#[derive(Debug, Clone, Copy)]
pub struct ExceptionInfo {
    pub exception_type: ExceptionType,
    pub error_code: Option<u32>,
    pub address: u64,
    pub timestamp: u64,
    pub cpu_id: u32,
}

impl ExceptionInfo {
    /// Crear nueva información de excepción
    pub fn new(exception_type: ExceptionType, error_code: Option<u32>, address: u64) -> Self {
        Self {
            exception_type,
            error_code,
            address,
            timestamp: 0, // Se establecería con el timestamp real
            cpu_id: 0,    // Se establecería con el ID del CPU real
        }
    }
}

/// Handler de excepciones
pub trait ExceptionHandler {
    /// Manejar una excepción
    fn handle_exception(&mut self, info: &ExceptionInfo) -> ExceptionResult;
    
    /// Obtener el tipo de excepción que maneja
    fn get_exception_type(&self) -> ExceptionType;
    
    /// Verificar si el handler está habilitado
    fn is_enabled(&self) -> bool;
}

/// Resultado del manejo de excepción
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExceptionResult {
    /// Excepción manejada exitosamente
    Handled,
    /// Excepción no manejada, pasar al siguiente handler
    NotHandled,
    /// Error fatal del sistema
    Fatal,
    /// Excepción recuperable
    Recoverable,
}

impl fmt::Display for ExceptionResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExceptionResult::Handled => write!(f, "Handled"),
            ExceptionResult::NotHandled => write!(f, "NotHandled"),
            ExceptionResult::Fatal => write!(f, "Fatal"),
            ExceptionResult::Recoverable => write!(f, "Recoverable"),
        }
    }
}

/// Handler por defecto para excepciones
pub struct DefaultExceptionHandler {
    exception_type: ExceptionType,
    enabled: bool,
}

impl DefaultExceptionHandler {
    /// Crear nuevo handler por defecto
    pub fn new(exception_type: ExceptionType) -> Self {
        Self {
            exception_type,
            enabled: true,
        }
    }
}

impl ExceptionHandler for DefaultExceptionHandler {
    fn handle_exception(&mut self, info: &ExceptionInfo) -> ExceptionResult {
        if !self.enabled {
            return ExceptionResult::NotHandled;
        }
        
        // Manejo básico de excepciones
        match info.exception_type {
            ExceptionType::DivisionByZero => {
                // Manejar división por cero
                ExceptionResult::Handled
            }
            ExceptionType::PageFault => {
                // Manejar fallo de página
                ExceptionResult::Handled
            }
            ExceptionType::GeneralProtectionFault => {
                // Manejar fallo de protección general
                ExceptionResult::Handled
            }
            _ => ExceptionResult::NotHandled,
        }
    }
    
    fn get_exception_type(&self) -> ExceptionType {
        self.exception_type
    }
    
    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Estado del sistema de excepciones
static mut EXCEPTION_HANDLERS_INITIALIZED: bool = false;

/// Inicializar handlers de excepciones
pub fn init_exception_handlers() -> Result<(), &'static str> {
    // En un sistema real, aquí se configurarían los vectores de excepción
    // y se instalarían los handlers apropiados
    
    unsafe {
        EXCEPTION_HANDLERS_INITIALIZED = true;
    }
    
    Ok(())
}

/// Verificar si los handlers de excepciones están inicializados
pub fn are_exception_handlers_initialized() -> bool {
    unsafe { EXCEPTION_HANDLERS_INITIALIZED }
}

/// Manejar una excepción del sistema
pub fn handle_system_exception(exception_number: u8, error_code: Option<u32>, address: u64) -> ExceptionResult {
    let exception_type = ExceptionType::from_number(exception_number);
    let info = ExceptionInfo::new(exception_type, error_code, address);
    
    // En un sistema real, aquí se llamaría al handler apropiado
    // Por ahora, usamos el handler por defecto
    let mut handler = DefaultExceptionHandler::new(exception_type);
    handler.handle_exception(&info)
}

/// Obtener estadísticas de excepciones
pub fn get_exception_stats() -> ExceptionStats {
    ExceptionStats {
        handlers_initialized: are_exception_handlers_initialized(),
        total_exceptions: 0, // Se incrementaría en un sistema real
        critical_exceptions: 0,
        recoverable_exceptions: 0,
    }
}

/// Estadísticas de excepciones
#[derive(Debug, Clone, Copy)]
pub struct ExceptionStats {
    pub handlers_initialized: bool,
    pub total_exceptions: u64,
    pub critical_exceptions: u64,
    pub recoverable_exceptions: u64,
}

impl Default for ExceptionStats {
    fn default() -> Self {
        Self {
            handlers_initialized: false,
            total_exceptions: 0,
            critical_exceptions: 0,
            recoverable_exceptions: 0,
        }
    }
}

impl fmt::Display for ExceptionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Exception Stats: initialized={}, total={}, critical={}, recoverable={}",
               self.handlers_initialized, self.total_exceptions, 
               self.critical_exceptions, self.recoverable_exceptions)
    }
}
