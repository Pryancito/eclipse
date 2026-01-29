//! Security Module
//! Gestión de seguridad y permisos

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Niveles de acceso
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AccessLevel {
    Read = 1,
    Write = 2,
    Execute = 4,
    Full = 7,
}

/// Usuario del sistema
#[derive(Debug, Clone)]
struct User {
    username: String,
    password_hash: String,
    permissions: HashMap<String, AccessLevel>,
}

/// Token de seguridad
#[derive(Debug, Clone)]
pub struct SecurityToken {
    pub user: String,
    pub timestamp: u64,
    pub session_id: String,
}

/// Gestor de seguridad
struct SecurityManager {
    users: HashMap<String, User>,
    active_tokens: HashMap<String, SecurityToken>,
    resource_permissions: HashMap<String, HashMap<String, AccessLevel>>,
}

static SECURITY_MANAGER: OnceLock<Arc<Mutex<SecurityManager>>> = OnceLock::new();

fn get_security_manager() -> Arc<Mutex<SecurityManager>> {
    SECURITY_MANAGER.get_or_init(|| {
        let mut users = HashMap::new();
        // Usuario admin por defecto
        users.insert("admin".to_string(), User {
            username: "admin".to_string(),
            password_hash: simple_hash("admin123"),
            permissions: HashMap::new(),
        });
        
        Arc::new(Mutex::new(SecurityManager {
            users,
            active_tokens: HashMap::new(),
            resource_permissions: HashMap::new(),
        }))
    }).clone()
}

/// Hash simple (en producción usar bcrypt o argon2)
fn simple_hash(password: &str) -> String {
    let mut hash = 0u64;
    for byte in password.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
    }
    format!("{:x}", hash)
}

/// Inicializar sistema de seguridad
pub fn security_init() {
    println!("🔒 Sistema de seguridad inicializado");
    let _ = get_security_manager(); // Inicializar
}

/// Crear token de seguridad
pub fn create_security_token(user: &str, password: &str) -> Option<SecurityToken> {
    let manager = get_security_manager();
    if let Ok(mut mgr) = manager.lock() {
        if let Some(stored_user) = mgr.users.get(user) {
            let password_hash = simple_hash(password);
            
            if stored_user.password_hash == password_hash {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                
                let session_id = format!("{}_{}", user, timestamp);
                
                let token = SecurityToken {
                    user: user.to_string(),
                    timestamp,
                    session_id: session_id.clone(),
                };
                
                mgr.active_tokens.insert(session_id, token.clone());
                println!("🔐 Token de seguridad creado para: {}", user);
                return Some(token);
            }
        }
    }
    
    eprintln!("❌ Autenticación fallida");
    None
}

/// Verificar permisos
pub fn check_permissions(token: &SecurityToken, resource: &str, access: AccessLevel) -> bool {
    let manager = get_security_manager();
    if let Ok(mgr) = manager.lock() {
        // Verificar que el token está activo
        if !mgr.active_tokens.contains_key(&token.session_id) {
            return false;
        }
        
        // Admin tiene acceso completo
        if token.user == "admin" {
            return true;
        }
        
        // Verificar permisos del recurso
        if let Some(resource_perms) = mgr.resource_permissions.get(resource) {
            if let Some(&user_access) = resource_perms.get(&token.user) {
                return (user_access as u32 & access as u32) == access as u32;
            }
        }
        
        // Verificar permisos del usuario
        if let Some(user) = mgr.users.get(&token.user) {
            if let Some(&user_access) = user.permissions.get(resource) {
                return (user_access as u32 & access as u32) == access as u32;
            }
        }
    }
    
    false
}

/// Establecer permisos
pub fn set_permissions(resource: &str, user: &str, access: AccessLevel) -> bool {
    let manager = get_security_manager();
    let result = if let Ok(mut mgr) = manager.lock() {
        mgr.resource_permissions
            .entry(resource.to_string())
            .or_insert_with(HashMap::new)
            .insert(user.to_string(), access);
        
        println!("🔒 Permisos establecidos para {}: {:?} en {}", user, access, resource);
        true
    } else {
        false
    };
    
    result
}

/// Autenticar usuario
pub fn authenticate_user(username: &str, password: &str) -> bool {
    let manager = get_security_manager();
    if let Ok(mgr) = manager.lock() {
        if let Some(user) = mgr.users.get(username) {
            let password_hash = simple_hash(password);
            return user.password_hash == password_hash;
        }
    }
    false
}

/// Crear nuevo usuario
pub fn create_user(username: &str, password: &str) -> bool {
    let manager = get_security_manager();
    let result = if let Ok(mut mgr) = manager.lock() {
        if mgr.users.contains_key(username) {
            eprintln!("❌ Usuario ya existe: {}", username);
            false
        } else {
            let user = User {
                username: username.to_string(),
                password_hash: simple_hash(password),
                permissions: HashMap::new(),
            };
            
            mgr.users.insert(username.to_string(), user);
            println!("✅ Usuario creado: {}", username);
            true
        }
    } else {
        false
    };
    
    result
}

/// Cerrar sesión
pub fn logout_user(token: &SecurityToken) -> bool {
    let manager = get_security_manager();
    let result = if let Ok(mut mgr) = manager.lock() {
        mgr.active_tokens.remove(&token.session_id);
        println!("🔓 Sesión cerrada para: {}", token.user);
        true
    } else {
        false
    };
    
    result
}

/// Encriptar datos (XOR simple - en producción usar AES)
pub fn encrypt_data(data: &[u8], key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }
    
    data.iter()
        .enumerate()
        .map(|(i, &byte)| byte ^ key[i % key.len()])
        .collect()
}

/// Desencriptar datos (XOR es simétrico)
pub fn decrypt_data(data: &[u8], key: &[u8]) -> Vec<u8> {
    encrypt_data(data, key)
}

/// Generar clave de encriptación simple
pub fn generate_key(size: usize) -> Vec<u8> {
    (0..size).map(|i| ((i * 7 + 13) % 256) as u8).collect()
}

/// Listar usuarios activos
pub fn list_active_users() -> Vec<String> {
    let manager = get_security_manager();
    let result = if let Ok(mgr) = manager.lock() {
        mgr.active_tokens.values()
            .map(|token| token.user.clone())
            .collect()
    } else {
        vec![]
    };
    
    result
}

/// Inicializar sistema de seguridad
pub fn init() {
    security_init();
}
