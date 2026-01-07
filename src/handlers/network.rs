//! Network management handlers - Non-blocking HTTP handlers for port management
//!
//! Port changes are tracked in state and applied when the container is recreated on restart.
//! This follows the same pattern as Pterodactyl Wings - containers are destroyed and recreated
//! on every start to apply the latest configuration.
//!
//! Provides endpoints for:
//! - Listing allocated ports for a container
//! - Adding new port bindings (applied on restart)
//! - Removing port bindings (applied on restart)
//! - Getting available ports
//! - Updating port bindings (change host port)

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::{
    docker::network::PortAllocation,
    models::ApiResponse,
    types::AppState,
};

#[derive(Debug, Deserialize)]
pub struct AddPortRequest {
    pub container_port: String,
    pub host_port: Option<String>, // "auto" or specific port
    pub host_ip: Option<String>,   // default: "0.0.0.0"
    pub protocol: Option<String>,  // default: "tcp"
}

#[derive(Debug, Deserialize)]
pub struct RemovePortRequest {
    pub container_port: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePortRequest {
    pub container_port: String,
    pub new_host_port: String, // "auto" or specific port
    pub host_ip: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NetworkInfo {
    pub container_id: String,
    pub uuid: String,
    pub allocated_ports: Vec<PortAllocation>,
    pub available_port_count: usize,
}

#[derive(Debug, Serialize)]
pub struct AvailablePortsResponse {
    pub available_ports: Vec<u16>,
    pub port_range: (u16, u16),
    pub allocated_count: usize,
}

/// Get network info for a container by UUID
pub async fn get_container_network(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Json<ApiResponse<NetworkInfo>>, StatusCode> {
    info!("Getting network info for container: {}", uuid);
    
    // Get container from tracker
    let tracker = match state.container_tracker.get_container(&uuid).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            // Try by container ID
            match state.container_tracker.find_by_container_id(&uuid).await {
                Ok(Some(t)) => t,
                _ => {
                    return Ok(Json(ApiResponse::error("Container not found".to_string())));
                }
            }
        }
        Err(e) => {
            error!("Failed to get container: {}", e);
            return Ok(Json(ApiResponse::error(e.to_string())));
        }
    };
    
    // Get available port count
    let net_mgr = state.network.read().await;
    let allocated = net_mgr.get_allocated_ports();
    let available_count = 50 - allocated.len(); // Approximate available ports
    drop(net_mgr);
    
    let info = NetworkInfo {
        container_id: tracker.container_id.clone(),
        uuid: tracker.custom_uuid.clone(),
        allocated_ports: tracker.allocated_ports.clone(),
        available_port_count: available_count,
    };
    
    Ok(Json(ApiResponse::success(info)))
}

/// Get available ports
pub async fn get_available_ports(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<AvailablePortsResponse>>, StatusCode> {
    info!("Getting available ports");
    
    let net_mgr = state.network.read().await;
    let allocated = net_mgr.get_allocated_ports();
    
    // Get port range from network config
    let port_range = net_mgr.get_port_range();
    
    // Get available ports from network config
    let available: Vec<u16> = (port_range.0..port_range.1)
        .filter(|p| !allocated.contains_key(p))
        .take(100) // Return up to 100 available ports
        .collect();
    
    let response = AvailablePortsResponse {
        available_ports: available,
        port_range,
        allocated_count: allocated.len(),
    };
    
    Ok(Json(ApiResponse::success(response)))
}

/// Add a port binding to a container
/// Port changes are tracked and applied when the container is recreated on restart
pub async fn add_port_binding(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    Json(req): Json<AddPortRequest>,
) -> Result<Json<ApiResponse<PortAllocation>>, StatusCode> {
    info!("Adding port binding to container {}: container_port={}", uuid, req.container_port);
    
    // Get container from tracker
    let tracker = match state.container_tracker.get_container(&uuid).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Ok(Json(ApiResponse::error("Container not found".to_string())));
        }
        Err(e) => {
            return Ok(Json(ApiResponse::error(e.to_string())));
        }
    };
    
    // Check if container port is already bound
    if tracker.allocated_ports.iter().any(|p| p.container_port == req.container_port) {
        return Ok(Json(ApiResponse::error(format!(
            "Container port {} is already bound", req.container_port
        ))));
    }
    
    let host_ip = req.host_ip.unwrap_or_else(|| "0.0.0.0".to_string());
    let protocol = req.protocol.unwrap_or_else(|| "tcp".to_string());
    
    // Allocate host port
    let mut net_mgr = state.network.write().await;
    
