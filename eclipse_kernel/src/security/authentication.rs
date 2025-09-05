//! Sistema de autenticación y autorización
//! 
//! Este módulo implementa autenticación de usuarios, gestión de sesiones
//! y control de acceso basado en roles.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use super::{SecurityError, SecurityResult, UserSession, AuthState, SecurityLevel, Capability};

/// Manager de autenticación
pub struct AuthenticationManager {
    /// Sesiones activas
    active_sessions: BTreeMap<u64, UserSession>,
    /// Usuarios del sistema
    users: BTreeMap<u32, User>,
    /// Grupos del sistema
    groups: BTreeMap<u32, Group>,
    /// Intentos de login fallidos por IP
    failed_attempts: BTreeMap<String, FailedAttemptInfo>,
    /// Configuración de autenticación
    config: AuthConfig,
    /// Contador de sesiones
    session_counter: u64,
}

/// Usuario del sistema
#[derive(Debug, Clone)]
pub struct User {
    pub id: u32,
    pub username: String,
    pub password_hash: String,
    pub salt: String,
    pub security_level: SecurityLevel,
    pub capabilities: Vec<Capability>,
    pub groups: Vec<u32>,
    pub is_active: bool,
    pub created_at: u64,
    pub last_login: Option<u64>,
    pub password_changed: u64,
    pub failed_attempts: u32,
    pub locked_until: Option<u64>,
}

/// Grupo del sistema
#[derive(Debug, Clone)]
pub struct Group {
    pub id: u32,
    pub name: String,
    pub capabilities: Vec<Capability>,
    pub members: Vec<u32>,
    pub is_active: bool,
}

/// Información de intentos fallidos
#[derive(Debug, Clone)]
pub struct FailedAttemptInfo {
    pub count: u32,
    pub last_attempt: u64,
    pub locked_until: Option<u64>,
}

/// Configuración de autenticación
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub max_session_duration: u64,
    pub max_failed_attempts: u32,
    pub lockout_duration: u64,
    pub password_min_length: usize,
    pub require_strong_passwords: bool,
    pub session_timeout: u64,
}

/// Estadísticas de autenticación
#[derive(Debug, Clone)]
pub struct AuthStats {
    pub active_sessions: usize,
    pub total_users: usize,
    pub total_groups: usize,
    pub failed_logins: usize,
    pub locked_accounts: usize,
    pub last_login_time: Option<u64>,
}

static mut AUTH_MANAGER: Option<AuthenticationManager> = None;

impl AuthenticationManager {
    /// Crear un nuevo manager de autenticación
    pub fn new() -> Self {
        Self {
            active_sessions: BTreeMap::new(),
            users: BTreeMap::new(),
            groups: BTreeMap::new(),
            failed_attempts: BTreeMap::new(),
            config: AuthConfig::default(),
            session_counter: 0,
        }
    }

    /// Autenticar un usuario
    pub fn authenticate_user(
        &mut self,
        username: &str,
        password: &str,
        source_ip: Option<String>,
    ) -> SecurityResult<UserSession> {
        // Verificar si la IP está bloqueada
        if let Some(ip) = &source_ip {
            if self.is_ip_locked(ip) {
                return Err(SecurityError::AccessDenied);
            }
        }

        // Buscar usuario
        let user_id = if let Some(user) = self.find_user_by_username(username) {
            user.id
        } else {
            return Err(SecurityError::AuthenticationFailed);
        };

        // Verificar si la cuenta está bloqueada
        if let Some(user) = self.users.get(&user_id) {
            if self.is_account_locked(user) {
                return Err(SecurityError::AccessDenied);
            }
        }

        // Verificar contraseña
        let password_valid = if let Some(user) = self.users.get(&user_id) {
            self.verify_password(password, &user.password_hash, &user.salt)
        } else {
            false
        };

        if !password_valid {
            self.record_failed_attempt(username, source_ip.clone());
            return Err(SecurityError::InvalidCredentials);
        }

        // Crear sesión - necesitamos clonar los datos del usuario
        let user_data = if let Some(user) = self.users.get(&user_id) {
            (user.id, user.username.clone(), user.security_level, user.capabilities.clone())
        } else {
            return Err(SecurityError::AuthenticationFailed);
        };
        
        let session = self.create_session_from_data(user_data, source_ip.clone())?;
        
        // Limpiar intentos fallidos
        if let Some(ip) = source_ip {
            self.failed_attempts.remove(&ip);
        }

        Ok(session)
    }

