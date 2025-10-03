//! Navegador web básico para Eclipse OS
//!
//! Proporciona funcionalidades básicas de navegación web con soporte para
//! HTML simplificado y protocolos HTTP básicos.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// Tipo de contenido web
#[derive(Debug, Clone)]
pub enum ContentType {
    Html,
    Text,
    Image,
    Unknown,
}

/// Página web
#[derive(Debug, Clone)]
pub struct WebPage {
    pub url: String,
    pub title: String,
    pub content: String,
    pub content_type: ContentType,
    pub links: Vec<String>,
    pub images: Vec<String>,
}

/// Navegador web
pub struct Browser {
    pub current_url: String,
    pub history: Vec<String>,
    pub history_index: usize,
    pub bookmarks: Vec<String>,
    pub cache: BTreeMap<String, WebPage>,
    pub user_agent: String,
    pub window_width: u32,
    pub window_height: u32,
}

impl Browser {
    pub fn new() -> Self {
        Self {
            current_url: String::new(),
            history: Vec::new(),
            history_index: 0,
            bookmarks: Vec::new(),
            cache: BTreeMap::new(),
            user_agent: "Eclipse Browser 1.0".to_string(),
            window_width: 1024,
            window_height: 768,
        }
    }

    /// Ejecutar el navegador
    pub fn run(&mut self) -> Result<(), &'static str> {
        self.show_welcome();
        self.show_help();

        // Simular navegación a una página de inicio
        self.navigate_to("http://eclipse-os.local/welcome")?;

