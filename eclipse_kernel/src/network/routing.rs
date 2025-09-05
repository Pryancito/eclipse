//! Sistema de routing de red
//! 
//! Tabla de routing y algoritmos de enrutamiento

#![allow(dead_code)] // Permitir código no utilizado - API completa del kernel

use alloc::vec::Vec;
use core::cmp::Ordering;

use super::ip::IpAddress;
use super::NetworkError;

/// Tipos de ruta
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RouteType {
    Direct,     // Ruta directa a la red local
    Gateway,    // Ruta a través de un gateway
    Host,       // Ruta a un host específico
    Default,    // Ruta por defecto
}

impl From<u8> for RouteType {
    fn from(value: u8) -> Self {
        match value {
            1 => RouteType::Direct,
            2 => RouteType::Gateway,
            3 => RouteType::Host,
            4 => RouteType::Default,
            _ => RouteType::Direct,
        }
    }
}

/// Entrada de la tabla de routing
#[derive(Debug, Clone)]
pub struct Route {
    pub destination: IpAddress,
    pub netmask: IpAddress,
    pub gateway: Option<IpAddress>,
    pub interface_index: u32,
    pub route_type: RouteType,
    pub metric: u32,
    pub is_active: bool,
    pub timestamp: u64,
}

impl Route {
    /// Crear nueva ruta
    pub fn new(
        destination: IpAddress,
        netmask: IpAddress,
        gateway: Option<IpAddress>,
        interface_index: u32,
        route_type: RouteType,
        metric: u32,
    ) -> Self {
        Self {
            destination,
            netmask,
            gateway,
            interface_index,
            route_type,
            metric,
            is_active: true,
            timestamp: 0,
        }
    }
    
    /// Crear ruta directa
    pub fn direct(destination: IpAddress, netmask: IpAddress, interface_index: u32) -> Self {
        Self::new(destination, netmask, None, interface_index, RouteType::Direct, 0)
    }
    
    /// Crear ruta a través de gateway
    pub fn gateway(
        destination: IpAddress,
        netmask: IpAddress,
        gateway: IpAddress,
        interface_index: u32,
        metric: u32,
    ) -> Self {
        Self::new(destination, netmask, Some(gateway), interface_index, RouteType::Gateway, metric)
    }
    
    /// Crear ruta por defecto
    pub fn default(gateway: IpAddress, interface_index: u32, metric: u32) -> Self {
        Self::new(
            IpAddress::zero(),
            IpAddress::zero(),
            Some(gateway),
            interface_index,
            RouteType::Default,
            metric,
        )
    }
    
    /// Establecer timestamp
    pub fn set_timestamp(&mut self, timestamp: u64) {
        self.timestamp = timestamp;
    }
    
    /// Activar ruta
    pub fn activate(&mut self) {
        self.is_active = true;
    }
    
    /// Desactivar ruta
    pub fn deactivate(&mut self) {
        self.is_active = false;
    }
    
    /// Verificar si la ruta coincide con una IP
    pub fn matches(&self, ip: IpAddress) -> bool {
        if !self.is_active {
            return false;
        }
        
        match self.route_type {
            RouteType::Default => true,
            RouteType::Direct | RouteType::Gateway | RouteType::Host => {
                let network = self.get_network_address();
                let target_network = self.get_target_network(ip);
                network == target_network
            }
        }
    }
    
    /// Obtener dirección de red
    pub fn get_network_address(&self) -> IpAddress {
        let network_bytes = [
            self.destination.bytes[0] & self.netmask.bytes[0],
            self.destination.bytes[1] & self.netmask.bytes[1],
            self.destination.bytes[2] & self.netmask.bytes[2],
            self.destination.bytes[3] & self.netmask.bytes[3],
        ];
        IpAddress::from_bytes(network_bytes)
    }
    
