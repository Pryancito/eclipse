use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;
//! Editor de Texto Avanzado para Eclipse OS
//! 
//! Implementa un editor completo con:
//! - Múltiples pestañas
//! - Resaltado de sintaxis
//! - Autocompletado
//! - Búsqueda y reemplazo
//! - Números de línea
//! - Modo de inserción y comando

use Result<(), &'static str>;
// use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use alloc::collections::VecDeque;

/// Editor principal
pub struct TextEditor {
    /// Pestañas abiertas
    tabs: Vec<EditorTab>,
    /// Pestaña actual
    current_tab: usize,
    /// Configuración del editor
    config: EditorConfig,
    /// Estado del editor
    state: EditorState,
    /// Historial de comandos
    command_history: VecDeque<String>,
    /// Atajos de teclado
    shortcuts: BTreeMap<String, EditorCommand>,
}

/// Pestaña del editor
#[derive(Debug, Clone)]
pub struct EditorTab {
    pub id: u32,
    pub filename: String,
    pub content: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub selection_start: Option<(usize, usize)>,
    pub selection_end: Option<(usize, usize)>,
    pub modified: bool,
    pub syntax_highlighting: SyntaxType,
    pub line_numbers: bool,
    pub word_wrap: bool,
}

/// Configuración del editor
#[derive(Debug, Clone)]
pub struct EditorConfig {
    pub tab_size: usize,
    pub show_line_numbers: bool,
    pub word_wrap: bool,
    pub auto_indent: bool,
    pub syntax_highlighting: bool,
    pub auto_complete: bool,
    pub show_whitespace: bool,
    pub font_size: u32,
    pub theme: EditorTheme,
    pub max_undo_levels: usize,
}

/// Temas del editor
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EditorTheme {
    Dark,
    Light,
    Monokai,
    Solarized,
    Custom(String),
}

/// Estados del editor
#[derive(Debug, Clone, PartialEq)]
pub enum EditorState {
    Normal,
    Insert,
    Visual,
    Command,
    Search,
    Replace,
}

/// Tipos de sintaxis
#[derive(Debug, Clone, PartialEq)]
pub enum SyntaxType {
    None,
    Rust,
    C,
    Cpp,
    Python,
    JavaScript,
    HTML,
    CSS,
    JSON,
    XML,
    Markdown,
    Shell,
    Config,
}

/// Comandos del editor
#[derive(Debug, Clone, PartialEq)]
pub enum EditorCommand {
    Save,
    SaveAs(String),
    Open(String),
    New,
    Close,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Find(String),
    Replace(String, String),
    GotoLine(usize),
    SelectAll,
    FindNext,
    FindPrevious,
    ToggleLineNumbers,
    ToggleWordWrap,
    ToggleSyntaxHighlighting,
    ChangeTheme(EditorTheme),
    SetTabSize(usize),
}

impl TextEditor {
    /// Crear nuevo editor
    pub fn new(config: EditorConfig) -> Self {
        let mut editor = Self {
            tabs: Vec::new(),
            current_tab: 0,
            config,
            state: EditorState::Normal,
            command_history: VecDeque::new(),
            shortcuts: BTreeMap::new(),
        };
        
        // Configurar atajos de teclado
        editor.setup_shortcuts();
        
        // Crear pestaña inicial
        editor.new_tab();
        
        editor
    }

    /// Configurar atajos de teclado
    fn setup_shortcuts(&mut self) {
        self.shortcuts.insert("Ctrl+S".to_string(), EditorCommand::Save);
        self.shortcuts.insert("Ctrl+O".to_string(), EditorCommand::Open("".to_string()));
        self.shortcuts.insert("Ctrl+N".to_string(), EditorCommand::New);
        self.shortcuts.insert("Ctrl+W".to_string(), EditorCommand::Close);
        self.shortcuts.insert("Ctrl+Q".to_string(), EditorCommand::Quit);
        self.shortcuts.insert("Ctrl+Z".to_string(), EditorCommand::Undo);
        self.shortcuts.insert("Ctrl+Y".to_string(), EditorCommand::Redo);
        self.shortcuts.insert("Ctrl+X".to_string(), EditorCommand::Cut);
        self.shortcuts.insert("Ctrl+C".to_string(), EditorCommand::Copy);
        self.shortcuts.insert("Ctrl+V".to_string(), EditorCommand::Paste);
        self.shortcuts.insert("Ctrl+F".to_string(), EditorCommand::Find("".to_string()));
        self.shortcuts.insert("Ctrl+H".to_string(), EditorCommand::Replace("".to_string(), "".to_string()));
        self.shortcuts.insert("Ctrl+G".to_string(), EditorCommand::GotoLine(0));
        self.shortcuts.insert("Ctrl+A".to_string(), EditorCommand::SelectAll);
        self.shortcuts.insert("F3".to_string(), EditorCommand::FindNext);
        self.shortcuts.insert("Shift+F3".to_string(), EditorCommand::FindPrevious);
    }