    let host_port = if let Some(ref port_str) = req.host_port {
        if port_str == "auto" || port_str.is_empty() {
            match net_mgr.find_available_port() {
                Some(port) => {
                    if let Err(e) = net_mgr.allocate_port(port, tracker.custom_uuid.clone()) {
                        return Ok(Json(ApiResponse::error(e)));
                    }
                    port
                }
                None => {
                    return Ok(Json(ApiResponse::error("No available ports".to_string())));
                }
            }
        } else {
            let port: u16 = match port_str.parse() {
                Ok(p) => p,
                Err(_) => {
                    return Ok(Json(ApiResponse::error(format!("Invalid port: {}", port_str))));
                }
            };
            
            if let Err(e) = net_mgr.allocate_port(port, tracker.custom_uuid.clone()) {
                return Ok(Json(ApiResponse::error(e)));
            }
            port
        }
    } else {
        match net_mgr.find_available_port() {
            Some(port) => {
                if let Err(e) = net_mgr.allocate_port(port, tracker.custom_uuid.clone()) {
                    return Ok(Json(ApiResponse::error(e)));
                }
                port
            }
            None => {
                return Ok(Json(ApiResponse::error("No available ports".to_string())));
            }
        }
    };
    
    // Save network state
    if let Err(e) = net_mgr.save_state().await {
        warn!("Failed to save network state: {}", e);
    }
    drop(net_mgr);
    
    let allocation = PortAllocation {
        container_port: req.container_port.clone(),
        host_port: host_port.to_string(),
        host_ip,
        protocol,
    };
    
    // Update container tracker with new port
    let mut updated_ports = tracker.allocated_ports.clone();
    updated_ports.push(allocation.clone());
    
    if let Err(e) = state.container_tracker.update_container_ports(&uuid, updated_ports).await {
        error!("Failed to update container ports: {}", e);
        return Ok(Json(ApiResponse::error(e.to_string())));
    }
    
    // Update state manager ports
    if let Some(mut container_state) = state.state_manager.get_container(&uuid) {
        container_state.ports.insert(req.container_port.clone(), host_port.to_string());
        let _ = state.state_manager.add_container(&uuid, container_state).await;
    }
    
    info!("Added port binding: {} -> {} for container {} (will apply on restart)", req.container_port, host_port, uuid);
    
    Ok(Json(ApiResponse::success(allocation)))
}

/// Remove a port binding from a container
/// Port changes are tracked and applied when the container is recreated on restart
pub async fn remove_port_binding(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    Json(req): Json<RemovePortRequest>,
) -> Result<Json<ApiResponse<String>>, StatusCode> {
    info!("Removing port binding from container {}: container_port={}", uuid, req.container_port);
    
    // Get container from tracker
    let tracker = match state.container_tracker.get_container(&uuid).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Ok(Json(ApiResponse::error("Container not found".to_string())));
        }
        Err(e) => {
            return Ok(Json(ApiResponse::error(e.to_string())));
        }
    };
    
    // Find the port allocation
    let allocation = match tracker.allocated_ports.iter().find(|p| p.container_port == req.container_port) {
        Some(a) => a.clone(),
        None => {
            return Ok(Json(ApiResponse::error(format!(
                "Container port {} is not bound", req.container_port
            ))));
        }
    };
    
    // Release the host port
    let mut net_mgr = state.network.write().await;
    if let Ok(port) = allocation.host_port.parse::<u16>() {
        net_mgr.release_port(port);
    }
    
    // Save network state
    if let Err(e) = net_mgr.save_state().await {
        warn!("Failed to save network state: {}", e);
    }
    drop(net_mgr);
    
    // Update container tracker - remove the port
    let updated_ports: Vec<PortAllocation> = tracker.allocated_ports
        .into_iter()
        .filter(|p| p.container_port != req.container_port)
        .collect();
    
    if let Err(e) = state.container_tracker.update_container_ports(&uuid, updated_ports).await {
        error!("Failed to update container ports: {}", e);
        return Ok(Json(ApiResponse::error(e.to_string())));
    }
    
    // Update state manager ports
    if let Some(mut container_state) = state.state_manager.get_container(&uuid) {
        container_state.ports.remove(&req.container_port);
        let _ = state.state_manager.add_container(&uuid, container_state).await;
    }
    
    info!("Removed port binding: {} from container {} (will apply on restart)", req.container_port, uuid);
    
    Ok(Json(ApiResponse::success(format!("Port {} removed (restart to apply)", req.container_port))))
}

