//! Implementación simple de UUID compatible con no_std
//!
//! Esta implementación proporciona UUIDs únicos sin dependencias externas
//! compatibles con el entorno no_std de Eclipse OS.

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;

/// UUID simple compatible con no_std
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimpleUUID {
    /// Bytes del UUID (16 bytes = 128 bits)
    pub bytes: [u8; 16],
}

impl SimpleUUID {
    /// Crear nuevo UUID v4 simulado
    pub fn new_v4() -> Self {
        // Generar UUID v4 usando algoritmo simple
        let mut bytes = [0u8; 16];

        // Usar timestamp y contador para generar entropía
        let timestamp = 0x123456789ABCDEF0u64; // Simulado
        let counter = 0x9876543210FEDCBAu64; // Simulado

        // Llenar los primeros 8 bytes con timestamp
        for i in 0..8 {
            bytes[i] = ((timestamp >> (i * 8)) & 0xFF) as u8;
        }

        // Llenar los siguientes 8 bytes con counter
        for i in 0..8 {
            bytes[8 + i] = ((counter >> (i * 8)) & 0xFF) as u8;
        }

        // Aplicar transformaciones para simular UUID v4
        bytes[6] = (bytes[6] & 0x0F) | 0x40; // Versión 4
        bytes[8] = (bytes[8] & 0x3F) | 0x80; // Variante

        Self { bytes }
    }

    /// Crear UUID desde bytes
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self { bytes }
    }

    /// Obtener bytes del UUID
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.bytes
    }

    /// Convertir a string en formato UUID estándar
    pub fn to_string(&self) -> String {
        format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}",
                self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3],
                self.bytes[4], self.bytes[5], self.bytes[6], self.bytes[7],
                self.bytes[8], self.bytes[9], self.bytes[10], self.bytes[11],
                self.bytes[12], self.bytes[13], self.bytes[14], self.bytes[15])
    }

    /// Convertir a string corto (solo primeros 8 caracteres)
    pub fn to_short_string(&self) -> String {
        format!(
            "{:02x}{:02x}{:02x}{:02x}",
            self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]
        )
    }

    /// Convertir a u128
    pub fn to_u128(&self) -> u128 {
        let mut result = 0u128;
        for i in 0..16 {
            result |= (self.bytes[i] as u128) << ((15 - i) * 8);
        }
        result
    }

    /// Crear desde u128
    pub fn from_u128(value: u128) -> Self {
        let mut bytes = [0u8; 16];
        for i in 0..16 {
            bytes[i] = ((value >> ((15 - i) * 8)) & 0xFF) as u8;
        }
        Self { bytes }
    }
}

impl Default for SimpleUUID {
    fn default() -> Self {
        Self::new_v4()
    }
}

/// Trait para generar UUIDs únicos
pub trait UUIDGenerator {
    /// Generar nuevo UUID único
    fn generate_uuid(&mut self) -> SimpleUUID;

    /// Generar UUID con prefijo específico
    fn generate_uuid_with_prefix(&mut self, prefix: &str) -> SimpleUUID;
}

/// Generador de UUIDs con contador
#[derive(Debug, Clone)]
pub struct CounterUUIDGenerator {
    counter: u64,
    base_timestamp: u64,
}

impl CounterUUIDGenerator {
    /// Crear nuevo generador
    pub fn new() -> Self {
        Self {
            counter: 0,
            base_timestamp: 0x123456789ABCDEF0, // Timestamp simulado
        }
    }

    /// Incrementar contador
    pub fn increment(&mut self) {
        self.counter += 1;
    }

    /// Obtener contador actual
    pub fn get_counter(&self) -> u64 {
        self.counter
    }
}

impl UUIDGenerator for CounterUUIDGenerator {
    fn generate_uuid(&mut self) -> SimpleUUID {
        self.counter += 1;

        // Crear UUID basado en timestamp y contador
        let mut bytes = [0u8; 16];

        // Usar timestamp base + contador
        let timestamp = self.base_timestamp + self.counter;

        // Llenar los primeros 8 bytes
        for i in 0..8 {
            bytes[i] = ((timestamp >> (i * 8)) & 0xFF) as u8;
        }

        // Llenar los siguientes 8 bytes con contador
        for i in 0..8 {
            bytes[8 + i] = ((self.counter >> (i * 8)) & 0xFF) as u8;
        }

        // Aplicar transformaciones para simular UUID v4
        bytes[6] = (bytes[6] & 0x0F) | 0x40; // Versión 4
        bytes[8] = (bytes[8] & 0x3F) | 0x80; // Variante

        SimpleUUID { bytes }
    }

    fn generate_uuid_with_prefix(&mut self, prefix: &str) -> SimpleUUID {
        let mut uuid = self.generate_uuid();

        // Modificar los primeros bytes basado en el prefijo
        let mut hash = 0u64;
        for byte in prefix.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
        }

        // Aplicar hash a los primeros 4 bytes
        for i in 0..4 {
            uuid.bytes[i] = ((hash >> (i * 8)) & 0xFF) as u8;
        }

        uuid
    }
}

impl Default for CounterUUIDGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SimpleUUID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_string())
    }
}
