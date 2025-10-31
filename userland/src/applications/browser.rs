use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;
//! Navegador Web para Eclipse OS
//! 
//! Implementa un navegador web básico con:
//! - Renderizado de HTML simple
//! - Navegación por pestañas
//! - Historial de navegación
//! - Marcadores
//! - Búsqueda en la web
//! - Descarga de archivos

use Result<(), &'static str>;
// use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
// use std::sync::{Arc, Mutex};
use core::time::Duration;

/// Navegador principal
pub struct WebBrowser {
    /// Pestañas abiertas
    tabs: Vec<BrowserTab>,
    /// Pestaña actual
    current_tab: usize,
    /// Historial de navegación
    history: VecDeque<HistoryEntry>,
    /// Marcadores
    bookmarks: Vec<Bookmark>,
    /// Configuración del navegador
    config: BrowserConfig,
    /// Estado del navegador
    state: BrowserState,
    /// Motor de renderizado
    renderer: HtmlRenderer,
    /// Gestor de descargas
    download_manager: DownloadManager,
}

/// Pestaña del navegador
#[derive(Debug, Clone)]
pub struct BrowserTab {
    pub id: u32,
    pub title: String,
    pub url: String,
    pub content: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub zoom_level: f32,
    pub scroll_position: (u32, u32),
    pub created_at: Instant,
}

/// Entrada del historial
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub title: String,
    pub url: String,
    pub visit_time: Instant,
    pub visit_count: u32,
}

/// Marcador
#[derive(Debug, Clone)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
    pub folder: String,
    pub created_at: Instant,
    pub tags: Vec<String>,
}

/// Configuración del navegador
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub homepage: String,
    pub search_engine: String,
    pub default_zoom: f32,
    pub enable_javascript: bool,
    pub enable_images: bool,
    pub enable_cookies: bool,
    pub enable_cache: bool,
    pub cache_size: u64,
    pub history_days: u32,
    pub download_location: String,
    pub user_agent: String,
}

/// Estados del navegador
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserState {
    Idle,
    Loading,
    Error(String),
    Offline,
}

/// Motor de renderizado HTML
pub struct HtmlRenderer {
    /// Parser HTML
    parser: HtmlParser,
    /// Renderizador de CSS
    css_engine: CssEngine,
    /// Motor de JavaScript
    js_engine: JsEngine,
}

/// Parser HTML
pub struct HtmlParser {
    /// Documento HTML parseado
    document: HtmlDocument,
}

/// Documento HTML
#[derive(Debug, Clone)]
pub struct HtmlDocument {
    pub title: String,
    pub body: String,
    pub links: Vec<HtmlLink>,
    pub images: Vec<HtmlImage>,
    pub scripts: Vec<String>,
    pub styles: Vec<String>,
}

/// Enlace HTML
#[derive(Debug, Clone)]
pub struct HtmlLink {
    pub text: String,
    pub url: String,
    pub target: String,
}

/// Imagen HTML
#[derive(Debug, Clone)]
pub struct HtmlImage {
    pub src: String,
    pub alt: String,
    pub width: u32,
    pub height: u32,
}

/// Motor CSS
pub struct CssEngine {
    /// Estilos aplicados
    styles: BTreeMap<String, CssRule>,
}

/// Regla CSS
#[derive(Debug, Clone)]
pub struct CssRule {
    pub selector: String,
    pub properties: BTreeMap<String, String>,
}

/// Motor JavaScript
pub struct JsEngine {
    /// Contexto de ejecución
    context: JsContext,
}

/// Contexto JavaScript
#[derive(Debug, Clone)]
pub struct JsContext {
    pub variables: BTreeMap<String, String>,
    pub functions: BTreeMap<String, String>,
}

/// Gestor de descargas
pub struct DownloadManager {
    /// Descargas activas
    downloads: BTreeMap<u32, Download>,
    /// Próximo ID de descarga
    next_download_id: u32,
}

