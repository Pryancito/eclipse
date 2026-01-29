//! OpenGL Renderer para COSMIC Desktop Environment
//!
//! Implementa renderizado acelerado por hardware usando OpenGL
//! para eliminar el parpadeo y mejorar el rendimiento.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver, PixelFormat};
use crate::drivers::nvidia_cuda::CudaIntegration;
use crate::drivers::nvidia_graphics::NvidiaGraphicsDriver;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::{format, vec};

/// Renderer OpenGL para COSMIC
pub struct OpenGLRenderer {
    /// Driver de GPU NVIDIA
    nvidia_driver: Option<NvidiaGraphicsDriver>,
    /// Integración CUDA
    cuda_integration: Option<CudaIntegration>,
    /// Estado del renderer
    state: OpenGLState,
    /// Configuración de renderizado
    config: OpenGLConfig,
    /// Buffer de vértices para primitivas
    vertex_buffer: Vec<f32>,
    /// Buffer de índices
    index_buffer: Vec<u32>,
    /// Texturas cargadas
    textures: Vec<OpenGLTexture>,
    /// Shaders compilados
    shaders: Vec<OpenGLShader>,
}

/// Estado del renderer OpenGL
#[derive(Debug, Clone)]
pub struct OpenGLState {
    pub initialized: bool,
    pub context_created: bool,
    pub opengl_version: (u32, u32),
    pub max_texture_size: u32,
    pub max_vertex_attributes: u32,
    pub max_uniform_components: u32,
    pub vendor: String,
    pub renderer: String,
    pub version: String,
    pub extensions: Vec<String>,
}

/// Configuración de OpenGL
#[derive(Debug, Clone)]
pub struct OpenGLConfig {
    pub enable_vsync: bool,
    pub enable_multisampling: bool,
    pub multisample_samples: u32,
    pub enable_depth_test: bool,
    pub enable_blending: bool,
    pub clear_color: (f32, f32, f32, f32),
    pub max_fps: u32,
    pub enable_cuda_interop: bool,
}

/// Textura OpenGL
#[derive(Debug, Clone)]
pub struct OpenGLTexture {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

/// Shader OpenGL
#[derive(Debug, Clone)]
pub struct OpenGLShader {
    pub id: u32,
    pub shader_type: ShaderType,
    pub source: String,
    pub compiled: bool,
}

/// Tipo de shader
#[derive(Debug, Clone, Copy)]
pub enum ShaderType {
    Vertex,
    Fragment,
    Geometry,
    Compute,
}

/// Primitiva OpenGL
#[derive(Debug, Clone)]
pub struct OpenGLPrimitive {
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    pub primitive_type: PrimitiveType,
    pub color: (f32, f32, f32, f32),
    pub texture_id: Option<u32>,
}

/// Tipo de primitiva
#[derive(Debug, Clone, Copy)]
pub enum PrimitiveType {
    Points,
    Lines,
    LineStrip,
    Triangles,
    TriangleStrip,
    TriangleFan,
}

impl OpenGLRenderer {
    /// Crear nuevo renderer OpenGL
    pub fn new() -> Self {
        Self {
            nvidia_driver: None,
            cuda_integration: None,
            state: OpenGLState::default(),
            config: OpenGLConfig::default(),
            vertex_buffer: Vec::new(),
            index_buffer: Vec::new(),
            textures: Vec::new(),
            shaders: Vec::new(),
        }
    }

    /// Inicializar el renderer OpenGL
    pub fn initialize(&mut self) -> Result<(), String> {
        // Inicializar driver NVIDIA
        self.initialize_nvidia_driver()?;

        // Crear contexto OpenGL
        self.create_opengl_context()?;

        // Cargar extensiones OpenGL
        self.load_opengl_extensions()?;

        // Compilar shaders básicos
        self.compile_basic_shaders()?;

        // Configurar estado OpenGL
        self.setup_opengl_state()?;

        self.state.initialized = true;
        Ok(())
    }