/// Update a port binding (change host port)
/// Port changes are tracked and applied when the container is recreated on restart
pub async fn update_port_binding(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    Json(req): Json<UpdatePortRequest>,
) -> Result<Json<ApiResponse<PortAllocation>>, StatusCode> {
    info!("Updating port binding for container {}: container_port={}", uuid, req.container_port);
    
    // Get container from tracker
    let tracker = match state.container_tracker.get_container(&uuid).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Ok(Json(ApiResponse::error("Container not found".to_string())));
        }
        Err(e) => {
            return Ok(Json(ApiResponse::error(e.to_string())));
        }
    };
    
    // Find the existing port allocation
    let old_allocation = match tracker.allocated_ports.iter().find(|p| p.container_port == req.container_port) {
        Some(a) => a.clone(),
        None => {
            return Ok(Json(ApiResponse::error(format!(
                "Container port {} is not bound", req.container_port
            ))));
        }
    };
    
    let mut net_mgr = state.network.write().await;
    
    // Release old port
    if let Ok(old_port) = old_allocation.host_port.parse::<u16>() {
        net_mgr.release_port(old_port);
    }
    
    // Allocate new port
    let new_host_port = if req.new_host_port == "auto" || req.new_host_port.is_empty() {
        match net_mgr.find_available_port() {
            Some(port) => {
                if let Err(e) = net_mgr.allocate_port(port, tracker.custom_uuid.clone()) {
                    return Ok(Json(ApiResponse::error(e)));
                }
                port
            }
            None => {
                return Ok(Json(ApiResponse::error("No available ports".to_string())));
            }
        }
    } else {
        let port: u16 = match req.new_host_port.parse() {
            Ok(p) => p,
            Err(_) => {
                return Ok(Json(ApiResponse::error(format!("Invalid port: {}", req.new_host_port))));
            }
        };
        
        if let Err(e) = net_mgr.allocate_port(port, tracker.custom_uuid.clone()) {
            return Ok(Json(ApiResponse::error(e)));
        }
        port
    };
    
    // Save network state
    if let Err(e) = net_mgr.save_state().await {
        warn!("Failed to save network state: {}", e);
    }
    drop(net_mgr);
    
    let new_allocation = PortAllocation {
        container_port: req.container_port.clone(),
        host_port: new_host_port.to_string(),
        host_ip: req.host_ip.unwrap_or(old_allocation.host_ip),
        protocol: old_allocation.protocol,
    };
    
    // Update container tracker
    let updated_ports: Vec<PortAllocation> = tracker.allocated_ports
        .into_iter()
        .map(|p| {
            if p.container_port == req.container_port {
                new_allocation.clone()
            } else {
                p
            }
        })
        .collect();
    
    if let Err(e) = state.container_tracker.update_container_ports(&uuid, updated_ports).await {
        error!("Failed to update container ports: {}", e);
        return Ok(Json(ApiResponse::error(e.to_string())));
    }
    
    // Update state manager ports
    if let Some(mut container_state) = state.state_manager.get_container(&uuid) {
        container_state.ports.insert(req.container_port.clone(), new_host_port.to_string());
        let _ = state.state_manager.add_container(&uuid, container_state).await;
    }
    
    info!("Updated port binding: {} -> {} for container {} (will apply on restart)", req.container_port, new_host_port, uuid);
    
    Ok(Json(ApiResponse::success(new_allocation)))
}

/// Get all port allocations across all containers
pub async fn get_all_port_allocations(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<Vec<(String, Vec<PortAllocation>)>>>, StatusCode> {
    info!("Getting all port allocations");
    
    let containers = match state.container_tracker.list_containers().await {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to list containers: {}", e);
            return Ok(Json(ApiResponse::error(e.to_string())));
        }
    };
    
    let allocations: Vec<(String, Vec<PortAllocation>)> = containers
        .into_iter()
        .filter(|c| !c.allocated_ports.is_empty())
        .map(|c| (c.custom_uuid, c.allocated_ports))
        .collect();
    
    Ok(Json(ApiResponse::success(allocations)))
}


/// Apply port changes by recreating the container
/// This is the recommended way to apply port changes - triggers container recreation
pub async fn apply_port_changes(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, StatusCode> {
    info!("Applying port changes for container: {}", uuid);
    
    // Check if locked
    if state.state_manager.is_container_locked(&uuid) {
        return Ok(Json(ApiResponse::error("Container is currently locked".to_string())));
    }
    
    // Check if container exists
    let tracker = match state.container_tracker.get_container(&uuid).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            error!("Container {} not found in tracker", uuid);
            return Ok(Json(ApiResponse::error("Container not found".to_string())));
        }
        Err(e) => {
            error!("Failed to get container {}: {}", uuid, e);
            return Ok(Json(ApiResponse::error(format!("Failed to get container: {}", e))));
        }
    };
    
    info!("Found container {} with {} ports, triggering recreation", uuid, tracker.allocated_ports.len());
    
    // Spawn background task for recreation
    let lifecycle = state.lifecycle.clone();
    let uuid_clone = uuid.clone();
    
    tokio::spawn(async move {
        info!("Background task started for container {} recreation", uuid_clone);
        match lifecycle.recreate_container(&uuid_clone, None).await {
            Ok(new_id) => {
                info!("Port changes applied for {}, new container ID: {}", uuid_clone, new_id);
            }
            Err(e) => {
                error!("Failed to apply port changes for {}: {}", uuid_clone, e);
            }
        }
    });
    
    Ok(Json(ApiResponse::success(serde_json::json!({
        "status": "accepted",
        "message": format!("Port changes being applied for container {}", uuid),
    }))))
}
