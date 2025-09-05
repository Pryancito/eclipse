//! Sistema de permisos y capabilities
//! 
//! Este módulo implementa el control de acceso basado en capabilities
//! y permisos granulares para recursos del sistema.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::vec;
use alloc::string::String;
use super::{SecurityError, SecurityResult, Capability, SecurityLevel};

/// Manager de permisos del sistema
pub struct PermissionManager {
    /// Mapa de capabilities por usuario
    user_capabilities: BTreeMap<u32, Vec<Capability>>,
    /// Mapa de capabilities por grupo
    group_capabilities: BTreeMap<u32, Vec<Capability>>,
    /// Mapa de capabilities por proceso
    process_capabilities: BTreeMap<u32, Vec<Capability>>,
    /// Políticas de acceso por recurso
    resource_policies: BTreeMap<String, ResourcePolicy>,
    /// Configuración de herencia de capabilities
    capability_inheritance: BTreeMap<Capability, Vec<Capability>>,
}

/// Política de acceso a un recurso
#[derive(Debug, Clone)]
pub struct ResourcePolicy {
    pub resource: String,
    pub required_capabilities: Vec<Capability>,
    pub required_security_level: SecurityLevel,
    pub allow_inheritance: bool,
    pub audit_required: bool,
}

/// Información de capability
#[derive(Debug, Clone)]
pub struct CapabilityInfo {
    pub capability: Capability,
    pub granted_by: CapabilitySource,
    pub granted_at: u64,
    pub expires_at: Option<u64>,
    pub is_active: bool,
}

/// Fuente de capability
#[derive(Debug, Clone, PartialEq)]
pub enum CapabilitySource {
    User,
    Group,
    Process,
    System,
    Inherited,
}

/// Estadísticas de permisos
#[derive(Debug, Clone)]
pub struct PermissionStats {
    pub total_users: usize,
    pub total_groups: usize,
    pub total_processes: usize,
    pub total_resources: usize,
    pub active_capabilities: usize,
    pub denied_requests: usize,
    pub granted_requests: usize,
}

static mut PERMISSION_MANAGER: Option<PermissionManager> = None;

impl PermissionManager {
    /// Crear un nuevo manager de permisos
    pub fn new() -> Self {
        Self {
            user_capabilities: BTreeMap::new(),
            group_capabilities: BTreeMap::new(),
            process_capabilities: BTreeMap::new(),
            resource_policies: BTreeMap::new(),
            capability_inheritance: Self::init_capability_inheritance(),
        }
    }

    /// Inicializar herencia de capabilities
    fn init_capability_inheritance() -> BTreeMap<Capability, Vec<Capability>> {
        let mut inheritance = BTreeMap::new();
        
        // SystemAdmin hereda todas las capabilities
        inheritance.insert(Capability::SystemAdmin, vec![
            Capability::SystemConfig,
            Capability::SystemShutdown,
            Capability::SystemReboot,
            Capability::MemoryAllocate,
            Capability::MemoryMap,
            Capability::MemoryUnmap,
            Capability::MemoryProtect,
            Capability::ProcessCreate,
            Capability::ProcessKill,
            Capability::ProcessSuspend,
            Capability::ProcessResume,
            Capability::ProcessDebug,
            Capability::FileRead,
            Capability::FileWrite,
            Capability::FileExecute,
            Capability::FileDelete,
            Capability::FileChmod,
            Capability::FileChown,
            Capability::NetworkListen,
            Capability::NetworkConnect,
            Capability::NetworkRaw,
            Capability::NetworkAdmin,
            Capability::DeviceRead,
            Capability::DeviceWrite,
            Capability::DeviceControl,
            Capability::DeviceAdmin,
            Capability::SecurityAudit,
            Capability::SecurityConfig,
            Capability::SecurityBypass,
            Capability::SecurityOverride,
        ]);

        // SecurityConfig hereda capabilities de auditoría
        inheritance.insert(Capability::SecurityConfig, vec![
            Capability::SecurityAudit,
        ]);

        // DeviceAdmin hereda capabilities de dispositivos
        inheritance.insert(Capability::DeviceAdmin, vec![
            Capability::DeviceRead,
            Capability::DeviceWrite,
            Capability::DeviceControl,
        ]);

        // NetworkAdmin hereda capabilities de red
        inheritance.insert(Capability::NetworkAdmin, vec![
            Capability::NetworkListen,
            Capability::NetworkConnect,
            Capability::NetworkRaw,
        ]);

        inheritance
    }

    /// Verificar si un usuario tiene una capability específica
    pub fn user_has_capability(&self, user_id: u32, capability: &Capability) -> bool {
        if let Some(capabilities) = self.user_capabilities.get(&user_id) {
            capabilities.contains(capability) || self.has_inherited_capability(capabilities, capability)
        } else {
            false
        }
    }

    /// Verificar si un proceso tiene una capability específica
    pub fn process_has_capability(&self, process_id: u32, capability: &Capability) -> bool {
        if let Some(capabilities) = self.process_capabilities.get(&process_id) {
            capabilities.contains(capability) || self.has_inherited_capability(capabilities, capability)
        } else {
            false
        }
    }

    /// Verificar herencia de capabilities
    fn has_inherited_capability(&self, capabilities: &[Capability], target: &Capability) -> bool {
        for capability in capabilities {
            if let Some(inherited) = self.capability_inheritance.get(capability) {
                if inherited.contains(target) {
                    return true;
                }
                // Verificar herencia recursiva
                if self.has_inherited_capability(inherited, target) {
                    return true;
                }
            }
        }
        false
    }