    /// Crear una nueva sesión
    fn create_session(&mut self, user: &User, source_ip: Option<String>) -> SecurityResult<UserSession> {
        self.session_counter += 1;
        let current_time = self.get_current_time();

        let session = UserSession {
            user_id: user.id,
            username: user.username.clone(),
            session_id: self.session_counter,
            auth_state: AuthState::Authenticated,
            security_level: user.security_level,
            capabilities: user.capabilities.clone(),
            login_time: current_time,
            last_activity: current_time,
            source_ip,
        };

        self.active_sessions.insert(session.session_id, session.clone());
        Ok(session)
    }

    /// Crear sesión desde datos clonados
    fn create_session_from_data(
        &mut self, 
        user_data: (u32, String, SecurityLevel, Vec<Capability>), 
        source_ip: Option<String>
    ) -> SecurityResult<UserSession> {
        self.session_counter += 1;
        let current_time = self.get_current_time();
        let (user_id, username, security_level, capabilities) = user_data;

        let session = UserSession {
            user_id,
            username,
            session_id: self.session_counter,
            auth_state: AuthState::Authenticated,
            security_level,
            capabilities,
            login_time: current_time,
            last_activity: current_time,
            source_ip,
        };

        self.active_sessions.insert(session.session_id, session.clone());
        Ok(session)
    }

    /// Verificar contraseña
    fn verify_password(&self, password: &str, hash: &str, salt: &str) -> bool {
        // En un sistema real, aquí se usaría una función de hash segura como bcrypt
        // Por simplicidad, usamos un hash básico
        let combined = format!("{}{}", password, salt);
        let computed_hash = self.simple_hash(&combined);
        computed_hash == *hash
    }

    /// Hash simple (en producción usar bcrypt o similar)
    fn simple_hash(&self, input: &str) -> String {
        let mut hash = 0u64;
        for byte in input.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
        }
        format!("{:x}", hash)
    }

    /// Generar salt aleatorio
    fn generate_salt(&self) -> String {
        // En un sistema real, usar un generador criptográficamente seguro
        format!("salt_{}", self.get_current_time())
    }

    /// Buscar usuario por nombre
    fn find_user_by_username(&self, username: &str) -> Option<&User> {
        self.users.values().find(|user| user.username == username)
    }

    /// Verificar si una IP está bloqueada
    fn is_ip_locked(&self, ip: &str) -> bool {
        if let Some(attempt_info) = self.failed_attempts.get(ip) {
            if let Some(locked_until) = attempt_info.locked_until {
                self.get_current_time() < locked_until
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Verificar si una cuenta está bloqueada
    fn is_account_locked(&self, user: &User) -> bool {
        if let Some(locked_until) = user.locked_until {
            self.get_current_time() < locked_until
        } else {
            false
        }
    }

    /// Registrar intento fallido
    fn record_failed_attempt(&mut self, username: &str, source_ip: Option<String>) {
        let current_time = self.get_current_time();
        let lockout_duration = self.config.lockout_duration;
        let max_attempts = self.config.max_failed_attempts;
        
        if let Some(ip) = source_ip {
            let attempt_info = self.failed_attempts.entry(ip).or_insert_with(|| FailedAttemptInfo {
                count: 0,
                last_attempt: 0,
                locked_until: None,
            });

            attempt_info.count += 1;
            attempt_info.last_attempt = current_time;

            if attempt_info.count >= max_attempts {
                attempt_info.locked_until = Some(current_time + lockout_duration);
            }
        }

        // También bloquear la cuenta del usuario si es necesario
        if let Some(user) = self.users.values_mut().find(|u| u.username == username) {
            user.failed_attempts += 1;
            if user.failed_attempts >= max_attempts {
                user.locked_until = Some(current_time + lockout_duration);
            }
        }
    }

    /// Cerrar sesión
    pub fn logout(&mut self, session_id: u64) -> SecurityResult<()> {
        if self.active_sessions.remove(&session_id).is_some() {
            Ok(())
        } else {
            Err(SecurityError::ResourceNotFound)
        }
    }

    /// Verificar si una sesión es válida
    pub fn is_session_valid(&self, session_id: u64) -> bool {
        if let Some(session) = self.active_sessions.get(&session_id) {
            let current_time = self.get_current_time();
            session.auth_state == AuthState::Authenticated &&
            (current_time - session.last_activity) < self.config.session_timeout
        } else {
            false
        }
    }

    /// Actualizar actividad de sesión
    pub fn update_session_activity(&mut self, session_id: u64) -> SecurityResult<()> {
        let current_time = self.get_current_time();
        if let Some(session) = self.active_sessions.get_mut(&session_id) {
            session.last_activity = current_time;
            Ok(())
        } else {
            Err(SecurityError::ResourceNotFound)
        }
    }

    /// Obtener sesión por ID
    pub fn get_session(&self, session_id: u64) -> Option<&UserSession> {
        self.active_sessions.get(&session_id)
    }

    /// Crear un nuevo usuario
    pub fn create_user(
        &mut self,
        username: String,
        password: String,
        security_level: SecurityLevel,
        capabilities: Vec<Capability>,
    ) -> SecurityResult<u32> {
        // Verificar que el usuario no existe
        if self.find_user_by_username(&username).is_some() {
            return Err(SecurityError::InvalidOperation);
        }

        let user_id = self.get_next_user_id();
        let salt = self.generate_salt();
        let password_hash = self.simple_hash(&format!("{}{}", password, salt));

        let user = User {
            id: user_id,
            username: username.clone(),
            password_hash,
            salt,
            security_level,
            capabilities,
            groups: Vec::new(),
            is_active: true,
            created_at: self.get_current_time(),
            last_login: None,
            password_changed: self.get_current_time(),
            failed_attempts: 0,
            locked_until: None,
        };

        self.users.insert(user_id, user);
        Ok(user_id)
    }

    /// Obtener siguiente ID de usuario
    fn get_next_user_id(&self) -> u32 {
        self.users.keys().max().map(|id| *id + 1).unwrap_or(1)
    }

    /// Obtener tiempo actual (simulado)
    fn get_current_time(&self) -> u64 {
        // En un sistema real, usar un reloj del sistema
        1234567890 // Timestamp simulado
    }

    /// Obtener estadísticas de autenticación
    pub fn get_stats(&self) -> AuthStats {
        AuthStats {
            active_sessions: self.active_sessions.len(),
            total_users: self.users.len(),
            total_groups: self.groups.len(),
            failed_logins: self.failed_attempts.values()
                .map(|info| info.count as usize)
                .sum(),
            locked_accounts: self.users.values()
                .filter(|user| user.locked_until.is_some())
                .count(),
            last_login_time: self.users.values()
                .filter_map(|user| user.last_login)
                .max(),
        }
    }

    /// Limpiar sesiones expiradas
    pub fn cleanup_expired_sessions(&mut self) {
        let current_time = self.get_current_time();
        self.active_sessions.retain(|_, session| {
            (current_time - session.last_activity) < self.config.session_timeout
        });
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            max_session_duration: 3600, // 1 hora
            max_failed_attempts: 3,
            lockout_duration: 900, // 15 minutos
            password_min_length: 8,
            require_strong_passwords: true,
            session_timeout: 1800, // 30 minutos
        }
    }
}

/// Inicializar el sistema de autenticación
pub fn init_auth_system() -> SecurityResult<()> {
    unsafe {
        AUTH_MANAGER = Some(AuthenticationManager::new());
    }
    Ok(())
}

/// Obtener el manager de autenticación
pub fn get_auth_manager() -> Option<&'static mut AuthenticationManager> {
    unsafe { AUTH_MANAGER.as_mut() }
}