    /// Obtener red objetivo para una IP
    fn get_target_network(&self, ip: IpAddress) -> IpAddress {
        let network_bytes = [
            ip.bytes[0] & self.netmask.bytes[0],
            ip.bytes[1] & self.netmask.bytes[1],
            ip.bytes[2] & self.netmask.bytes[2],
            ip.bytes[3] & self.netmask.bytes[3],
        ];
        IpAddress::from_bytes(network_bytes)
    }
    
    /// Obtener gateway
    pub fn get_gateway(&self) -> Option<IpAddress> {
        self.gateway
    }
    
    /// Obtener interfaz
    pub fn get_interface(&self) -> u32 {
        self.interface_index
    }
    
    /// Obtener métrica
    pub fn get_metric(&self) -> u32 {
        self.metric
    }
    
    /// Comparar rutas por métrica
    pub fn compare_by_metric(&self, other: &Route) -> Ordering {
        self.metric.cmp(&other.metric)
    }
}

/// Tabla de routing
pub struct RoutingTable {
    pub routes: Vec<Route>,
    pub max_routes: usize,
}

impl RoutingTable {
    /// Crear nueva tabla de routing
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            max_routes: super::MAX_ROUTES,
        }
    }
    
    /// Agregar ruta
    pub fn add_route(&mut self, route: Route) -> Result<(), NetworkError> {
        if self.routes.len() >= self.max_routes {
            return Err(NetworkError::OutOfMemory);
        }
        
        // Verificar si ya existe una ruta similar
        if let Some(existing_route) = self.find_specific_route(route.destination, route.netmask) {
            // Actualizar ruta existente
            if let Some(pos) = self.routes.iter().position(|r| r.destination == existing_route.destination && r.netmask == existing_route.netmask) {
                self.routes[pos] = route;
            }
        } else {
            self.routes.push(route);
        }
        
        Ok(())
    }
    
    /// Buscar ruta para una IP
    pub fn find_route(&self, ip: IpAddress) -> Option<&Route> {
        let mut best_route: Option<&Route> = None;
        let mut best_metric = u32::MAX;
        
        for route in &self.routes {
            if route.matches(ip) {
                // Preferir rutas más específicas (métricas más bajas)
                if route.metric < best_metric {
                    best_route = Some(route);
                    best_metric = route.metric;
                }
            }
        }
        
        best_route
    }
    
    /// Buscar ruta específica
    pub fn find_specific_route(&self, destination: IpAddress, netmask: IpAddress) -> Option<&Route> {
        self.routes.iter().find(|route| {
            route.destination == destination && route.netmask == netmask
        })
    }
    
    /// Remover ruta
    pub fn remove_route(&mut self, destination: IpAddress, netmask: IpAddress) -> bool {
        if let Some(pos) = self.routes.iter().position(|route| {
            route.destination == destination && route.netmask == netmask
        }) {
            self.routes.remove(pos);
            true
        } else {
            false
        }
    }
    
    /// Obtener todas las rutas
    pub fn get_routes(&self) -> &[Route] {
        &self.routes
    }
    
    /// Obtener rutas activas
    pub fn get_active_routes(&self) -> Vec<&Route> {
        self.routes.iter().filter(|route| route.is_active).collect()
    }
    
    /// Obtener rutas por interfaz
    pub fn get_routes_by_interface(&self, interface_index: u32) -> Vec<&Route> {
        self.routes.iter()
            .filter(|route| route.interface_index == interface_index)
            .collect()
    }
    
    /// Limpiar rutas expiradas
    pub fn cleanup_expired(&mut self, current_time: u64, ttl: u64) {
        self.routes.retain(|route| {
            if route.is_active && (current_time - route.timestamp) > ttl {
                false
            } else {
                true
            }
        });
    }
    
    /// Obtener número de rutas
    pub fn size(&self) -> usize {
        self.routes.len()
    }
    
    /// Verificar si la tabla está llena
    pub fn is_full(&self) -> bool {
        self.routes.len() >= self.max_routes
    }
    
    /// Limpiar tabla
    pub fn clear(&mut self) {
        self.routes.clear();
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> RoutingTableStats {
        let active_routes = self.routes.iter().filter(|r| r.is_active).count();
        let direct_routes = self.routes.iter().filter(|r| r.route_type == RouteType::Direct).count();
        let gateway_routes = self.routes.iter().filter(|r| r.route_type == RouteType::Gateway).count();
        let default_routes = self.routes.iter().filter(|r| r.route_type == RouteType::Default).count();
        
        RoutingTableStats {
            total_routes: self.routes.len(),
            active_routes,
            direct_routes,
            gateway_routes,
            default_routes,
            max_routes: self.max_routes,
        }
    }
}