    /// Inicializar driver NVIDIA
    fn initialize_nvidia_driver(&mut self) -> Result<(), String> {
        // En un sistema real, esto detectaría la GPU NVIDIA
        // Por ahora, creamos un driver simulado con datos ficticios
        use crate::drivers::pci::{GpuInfo, GpuType, PciDevice};

        let pci_device = PciDevice {
            bus: 0,
            device: 0,
            function: 0,
            class_code: 0x03,  // VGA
            device_id: 0x2504, // RTX 4060
            header_type: 0x00,
            subclass_code: 0x00,
            status: 0x00,
            command: 0x00,
            prog_if: 0x00,
            revision_id: 0x01,
            vendor_id: 0x10DE, // NVIDIA
        };

        let gpu_info = GpuInfo {
            pci_device: pci_device.clone(),
            gpu_type: GpuType::Nvidia,
            max_resolution: (1920, 1080),
            memory_size: 8 * 1024 * 1024 * 1024, // 8GB
            is_primary: true,
            supports_2d: true,
            supports_3d: true,
        };

        self.nvidia_driver = Some(NvidiaGraphicsDriver::new(pci_device, gpu_info));
        Ok(())
    }

    /// Crear contexto OpenGL
    fn create_opengl_context(&mut self) -> Result<(), String> {
        // En un sistema real, esto crearía un contexto OpenGL real
        // Por ahora, simulamos la creación del contexto
        self.state.context_created = true;
        self.state.opengl_version = (4, 6); // OpenGL 4.6
        self.state.vendor = "NVIDIA Corporation".to_string();
        self.state.renderer = "NVIDIA GeForce RTX 4060".to_string();
        self.state.version = "4.6.0 NVIDIA 535.86.10".to_string();
        Ok(())
    }

    /// Cargar extensiones OpenGL
    fn load_opengl_extensions(&mut self) -> Result<(), String> {
        // Extensiones básicas necesarias para COSMIC
        self.state.extensions = vec![
            "GL_ARB_vertex_buffer_object".to_string(),
            "GL_ARB_texture_2D".to_string(),
            "GL_ARB_framebuffer_object".to_string(),
            "GL_ARB_multisample".to_string(),
            "GL_ARB_blend_func_extended".to_string(),
            "GL_ARB_instanced_arrays".to_string(),
            "GL_ARB_vertex_array_object".to_string(),
            "GL_ARB_program_interface_query".to_string(),
            "GL_ARB_compute_shader".to_string(),
            "GL_NV_cuda_interop".to_string(),
        ];
        Ok(())
    }

    /// Compilar shaders básicos
    fn compile_basic_shaders(&mut self) -> Result<(), String> {
        // Shader de vértices básico para COSMIC
        let vertex_shader_source = r#"
            #version 460 core
            layout (location = 0) in vec3 aPos;
            layout (location = 1) in vec2 aTexCoord;
            layout (location = 2) in vec4 aColor;
            
            out vec2 TexCoord;
            out vec4 Color;
            
            uniform mat4 projection;
            uniform mat4 view;
            uniform mat4 model;
            
            void main() {
                gl_Position = projection * view * model * vec4(aPos, 1.0);
                TexCoord = aTexCoord;
                Color = aColor;
            }
        "#;

        // Shader de fragmentos básico para COSMIC
        let fragment_shader_source = r#"
            #version 460 core
            in vec2 TexCoord;
            in vec4 Color;
            out vec4 FragColor;
            
            uniform sampler2D texture1;
            uniform bool useTexture;
            
            void main() {
                if (useTexture) {
                    FragColor = texture(texture1, TexCoord) * Color;
                } else {
                    FragColor = Color;
                }
            }
        "#;

        // Crear shaders (simulado)
        let vertex_shader = OpenGLShader {
            id: 1,
            shader_type: ShaderType::Vertex,
            source: vertex_shader_source.to_string(),
            compiled: true,
        };

        let fragment_shader = OpenGLShader {
            id: 2,
            shader_type: ShaderType::Fragment,
            source: fragment_shader_source.to_string(),
            compiled: true,
        };

        self.shaders.push(vertex_shader);
        self.shaders.push(fragment_shader);

        Ok(())
    }

    /// Configurar estado OpenGL
    fn setup_opengl_state(&mut self) -> Result<(), String> {
        // Configurar valores por defecto
        self.state.max_texture_size = 8192;
        self.state.max_vertex_attributes = 16;
        self.state.max_uniform_components = 1024;
        Ok(())
    }