/// Autenticar usuario
pub fn authenticate_user(
    username: &str,
    password: &str,
    source_ip: Option<String>,
) -> SecurityResult<UserSession> {
    if let Some(manager) = get_auth_manager() {
        manager.authenticate_user(username, password, source_ip)
    } else {
        Err(SecurityError::Unknown)
    }
}

/// Cerrar sesión
pub fn logout(session_id: u64) -> SecurityResult<()> {
    if let Some(manager) = get_auth_manager() {
        manager.logout(session_id)
    } else {
        Err(SecurityError::Unknown)
    }
}

/// Verificar sesión válida
pub fn is_session_valid(session_id: u64) -> bool {
    if let Some(manager) = get_auth_manager() {
        manager.is_session_valid(session_id)
    } else {
        false
    }
}

/// Obtener sesión
pub fn get_session(session_id: u64) -> Option<&'static UserSession> {
    if let Some(manager) = get_auth_manager() {
        manager.get_session(session_id)
    } else {
        None
    }
}

/// Obtener número de sesiones activas
pub fn get_active_session_count() -> usize {
    if let Some(manager) = get_auth_manager() {
        manager.active_sessions.len()
    } else {
        0
    }
}

/// Obtener número de logins fallidos
pub fn get_failed_login_count() -> usize {
    if let Some(manager) = get_auth_manager() {
        manager.failed_attempts.values()
            .map(|info| info.count as usize)
            .sum()
    } else {
        0
    }
}

/// Obtener estadísticas de autenticación
pub fn get_auth_stats() -> Option<AuthStats> {
    get_auth_manager().map(|manager| manager.get_stats())
}