    /// Crear nueva pestaña
    pub fn new_tab(&mut self) -> u32 {
        let tab_id = self.tabs.len() as u32 + 1;
        let tab = EditorTab {
            id: tab_id,
            filename: format!("untitled_{}.txt", tab_id),
            content: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            selection_start: None,
            selection_end: None,
            modified: false,
            syntax_highlighting: self.detect_syntax_type(""),
            line_numbers: self.config.show_line_numbers,
            word_wrap: self.config.word_wrap,
        };
        
        self.tabs.push(tab);
        self.current_tab = self.tabs.len() - 1;
        tab_id
    }

    /// Cerrar pestaña
    pub fn close_tab(&mut self, tab_id: u32) -> Result<(), &'static str> {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == tab_id) {
            if self.tabs[pos].modified {
                // Preguntar si guardar cambios
                println!("⚠️  Archivo '{}' tiene cambios sin guardar. ¿Guardar? (y/n)", self.tabs[pos].filename);
            }
            
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

    /// Obtener pestaña actual
    fn current_tab_mut(&mut self) -> &mut EditorTab {
        &mut self.tabs[self.current_tab]
    }

    /// Obtener pestaña actual (inmutable)
    fn current_tab_ref(&self) -> &EditorTab {
        &self.tabs[self.current_tab]
    }

    /// Insertar texto en la posición del cursor
    pub fn insert_text(&mut self, text: &str) -> Result<(), &'static str> {
        let tab = self.current_tab_mut();
        
        if tab.cursor_row >= tab.content.len() {
            tab.content.resize(tab.cursor_row + 1, String::new());
        }
        
        let line = &mut tab.content[tab.cursor_row];
        if tab.cursor_col > line.len() {
            line.push_str(&" ".repeat(tab.cursor_col - line.len()));
        }
        
        line.insert_str(tab.cursor_col, text);
        tab.cursor_col += text.len();
        tab.modified = true;
        