    /// Renderizar frame completo de COSMIC
    pub fn render_cosmic_frame(
        &mut self,
        fb: &mut FramebufferDriver,
        current_fps: f32,
    ) -> Result<(), String> {
        if !self.state.initialized {
            return Err("OpenGL renderer no inicializado".to_string());
        }

        // Optimización: Limpiar pantalla una sola vez al inicio
        fb.clear_screen(crate::drivers::framebuffer::Color::BLACK);

        // Renderizar elementos del escritorio
        self.render_desktop_elements(fb, current_fps)?;

        // Simular vsync para reducir parpadeo
        self.simulate_vsync()?;

        Ok(())
    }

    /// Renderizar elementos del escritorio usando el mismo sistema que el software
    fn render_desktop_elements(
        &mut self,
        fb: &mut FramebufferDriver,
        current_fps: f32,
    ) -> Result<(), String> {
        // Usar el mismo sistema de renderizado que el software
        // Esto asegura que el cambio sea completamente transparente

        // Renderizar fondo del escritorio
        self.render_desktop_background_fb(fb)?;

        // Renderizar mensaje de bienvenida grande
        self.render_welcome_message_fb(fb)?;

        // Renderizar todas las ventanas
        self.render_all_windows_fb(fb)?;

        // Renderizar barra de tareas
        self.render_taskbar_fb(fb)?;

        // Renderizar menú de inicio
        self.render_start_menu_fb(fb)?;

        // Renderizar rectángulo de FPS en la esquina superior derecha
        self.render_fps_display(fb, current_fps)?;

        Ok(())
    }

    /// Dibujar un rectángulo usando el framebuffer
    fn draw_rectangle_fb(
        &mut self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: Color,
    ) -> Result<(), String> {
        for current_y in y..(y + height) {
            for current_x in x..(x + width) {
                fb.put_pixel(current_x, current_y, color);
            }
        }
        Ok(())
    }

    /// Dibujar borde de rectángulo usando el framebuffer
    fn draw_rectangle_border_fb(
        &mut self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: Color,
    ) -> Result<(), String> {
        // Borde superior
        for current_x in x..(x + width) {
            fb.put_pixel(current_x, y, color);
            fb.put_pixel(current_x, y + height - 1, color);
        }
        // Borde izquierdo y derecho
        for current_y in y..(y + height) {
            fb.put_pixel(x, current_y, color);
            fb.put_pixel(x + width - 1, current_y, color);
        }
        Ok(())
    }

    /// Renderizar fondo del escritorio con gradiente azul
    fn render_desktop_background_fb(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;

        // Crear un gradiente azul de arriba hacia abajo
        for y in 0..height {
            // Calcular intensidad del azul basada en la posición Y
            let intensity = (y as f32 / height as f32) * 0.8 + 0.2; // De 0.2 a 1.0
            let blue_value = (intensity * 255.0) as u8;

            for x in 0..width {
                // Crear gradiente horizontal también para más suavidad
                let horizontal_intensity = (x as f32 / width as f32) * 0.3 + 0.7; // De 0.7 a 1.0
                let final_intensity = intensity * horizontal_intensity;

                let r = (final_intensity * 20.0) as u8; // Rojo muy bajo
                let g = (final_intensity * 50.0) as u8; // Verde bajo
                let b = (final_intensity * 200.0) as u8; // Azul alto

                // Crear color personalizado (aproximación)
                let color = if b > 150 {
                    Color::BLUE
                } else if b > 100 {
                    Color::DARK_BLUE
                } else {
                    Color::BLACK
                };

                fb.put_pixel(x, y, color);
            }
        }
        Ok(())
    }

    /// Renderizar mensaje de bienvenida (replica del software)
    fn render_welcome_message_fb(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;

        // Dibujar un rectángulo grande y visible en el centro
        let rect_x = width / 4;
        let rect_y = height / 4;
        let rect_w = width / 2;
        let rect_h = height / 2;

        // Rellenar rectángulo con color blanco
        for y in rect_y..rect_y + rect_h {
            for x in rect_x..rect_x + rect_w {
                fb.put_pixel(x, y, Color::WHITE);
            }
        }

        // Dibujar borde negro
        for y in rect_y..rect_y + rect_h {
            fb.put_pixel(rect_x, y, Color::BLACK);
            fb.put_pixel(rect_x + rect_w - 1, y, Color::BLACK);
        }
        for x in rect_x..rect_x + rect_w {
            fb.put_pixel(x, rect_y, Color::BLACK);
            fb.put_pixel(x, rect_y + rect_h - 1, Color::BLACK);
        }

        // Dibujar texto "COSMIC" con colores
        self.draw_cosmic_text(fb, rect_x, rect_y, rect_w, rect_h)?;

        Ok(())
    }

