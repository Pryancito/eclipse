//! Sistema de AI para escritura caracter por caracter
//! Este módulo implementa un sistema de AI que puede escribir mensajes
//! caracter por caracter con efectos visuales y personalización.

#![no_std]

use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Tipos de efectos de escritura disponibles
#[derive(Debug, Clone, Copy)]
pub enum TypingEffect {
    /// Escritura normal con pausa básica
    Normal,
    /// Escritura rápida
    Fast,
    /// Escritura lenta y dramática
    Slow,
    /// Efecto de máquina de escribir con cursor
    Typewriter,
    /// Efecto de escritura con sonido (simulado)
    WithSound,
    /// Efecto de escritura con colores cambiantes
    Rainbow,
}

/// Configuración del sistema de AI
#[derive(Debug, Clone)]
pub struct AiTypingConfig {
    pub effect: TypingEffect,
    pub delay_ms: u32,
    pub color: Color,
    pub cursor_color: Color,
    pub rainbow_colors: Vec<Color>,
    pub sound_enabled: bool,
}

impl Default for AiTypingConfig {
    fn default() -> Self {
        Self {
            effect: TypingEffect::Normal,
            delay_ms: 50,
            color: Color::WHITE,
            cursor_color: Color::WHITE,
            rainbow_colors: vec![
                Color::RED,
                Color::ORANGE,
                Color::YELLOW,
                Color::GREEN,
                Color::CYAN,
                Color::BLUE,
                Color::MAGENTA,
            ],
            sound_enabled: false,
        }
    }
}

/// Mensajes predefinidos de la AI
pub struct AiMessages {
    pub welcome_messages: Vec<String>,
    pub system_messages: Vec<String>,
    pub error_messages: Vec<String>,
    pub success_messages: Vec<String>,
}

impl AiMessages {
    pub fn new() -> Self {
        // VERSIÓN SEGURA: Usar mensajes estáticos para evitar problemas con el allocator
        Self {
            welcome_messages: vec![],
            system_messages: vec![],
            error_messages: vec![],
            success_messages: vec![],
        }
    }

    /// Obtener mensaje de bienvenida por índice (versión segura)
    pub fn get_welcome_message(&self, index: usize) -> &'static str {
        match index {
            0 => "Bienvenido a Eclipse OS",
            1 => "Sistema inicializado correctamente",
            2 => "AI Kernel activado",
            3 => "Framebuffer detectado",
            4 => "Sistema listo para usar",
            _ => "Bienvenido al sistema",
        }
    }

    /// Obtener mensaje del sistema por índice (versión segura)
    pub fn get_system_message(&self, index: usize) -> &'static str {
        match index {
            0 => "Cargando sistema de archivos...",
            1 => "Inicializando drivers de hardware...",
            2 => "Configurando red...",
            3 => "Preparando interfaz grafica...",
            4 => "Sistema optimizado y listo",
            _ => "Procesando...",
        }
    }

    /// Obtener mensaje de error por índice (versión segura)
    pub fn get_error_message(&self, index: usize) -> &'static str {
        match index {
            0 => "Error: No se pudo inicializar el framebuffer",
            1 => "Advertencia: Hardware no detectado",
            2 => "Error: Memoria insuficiente",
            3 => "Sistema en modo de recuperacion",
            _ => "Error del sistema",
        }
    }

    /// Obtener mensaje de éxito por índice (versión segura)
    pub fn get_success_message(&self, index: usize) -> &'static str {
        match index {
            0 => "Operacion completada exitosamente",
            1 => "Sistema funcionando correctamente",
            2 => "Todos los drivers cargados",
            3 => "Sistema estable y optimizado",
            _ => "Operacion exitosa",
        }
    }
}

/// Sistema principal de AI para escritura
pub struct AiTypingSystem {
    config: AiTypingConfig,
    messages: AiMessages,
    current_position: (u32, u32),
    is_typing: bool,
}

impl AiTypingSystem {
    pub fn new() -> Self {
        Self {
            config: AiTypingConfig::default(),
            messages: AiMessages::new(),
            current_position: (20, 20),
            is_typing: false,
        }
    }

    pub fn with_config(config: AiTypingConfig) -> Self {
        Self {
            config,
            messages: AiMessages::new(),
            current_position: (20, 20),
            is_typing: false,
        }
    }

    /// Escribir un mensaje con el efecto configurado (versión optimizada para kernel)
    pub fn write_message(&mut self, fb: &mut FramebufferDriver, message: &String) {
        self.is_typing = true;

        // VERSIÓN SEGURA: Usar write_message_direct para evitar problemas con el allocator
        self.write_message_direct(fb, message.as_str());

        self.is_typing = false;
    }