    /// Otorgar capability a un usuario
    pub fn grant_user_capability(&mut self, user_id: u32, capability: Capability) -> SecurityResult<()> {
        self.user_capabilities
            .entry(user_id)
            .or_insert_with(Vec::new)
            .push(capability);
        Ok(())
    }

    /// Revocar capability de un usuario
    pub fn revoke_user_capability(&mut self, user_id: u32, capability: &Capability) -> SecurityResult<()> {
        if let Some(capabilities) = self.user_capabilities.get_mut(&user_id) {
            capabilities.retain(|c| c != capability);
        }
        Ok(())
    }

    /// Otorgar capability a un proceso
    pub fn grant_process_capability(&mut self, process_id: u32, capability: Capability) -> SecurityResult<()> {
        self.process_capabilities
            .entry(process_id)
            .or_insert_with(Vec::new)
            .push(capability);
        Ok(())
    }

    /// Revocar capability de un proceso
    pub fn revoke_process_capability(&mut self, process_id: u32, capability: &Capability) -> SecurityResult<()> {
        if let Some(capabilities) = self.process_capabilities.get_mut(&process_id) {
            capabilities.retain(|c| c != capability);
        }
        Ok(())
    }

    /// Verificar acceso a un recurso
    pub fn check_resource_access(
        &self,
        user_id: u32,
        process_id: u32,
        resource: &str,
        required_capability: &Capability,
        security_level: SecurityLevel,
    ) -> SecurityResult<()> {
        // Verificar si existe política para el recurso
        if let Some(policy) = self.resource_policies.get(resource) {
            // Verificar capability requerida
            if !policy.required_capabilities.contains(required_capability) {
                return Err(SecurityError::InsufficientPermissions);
            }

            // Verificar nivel de seguridad
            if security_level < policy.required_security_level {
                return Err(SecurityError::InsufficientPermissions);
            }

            // Verificar capabilities del usuario o proceso
            if !self.user_has_capability(user_id, required_capability) &&
               !self.process_has_capability(process_id, required_capability) {
                return Err(SecurityError::AccessDenied);
            }
        }

        Ok(())
    }

    /// Definir política de acceso para un recurso
    pub fn set_resource_policy(&mut self, policy: ResourcePolicy) -> SecurityResult<()> {
        self.resource_policies.insert(policy.resource.clone(), policy);
        Ok(())
    }

    /// Obtener capabilities de un usuario
    pub fn get_user_capabilities(&self, user_id: u32) -> Option<&Vec<Capability>> {
        self.user_capabilities.get(&user_id)
    }

    /// Obtener capabilities de un proceso
    pub fn get_process_capabilities(&self, process_id: u32) -> Option<&Vec<Capability>> {
        self.process_capabilities.get(&process_id)
    }

    /// Obtener estadísticas de permisos
    pub fn get_stats(&self) -> PermissionStats {
        PermissionStats {
            total_users: self.user_capabilities.len(),
            total_groups: self.group_capabilities.len(),
            total_processes: self.process_capabilities.len(),
            total_resources: self.resource_policies.len(),
            active_capabilities: self.user_capabilities.values()
                .chain(self.process_capabilities.values())
                .map(|v| v.len())
                .sum(),
            denied_requests: 0, // TODO: Implementar contador
            granted_requests: 0, // TODO: Implementar contador
        }
    }

    /// Limpiar capabilities de un proceso (al terminar)
    pub fn cleanup_process(&mut self, process_id: u32) {
        self.process_capabilities.remove(&process_id);
    }

    /// Limpiar capabilities de un usuario (al cerrar sesión)
    pub fn cleanup_user(&mut self, user_id: u32) {
        self.user_capabilities.remove(&user_id);
    }
}

/// Inicializar el sistema de permisos
pub fn init_permission_system() -> SecurityResult<()> {
    unsafe {
        PERMISSION_MANAGER = Some(PermissionManager::new());
    }
    Ok(())
}

/// Obtener el manager de permisos
pub fn get_permission_manager() -> Option<&'static mut PermissionManager> {
    unsafe { PERMISSION_MANAGER.as_mut() }
}

/// Verificar si un usuario tiene una capability
pub fn user_has_capability(user_id: u32, capability: &Capability) -> bool {
    if let Some(manager) = get_permission_manager() {
        manager.user_has_capability(user_id, capability)
    } else {
        false
    }
}

/// Verificar si un proceso tiene una capability
pub fn process_has_capability(process_id: u32, capability: &Capability) -> bool {
    if let Some(manager) = get_permission_manager() {
        manager.process_has_capability(process_id, capability)
    } else {
        false
    }
}

/// Verificar acceso a un recurso
pub fn check_resource_access(
    user_id: u32,
    process_id: u32,
    resource: &str,
    required_capability: &Capability,
    security_level: SecurityLevel,
) -> SecurityResult<()> {
    if let Some(manager) = get_permission_manager() {
        manager.check_resource_access(user_id, process_id, resource, required_capability, security_level)
    } else {
        Err(SecurityError::Unknown)
    }
}

/// Obtener estadísticas de permisos
pub fn get_permission_stats() -> Option<PermissionStats> {
    get_permission_manager().map(|manager| manager.get_stats())
}