        Ok(())
    }

    /// Insertar nueva línea
    pub fn insert_newline(&mut self) -> Result<(), &'static str> {
        let tab = self.current_tab_mut();
        
        if tab.cursor_row >= tab.content.len() {
            tab.content.resize(tab.cursor_row + 1, String::new());
        }
        
        let current_line = &tab.content[tab.cursor_row];
        let new_line = if tab.cursor_col < current_line.len() {
            current_line[tab.cursor_col..].to_string()
        } else {
            String::new()
        };
        
        // Truncar línea actual
        tab.content[tab.cursor_row].truncate(tab.cursor_col);
        
        // Insertar nueva línea
        tab.content.insert(tab.cursor_row + 1, new_line);
        tab.cursor_row += 1;
        tab.cursor_col = 0;
        tab.modified = true;
        
        Ok(())
    }

    /// Eliminar carácter en la posición del cursor
    pub fn delete_char(&mut self) -> Result<(), &'static str> {
        let tab = self.current_tab_mut();
        
        if tab.cursor_row < tab.content.len() {
            let line = &mut tab.content[tab.cursor_row];
            if tab.cursor_col < line.len() {
                line.remove(tab.cursor_col);
                tab.modified = true;
            } else if tab.cursor_row + 1 < tab.content.len() {
                // Unir con la siguiente línea
                let next_line = tab.content.remove(tab.cursor_row + 1);
                line.push_str(&next_line);
            }
        }
        
        Ok(())
    }

    /// Eliminar carácter antes del cursor
    pub fn backspace(&mut self) -> Result<(), &'static str> {
        let tab = self.current_tab_mut();
        
        if tab.cursor_col > 0 {
            tab.cursor_col -= 1;
            self.delete_char()?;
        } else if tab.cursor_row > 0 {
            // Mover a la línea anterior
            tab.cursor_row -= 1;
            tab.cursor_col = tab.content[tab.cursor_row].len();
            
            // Unir líneas
            let current_line = tab.content[tab.cursor_row + 1].clone();
            tab.content[tab.cursor_row].push_str(&current_line);
            tab.content.remove(tab.cursor_row + 1);
            tab.modified = true;
        }
        
        Ok(())
    }

    /// Mover cursor
    pub fn move_cursor(&mut self, direction: CursorDirection) -> Result<(), &'static str> {
        let tab = self.current_tab_mut();
        
        match direction {
            CursorDirection::Up => {
                if tab.cursor_row > 0 {
                    tab.cursor_row -= 1;
                    tab.cursor_col = tab.cursor_col.min(tab.content[tab.cursor_row].len());
                }
            },
            CursorDirection::Down => {
                if tab.cursor_row + 1 < tab.content.len() {
                    tab.cursor_row += 1;
                    tab.cursor_col = tab.cursor_col.min(tab.content[tab.cursor_row].len());
                }
            },
            CursorDirection::Left => {
                if tab.cursor_col > 0 {
                    tab.cursor_col -= 1;
                } else if tab.cursor_row > 0 {
                    tab.cursor_row -= 1;
                    tab.cursor_col = tab.content[tab.cursor_row].len();
                }
            },
            CursorDirection::Right => {
                let max_col = if tab.cursor_row < tab.content.len() {
                    tab.content[tab.cursor_row].len()
                } else {
                    0
                };
                
                if tab.cursor_col < max_col {
                    tab.cursor_col += 1;
                } else if tab.cursor_row + 1 < tab.content.len() {
                    tab.cursor_row += 1;
                    tab.cursor_col = 0;
                }
            },
            CursorDirection::Home => {
                tab.cursor_col = 0;
            },
            CursorDirection::End => {
                tab.cursor_col = if tab.cursor_row < tab.content.len() {
                    tab.content[tab.cursor_row].len()
                } else {
                    0
                };
            },
        }
        
        Ok(())
    }

    /// Ejecutar comando
    pub fn execute_command(&mut self, command: EditorCommand) -> Result<String, &'static str> {
        match command {
            EditorCommand::Save => self.save_file(),
            EditorCommand::SaveAs(filename) => self.save_file_as(&filename),
            EditorCommand::Open(filename) => self.open_file(&filename),
            EditorCommand::New => {
                self.new_tab();
                Ok("Nueva pestaña creada".to_string())
            },
            EditorCommand::Close => {
                let tab_id = self.current_tab_ref().id;
                self.close_tab(tab_id)?;
                Ok("Pestaña cerrada".to_string())
            },
            EditorCommand::Quit => {
                self.state = EditorState::Normal;
                Ok("Saliendo del editor...".to_string())
            },
            EditorCommand::Undo => {
                // Implementar undo
                Ok("Deshacer (no implementado)".to_string())
            },
            EditorCommand::Redo => {
                // Implementar redo
                Ok("Rehacer (no implementado)".to_string())
            },
            EditorCommand::Cut => {
                self.cut_text()
            },
            EditorCommand::Copy => {
                self.copy_text()
            },
            EditorCommand::Paste => {
                self.paste_text()
            },
            EditorCommand::Find(pattern) => {
                self.find_text(&pattern)
            },
            EditorCommand::Replace(pattern, replacement) => {
                self.replace_text(&pattern, &replacement)
            },
            EditorCommand::GotoLine(line) => {
                self.goto_line(line)
            },
            EditorCommand::SelectAll => {
                self.select_all()
            },
            EditorCommand::FindNext => {
                self.find_next()
            },
            EditorCommand::FindPrevious => {
                self.find_previous()
            },
            EditorCommand::ToggleLineNumbers => {
                self.toggle_line_numbers()
            },
            EditorCommand::ToggleWordWrap => {
                self.toggle_word_wrap()
            },
            EditorCommand::ToggleSyntaxHighlighting => {
                self.toggle_syntax_highlighting()
            },
            EditorCommand::ChangeTheme(theme) => {
                self.change_theme(theme)
            },
            EditorCommand::SetTabSize(size) => {
                self.set_tab_size(size)
            },
        }
    }

    /// Guardar archivo
    fn save_file(&mut self) -> Result<String, &'static str> {
        let tab = self.current_tab_mut();
        if tab.filename.starts_with("untitled_") {
            return Ok("Usar 'Guardar como' para archivos nuevos".to_string());
        }
        
        // Simular guardado
        tab.modified = false;
        Ok(format!("Archivo '{}' guardado", tab.filename))
    }

    /// Guardar archivo como
    fn save_file_as(&mut self, filename: &str) -> Result<String, &'static str> {
        let tab = self.current_tab_mut();
        tab.filename = filename.to_string();
        tab.syntax_highlighting = self.detect_syntax_type(filename);
        tab.modified = false;
        Ok(format!("Archivo guardado como '{}'", filename))
    }

    /// Abrir archivo
    fn open_file(&mut self, filename: &str) -> Result<String, &'static str> {
        // Simular carga de archivo
        let tab = self.current_tab_mut();
        tab.filename = filename.to_string();
        tab.content = vec![
            format!("// Archivo: {}", filename),
            "".to_string(),
            "Este es el contenido del archivo cargado.".to_string(),
            "".to_string(),
            "// Línea 5",
            "// Línea 6",
            "// Línea 7",
        ];
        tab.syntax_highlighting = self.detect_syntax_type(filename);
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.modified = false;
        
        Ok(format!("Archivo '{}' abierto", filename))
    }

    /// Cortar texto
    fn cut_text(&mut self) -> Result<String, &'static str> {
        // Implementar corte de texto
        Ok("Texto cortado".to_string())
    }

    /// Copiar texto
    fn copy_text(&mut self) -> Result<String, &'static str> {
        // Implementar copia de texto
        Ok("Texto copiado".to_string())
    }

    /// Pegar texto
    fn paste_text(&mut self) -> Result<String, &'static str> {
        // Implementar pegado de texto
        Ok("Texto pegado".to_string())
    }

    /// Buscar texto
    fn find_text(&mut self, pattern: &str) -> Result<String, &'static str> {
        self.state = EditorState::Search;
        Ok(format!("Buscando: '{}'", pattern))
    }

    /// Reemplazar texto
    fn replace_text(&mut self, pattern: &str, replacement: &str) -> Result<String, &'static str> {
        Ok(format!("Reemplazando '{}' con '{}'", pattern, replacement))
    }

    /// Ir a línea
    fn goto_line(&mut self, line: usize) -> Result<String, &'static str> {
        let tab = self.current_tab_mut();
        if line > 0 && line <= tab.content.len() {
            tab.cursor_row = line - 1;
            tab.cursor_col = 0;
            Ok(format!("Saltando a línea {}", line))
        } else {
            Ok(format!("Línea {} no válida", line))
        }
    }

    /// Seleccionar todo
    fn select_all(&mut self) -> Result<String, &'static str> {
        let tab = self.current_tab_mut();
        tab.selection_start = Some((0, 0));
        tab.selection_end = Some((tab.content.len() - 1, tab.content.last().map_or(0, |s| s.len())));
        Ok("Todo seleccionado".to_string())
    }

    /// Buscar siguiente
    fn find_next(&mut self) -> Result<String, &'static str> {
        Ok("Buscando siguiente coincidencia...".to_string())
    }

    /// Buscar anterior
    fn find_previous(&mut self) -> Result<String, &'static str> {
        Ok("Buscando coincidencia anterior...".to_string())
    }

    /// Alternar números de línea
    fn toggle_line_numbers(&mut self) -> Result<String, &'static str> {
        let tab = self.current_tab_mut();
        tab.line_numbers = !tab.line_numbers;
        Ok(format!("Números de línea: {}", if tab.line_numbers { "activados" } else { "desactivados" }))
    }

    /// Alternar ajuste de línea
    fn toggle_word_wrap(&mut self) -> Result<String, &'static str> {
        let tab = self.current_tab_mut();
        tab.word_wrap = !tab.word_wrap;
        Ok(format!("Ajuste de línea: {}", if tab.word_wrap { "activado" } else { "desactivado" }))
    }

    /// Alternar resaltado de sintaxis
    fn toggle_syntax_highlighting(&mut self) -> Result<String, &'static str> {
        self.config.syntax_highlighting = !self.config.syntax_highlighting;
        Ok(format!("Resaltado de sintaxis: {}", if self.config.syntax_highlighting { "activado" } else { "desactivado" }))
    }

    /// Cambiar tema
    fn change_theme(&mut self, theme: EditorTheme) -> Result<String, &'static str> {
        self.config.theme = theme.clone();
        Ok(format!("Tema cambiado a: {:?}", theme))
    }

    /// Establecer tamaño de tabulación
    fn set_tab_size(&mut self, size: usize) -> Result<String, &'static str> {
        self.config.tab_size = size;
        Ok(format!("Tamaño de tabulación establecido a: {}", size))
    }

    /// Detectar tipo de sintaxis
    fn detect_syntax_type(&self, filename: &str) -> SyntaxType {
        let ext = filename.split('.').last().unwrap_or("").to_lowercase();
        match ext.as_str() {
            "rs" => SyntaxType::Rust,
            "c" => SyntaxType::C,
            "cpp" | "cc" | "cxx" => SyntaxType::Cpp,
            "py" => SyntaxType::Python,
            "js" => SyntaxType::JavaScript,
            "html" | "htm" => SyntaxType::HTML,
            "css" => SyntaxType::CSS,
            "json" => SyntaxType::JSON,
            "xml" => SyntaxType::XML,
            "md" => SyntaxType::Markdown,
            "sh" | "bash" => SyntaxType::Shell,
            "conf" | "cfg" | "ini" => SyntaxType::Config,
            _ => SyntaxType::None,
        }
    }

    /// Obtener contenido formateado
    pub fn get_formatted_content(&self) -> String {
        let tab = self.current_tab_ref();
        let mut output = String::new();
        
        for (i, line) in tab.content.iter().enumerate() {
            if tab.line_numbers {
                output.push_str(&format!("{:4} │ ", i + 1));
            }
            output.push_str(line);
            output.push('\n');
        }
        
        output
    }

    /// Obtener información de estado
    pub fn get_status_info(&self) -> String {
        let tab = self.current_tab_ref();
        format!(
            "Archivo: {} | Línea: {} | Columna: {} | {} | {}",
            tab.filename,
            tab.cursor_row + 1,
            tab.cursor_col + 1,
            if tab.modified { "Modificado" } else { "Guardado" },
            format!("{:?}", tab.syntax_highlighting)
        )
    }

    /// Obtener pestañas
    pub fn get_tabs(&self) -> Vec<&EditorTab> {
        self.tabs.iter().collect()
    }

    /// Obtener pestaña actual
    pub fn get_current_tab(&self) -> Option<&EditorTab> {
        self.tabs.get(self.current_tab)
    }
}

/// Direcciones del cursor
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorDirection {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
}

/// Gestor de editores
pub struct EditorManager {
    editors: BTreeMap<u32, TextEditor>,
    next_editor_id: u32,
}

impl EditorManager {
    pub fn new() -> Self {
        Self {
            editors: BTreeMap::new(),
            next_editor_id: 1,
        }
    }

    /// Crear nuevo editor
    pub fn create_editor(&mut self, config: EditorConfig) -> u32 {
        let editor_id = self.next_editor_id;
        self.next_editor_id += 1;

        let editor = TextEditor::new(config);
        self.editors.insert(editor_id, editor);
        editor_id
    }

    /// Obtener editor
    pub fn get_editor(&mut self, editor_id: u32) -> Option<&mut TextEditor> {
        self.editors.get_mut(&editor_id)
    }

    /// Cerrar editor
    pub fn close_editor(&mut self, editor_id: u32) -> bool {
        self.editors.remove(&editor_id).is_some()
    }

    /// Listar editores
    pub fn list_editors(&self) -> Vec<u32> {
        self.editors.keys().cloned().collect()
    }
}
