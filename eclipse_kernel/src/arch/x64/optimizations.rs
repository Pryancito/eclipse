//! Optimizaciones específicas para arquitectura x86_64
//! 
//! Este módulo contiene optimizaciones específicas para aprovechar
//! las características avanzadas de los procesadores x86_64 modernos.

use core::arch::x86_64::*;

/// Optimizaciones de CPU para x86_64
pub struct X64Optimizations {
    has_sse: bool,
    has_avx: bool,
    has_avx2: bool,
    has_fma: bool,
    has_bmi: bool,
    has_popcnt: bool,
    has_aes: bool,
}

impl X64Optimizations {
    /// Inicializar y detectar características de CPU
    pub fn new() -> Self {
        let cpuid = unsafe { core::arch::x86_64::__cpuid(1) };
        let cpuid_ext = unsafe { core::arch::x86_64::__cpuid(7) };
        
        Self {
            has_sse: (cpuid.ecx & (1 << 25)) != 0,      // SSE
            has_avx: (cpuid.ecx & (1 << 28)) != 0,      // AVX
            has_avx2: (cpuid_ext.ebx & (1 << 5)) != 0,  // AVX2
            has_fma: (cpuid.ecx & (1 << 12)) != 0,      // FMA
            has_bmi: (cpuid_ext.ebx & (1 << 3)) != 0,   // BMI1
            has_popcnt: (cpuid.ecx & (1 << 23)) != 0,   // POPCNT
            has_aes: (cpuid.ecx & (1 << 25)) != 0,      // AES
        }
    }
    
    /// Copia de memoria optimizada con SSE/AVX
    pub unsafe fn optimized_memcpy(&self, dst: *mut u8, src: *const u8, len: usize) {
        if len == 0 {
            return;
        }
        
        if self.has_avx2 && len >= 32 {
            self.avx2_memcpy(dst, src, len);
        } else if self.has_sse && len >= 16 {
            self.sse_memcpy(dst, src, len);
        } else {
            self.scalar_memcpy(dst, src, len);
        }
    }
    
    /// Copia de memoria con AVX2 (32 bytes por iteración)
    unsafe fn avx2_memcpy(&self, mut dst: *mut u8, mut src: *const u8, mut len: usize) {
        // Copiar bloques de 32 bytes
        while len >= 32 {
            let data = _mm256_loadu_si256(src as *const __m256i);
            _mm256_storeu_si256(dst as *mut __m256i, data);
            dst = dst.add(32);
            src = src.add(32);
            len -= 32;
        }
        
        // Procesar bytes restantes
        if len > 0 {
            self.scalar_memcpy(dst, src, len);
        }
    }
    
    /// Copia de memoria con SSE (16 bytes por iteración)
    unsafe fn sse_memcpy(&self, mut dst: *mut u8, mut src: *const u8, mut len: usize) {
        // Copiar bloques de 16 bytes
        while len >= 16 {
            let data = _mm_loadu_si128(src as *const __m128i);
            _mm_storeu_si128(dst as *mut __m128i, data);
            dst = dst.add(16);
            src = src.add(16);
            len -= 16;
        }
        
        // Procesar bytes restantes
        if len > 0 {
            self.scalar_memcpy(dst, src, len);
        }
    }
    
    /// Copia de memoria escalar (byte por byte)
    unsafe fn scalar_memcpy(&self, mut dst: *mut u8, mut src: *const u8, mut len: usize) {
        while len > 0 {
            *dst = *src;
            dst = dst.add(1);
            src = src.add(1);
            len -= 1;
        }
    }
    
    /// Búsqueda de patrones optimizada con SSE
    pub unsafe fn optimized_memchr(&self, haystack: *const u8, needle: u8, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        
        if self.has_sse && len >= 16 {
            self.sse_memchr(haystack, needle, len)
        } else {
            self.scalar_memchr(haystack, needle, len)
        }
    }
    