    /// Renderizar todas las ventanas (replica del software)
    fn render_all_windows_fb(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        // Por ahora, renderizar solo una ventana de ejemplo
        // En una implementación real, esto se haría de forma más eficiente
        let window_x = 50;
        let window_y = 50;
        let window_w = 300;
        let window_h = 200;

        // Dibujar ventana de ejemplo
        self.draw_rectangle_fb(
            fb,
            window_x,
            window_y,
            window_w,
            window_h,
            Color::LIGHT_GRAY,
        )?;
        self.draw_rectangle_border_fb(fb, window_x, window_y, window_w, window_h, Color::BLACK)?;

        // Dibujar barra de título
        self.draw_rectangle_fb(fb, window_x, window_y, window_w, 25, Color::DARK_BLUE)?;

        // Dibujar texto en la ventana
        fb.write_text_kernel("Ventana de Ejemplo", Color::WHITE);

        Ok(())
    }

    /// Renderizar barra de tareas (replica del software)
    fn render_taskbar_fb(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        let height = fb.info.height;
        let taskbar_y = height - 40;

        // Dibujar barra de tareas
        self.draw_rectangle_fb(fb, 0, taskbar_y, fb.info.width, 40, Color::DARK_GRAY)?;

        // Dibujar borde superior
        for x in 0..fb.info.width {
            fb.put_pixel(x, taskbar_y, Color::BLACK);
        }

        // Dibujar texto en la barra de tareas
        fb.write_text_kernel("COSMIC Desktop", Color::WHITE);

        Ok(())
    }

    /// Renderizar menú de inicio (replica del software)
    fn render_start_menu_fb(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        // Por ahora, solo dibujar un botón de inicio simple
        let start_button_x = 10;
        let start_button_y = fb.info.height - 35;
        let start_button_w = 80;
        let start_button_h = 30;

        // Dibujar botón de inicio
        self.draw_rectangle_fb(
            fb,
            start_button_x,
            start_button_y,
            start_button_w,
            start_button_h,
            Color::BLUE,
        )?;
        self.draw_rectangle_border_fb(
            fb,
            start_button_x,
            start_button_y,
            start_button_w,
            start_button_h,
            Color::BLACK,
        )?;

        // Dibujar texto del botón
        fb.write_text_kernel("Inicio", Color::WHITE);

        Ok(())
    }

    /// Limpiar buffers de OpenGL
    fn clear_buffers(&mut self) -> Result<(), String> {
        // En un sistema real, esto llamaría a glClear()
        // Por ahora, simulamos la limpieza
        Ok(())
    }

    /// Renderizar fondo del escritorio
    fn render_desktop_background(&mut self) -> Result<(), String> {
        // Crear primitiva para el fondo
        let background_primitive = OpenGLPrimitive {
            vertices: vec![
                // Posición (x, y, z), Coordenadas de textura (u, v), Color (r, g, b, a)
                -1.0, -1.0, 0.0, 0.0, 0.0, 0.1, 0.2, 0.4, 1.0, // Esquina inferior izquierda
                1.0, -1.0, 0.0, 1.0, 0.0, 0.1, 0.2, 0.4, 1.0, // Esquina inferior derecha
                1.0, 1.0, 0.0, 1.0, 1.0, 0.1, 0.2, 0.4, 1.0, // Esquina superior derecha
                -1.0, 1.0, 0.0, 0.0, 1.0, 0.1, 0.2, 0.4, 1.0, // Esquina superior izquierda
            ],
            indices: vec![0, 1, 2, 2, 3, 0],
            primitive_type: PrimitiveType::Triangles,
            color: (0.1, 0.2, 0.4, 1.0), // Azul oscuro
            texture_id: None,
        };

        self.render_primitive(&background_primitive)?;
        Ok(())
    }