    /// Escribir mensaje directo con función optimizada (sin efectos)
    pub fn write_message_direct(&mut self, fb: &mut FramebufferDriver, message: &str) {
        self.is_typing = true;

        // Usar función optimizada para kernel sin efectos
        fb.write_text_kernel(message, self.config.color);

        self.is_typing = false;
    }

    /// Escribir mensaje de bienvenida aleatorio (versión segura)
    pub fn write_welcome_message(&mut self, fb: &mut FramebufferDriver) {
        let message = self.messages.get_welcome_message(0);
        self.write_message_direct(fb, message);
    }

    /// Escribir mensaje del sistema (versión segura)
    pub fn write_system_message(&mut self, fb: &mut FramebufferDriver, message_index: usize) {
        let message = self.messages.get_system_message(message_index);
        self.write_message_direct(fb, message);
    }

    /// Escribir mensaje de error (versión segura)
    pub fn write_error_message(&mut self, fb: &mut FramebufferDriver, message_index: usize) {
        let message = self.messages.get_error_message(message_index);
        self.config.color = Color::RED; // Cambiar color a rojo para errores
        self.write_message_direct(fb, message);
        self.config.color = Color::WHITE; // Restaurar color original
    }

    /// Escribir mensaje de éxito (versión segura)
    pub fn write_success_message(&mut self, fb: &mut FramebufferDriver, message_index: usize) {
        let message = self.messages.get_success_message(message_index);
        self.config.color = Color::GREEN; // Cambiar color a verde para éxito
        self.write_message_direct(fb, message);
        self.config.color = Color::WHITE; // Restaurar color original
    }

    /// Efecto de escritura con sonido simulado
    fn write_with_sound_effect(&mut self, fb: &mut FramebufferDriver, message: &String) {
        let mut current_x = self.current_position.0;
        let char_width = 8;

        for (i, &byte) in message.as_bytes().iter().enumerate() {
            // Simular sonido con parpadeo del cursor
            if i % 3 == 0 {
                fb.draw_rect(current_x, self.current_position.1 + 10, 2, 8, Color::YELLOW);
            }

            fb.draw_character(
                current_x,
                self.current_position.1,
                byte as char,
                self.config.color,
            );
            current_x += char_width;

            // Pausa para efecto de sonido
            for _ in 0..30000 {
                core::hint::spin_loop();
            }
        }
    }

    /// Efecto de escritura con colores del arcoíris (versión optimizada para kernel)
    fn write_rainbow_effect(&mut self, fb: &mut FramebufferDriver, message: &String) {
        let mut current_x = self.current_position.0;
        let char_width = 8;
        let mut char_index = 0;

        // Obtener punteros directos para acceso optimizado
        let mut current_ptr = message.as_ptr();
        let end_ptr = unsafe { current_ptr.add(message.len()) };

        // Bucle optimizado con punteros directos
        while current_ptr < end_ptr {
            let char_code = unsafe { core::ptr::read_volatile(current_ptr) };

            // Seleccionar color del arcoíris
            let color_index = char_index % self.config.rainbow_colors.len();
            let color = self.config.rainbow_colors[color_index];

            // Verificar límites de pantalla
            if current_x + char_width > fb.info.width {
                break;
            }

            // Dibujar caracter con color del arcoíris
            fb.draw_character(current_x, self.current_position.1, char_code as char, color);
            current_x += char_width;
            char_index += 1;

            // Pausa para efecto visual (optimizada para kernel)
            for _ in 0..20000 {
                core::hint::spin_loop();
            }

            // Avanzar puntero
            current_ptr = unsafe { current_ptr.add(1) };
        }
    }

    /// Cambiar configuración del sistema
    pub fn set_config(&mut self, config: AiTypingConfig) {
        self.config = config;
    }

    /// Cambiar posición de escritura
    pub fn set_position(&mut self, x: u32, y: u32) {
        self.current_position = (x, y);
    }

    /// Verificar si está escribiendo
    pub fn is_typing(&self) -> bool {
        self.is_typing
    }

    /// Limpiar área de escritura
    pub fn clear_writing_area(&mut self, fb: &mut FramebufferDriver, width: u32, height: u32) {
        fb.fill_rect_fast(
            self.current_position.0,
            self.current_position.1,
            width,
            height,
            Color::BLACK,
        );
    }