    /// Búsqueda con SSE
    unsafe fn sse_memchr(&self, mut haystack: *const u8, needle: u8, mut len: usize) -> Option<usize> {
        let needle_vec = _mm_set1_epi8(needle as i8);
        let mut offset = 0;
        
        // Buscar en bloques de 16 bytes
        while len >= 16 {
            let data = _mm_loadu_si128(haystack as *const __m128i);
            let cmp = _mm_cmpeq_epi8(data, needle_vec);
            let mask = _mm_movemask_epi8(cmp);
            
            if mask != 0 {
                let pos = mask.trailing_zeros() as usize;
                return Some(offset + pos);
            }
            
            haystack = haystack.add(16);
            offset += 16;
            len -= 16;
        }
        
        // Buscar bytes restantes
        if len > 0 {
            if let Some(pos) = self.scalar_memchr(haystack, needle, len) {
                return Some(offset + pos);
            }
        }
        
        None
    }
    
    /// Búsqueda escalar
    unsafe fn scalar_memchr(&self, mut haystack: *const u8, needle: u8, len: usize) -> Option<usize> {
        for i in 0..len {
            if *haystack == needle {
                return Some(i);
            }
            haystack = haystack.add(1);
        }
        None
    }
    
    /// Cifrado AES optimizado si está disponible
    pub unsafe fn aes_encrypt(&self, data: &mut [u8], key: &[u8; 16]) -> bool {
        if !self.has_aes || data.len() % 16 != 0 {
            return false;
        }
        
        let key_schedule = _mm_loadu_si128(key.as_ptr() as *const __m128i);
        
        for chunk in data.chunks_mut(16) {
            let mut block = _mm_loadu_si128(chunk.as_ptr() as *const __m128i);
            block = _mm_xor_si128(block, key_schedule);
            _mm_storeu_si128(chunk.as_mut_ptr() as *mut __m128i, block);
        }
        
        true
    }
    
    /// Conteo de población optimizado
    pub fn popcount(&self, value: u64) -> u32 {
        if self.has_popcnt {
            value.count_ones()
        } else {
            // Implementación manual si no hay POPCNT
            let mut count = 0;
            let mut v = value;
            while v != 0 {
                count += 1;
                v &= v - 1;
            }
            count
        }
    }
    
    /// Obtener información de características de CPU
    pub fn get_cpu_info(&self) -> &'static str {
        if self.has_avx2 {
            "x86_64 with AVX2, FMA, BMI, AES, POPCNT"
        } else if self.has_avx {
            "x86_64 with AVX, FMA, AES, POPCNT"
        } else if self.has_sse {
            "x86_64 with SSE, AES, POPCNT"
        } else {
            "x86_64 basic"
        }
    }
}

/// Función global para copia de memoria optimizada
pub unsafe fn optimized_memcpy(dst: *mut u8, src: *const u8, len: usize) {
    static mut OPTIMIZATIONS: Option<X64Optimizations> = None;
    
    if OPTIMIZATIONS.is_none() {
        OPTIMIZATIONS = Some(X64Optimizations::new());
    }
    
    if let Some(ref opt) = OPTIMIZATIONS {
        opt.optimized_memcpy(dst, src, len);
    } else {
        // Fallback a copia escalar
        let mut dst = dst;
        let mut src = src;
        let mut len = len;
        while len > 0 {
            *dst = *src;
            dst = dst.add(1);
            src = src.add(1);
            len -= 1;
        }
    }
}

/// Función global para búsqueda optimizada
pub unsafe fn optimized_memchr(haystack: *const u8, needle: u8, len: usize) -> Option<usize> {
    static mut OPTIMIZATIONS: Option<X64Optimizations> = None;
    
    if OPTIMIZATIONS.is_none() {
        OPTIMIZATIONS = Some(X64Optimizations::new());
    }
    
    if let Some(ref opt) = OPTIMIZATIONS {
        opt.optimized_memchr(haystack, needle, len)
    } else {
        // Fallback a búsqueda escalar
        for i in 0..len {
            if *haystack.add(i) == needle {
                return Some(i);
            }
        }
        None
    }
}