/// Descarga
#[derive(Debug, Clone)]
pub struct Download {
    pub id: u32,
    pub url: String,
    pub filename: String,
    pub size: u64,
    pub downloaded: u64,
    pub status: DownloadStatus,
    pub start_time: Instant,
}

/// Estado de descarga
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed(String),
    Cancelled,
}

impl WebBrowser {
    /// Crear nuevo navegador
    pub fn new(config: BrowserConfig) -> Self {
        let mut browser = Self {
            tabs: Vec::new(),
            current_tab: 0,
            history: VecDeque::new(),
            bookmarks: Vec::new(),
            config,
            state: BrowserState::Idle,
            renderer: HtmlRenderer::new(),
            download_manager: DownloadManager::new(),
        };
        
        // Crear pestaña inicial
        browser.new_tab();
        
        browser
    }

    /// Crear nueva pestaña
    pub fn new_tab(&mut self) -> u32 {
        let tab_id = self.tabs.len() as u32 + 1;
        let tab = BrowserTab {
            id: tab_id,
            title: "Nueva pestaña".to_string(),
            url: "about:blank".to_string(),
            content: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            zoom_level: self.config.default_zoom,
            scroll_position: (0, 0),
            created_at: 0 // Simulado,
        };
        
        self.tabs.push(tab);
        self.current_tab = self.tabs.len() - 1;
        tab_id
    }

    /// Cerrar pestaña
    pub fn close_tab(&mut self, tab_id: u32) -> Result<(), &'static str> {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == tab_id) {
            self.tabs.remove(pos);
            
            // Ajustar pestaña actual
            if self.current_tab >= self.tabs.len() && !self.tabs.is_empty() {
                self.current_tab = self.tabs.len() - 1;
            } else if self.tabs.is_empty() {
                self.new_tab();
            }
        }
        Ok(())
    }

    /// Cambiar a pestaña
    pub fn switch_tab(&mut self, tab_id: u32) -> Result<(), &'static str> {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == tab_id) {
            self.current_tab = pos;
        } else {
            return Err(anyhow::anyhow!("Pestaña con ID {} no encontrada", tab_id));
        }
        Ok(())
    }

    /// Navegar a URL
    pub fn navigate_to(&mut self, url: &str) -> Result<(), &'static str> {
        let tab = &mut self.tabs[self.current_tab];
        
        println!("🌐 Navegando a: {}", url);
        
        tab.loading = true;
        tab.url = url.to_string();
        self.state = BrowserState::Loading;
        
        // Simular carga de página
        tokio::time::sleep(Duration::from_millis(500));
        
        // Renderizar contenido
        let content = self.render_page(url)?;
        tab.content = content;
        tab.loading = false;
        tab.title = self.extract_title(&tab.content);
        
        // Agregar al historial
        self.add_to_history(&tab.title, url);
        
        self.state = BrowserState::Idle;
        println!("   ✓ Página cargada: {}", tab.title);
        
        Ok(())
    }

    /// Renderizar página
    fn render_page(&self, url: &str) -> Result<String, &'static str> {
        match url {
            "about:blank" => Ok(self.render_blank_page()),
            "about:home" => Ok(self.render_home_page()),
            "about:history" => Ok(self.render_history_page()),
            "about:bookmarks" => Ok(self.render_bookmarks_page()),
            "about:downloads" => Ok(self.render_downloads_page()),
            _ => Ok(self.render_web_page(url)?),
        }
    }

    /// Renderizar página en blanco
    fn render_blank_page(&self) -> String {
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>Página en blanco</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 50px; }
        .center { text-align: center; margin-top: 100px; }
    </style>
</head>
<body>
    <div class="center">
        <h1>Página en blanco</h1>
        <p>Escribe una URL en la barra de direcciones para comenzar a navegar.</p>
    </div>
