//! Container Lifecycle Manager - Stateless container recreation
//!
//! Containers are disposable shells around persistent volumes.
//! UUID is the permanent identity, Docker container ID is ephemeral.
//!
//! Key principles:
//! - UUID never changes, container_id can change anytime
//! - Volume named by UUID persists across container recreations
//! - All config changes (env, limits, ports) trigger container recreation
//! - Lock during transitions to prevent concurrent operations
//! - Async, non-blocking - all operations fire-and-forget

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, error, warn};

use crate::docker::{ContainerManager, NetworkManager};
use crate::models::{CreateContainerRequest, ResourceLimits};
use crate::container_tracker::ContainerTrackingManager;
use crate::state_manager::StateManager;
use crate::docker::network::PortAllocation;

/// Request to update container configuration
#[derive(Debug, Clone)]
pub struct ContainerUpdateRequest {
    pub env: Option<HashMap<String, String>>,
    pub limits: Option<ResourceLimits>,
    pub ports: Option<HashMap<String, String>>,
    pub image: Option<String>,
    pub startup_command: Option<Vec<String>>,
}

/// Container lifecycle manager - handles stateless container recreation
pub struct ContainerLifecycleManager {
    docker: Arc<bollard::Docker>,
    state_manager: Arc<StateManager>,
    container_tracker: Arc<ContainerTrackingManager>,
    network: Arc<RwLock<NetworkManager>>,
    volumes_path: String,
}

impl ContainerLifecycleManager {
    pub fn new(
        docker: Arc<bollard::Docker>,
        state_manager: Arc<StateManager>,
        container_tracker: Arc<ContainerTrackingManager>,
        network: Arc<RwLock<NetworkManager>>,
        volumes_path: String,
    ) -> Self {
        Self {
            docker,
            state_manager,
            container_tracker,
            network,
            volumes_path,
        }
    }