    /// Función para que la IA practique escritura caracter por caracter
    /// Utiliza diferentes patrones de aprendizaje y ejercicios
    pub fn feed_framebuffer(&mut self, fb: &mut FramebufferDriver) {
        static mut PRACTICE_COUNTER: u32 = 0;
        static mut CURRENT_PATTERN: u8 = 0;
        static mut CHAR_INDEX: usize = 0;

        unsafe {
            PRACTICE_COUNTER += 1;

            // Cambiar patrón cada 1000 iteraciones
            if PRACTICE_COUNTER % 1000 == 0 {
                CURRENT_PATTERN = (CURRENT_PATTERN + 1) % 4;
                CHAR_INDEX = 0;
            }

            match CURRENT_PATTERN {
                0 => self.practice_alphabet_sequence(fb, &mut CHAR_INDEX),
                1 => self.practice_random_characters(fb),
                2 => self.practice_word_spelling(fb, &mut CHAR_INDEX),
                3 => self.practice_bitmap_analysis(fb),
                _ => self.practice_alphabet_sequence(fb, &mut CHAR_INDEX),
            }
        }
    }

    /// Practicar secuencia del alfabeto
    fn practice_alphabet_sequence(&mut self, fb: &mut FramebufferDriver, char_index: &mut usize) {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";

        if *char_index < alphabet.len() {
            let char_code = alphabet[*char_index];
            let x = 100 + (*char_index as u32 * 10);
            let y = 150;

            // Limpiar área pequeña
            fb.fill_rect_fast(x, y, 8, 8, Color::BLACK);

            // Dibujar caracter
            fb.draw_character(x, y, char_code as char, Color::GREEN);

            *char_index += 1;
        } else {
            *char_index = 0; // Reiniciar secuencia
        }
    }

    /// Practicar caracteres aleatorios
    fn practice_random_characters(&mut self, fb: &mut FramebufferDriver) {
        static mut RANDOM_SEED: u32 = 12345;

        unsafe {
            // Generador de números pseudoaleatorios simple
            RANDOM_SEED = RANDOM_SEED.wrapping_mul(1103515245).wrapping_add(12345);
            let random_char = (RANDOM_SEED % 26) as u8 + b'A';

            let x = 200 + (RANDOM_SEED % 100) as u32;
            let y = 200 + (RANDOM_SEED % 50) as u32;

            // Limpiar área pequeña
            fb.fill_rect_fast(x, y, 8, 8, Color::WHITE);

            // Dibujar caracter aleatorio
            fb.draw_character(x, y, random_char as char, Color::BLUE);
        }
    }

    /// Practicar escritura de palabras
    fn practice_word_spelling(&mut self, fb: &mut FramebufferDriver, char_index: &mut usize) {
        let words = ["HELLO", "WORLD", "ECLIPSE", "KERNEL", "RUST"];
        let current_word = words[(*char_index / 10) % words.len()];
        let word_char_index = *char_index % current_word.len();

        if word_char_index < current_word.len() {
            let char_code = current_word.as_bytes()[word_char_index];
            let x = 50 + (word_char_index as u32 * 10);
            let y = 100;

            // Limpiar área de la palabra
            if word_char_index == 0 {
                fb.fill_rect_fast(x, y, (current_word.len() * 10) as u32, 8, Color::BLACK);
            }

            // Dibujar caracter de la palabra
            fb.draw_character(x, y, char_code as char, Color::YELLOW);
        }

        *char_index += 1;
    }

    /// Practicar análisis de mapas de bits
    fn practice_bitmap_analysis(&mut self, fb: &mut FramebufferDriver) {
        static mut BITMAP_STATE: u8 = 0;

        unsafe {
            BITMAP_STATE = (BITMAP_STATE + 1) % 64; // 8x8 bitmap

            let x = 300;
            let y = 150;
            let px = (BITMAP_STATE % 8) as u32;
            let py = (BITMAP_STATE / 8) as u32;

            // Simular análisis de bitmap
            let should_draw = (BITMAP_STATE % 3) == 0; // Patrón de dibujo

            if should_draw {
                fb.put_pixel(x + px, y + py, Color::CYAN);
            } else {
                fb.put_pixel(x + px, y + py, Color::BLACK);
            }
        }
    }