        Ok(())
    }

    fn show_welcome(&self) {
        self.print_info("╔══════════════════════════════════════════════════════════════╗");
        self.print_info("║                                                              ║");
        self.print_info("║                    ECLIPSE BROWSER                           ║");
        self.print_info("║                                                              ║");
        self.print_info("║  Navegador web básico con soporte para HTML simplificado   ║");
        self.print_info("║  Escribe 'help' para ver comandos disponibles              ║");
        self.print_info("║  Escribe 'quit' para salir                                 ║");
        self.print_info("║                                                              ║");
        self.print_info("╚══════════════════════════════════════════════════════════════╝");
        self.print_info("");
    }

    fn show_help(&self) {
        self.print_info("Comandos disponibles:");
        self.print_info("  navigate <url>  - Navega a una URL");
        self.print_info("  back            - Página anterior");
        self.print_info("  forward         - Página siguiente");
        self.print_info("  refresh         - Recarga la página actual");
        self.print_info("  home            - Página de inicio");
        self.print_info("  bookmarks       - Muestra marcadores");
        self.print_info("  add-bookmark    - Añade marcador");
        self.print_info("  history         - Muestra historial");
        self.print_info("  clear-cache     - Limpia la caché");
        self.print_info("  help            - Muestra esta ayuda");
        self.print_info("  quit            - Sale del navegador");
        self.print_info("");
    }

    /// Navegar a una URL
    pub fn navigate_to(&mut self, url: &str) -> Result<(), &'static str> {
        self.print_info(&format!("Navegando a: {}", url));

        // Añadir a historial
        if self.current_url != url {
            self.history.push(url.to_string());
            self.history_index = self.history.len();
        }

        self.current_url = url.to_string();

        // Verificar caché
        if let Some(page) = self.cache.get(url) {
            self.display_page(page);
            return Ok(());
        }

        // Simular descarga de página
        let page = self.download_page(url)?;
        self.cache.insert(url.to_string(), page.clone());
        self.display_page(&page);

        Ok(())
    }

    fn download_page(&self, url: &str) -> Result<WebPage, &'static str> {
        // Simular descarga de diferentes páginas
        match url {
            "http://eclipse-os.local/welcome" => Ok(WebPage {
                url: url.to_string(),
                title: "Bienvenido a Eclipse OS".to_string(),
                content: self.get_welcome_page_content(),
                content_type: ContentType::Html,
                links: vec![
                    "http://eclipse-os.local/about".to_string(),
                    "http://eclipse-os.local/features".to_string(),
                    "http://eclipse-os.local/download".to_string(),
                ],
                images: vec!["http://eclipse-os.local/logo.png".to_string()],
            }),
            "http://eclipse-os.local/about" => Ok(WebPage {
                url: url.to_string(),
                title: "Acerca de Eclipse OS".to_string(),
                content: self.get_about_page_content(),
                content_type: ContentType::Html,
                links: vec![
                    "http://eclipse-os.local/welcome".to_string(),
                    "http://eclipse-os.local/features".to_string(),
                ],
                images: vec![],
            }),
            "http://eclipse-os.local/features" => Ok(WebPage {
                url: url.to_string(),
                title: "Características de Eclipse OS".to_string(),
                content: self.get_features_page_content(),
                content_type: ContentType::Html,
                links: vec![
                    "http://eclipse-os.local/welcome".to_string(),
                    "http://eclipse-os.local/about".to_string(),
                ],
                images: vec![],
            }),
            _ => Ok(WebPage {
                url: url.to_string(),
                title: "Página no encontrada".to_string(),
                content: self.get_404_page_content(),
                content_type: ContentType::Html,
                links: vec!["http://eclipse-os.local/welcome".to_string()],
                images: vec![],
            }),
        }
    }

    fn display_page(&self, page: &WebPage) {
        self.print_info(&format!("Título: {}", page.title));
        self.print_info(&format!("URL: {}", page.url));
        self.print_info(&"─".repeat(60));
        self.print_info("");

        // Mostrar contenido HTML simplificado
        self.render_html(&page.content);

        self.print_info("");
        self.print_info(&"─".repeat(60));

        if !page.links.is_empty() {
            self.print_info("Enlaces disponibles:");
            for (i, link) in page.links.iter().enumerate() {
                self.print_info(&format!("  {}: {}", i + 1, link));
            }
        }

        self.print_info("");
    }

    fn render_html(&self, html: &str) {
        // Renderizado muy básico de HTML
        let mut in_tag = false;
        let mut current_text = String::new();

        for ch in html.chars() {
            match ch {
                '<' => {
                    if !current_text.trim().is_empty() {
                        self.print_info(&current_text);
                        current_text.clear();
                    }
                    in_tag = true;
                }
                '>' => {
                    in_tag = false;
                }
                _ => {
                    if !in_tag {
                        current_text.push(ch);
                    }
                }
            }
        }

        if !current_text.trim().is_empty() {
            self.print_info(&current_text);
        }
    }

    fn get_welcome_page_content(&self) -> String {
        "<h1>Bienvenido a Eclipse OS</h1>
<p>Eclipse OS es un sistema operativo moderno construido en Rust.</p>

<h2>Características principales:</h2>
<ul>
<li>Kernel monolítico con microkernel</li>
<li>Sistema de ventanas avanzado</li>
<li>Soporte para Wayland</li>
<li>Drivers de hardware modernos</li>
<li>Sistema de archivos robusto</li>
<li>Navegador web integrado</li>
</ul>

<p>Para más información, visita nuestras páginas:</p>
<ul>
<li><a href='http://eclipse-os.local/about'>Acerca de</a></li>
<li><a href='http://eclipse-os.local/features'>Características</a></li>
<li><a href='http://eclipse-os.local/download'>Descargar</a></li>
</ul>"
            .to_string()
    }

    fn get_about_page_content(&self) -> String {
        "<h1>Acerca de Eclipse OS</h1>
<p>Eclipse OS es un proyecto de sistema operativo de código abierto desarrollado en Rust.</p>

<h2>Historia del proyecto</h2>
<p>Eclipse OS comenzó como un experimento para crear un sistema operativo moderno
utilizando las características de seguridad y rendimiento de Rust.</p>

<h2>Equipo de desarrollo</h2>
<p>El proyecto está desarrollado por un equipo de entusiastas de los sistemas operativos
y desarrolladores de Rust.</p>

<h2>Licencia</h2>
<p>Eclipse OS está licenciado bajo la Licencia MIT.</p>"
            .to_string()
    }

    fn get_features_page_content(&self) -> String {
        "<h1>Características de Eclipse OS</h1>

<h2>Kernel</h2>
<ul>
<li>Arquitectura híbrida monolítica/microkernel</li>
<li>Gestión de memoria avanzada</li>
<li>Soporte para múltiples arquitecturas</li>
<li>Drivers de hardware modernos</li>
</ul>

<h2>Sistema de archivos</h2>
<ul>
<li>Soporte para FAT32 y sistemas nativos</li>
<li>Cache inteligente con algoritmo LRU</li>
<li>Persistencia en disco</li>
<li>Operaciones de archivo completas</li>
</ul>

<h2>Interfaz de usuario</h2>
<ul>
<li>Sistema de ventanas avanzado</li>
<li>Soporte para Wayland</li>
<li>Aplicaciones nativas</li>
<li>Terminal integrado</li>
</ul>"
            .to_string()
    }

    fn get_404_page_content(&self) -> String {
        "<h1>Error 404 - Página no encontrada</h1>
<p>La página que buscas no existe o ha sido movida.</p>
<p><a href='http://eclipse-os.local/welcome'>Volver al inicio</a></p>"
            .to_string()
    }

    /// Ir a la página anterior
    pub fn go_back(&mut self) -> Result<(), &'static str> {
        if self.history_index > 1 {
            self.history_index -= 1;
            let url = self.history[self.history_index - 1].clone();
            self.navigate_to(&url)?;
        } else {
            self.print_info("No hay páginas anteriores");
        }
        Ok(())
    }

    /// Ir a la página siguiente
    pub fn go_forward(&mut self) -> Result<(), &'static str> {
        if self.history_index < self.history.len() {
            let url = self.history[self.history_index].clone();
            self.history_index += 1;
            self.navigate_to(&url)?;
        } else {
            self.print_info("No hay páginas siguientes");
        }
        Ok(())
    }

    /// Recargar página actual
    pub fn refresh(&mut self) -> Result<(), &'static str> {
        if !self.current_url.is_empty() {
            // Remover de caché para forzar recarga
            self.cache.remove(&self.current_url);
            let url = self.current_url.clone();
            self.navigate_to(&url)?;
        } else {
            self.print_info("No hay página para recargar");
        }
        Ok(())
    }

    /// Ir a página de inicio
    pub fn go_home(&mut self) -> Result<(), &'static str> {
        self.navigate_to("http://eclipse-os.local/welcome")
    }

    /// Mostrar marcadores
    pub fn show_bookmarks(&self) {
        if self.bookmarks.is_empty() {
            self.print_info("No hay marcadores guardados");
        } else {
            self.print_info("Marcadores guardados:");
            for (i, bookmark) in self.bookmarks.iter().enumerate() {
                self.print_info(&format!("  {}: {}", i + 1, bookmark));
            }
        }
    }

    /// Añadir marcador
    pub fn add_bookmark(&mut self, url: &str) {
        if !self.bookmarks.contains(&url.to_string()) {
            self.bookmarks.push(url.to_string());
            self.print_info(&format!("Marcador añadido: {}", url));
        } else {
            self.print_info("El marcador ya existe");
        }
    }

    /// Mostrar historial
    pub fn show_history(&self) {
        if self.history.is_empty() {
            self.print_info("No hay historial");
        } else {
            self.print_info("Historial de navegación:");
            for (i, url) in self.history.iter().enumerate() {
                let marker = if i == self.history_index - 1 {
                    ">"
                } else {
                    " "
                };
                self.print_info(&format!("{} {}: {}", marker, i + 1, url));
            }
        }
    }

    /// Limpiar caché
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.print_info("Caché limpiada");
    }

    fn print_info(&self, text: &str) {
        // En una implementación real, esto renderizaría en la interfaz gráfica
        // Por ahora solo simulamos
    }
}

/// Función principal para ejecutar el navegador
pub fn run() -> Result<(), &'static str> {
    let mut browser = Browser::new();
    browser.run()
}