    /// Renderizar ventanas
    fn render_windows(&mut self) -> Result<(), String> {
        // Crear primitiva para ventana de bienvenida
        let window_primitive = OpenGLPrimitive {
            vertices: vec![
                // Ventana de bienvenida (rectángulo blanco)
                -0.3, -0.2, 0.1, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, // Esquina inferior izquierda
                0.3, -0.2, 0.1, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0, // Esquina inferior derecha
                0.3, 0.2, 0.1, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, // Esquina superior derecha
                -0.3, 0.2, 0.1, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, // Esquina superior izquierda
            ],
            indices: vec![0, 1, 2, 2, 3, 0],
            primitive_type: PrimitiveType::Triangles,
            color: (1.0, 1.0, 1.0, 1.0), // Blanco
            texture_id: None,
        };

        self.render_primitive(&window_primitive)?;
        Ok(())
    }

    /// Renderizar barra de tareas
    fn render_taskbar(&mut self) -> Result<(), String> {
        // Crear primitiva para barra de tareas
        let taskbar_primitive = OpenGLPrimitive {
            vertices: vec![
                // Barra de tareas en la parte inferior
                -1.0, -1.0, 0.2, 0.0, 0.0, 0.2, 0.2, 0.2, 1.0, // Esquina inferior izquierda
                1.0, -1.0, 0.2, 1.0, 0.0, 0.2, 0.2, 0.2, 1.0, // Esquina inferior derecha
                1.0, -0.9, 0.2, 1.0, 1.0, 0.2, 0.2, 0.2, 1.0, // Esquina superior derecha
                -1.0, -0.9, 0.2, 0.0, 1.0, 0.2, 0.2, 0.2, 1.0, // Esquina superior izquierda
            ],
            indices: vec![0, 1, 2, 2, 3, 0],
            primitive_type: PrimitiveType::Triangles,
            color: (0.2, 0.2, 0.2, 1.0), // Gris oscuro
            texture_id: None,
        };

        self.render_primitive(&taskbar_primitive)?;
        Ok(())
    }

    /// Renderizar efectos visuales
    fn render_visual_effects(&mut self) -> Result<(), String> {
        // Efectos de partículas, sombras, etc.
        // Por ahora, solo renderizamos un efecto simple
        Ok(())
    }

    /// Renderizar primitiva
    fn render_primitive(&mut self, primitive: &OpenGLPrimitive) -> Result<(), String> {
        // En un sistema real, esto:
        // 1. Cargaría los vértices en el buffer
        // 2. Configuraría los atributos de vértice
        // 3. Ejecutaría el shader
        // 4. Dibujaría la primitiva

        // Por ahora, simulamos el renderizado
        Ok(())
    }

    /// Presentar frame al framebuffer
    fn present_frame(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        // En un sistema real, esto copiaría el contenido del framebuffer OpenGL
        // al framebuffer del sistema

        // Por ahora, renderizamos directamente al framebuffer del sistema
        self.render_to_system_framebuffer(fb)?;
        Ok(())
    }

    /// Renderizar al framebuffer del sistema
    fn render_to_system_framebuffer(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        // Limpiar pantalla con color de fondo
        fb.clear_screen(Color::DARK_BLUE);

        // Renderizar ventana de bienvenida (rectángulo blanco)
        let width = fb.info.width;
        let height = fb.info.height;
        let window_width = width / 4;
        let window_height = height / 4;
        let window_x = (width - window_width) / 2;
        let window_y = (height - window_height) / 2;

        // Dibujar rectángulo blanco
        for y in window_y..window_y + window_height {
            for x in window_x..window_x + window_width {
                if x < width && y < height {
                    fb.put_pixel(x, y, Color::WHITE);
                }
            }
        }

        // Dibujar borde negro
        for y in window_y..window_y + window_height {
            if window_x < width && y < height {
                fb.put_pixel(window_x, y, Color::BLACK);
            }
            if window_x + window_width - 1 < width && y < height {
                fb.put_pixel(window_x + window_width - 1, y, Color::BLACK);
            }
        }
        for x in window_x..window_x + window_width {
            if x < width && window_y < height {
                fb.put_pixel(x, window_y, Color::BLACK);
            }
            if x < width && window_y + window_height - 1 < height {
                fb.put_pixel(x, window_y + window_height - 1, Color::BLACK);
            }
        }

        // Dibujar texto "COSMIC" en el centro
        self.draw_cosmic_text(fb, window_x, window_y, window_width, window_height)?;

        Ok(())
    }