</body>
</html>
        "#.to_string()
    }

    /// Renderizar página de inicio
    fn render_home_page(&self) -> String {
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>Eclipse OS Browser</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 0; padding: 20px; background: #f0f0f0; }
        .header { text-align: center; margin-bottom: 30px; }
        .search-box { width: 500px; padding: 10px; font-size: 16px; border: 1px solid #ccc; border-radius: 5px; }
        .search-btn { padding: 10px 20px; font-size: 16px; background: #007bff; color: white; border: none; border-radius: 5px; cursor: pointer; }
        .quick-links { display: grid; grid-template-columns: repeat(4, 1fr); gap: 20px; margin-top: 30px; }
        .quick-link { background: white; padding: 20px; border-radius: 5px; text-align: center; box-shadow: 0 2px 5px rgba(0,0,0,0.1); }
        .quick-link a { text-decoration: none; color: #333; }
    </style>
</head>
<body>
    <div class="header">
        <h1>🌐 Eclipse OS Browser</h1>
        <p>Navegador web moderno para Eclipse OS</p>
        <input type="text" class="search-box" placeholder="Buscar en la web o escribir URL...">
        <button class="search-btn">Buscar</button>
    </div>
    
    <div class="quick-links">
        <div class="quick-link">
            <a href="https://eclipse-os.org">
                <h3>Eclipse OS</h3>
                <p>Sitio oficial</p>
            </a>
        </div>
        <div class="quick-link">
            <a href="https://github.com/eclipse-os">
                <h3>GitHub</h3>
                <p>Código fuente</p>
            </a>
        </div>
        <div class="quick-link">
            <a href="https://docs.eclipse-os.org">
                <h3>Documentación</h3>
                <p>Guías y manuales</p>
            </a>
        </div>
        <div class="quick-link">
            <a href="https://forum.eclipse-os.org">
                <h3>Foro</h3>
                <p>Comunidad</p>
            </a>
        </div>
    </div>
</body>
</html>
        "#.to_string()
    }

    /// Renderizar página de historial
    fn render_history_page(&self) -> String {
        let mut html = r#"
<!DOCTYPE html>
<html>
<head>
    <title>Historial - Eclipse OS Browser</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 20px; }
        .history-item { padding: 10px; border-bottom: 1px solid #eee; }
        .history-item:hover { background: #f5f5f5; }
        .history-title { font-weight: bold; color: #333; }
        .history-url { color: #666; font-size: 14px; }
        .history-time { color: #999; font-size: 12px; }
    </style>
</head>
<body>
    <h1>📚 Historial de Navegación</h1>
        "#.to_string();

        for entry in &self.history {
            html.push_str(&format!(
                r#"<div class="history-item">
                    <div class="history-title">{}</div>
                    <div class="history-url">{}</div>
                    <div class="history-time">Visitado {} veces</div>
                </div>"#,
                entry.title, entry.url, entry.visit_count
            ));
        }

        html.push_str("</body></html>");
        html
    }

    /// Renderizar página de marcadores
    fn render_bookmarks_page(&self) -> String {
        let mut html = r#"
<!DOCTYPE html>
<html>
<head>
    <title>Marcadores - Eclipse OS Browser</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 20px; }
        .bookmark-item { padding: 10px; border-bottom: 1px solid #eee; }
        .bookmark-item:hover { background: #f5f5f5; }
        .bookmark-title { font-weight: bold; color: #333; }
        .bookmark-url { color: #666; font-size: 14px; }
        .bookmark-folder { color: #999; font-size: 12px; }
    </style>
</head>
<body>
    <h1>🔖 Marcadores</h1>
        "#.to_string();

        for bookmark in &self.bookmarks {
            html.push_str(&format!(
                r#"<div class="bookmark-item">
                    <div class="bookmark-title">{}</div>
                    <div class="bookmark-url">{}</div>
                    <div class="bookmark-folder">Carpeta: {}</div>
                </div>"#,
                bookmark.title, bookmark.url, bookmark.folder
            ));
        }

        html.push_str("</body></html>");
        html
    }

    /// Renderizar página de descargas
    fn render_downloads_page(&self) -> String {
        let mut html = r#"
<!DOCTYPE html>
<html>
<head>
    <title>Descargas - Eclipse OS Browser</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 20px; }
        .download-item { padding: 10px; border-bottom: 1px solid #eee; }
        .download-item:hover { background: #f5f5f5; }
        .download-filename { font-weight: bold; color: #333; }
        .download-url { color: #666; font-size: 14px; }
        .download-status { color: #999; font-size: 12px; }
        .progress-bar { width: 100%; height: 20px; background: #f0f0f0; border-radius: 10px; overflow: hidden; }
        .progress-fill { height: 100%; background: #007bff; transition: width 0.3s; }
    </style>
</head>
<body>
    <h1>⬇️ Descargas</h1>
        "#.to_string();

        for download in self.download_manager.downloads.values() {
            let progress = if download.size > 0 {
                (download.downloaded as f32 / download.size as f32) * 100.0
            } else {
                0.0
            };

            html.push_str(&format!(
                r#"<div class="download-item">
                    <div class="download-filename">{}</div>
                    <div class="download-url">{}</div>
                    <div class="download-status">Estado: {:?}</div>
                    <div class="progress-bar">
                        <div class="progress-fill" style="width: {}%"></div>
                    </div>
                </div>"#,
                download.filename, download.url, download.status, progress
            ));
        }

        html.push_str("</body></html>");
        html
    }

    /// Renderizar página web
    fn render_web_page(&self, url: &str) -> Result<String, &'static str> {
        // Simular descarga de página web
        tokio::time::sleep(Duration::from_millis(300));
        
        // Simular contenido de página web
        let content = match url {
            url if url.contains("eclipse-os.org") => self.render_eclipse_os_page(),
            url if url.contains("github.com") => self.render_github_page(),
            url if url.contains("wikipedia.org") => self.render_wikipedia_page(),
            _ => self.render_generic_page(url),
        };
        
        Ok(content)
    }

    /// Renderizar página de Eclipse OS
    fn render_eclipse_os_page(&self) -> String {
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>Eclipse OS - Sistema Operativo Moderno</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 0; padding: 20px; background: linear-gradient(135deg, #667eea 0%, #764ba2 100%); color: white; }
        .container { max-width: 1200px; margin: 0 auto; }
        .header { text-align: center; margin-bottom: 50px; }
        .feature { background: rgba(255,255,255,0.1); padding: 20px; margin: 20px 0; border-radius: 10px; }
        .feature h3 { color: #ffd700; }
    </style>
</head>
<body>
    <div class="container">
        <div class="header">
            <h1>🌟 Eclipse OS</h1>
            <p>Sistema Operativo Moderno Desarrollado en Rust</p>
        </div>
        
        <div class="feature">
            <h3>🚀 Kernel Híbrido</h3>
            <p>Kernel híbrido que combina lo mejor de los kernels monolíticos y microkernels, desarrollado completamente en Rust para máxima seguridad y rendimiento.</p>
        </div>
        
        <div class="feature">
            <h3>🖥️ GUI Avanzada</h3>
            <p>Interfaz gráfica moderna con compositor de capas, animaciones suaves y soporte para múltiples monitores.</p>
        </div>
        
        <div class="feature">
            <h3>🔧 Drivers Modulares</h3>
            <p>Sistema de drivers modulares con soporte para hardware moderno, incluyendo GPU, audio, red y dispositivos de entrada.</p>
        </div>
        
        <div class="feature">
            <h3>⚡ Optimización de Rendimiento</h3>
            <p>Sistema de optimización avanzado con gestión inteligente de memoria, scheduler predictivo y balanceador de carga.</p>
        </div>
        
        <div class="feature">
            <h3>🔒 Seguridad Integrada</h3>
            <p>Sistema de seguridad robusto con autenticación, control de acceso y auditoría de seguridad integrados.</p>
        </div>
    </div>
</body>
</html>
        "#.to_string()
    }

    /// Renderizar página de GitHub
    fn render_github_page(&self) -> String {
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>eclipse-os - GitHub</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Arial, sans-serif; margin: 0; padding: 20px; background: #0d1117; color: #c9d1d9; }
        .header { border-bottom: 1px solid #30363d; padding-bottom: 20px; margin-bottom: 20px; }
        .repo-name { color: #58a6ff; font-size: 24px; font-weight: bold; }
        .repo-description { color: #8b949e; margin: 10px 0; }
        .stats { display: flex; gap: 20px; margin: 20px 0; }
        .stat { background: #161b22; padding: 10px; border-radius: 6px; border: 1px solid #30363d; }
        .readme { background: #0d1117; padding: 20px; border-radius: 6px; border: 1px solid #30363d; }
    </style>
</head>
<body>
    <div class="header">
        <div class="repo-name">eclipse-os/eclipse-os</div>
        <div class="repo-description">Sistema operativo moderno desarrollado en Rust con kernel híbrido, GUI avanzada y optimizaciones de rendimiento.</div>
        <div class="stats">
            <div class="stat">⭐ 1,234 stars</div>
            <div class="stat">🍴 456 forks</div>
            <div class="stat">🐛 12 issues</div>
            <div class="stat">📝 MIT License</div>
        </div>
    </div>
    
    <div class="readme">
        <h2>📖 README</h2>
        <h3>Eclipse OS</h3>
        <p>Un sistema operativo moderno desarrollado desde cero en Rust, diseñado para ser seguro, eficiente y fácil de usar.</p>
        
        <h3>Características Principales</h3>
        <ul>
            <li><strong>Kernel Híbrido:</strong> Combina lo mejor de los kernels monolíticos y microkernels</li>
            <li><strong>Desarrollado en Rust:</strong> Máxima seguridad de memoria y rendimiento</li>
            <li><strong>GUI Moderna:</strong> Interfaz gráfica con compositor de capas</li>
            <li><strong>Drivers Modulares:</strong> Sistema de drivers extensible</li>
            <li><strong>Optimización:</strong> Gestión inteligente de recursos</li>
        </ul>
        
        <h3>Instalación</h3>
        <pre><code># Clonar el repositorio
git clone https://github.com/eclipse-os/eclipse-os.git
cd eclipse-os

# Compilar el sistema
cargo build --release

# Instalar
sudo ./install.sh</code></pre>
        
        <h3>Contribuir</h3>
        <p>Las contribuciones son bienvenidas. Por favor, lee nuestro <a href="#">CONTRIBUTING.md</a> para más detalles.</p>
    </div>
</body>
</html>
        "#.to_string()
    }

    /// Renderizar página de Wikipedia
    fn render_wikipedia_page(&self) -> String {
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>Sistema operativo - Wikipedia</title>
    <style>
        body { font-family: 'Linux Libertine', 'Times New Roman', serif; margin: 0; padding: 20px; background: #f6f6f6; }
        .content { max-width: 800px; margin: 0 auto; background: white; padding: 20px; border-radius: 5px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }
        .infobox { float: right; width: 300px; background: #f8f9fa; border: 1px solid #a2a9b1; margin: 0 0 20px 20px; padding: 10px; }
        .infobox th { background: #c6c6c6; padding: 5px; }
        .infobox td { padding: 5px; }
        h1 { color: #000; border-bottom: 1px solid #a2a9b1; }
        h2 { color: #000; border-bottom: 1px solid #a2a9b1; }
        .toc { background: #f8f9fa; border: 1px solid #a2a9b1; padding: 10px; margin: 20px 0; }
    </style>
</head>
<body>
    <div class="content">
        <h1>Sistema operativo</h1>
        
        <div class="infobox">
            <table>
                <tr><th colspan="2">Sistema operativo</th></tr>
                <tr><td>Familia</td><td>Unix-like, Windows, etc.</td></tr>
                <tr><td>Modelo de desarrollo</td><td>Open source, Closed source</td></tr>
                <tr><td>Primera versión</td><td>1950s</td></tr>
                <tr><td>Última versión</td><td>Varia por sistema</td></tr>
            </table>
        </div>
        
        <p>Un <strong>sistema operativo</strong> (SO) es el software que gestiona los recursos del hardware y proporciona servicios comunes para los programas de aplicación. El sistema operativo es un componente esencial del software del sistema en un sistema informático.</p>
        
        <div class="toc">
            <h3>Índice</h3>
            <ol>
                <li><a href="#definicion">Definición</a></li>
                <li><a href="#funciones">Funciones principales</a></li>
                <li><a href="#tipos">Tipos de sistemas operativos</a></li>
                <li><a href="#historia">Historia</a></li>
                <li><a href="#ejemplos">Ejemplos</a></li>
            </ol>
        </div>
        
        <h2 id="definicion">Definición</h2>
        <p>Un sistema operativo es un conjunto de programas que actúan como intermediario entre el usuario y el hardware de un ordenador. Su propósito es proporcionar un entorno en el cual el usuario pueda ejecutar programas de manera conveniente y eficiente.</p>
        
        <h2 id="funciones">Funciones principales</h2>
        <ul>
            <li><strong>Gestión de procesos:</strong> Controla la ejecución de programas</li>
            <li><strong>Gestión de memoria:</strong> Asigna y libera memoria</li>
            <li><strong>Gestión de archivos:</strong> Organiza y controla el acceso a archivos</li>
            <li><strong>Gestión de dispositivos:</strong> Controla el hardware</li>
            <li><strong>Interfaz de usuario:</strong> Proporciona una interfaz para interactuar</li>
        </ul>
        
        <h2 id="tipos">Tipos de sistemas operativos</h2>
        <h3>Por número de usuarios</h3>
        <ul>
            <li><strong>Monousuario:</strong> Un solo usuario a la vez</li>
            <li><strong>Multiusuario:</strong> Múltiples usuarios simultáneos</li>
        </ul>
        
        <h3>Por número de tareas</h3>
        <ul>
            <li><strong>Monotarea:</strong> Una tarea a la vez</li>
            <li><strong>Multitarea:</strong> Múltiples tareas simultáneas</li>
        </ul>
        
        <h2 id="ejemplos">Ejemplos</h2>
        <ul>
            <li><strong>Windows:</strong> Microsoft Windows</li>
            <li><strong>macOS:</strong> Apple macOS</li>
            <li><strong>Linux:</strong> GNU/Linux</li>
            <li><strong>Unix:</strong> Varios sistemas Unix</li>
            <li><strong>Eclipse OS:</strong> Sistema operativo moderno en Rust</li>
        </ul>
    </div>
</body>
</html>
        "#.to_string()
    }

    /// Renderizar página genérica
    fn render_generic_page(&self, url: &str) -> String {
        format!(r#"
<!DOCTYPE html>
<html>
<head>
    <title>Página Web - {}</title>
    <style>
        body {{ font-family: Arial, sans-serif; margin: 50px; }}
        .header {{ text-align: center; margin-bottom: 30px; }}
        .content {{ max-width: 800px; margin: 0 auto; }}
    </style>
</head>
<body>
    <div class="header">
        <h1>🌐 Página Web</h1>
        <p>URL: {}</p>
    </div>
    
    <div class="content">
        <h2>Contenido de la Página</h2>
        <p>Esta es una página web simulada para demostrar las capacidades del navegador Eclipse OS.</p>
        
        <h3>Características del Navegador</h3>
        <ul>
            <li>Renderizado de HTML básico</li>
            <li>Navegación por pestañas</li>
            <li>Historial de navegación</li>
            <li>Sistema de marcadores</li>
            <li>Gestor de descargas</li>
        </ul>
        
        <h3>Enlaces de Prueba</h3>
        <p><a href="about:home">Página de inicio</a></p>
        <p><a href="about:history">Historial</a></p>
        <p><a href="about:bookmarks">Marcadores</a></p>
        <p><a href="about:downloads">Descargas</a></p>
    </div>
</body>
</html>
        "#, url, url)
    }

    /// Extraer título de la página
    fn extract_title(&self, content: &str) -> String {
        if let Some(start) = content.find("<title>") {
            if let Some(end) = content[start + 7..].find("</title>") {
                return content[start + 7..start + 7 + end].to_string();
            }
        }
        "Sin título".to_string()
    }

    /// Agregar al historial
    fn add_to_history(&mut self, title: &str, url: &str) {
        // Buscar si ya existe en el historial
        if let Some(entry) = self.history.iter_mut().find(|e| e.url == url) {
            entry.visit_count += 1;
            entry.visit_time = 0 // Simulado;
        } else {
            self.history.push_back(HistoryEntry {
                title: title.to_string(),
                url: url.to_string(),
                visit_time: 0 // Simulado,
                visit_count: 1,
            });
        }
        
        // Limitar tamaño del historial
        if self.history.len() > 1000 {
            self.history.pop_front();
        }
    }

    /// Agregar marcador
    pub fn add_bookmark(&mut self, title: &str, url: &str, folder: &str) {
        let bookmark = Bookmark {
            title: title.to_string(),
            url: url.to_string(),
            folder: folder.to_string(),
            created_at: 0 // Simulado,
            tags: Vec::new(),
        };
        
        self.bookmarks.push(bookmark);
    }

    /// Buscar en la web
    pub fn search(&mut self, query: &str) -> Result<(), &'static str> {
        let search_url = format!("https://www.google.com/search?q={}", query);
        self.navigate_to(&search_url)?;
        Ok(())
    }

    /// Obtener pestaña actual
    pub fn get_current_tab(&self) -> Option<&BrowserTab> {
        self.tabs.get(self.current_tab)
    }

    /// Obtener todas las pestañas
    pub fn get_tabs(&self) -> &[BrowserTab] {
        &self.tabs
    }

    /// Obtener historial
    pub fn get_history(&self) -> &VecDeque<HistoryEntry> {
        &self.history
    }

    /// Obtener marcadores
    pub fn get_bookmarks(&self) -> &[Bookmark] {
        &self.bookmarks
    }
}

impl HtmlRenderer {
    fn new() -> Self {
        Self {
            parser: HtmlParser {
                document: HtmlDocument {
                    title: String::new(),
                    body: String::new(),
                    links: Vec::new(),
                    images: Vec::new(),
                    scripts: Vec::new(),
                    styles: Vec::new(),
                },
            },
            css_engine: CssEngine {
                styles: BTreeMap::new(),
            },
            js_engine: JsEngine {
                context: JsContext {
                    variables: BTreeMap::new(),
                    functions: BTreeMap::new(),
                },
            },
        }
    }
}

impl DownloadManager {
    fn new() -> Self {
        Self {
            downloads: BTreeMap::new(),
            next_download_id: 1,
        }
    }

    /// Iniciar descarga
    pub fn start_download(&mut self, url: &str, filename: &str) -> u32 {
        let download_id = self.next_download_id;
        self.next_download_id += 1;

        let download = Download {
            id: download_id,
            url: url.to_string(),
            filename: filename.to_string(),
            size: 0, // Se determinará durante la descarga
            downloaded: 0,
            status: DownloadStatus::Queued,
            start_time: 0 // Simulado,
        };

        self.downloads.insert(download_id, download);
        download_id
    }

    /// Obtener descargas
    pub fn get_downloads(&self) -> &BTreeMap<u32, Download> {
        &self.downloads
    }
}