    /// Recreate container with updated configuration
    /// This is the core operation - destroy old container, create new one with same UUID
    /// Volume persists, container_id changes
    pub async fn recreate_container(
        &self,
        uuid: &str,
        update: Option<ContainerUpdateRequest>,
    ) -> Result<String, String> {
        info!("Lifecycle: Starting container recreation for UUID {}", uuid);

        // 1. Lock the container
        self.state_manager.lock_container(uuid, "Recreating container").await
            .map_err(|e| format!("Failed to lock container: {}", e))?;
        self.state_manager.update_container_state(uuid, "recreating").await
            .map_err(|e| format!("Failed to update state: {}", e))?;

        // 2. Load current tracker data
        let tracker = match self.container_tracker.get_container(uuid).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.unlock_container(uuid).await;
                return Err("Container tracker data not found".to_string());
            }
            Err(e) => {
                self.unlock_container(uuid).await;
                return Err(format!("Failed to load container data: {}", e));
            }
        };

        let old_container_id = tracker.container_id.clone();
        let manager = ContainerManager::new(self.docker.as_ref().clone());

        // 3. Stop and remove old container (ignore errors - might not exist)
        info!("Lifecycle: Stopping old container {}", old_container_id);
        let _ = manager.stop(&old_container_id).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        info!("Lifecycle: Removing old container {}", old_container_id);
        if let Err(e) = manager.remove(&old_container_id).await {
            warn!("Lifecycle: Failed to remove old container (may not exist): {}", e);
        }

        // 4. Release old port allocations so they can be re-allocated
        // This is critical - ports are tracked by UUID, we need to release them first
        {
            let mut net_mgr = self.network.write().await;
            for alloc in &tracker.allocated_ports {
                if let Ok(port) = alloc.host_port.parse::<u16>() {
                    net_mgr.release_port(port);
                    info!("Lifecycle: Released port {} for recreation", port);
                }
            }
        }

        // 5. Merge update with existing config
        let (new_env, new_limits, _new_ports, new_image, _new_startup) = if let Some(upd) = update {
            (
                upd.env.or(tracker.env.clone()),
                upd.limits.map(|l| l).or(Some(tracker.limits.clone())),
                upd.ports.or(Some(tracker.ports.clone())),
                upd.image.unwrap_or(tracker.image.clone()),
                upd.startup_command.or(tracker.startup_command.clone()),
            )
        } else {
            (
                tracker.env.clone(),
                Some(tracker.limits.clone()),
                Some(tracker.ports.clone()),
                tracker.image.clone(),
                tracker.startup_command.clone(),
            )
        };

        // 6. Build port config from tracker's allocated_ports (preserve existing allocations)
        let ports_map: HashMap<String, String> = tracker.allocated_ports
            .iter()
            .map(|p| (p.container_port.clone(), p.host_port.clone()))
            .collect();

        // 7. Create new container with same UUID
        info!("Lifecycle: Creating new container for UUID {} with {} ports", uuid, ports_map.len());
        let create_req = CreateContainerRequest {
            image: new_image,
            name: Some(tracker.name.clone()),
            description: tracker.description.clone(),
            startup_command: Some(vec!["/bin/sh".to_string(), "/data/entrypoint.sh".to_string()]),
            env: new_env,
            ports: if ports_map.is_empty() { None } else { Some(ports_map) },
            volumes: Some(tracker.attached_volumes.clone()),
            command: None,
            working_dir: None,
            restart_policy: None,
            custom_uuid: Some(uuid.to_string()),
            limits: new_limits,
            install_content: None, // Don't re-run install
            update_content: tracker.update_content.clone(),
        };

        match manager.create_with_networking(create_req, &self.network, uuid, &self.volumes_path).await {
            Ok((new_container_id, allocations)) => {
                info!("Lifecycle: New container created: {}", new_container_id);

                // 8. Update tracker with new container ID
                let mut updated_tracker = tracker.clone();
                updated_tracker.container_id = new_container_id.clone();
                updated_tracker.allocated_ports = allocations;
                if let Err(e) = self.container_tracker.save_container(&updated_tracker).await {
                    error!("Lifecycle: Failed to save tracker: {}", e);
                }

                // 9. Update state manager with new container ID
                let _ = self.state_manager.update_container_id(uuid, &new_container_id).await;

                // 10. Start the new container
                info!("Lifecycle: Starting new container {}", new_container_id);
                if let Err(e) = manager.start(&new_container_id).await {
                    error!("Lifecycle: Failed to start container: {}", e);
                    let _ = self.state_manager.update_container_state(uuid, "failed").await;
                } else {
                    let _ = self.state_manager.update_container_state(uuid, "running").await;
                }

                // 11. Unlock
                self.unlock_container(uuid).await;
                info!("Lifecycle: Container {} recreation complete, new ID: {}", uuid, new_container_id);
                Ok(new_container_id)
            }
            Err(e) => {
                error!("Lifecycle: Failed to create container: {}", e);
                let _ = self.state_manager.update_container_state(uuid, "failed").await;
                self.unlock_container(uuid).await;
                Err(format!("Failed to create container: {}", e))
            }
        }
    }

    /// Update container environment variables (triggers recreation)
    pub async fn update_env(
        &self,
        uuid: &str,
        env: HashMap<String, String>,
    ) -> Result<String, String> {
        self.recreate_container(uuid, Some(ContainerUpdateRequest {
            env: Some(env),
            limits: None,
            ports: None,
            image: None,
            startup_command: None,
        })).await
    }

    /// Update container resource limits (triggers recreation)
    pub async fn update_limits(
        &self,
        uuid: &str,
        limits: ResourceLimits,
    ) -> Result<String, String> {
        self.recreate_container(uuid, Some(ContainerUpdateRequest {
            env: None,
            limits: Some(limits),
            ports: None,
            image: None,
            startup_command: None,
        })).await
    }

    /// Update container ports (triggers recreation)
    pub async fn update_ports(
        &self,
        uuid: &str,
        ports: HashMap<String, String>,
    ) -> Result<String, String> {
        // First update the port allocations in tracker
        let tracker = self.container_tracker.get_container(uuid).await
            .map_err(|e| e.to_string())?
            .ok_or("Container not found")?;

        // Release old ports and allocate new ones
        let mut net_mgr = self.network.write().await;
        
        // Release existing ports
        for alloc in &tracker.allocated_ports {
            if let Ok(port) = alloc.host_port.parse::<u16>() {
                net_mgr.release_port(port);
            }
        }

        // Allocate new ports
        let new_allocations = net_mgr.auto_allocate_ports(uuid, &ports)
            .map_err(|e| format!("Port allocation failed: {}", e))?;
        
        drop(net_mgr);

        // Update tracker with new port allocations
        let new_port_allocs: Vec<PortAllocation> = new_allocations
            .iter()
            .map(|(cp, hp)| PortAllocation {
                container_port: cp.clone(),
                host_port: hp.clone(),
                host_ip: "0.0.0.0".to_string(),
                protocol: "tcp".to_string(),
            })
            .collect();

        self.container_tracker.update_container_ports(uuid, new_port_allocs).await
            .map_err(|e| e.to_string())?;

        // Now recreate with new ports
        self.recreate_container(uuid, Some(ContainerUpdateRequest {
            env: None,
            limits: None,
            ports: Some(new_allocations),
            image: None,
            startup_command: None,
        })).await
    }

    /// Change container image (triggers recreation)
    pub async fn change_image(
        &self,
        uuid: &str,
        image: String,
    ) -> Result<String, String> {
        self.recreate_container(uuid, Some(ContainerUpdateRequest {
            env: None,
            limits: None,
            ports: None,
            image: Some(image),
            startup_command: None,
        })).await
    }

    /// Helper to unlock container
    async fn unlock_container(&self, uuid: &str) {
        let _ = self.state_manager.unlock_container(uuid).await;
    }

    /// Create a new container with installation
    /// Uses temp container pattern: install in temp container, then create final container
    pub async fn create_with_install(
        &self,
        uuid: &str,
        image: &str,
        name: &str,
        description: Option<String>,
        startup_command: Option<Vec<String>>,
        env: Option<HashMap<String, String>>,
        ports: Option<HashMap<String, String>>,
        limits: Option<ResourceLimits>,
        install_content: Option<String>,
        update_content: Option<String>,
    ) -> Result<String, String> {
        info!("Lifecycle: Creating container {} with install", uuid);

        // 1. Lock the container
        self.state_manager.lock_container(uuid, "Installing").await
            .map_err(|e| format!("Failed to lock: {}", e))?;
        self.state_manager.update_container_state(uuid, "installing").await
            .map_err(|e| format!("Failed to update state: {}", e))?;

        let manager = ContainerManager::new(self.docker.as_ref().clone());

        // 2. Create volume directory for this UUID
        let volume_path = format!("{}/{}", self.volumes_path, uuid);
        tokio::fs::create_dir_all(&volume_path).await
            .map_err(|e| format!("Failed to create volume dir: {}", e))?;

        // 3. Create data directory for entrypoint
        let data_path = format!("{}/{}_data", self.volumes_path, uuid);
        tokio::fs::create_dir_all(&data_path).await
            .map_err(|e| format!("Failed to create data dir: {}", e))?;

        // Track install result - None means no install needed, Some(true) = success, Some(false) = failed
        let mut install_result: Option<bool> = None;

        // 4. If install_content provided, run install in temp container
        if let Some(install_script) = &install_content {
            info!("Lifecycle: Running install script in temp container for {}", uuid);

            // Write install script
            let install_script_path = format!("{}/install.sh", data_path);
            tokio::fs::write(&install_script_path, install_script).await
                .map_err(|e| format!("Failed to write install script: {}", e))?;
            
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&install_script_path, std::fs::Permissions::from_mode(0o755));
            }

            // Create temp install container (no ports, no tracking)
            let temp_container_name = format!("{}-install-{}", uuid, chrono::Utc::now().timestamp());
            let abs_volume_path = std::fs::canonicalize(&volume_path)
                .map_err(|e| format!("Failed to get abs path: {}", e))?
                .to_string_lossy()
                .to_string();
            let abs_data_path = std::fs::canonicalize(&data_path)
                .map_err(|e| format!("Failed to get abs data path: {}", e))?
                .to_string_lossy()
                .to_string();

            let temp_config = bollard::container::Config {
                image: Some(image.to_string()),
                cmd: Some(vec!["/bin/sh".to_string(), "/data/install.sh".to_string()]),
                working_dir: Some("/home/container".to_string()),
                tty: Some(true),
                env: Some(vec!["MODE=INSTALL".to_string()]),
                host_config: Some(bollard::models::HostConfig {
                    binds: Some(vec![
                        format!("{}:/home/container", abs_volume_path),
                        format!("{}:/data:ro", abs_data_path),
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let temp_options = bollard::container::CreateContainerOptions {
                name: &temp_container_name,
                platform: None,
            };

            // Pull image first
            manager.pull_image_if_needed(image).await
                .map_err(|e| format!("Failed to pull image: {}", e))?;

            // Create temp container
            let temp_response = self.docker.create_container(Some(temp_options), temp_config).await
                .map_err(|e| format!("Failed to create temp container: {}", e))?;
            let temp_id = temp_response.id;

            info!("Lifecycle: Starting temp install container {}", temp_id);

            // Start temp container
            self.docker.start_container::<String>(&temp_id, None).await
                .map_err(|e| format!("Failed to start temp container: {}", e))?;

            // Wait for install to complete (max 10 minutes)
            let max_wait = 600;
            let mut waited = 0;
            let mut install_success = false;
            let mut install_timed_out = false;

            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                waited += 2;

                if waited > max_wait {
                    error!("Lifecycle: Install timed out for {}", uuid);
                    install_timed_out = true;
                    // Cleanup temp container but continue to create final container
                    let _ = self.docker.remove_container(&temp_id, Some(bollard::container::RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    })).await;
                    break;
                }

                match self.docker.inspect_container(&temp_id, None).await {
                    Ok(info) => {
                        if let Some(state) = info.state {
                            if state.running != Some(true) {
                                let exit_code = state.exit_code.unwrap_or(-1);
                                info!("Lifecycle: Install finished with exit code {}", exit_code);
                                install_success = exit_code == 0;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Lifecycle: Failed to inspect temp container: {}", e);
                        break;
                    }
                }
            }

            // Remove temp container (if not already removed due to timeout)
            if !install_timed_out {
                info!("Lifecycle: Removing temp install container {}", temp_id);
                let _ = self.docker.remove_container(&temp_id, Some(bollard::container::RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                })).await;
            }

            // Track the install result
            if install_timed_out {
                install_result = Some(false);
                warn!("Lifecycle: Install timed out for {}, but continuing to create final container", uuid);
            } else if !install_success {
                install_result = Some(false);
                warn!("Lifecycle: Install script failed for {}, but continuing to create final container", uuid);
            } else {
                install_result = Some(true);
                info!("Lifecycle: Install succeeded for {}", uuid);
            }
        }

        // 5. Write startup entrypoint
        let startup_cmd = startup_command.as_ref()
            .map(|cmd| cmd.join(" "))
            .unwrap_or_else(|| "sleep infinity".to_string());
        let entrypoint_path = format!("{}/entrypoint.sh", data_path);
        tokio::fs::write(&entrypoint_path, &startup_cmd).await
            .map_err(|e| format!("Failed to write entrypoint: {}", e))?;
        
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&entrypoint_path, std::fs::Permissions::from_mode(0o755));
        }

        // 6. Create final container with ports
        info!("Lifecycle: Creating final container for {}", uuid);
        let limits_for_tracker = limits.clone();
        let update_content_for_tracker = update_content.clone();
        let create_req = CreateContainerRequest {
            image: image.to_string(),
            name: Some(name.to_string()),
            description,
            startup_command: Some(vec!["/bin/sh".to_string(), "/data/entrypoint.sh".to_string()]),
            env,
            ports,
            volumes: None, // Volume is auto-mounted by create_with_networking
            command: None,
            working_dir: None,
            restart_policy: None,
            custom_uuid: Some(uuid.to_string()),
            limits,
            install_content: None, // Already installed
            update_content,
        };

        match manager.create_with_networking(create_req, &self.network, uuid, &self.volumes_path).await {
            Ok((container_id, allocations)) => {
                info!("Lifecycle: Final container created: {}", container_id);

                // 7. Determine final state based on install result
                let final_state = match install_result {
                    Some(false) => "install_failed", // Install ran but failed
                    _ => "stopped", // No install or install succeeded - container is stopped, ready to start
                };

                // 8. Update state manager with final state (don't auto-start if install failed)
                self.state_manager.update_container_state(uuid, final_state).await.ok();
                self.state_manager.update_container_id(uuid, &container_id).await.ok();

                // 9. Save tracker
                let tracker = crate::models::ContainerTracker {
                    custom_uuid: uuid.to_string(),
                    container_id: container_id.clone(),
                    name: name.to_string(),
                    image: image.to_string(),
                    description: None,
                    startup_command: startup_command.clone(),
                    created_at: chrono::Utc::now(),
                    limits: limits_for_tracker.unwrap_or_default(),
                    allocated_ports: allocations,
                    attached_volumes: vec![],
                    ports: HashMap::new(),
                    env: None,
                    status: final_state.to_string(),
                    install_content,
                    update_content: update_content_for_tracker,
                };
                self.container_tracker.save_container(&tracker).await.ok();

                self.unlock_container(uuid).await;
                
                if install_result == Some(false) {
                    info!("Lifecycle: Container {} created but install failed - container is available for retry", uuid);
                } else {
                    info!("Lifecycle: Container {} creation complete", uuid);
                }
                
                Ok(container_id)
            }
            Err(e) => {
                error!("Lifecycle: Failed to create final container: {}", e);
                self.state_manager.update_container_state(uuid, "failed").await.ok();
                self.unlock_container(uuid).await;
                Err(format!("Failed to create container: {}", e))
            }
        }
    }

    /// Reinstall a container using temp container pattern
    /// 1. Stop and remove existing container
    /// 2. Run install script in temp container (mounts same volume)
    /// 3. Remove temp container
    /// 4. Create new final container with startup command
    pub async fn reinstall(
        &self,
        uuid: &str,
        install_script: &str,
    ) -> Result<String, String> {
        info!("Lifecycle: Starting reinstall for UUID {}", uuid);

        // 1. Lock the container
        self.state_manager.lock_container(uuid, "Reinstalling").await
            .map_err(|e| format!("Failed to lock: {}", e))?;
        self.state_manager.update_container_state(uuid, "installing").await
            .map_err(|e| format!("Failed to update state: {}", e))?;

        // 2. Load current tracker data
        let tracker = match self.container_tracker.get_container(uuid).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.unlock_container(uuid).await;
                return Err("Container tracker data not found".to_string());
            }
            Err(e) => {
                self.unlock_container(uuid).await;
                return Err(format!("Failed to load container data: {}", e));
            }
        };

        let old_container_id = tracker.container_id.clone();
        let manager = ContainerManager::new(self.docker.as_ref().clone());

        // 3. Stop and remove old container
        info!("Lifecycle: Stopping old container {} for reinstall", old_container_id);
        let _ = manager.stop(&old_container_id).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        info!("Lifecycle: Removing old container {}", old_container_id);
        if let Err(e) = manager.remove(&old_container_id).await {
            warn!("Lifecycle: Failed to remove old container (may not exist): {}", e);
        }

        // 4. Release old port allocations
        {
            let mut net_mgr = self.network.write().await;
            for alloc in &tracker.allocated_ports {
                if let Ok(port) = alloc.host_port.parse::<u16>() {
                    net_mgr.release_port(port);
                    info!("Lifecycle: Released port {} for reinstall", port);
                }
            }
        }

        // 5. Setup paths
        let volume_path = format!("{}/{}", self.volumes_path, uuid);
        let data_path = format!("{}/{}_data", self.volumes_path, uuid);
        
        // Ensure directories exist
        tokio::fs::create_dir_all(&volume_path).await
            .map_err(|e| format!("Failed to create volume dir: {}", e))?;
        tokio::fs::create_dir_all(&data_path).await
            .map_err(|e| format!("Failed to create data dir: {}", e))?;

        // 6. Write install script
        let install_script_path = format!("{}/install.sh", data_path);
        tokio::fs::write(&install_script_path, install_script).await
            .map_err(|e| format!("Failed to write install script: {}", e))?;
        
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&install_script_path, std::fs::Permissions::from_mode(0o755));
        }

        // 7. Create temp install container
        info!("Lifecycle: Creating temp install container for {}", uuid);
        let temp_container_name = format!("{}-reinstall-{}", uuid, chrono::Utc::now().timestamp());
        let abs_volume_path = std::fs::canonicalize(&volume_path)
            .map_err(|e| format!("Failed to get abs path: {}", e))?
            .to_string_lossy()
            .to_string();
        let abs_data_path = std::fs::canonicalize(&data_path)
            .map_err(|e| format!("Failed to get abs data path: {}", e))?
            .to_string_lossy()
            .to_string();

        let temp_config = bollard::container::Config {
            image: Some(tracker.image.clone()),
            cmd: Some(vec!["/bin/sh".to_string(), "/data/install.sh".to_string()]),
            working_dir: Some("/home/container".to_string()),
            tty: Some(true),
            env: Some(vec!["MODE=INSTALL".to_string()]),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(vec![
                    format!("{}:/home/container", abs_volume_path),
                    format!("{}:/data:ro", abs_data_path),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let temp_options = bollard::container::CreateContainerOptions {
            name: &temp_container_name,
            platform: None,
        };

        // Pull image first
        manager.pull_image_if_needed(&tracker.image).await
            .map_err(|e| format!("Failed to pull image: {}", e))?;

        // Create temp container
        let temp_response = self.docker.create_container(Some(temp_options), temp_config).await
            .map_err(|e| format!("Failed to create temp container: {}", e))?;
        let temp_id = temp_response.id;

        info!("Lifecycle: Starting temp install container {}", temp_id);

        // Start temp container
        self.docker.start_container::<String>(&temp_id, None).await
            .map_err(|e| format!("Failed to start temp container: {}", e))?;

        // 8. Wait for install to complete
        let max_wait = 600;
        let mut waited = 0;
        let mut install_success = false;
        let mut install_timed_out = false;

        info!("Lifecycle: Waiting for temp install container {} to complete", temp_id);

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            waited += 2;

            if waited % 10 == 0 {
                info!("Lifecycle: Still waiting for install... {}s elapsed", waited);
            }

            if waited > max_wait {
                error!("Lifecycle: Reinstall timed out for {}", uuid);
                install_timed_out = true;
                let _ = self.docker.remove_container(&temp_id, Some(bollard::container::RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                })).await;
                break; // Continue to create final container instead of returning error
            }

            match self.docker.inspect_container(&temp_id, None).await {
                Ok(info) => {
                    if let Some(state) = info.state {
                        let is_running = state.running.unwrap_or(false);
                        if !is_running {
                            let exit_code = state.exit_code.unwrap_or(-1);
                            info!("Lifecycle: Reinstall finished with exit code {}", exit_code);
                            install_success = exit_code == 0;
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("Lifecycle: Failed to inspect temp container: {}", e);
                    break;
                }
            }
        }

        // 9. Remove temp container (if not already removed due to timeout)
        if !install_timed_out {
            info!("Lifecycle: Removing temp install container {}", temp_id);
            let _ = self.docker.remove_container(&temp_id, Some(bollard::container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            })).await;
        }

        // Track install result for final state
        let install_failed = install_timed_out || !install_success;
        if install_failed {
            warn!("Lifecycle: Reinstall {} for {}, but continuing to create final container", 
                if install_timed_out { "timed out" } else { "failed" }, uuid);
        }

        // 10. Write startup entrypoint
        let startup_cmd = tracker.startup_command.as_ref()
            .map(|cmd| cmd.join(" "))
            .unwrap_or_else(|| "sleep infinity".to_string());
        let entrypoint_path = format!("{}/entrypoint.sh", data_path);
        tokio::fs::write(&entrypoint_path, &startup_cmd).await
            .map_err(|e| format!("Failed to write entrypoint: {}", e))?;
        
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&entrypoint_path, std::fs::Permissions::from_mode(0o755));
        }

        // 11. Build port config from tracker's allocated_ports
        let ports_map: HashMap<String, String> = tracker.allocated_ports
            .iter()
            .map(|p| (p.container_port.clone(), p.host_port.clone()))
            .collect();

        // 12. Create final container
        info!("Lifecycle: Creating final container for {} after reinstall", uuid);
        let create_req = crate::models::CreateContainerRequest {
            image: tracker.image.clone(),
            name: Some(tracker.name.clone()),
            description: tracker.description.clone(),
            startup_command: Some(vec!["/bin/sh".to_string(), "/data/entrypoint.sh".to_string()]),
            env: tracker.env.clone(),
            ports: if ports_map.is_empty() { None } else { Some(ports_map) },
            volumes: None,
            command: None,
            working_dir: None,
            restart_policy: None,
            custom_uuid: Some(uuid.to_string()),
            limits: Some(tracker.limits.clone()),
            install_content: None,
            update_content: tracker.update_content.clone(),
        };

        match manager.create_with_networking(create_req, &self.network, uuid, &self.volumes_path).await {
            Ok((new_container_id, allocations)) => {
                info!("Lifecycle: Final container created after reinstall: {}", new_container_id);

                // 13. Determine final state based on install result
                let final_state = if install_failed {
                    "install_failed"
                } else {
                    "stopped" // Container is stopped, ready to start
                };

                // 14. Update state manager with final state
                self.state_manager.update_container_state(uuid, final_state).await.ok();
                self.state_manager.update_container_id(uuid, &new_container_id).await.ok();

                // 15. Update tracker
                let mut updated_tracker = tracker.clone();
                updated_tracker.container_id = new_container_id.clone();
                updated_tracker.allocated_ports = allocations;
                updated_tracker.status = final_state.to_string();
                self.container_tracker.save_container(&updated_tracker).await.ok();

                self.unlock_container(uuid).await;
                
                if install_failed {
                    info!("Lifecycle: Reinstall for {} completed with failures - container is available for retry", uuid);
                } else {
                    info!("Lifecycle: Reinstall complete for {}, new ID: {}", uuid, new_container_id);
                }
                
                Ok(new_container_id)
            }
            Err(e) => {
                error!("Lifecycle: Failed to create final container after reinstall: {}", e);
                self.state_manager.update_container_state(uuid, "failed").await.ok();
                self.unlock_container(uuid).await;
                Err(format!("Failed to create container: {}", e))
            }
        }
    }
}