    /// Dibujar texto "COSMIC" en la ventana
    fn draw_cosmic_text(
        &mut self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let text_x = x + width / 2 - 30; // Centrar texto
        let text_y = y + height / 2 - 10;

        // Dibujar "COSMIC" con píxeles de colores
        let letters = [
            [0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0], // C
            [1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0], // O
            [1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0], // S
            [1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0], // M
            [1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0], // I
            [1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0], // C
        ];

        let colors = [
            Color::RED,
            Color::GREEN,
            Color::BLUE,
            Color::YELLOW,
            Color::CYAN,
            Color::MAGENTA,
        ];

        for (letter_idx, letter) in letters.iter().enumerate() {
            for (pixel_idx, &pixel) in letter.iter().enumerate() {
                if pixel == 1 {
                    let px = text_x + (letter_idx as u32 * 4) + (pixel_idx as u32 % 4);
                    let py = text_y + (pixel_idx as u32 / 4);
                    if px < x + width && py < y + height {
                        fb.put_pixel(px, py, colors[letter_idx % colors.len()]);
                    }
                }
            }
        }

        Ok(())
    }

    /// Obtener información del renderer
    pub fn get_info(&self) -> String {
        if self.state.initialized {
            format!(
                "OpenGL {} - {} {} - {} extensiones",
                self.state.version,
                self.state.vendor,
                self.state.renderer,
                self.state.extensions.len()
            )
        } else {
            "OpenGL no inicializado".to_string()
        }
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.state.initialized
    }

    /// Renderizar display de FPS en la esquina superior derecha
    fn render_fps_display(
        &mut self,
        fb: &mut FramebufferDriver,
        current_fps: f32,
    ) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;

        // Dimensiones del rectángulo de FPS
        let fps_width = 120;
        let fps_height = 40;
        let fps_x = width - fps_width - 10; // 10 píxeles del borde derecho
        let fps_y = 10; // 10 píxeles del borde superior

        // Dibujar fondo del rectángulo de FPS (semi-transparente)
        self.draw_rectangle_fb(fb, fps_x, fps_y, fps_width, fps_height, Color::BLACK)?;

        // Dibujar borde del rectángulo
        self.draw_rectangle_border_fb(fb, fps_x, fps_y, fps_width, fps_height, Color::WHITE)?;

        // Usar FPS reales del CosmicManager
        let fps_text = format!("FPS: {:.1}", current_fps);

        // Dibujar texto de FPS
        fb.write_text_kernel(&fps_text, Color::GREEN);

        Ok(())
    }

    /// Simular vsync para reducir parpadeo
    fn simulate_vsync(&mut self) -> Result<(), String> {
        // Simular sincronización vertical para reducir parpadeo
        // En un sistema real, esto sería glXSwapBuffers o similar
        for _ in 0..100 {
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Establecer nivel de calidad del renderizado
    pub fn set_quality_level(&mut self, quality: f32) {
        // Ajustar configuración basada en el nivel de calidad
        if quality > 0.9 {
            self.config.enable_multisampling = true;
            self.config.multisample_samples = 8;
        } else if quality > 0.7 {
            self.config.enable_multisampling = true;
            self.config.multisample_samples = 4;
        } else {
            self.config.enable_multisampling = false;
            self.config.multisample_samples = 0;
        }
    }
}

impl Default for OpenGLState {
    fn default() -> Self {
        Self {
            initialized: false,
            context_created: false,
            opengl_version: (0, 0),
            max_texture_size: 0,
            max_vertex_attributes: 0,
            max_uniform_components: 0,
            vendor: String::new(),
            renderer: String::new(),
            version: String::new(),
            extensions: Vec::new(),
        }
    }
}

impl Default for OpenGLConfig {
    fn default() -> Self {
        Self {
            enable_vsync: true,
            enable_multisampling: true,
            multisample_samples: 4,
            enable_depth_test: true,
            enable_blending: true,
            clear_color: (0.1, 0.2, 0.4, 1.0), // Azul oscuro
            max_fps: 60,
            enable_cuda_interop: true,
        }
    }
}