    /// Mostrar estadísticas de aprendizaje de la IA
    pub fn display_learning_stats(&mut self, fb: &mut FramebufferDriver) {
        static mut STATS_COUNTER: u32 = 0;

        unsafe {
            STATS_COUNTER += 1;

            // Actualizar estadísticas cada 500 iteraciones
            if STATS_COUNTER % 500 == 0 {
                let stats_x = 20;
                let stats_y = 300;

                // Limpiar área de estadísticas
                fb.fill_rect_fast(stats_x, stats_y, 300, 60, Color::BLACK);

                // Mostrar contador de práctica
                let counter_text = format!("Practicas: {}", STATS_COUNTER);
                fb.write_text_kernel(&counter_text, Color::WHITE);

                // Mostrar patrón actual
                let pattern_names = ["Alfabeto", "Aleatorio", "Palabras", "Bitmaps"];
                let current_pattern = (STATS_COUNTER / 1000) % 4;
                let pattern_text = format!("Patron: {}", pattern_names[current_pattern as usize]);
                fb.write_text_kernel(&pattern_text, Color::GREEN);

                // Mostrar estado del sistema
                let system_text = format!(
                    "IA: {}",
                    if self.is_typing {
                        "Escribiendo"
                    } else {
                        "Aprendiendo"
                    }
                );
                fb.write_text_kernel(&system_text, Color::YELLOW);
            }
        }
    }

    /// Función de aprendizaje adaptativo
    pub fn adaptive_learning(&mut self, fb: &mut FramebufferDriver) {
        static mut LEARNING_PHASE: u8 = 0;
        static mut SUCCESS_RATE: u32 = 0;
        static mut ATTEMPTS: u32 = 0;

        unsafe {
            ATTEMPTS += 1;

            // Cambiar fase de aprendizaje basado en éxito
            if ATTEMPTS % 100 == 0 {
                LEARNING_PHASE = (LEARNING_PHASE + 1) % 3;
                SUCCESS_RATE = (SUCCESS_RATE + 1) % 100; // Simular mejora
            }

            match LEARNING_PHASE {
                0 => {
                    // Fase de exploración - caracteres simples
                    self.practice_simple_characters(fb);
                }
                1 => {
                    // Fase de consolidación - palabras cortas
                    self.practice_short_words(fb);
                }
                2 => {
                    // Fase de maestría - patrones complejos
                    self.practice_complex_patterns(fb);
                }
                _ => self.practice_simple_characters(fb),
            }
        }
    }

    /// Practicar caracteres simples
    fn practice_simple_characters(&mut self, fb: &mut FramebufferDriver) {
        static mut SIMPLE_CHAR_INDEX: u8 = 0;

        unsafe {
            let simple_chars = b"AEIOU";
            let char_code = simple_chars[SIMPLE_CHAR_INDEX as usize % simple_chars.len()];

            let x = 400;
            let y = 100;

            fb.fill_rect_fast(x, y, 8, 8, Color::BLACK);
            fb.draw_character(x, y, char_code as char, Color::RED);

            SIMPLE_CHAR_INDEX += 1;
        }
    }

    /// Practicar palabras cortas
    fn practice_short_words(&mut self, fb: &mut FramebufferDriver) {
        static mut WORD_INDEX: u8 = 0;
        static mut CHAR_INDEX: u8 = 0;

        unsafe {
            let short_words = ["HI", "BYE", "YES", "NO"];
            let current_word = short_words[WORD_INDEX as usize % short_words.len()];

            if CHAR_INDEX < current_word.len() as u8 {
                let char_code = current_word.as_bytes()[CHAR_INDEX as usize];
                let x = 400 + (CHAR_INDEX as u32 * 10);
                let y = 120;

                fb.draw_character(x, y, char_code as char, Color::GREEN);
                CHAR_INDEX += 1;
            } else {
                CHAR_INDEX = 0;
                WORD_INDEX += 1;
            }
        }
    }

    /// Practicar patrones complejos
    fn practice_complex_patterns(&mut self, fb: &mut FramebufferDriver) {
        static mut PATTERN_STATE: u8 = 0;

        unsafe {
            PATTERN_STATE += 1;

            // Crear patrón en espiral
            let center_x = 450;
            let center_y = 200;
            let radius = (PATTERN_STATE / 8) as u32;
            let angle = (PATTERN_STATE % 8) as u32;

            let x = center_x + radius * (angle * 2);
            let y = center_y + radius * (angle * 2);

            if x < 600 && y < 400 {
                fb.put_pixel(x, y, Color::MAGENTA);
            }
        }
    }
}

/// Función de conveniencia para crear el sistema de AI
pub fn create_ai_typing_system() -> AiTypingSystem {
    AiTypingSystem::new()
}

/// Función para crear sistema con configuración personalizada
pub fn create_ai_typing_system_with_config(config: AiTypingConfig) -> AiTypingSystem {
    AiTypingSystem::with_config(config)
}