/// Estadísticas de tabla de routing
#[derive(Debug, Clone)]
pub struct RoutingTableStats {
    pub total_routes: usize,
    pub active_routes: usize,
    pub direct_routes: usize,
    pub gateway_routes: usize,
    pub default_routes: usize,
    pub max_routes: usize,
}

/// Algoritmo de routing
pub struct RoutingAlgorithm {
    pub table: RoutingTable,
    pub max_hops: u32,
    pub route_ttl: u64,
}

impl RoutingAlgorithm {
    /// Crear nuevo algoritmo de routing
    pub fn new() -> Self {
        Self {
            table: RoutingTable::new(),
            max_hops: 15,
            route_ttl: 300, // 5 minutos
        }
    }
    
    /// Agregar ruta
    pub fn add_route(&mut self, route: Route) -> Result<(), NetworkError> {
        self.table.add_route(route)
    }
    
    /// Buscar mejor ruta
    pub fn find_best_route(&self, ip: IpAddress) -> Option<&Route> {
        self.table.find_route(ip)
    }
    
    /// Obtener siguiente salto
    pub fn get_next_hop(&self, ip: IpAddress) -> Option<IpAddress> {
        if let Some(route) = self.find_best_route(ip) {
            match route.route_type {
                RouteType::Direct => Some(ip),
                RouteType::Gateway | RouteType::Default => route.get_gateway(),
                RouteType::Host => Some(ip),
            }
        } else {
            None
        }
    }
    
    /// Obtener interfaz de salida
    pub fn get_output_interface(&self, ip: IpAddress) -> Option<u32> {
        self.find_best_route(ip).map(|route| route.get_interface())
    }
    
    /// Verificar si una IP es local
    pub fn is_local(&self, ip: IpAddress) -> bool {
        if let Some(route) = self.find_best_route(ip) {
            route.route_type == RouteType::Direct
        } else {
            false
        }
    }
    
    /// Actualizar métricas
    pub fn update_metrics(&mut self, current_time: u64) {
        for route in &mut self.table.routes {
            // Simular actualización de métricas basada en el tiempo
            if current_time - route.timestamp > 60 {
                // Incrementar métrica para rutas antiguas
                route.metric = route.metric.saturating_add(1);
                route.set_timestamp(current_time);
            }
        }
    }
    
    /// Limpiar rutas expiradas
    pub fn cleanup(&mut self, current_time: u64) {
        self.table.cleanup_expired(current_time, self.route_ttl);
    }
    
    /// Obtener tabla de routing
    pub fn get_table(&self) -> &RoutingTable {
        &self.table
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> RoutingTableStats {
        self.table.get_stats()
    }
}

/// Instancia global del algoritmo de routing
static mut ROUTING_ALGORITHM: Option<RoutingAlgorithm> = None;

/// Inicializar tabla de routing
pub fn init_routing_table() -> Result<(), NetworkError> {
    unsafe {
        if ROUTING_ALGORITHM.is_some() {
            return Err(NetworkError::ProtocolError);
        }
        
        ROUTING_ALGORITHM = Some(RoutingAlgorithm::new());
        Ok(())
    }
}

/// Obtener algoritmo de routing
pub fn get_routing_algorithm() -> Option<&'static mut RoutingAlgorithm> {
    unsafe { ROUTING_ALGORITHM.as_mut() }
}

/// Obtener estadísticas de routing
pub fn get_routing_stats() -> Option<RoutingTableStats> {
    unsafe { ROUTING_ALGORITHM.as_ref().map(|ra| ra.get_stats()) }
}
