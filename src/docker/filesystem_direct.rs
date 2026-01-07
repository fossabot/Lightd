use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::fs;
use std::io::{Read, Write};
use tracing::info;

use chrono::{DateTime, Utc};

fn system_time_to_rfc3339_utc(t: std::time::SystemTime) -> Option<String> {
    let dt: DateTime<Utc> = t.into();
    Some(dt.to_rfc3339())
}

fn rwx_triplet(bits: u32) -> String {
    let r = if (bits & 0b100) != 0 { 'r' } else { '-' };
    let w = if (bits & 0b010) != 0 { 'w' } else { '-' };
    let x = if (bits & 0b001) != 0 { 'x' } else { '-' };
    format!("{}{}{}", r, w, x)
}

fn mode_to_string(mode: u32, is_dir: bool, is_symlink: bool) -> String {
    let kind = if is_symlink {
        'l'
    } else if is_dir {
        'd'
    } else {
        '-'
    };

    let perms = mode & 0o777;
    let user = rwx_triplet((perms >> 6) & 0b111);
    let group = rwx_triplet((perms >> 3) & 0b111);
    let other = rwx_triplet(perms & 0b111);
    format!("{}{}{}{}", kind, user, group, other)
}

fn sniff_is_text_file(path: &Path) -> Option<bool> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = [0u8; 8192];
    let n = file.read(&mut buf).ok()?;
    let slice = &buf[..n];

    // NUL bytes are a strong binary signal.
    if slice.iter().any(|b| *b == 0) {
        return Some(false);
    }

    // Treat valid UTF-8 (even if empty) as text.
    Some(std::str::from_utf8(slice).is_ok())
}

fn detect_mimetype(path: &Path, file_name: &str, is_directory: bool, is_symlink: bool, is_file: bool) -> String {
    if is_directory {
        return "inode/directory".to_string();
    }
    if is_symlink {
        return "inode/symlink".to_string();
    }

    // If it's a regular file, prefer content sniffing so extensions like `.ts` don't get misclassified
    // as MPEG transport stream. For text-like files, return more specific code mimetypes where helpful.
    if is_file {
        if let Some(true) = sniff_is_text_file(path) {
            let lower = file_name.to_lowercase();
            if lower.ends_with(".ts") || lower.ends_with(".tsx") {
                return "application/typescript".to_string();
            }
            if lower.ends_with(".js") || lower.ends_with(".jsx") {
                return "application/javascript".to_string();
            }
            if lower.ends_with(".json") {
                return "application/json".to_string();
            }
            if lower.ends_with(".yml") || lower.ends_with(".yaml") {
                return "text/yaml".to_string();
            }
            if lower.ends_with(".toml") {
                return "text/plain".to_string();
            }
            if lower.ends_with(".md") {
                return "text/markdown".to_string();
            }
            if lower.ends_with(".rs") {
                return "text/x-rust".to_string();
            }
            if lower.ends_with(".html") {
                return "text/html".to_string();
            }
            if lower.ends_with(".css") {
                return "text/css".to_string();
            }
            if lower.ends_with(".xml") {
                return "application/xml".to_string();
            }
            if lower.ends_with(".env") || lower.ends_with(".ini") || lower.ends_with(".cfg") {
                return "text/plain".to_string();
            }
            return "text/plain".to_string();
        }

        return mime_guess::from_path(file_name)
            .first_or_octet_stream()
            .essence_str()
            .to_string();
    }

    "application/octet-stream".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    #[serde(rename = "isDirectory")]
    pub is_directory: bool,
    pub size: u64,
    #[serde(rename = "modifiedAt")]
    pub modified: Option<String>,

    // Pterodactyl-like fields
    pub mode: String,
    pub mode_bits: String,
    pub is_file: bool,
    pub is_symlink: bool,
    pub mimetype: String,
    pub created_at: String,
    pub modified_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub path: String,
    #[serde(rename = "items")]
    pub files: Vec<FileInfo>,
    #[serde(rename = "totalFiles")]
    pub total_files: usize,
    #[serde(rename = "totalDirectories")]
    pub total_directories: usize,
}

pub struct FilesystemManagerDirect {
    volumes_base_path: String,
}

impl FilesystemManagerDirect {
    pub fn new(volumes_base_path: String) -> Self {
        Self { volumes_base_path }
    }

    /// Get the host path for a container's volume
    /// /home/container is mounted to {volumes_path}/{container_id} (user data)
    /// /app/data is mounted to {volumes_path}/{container_id}_data (entrypoint, read-only)
    /// We only work with /home/container mount
    fn get_volume_path(&self, container_id: &str, container_path: &str) -> anyhow::Result<PathBuf> {
        // Container's /home/container is stored at {volumes_path}/{container_id} (NOT _data!)
        let volume_root = PathBuf::from(&self.volumes_base_path)
            .join(container_id);
        
        // Remove /home/container prefix if present to get relative path
        let relative_path = container_path
            .trim_start_matches("/home/container")
            .trim_start_matches('/');
        
        let full_path = if relative_path.is_empty() {
            volume_root.clone()
        } else {
            volume_root.join(relative_path)
        };
        
        // Security check: ensure the resolved path is within the volume root
        // For paths that don't exist yet, we need to check the parent directory
        let path_to_check = if full_path.exists() {
            full_path.clone()
        } else {
            // For non-existent paths, check the deepest existing parent
            let mut current = full_path.clone();
            while !current.exists() && current.parent().is_some() {
                current = current.parent().unwrap().to_path_buf();
            }
            current
        };
        
        // Canonicalize both paths for comparison
        let canonical = path_to_check.canonicalize().unwrap_or_else(|_| path_to_check.clone());
        let canonical_root = volume_root.canonicalize().unwrap_or(volume_root.clone());
        
        if !canonical.starts_with(&canonical_root) {
            return Err(anyhow::anyhow!("Path traversal attempt detected"));
        }
        
        Ok(full_path)
    }

    /// List files and directories
    pub fn list_directory(&self, container_id: &str, path: &str) -> anyhow::Result<DirectoryListing> {
        info!("Listing directory {} in container {}", path, container_id);
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        if !host_path.exists() {
            return Err(anyhow::anyhow!("Path does not exist"));
        }
        
        if !host_path.is_dir() {
            return Err(anyhow::anyhow!("Path is not a directory"));
        }
        
        let mut files = Vec::new();
        let mut total_files = 0;
        let mut total_directories = 0;
        
        let entries = fs::read_dir(&host_path)?;
        
        for entry in entries {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;
            let file_type = entry.file_type()?;
            let file_name = entry.file_name().to_string_lossy().to_string();
            
            // Skip hidden files starting with .
            if file_name.starts_with('.') {
                continue;
            }
            
            let is_directory = metadata.is_dir();
            let is_file = metadata.is_file();
            let is_symlink = file_type.is_symlink();

            // Match common Pterodactyl behavior: dirs report their actual metadata size (often 4096)
            let size = metadata.len();

            #[cfg(unix)]
            let unix_mode: u32 = {
                use std::os::unix::fs::MetadataExt;
                metadata.mode()
            };
            #[cfg(not(unix))]
            let unix_mode: u32 = 0;

            let mode_bits_num = unix_mode & 0o777;
            let mode_bits = format!("{:03o}", mode_bits_num);
            let mode = mode_to_string(unix_mode, is_directory, is_symlink);

            let modified_at = metadata
                .modified()
                .ok()
                .and_then(system_time_to_rfc3339_utc)
                .unwrap_or_else(|| Utc::now().to_rfc3339());
            let created_at = metadata
                .created()
                .ok()
                .and_then(system_time_to_rfc3339_utc)
                .unwrap_or_else(|| modified_at.clone());

            // Preserve the existing frontend field too
            let modified = Some(modified_at.clone());

            let mimetype = detect_mimetype(&entry_path, &file_name, is_directory, is_symlink, is_file);
            
            // Normalize path to always show /home/container prefix
            // Strip the /home/container prefix from input path to get relative path
            let relative_input = path
                .trim_start_matches("/home/container")
                .trim_start_matches('/')
                .trim_end_matches('/');
            
            let normalized_path = if relative_input.is_empty() {
                // We're at root, so files are directly under /home/container
                format!("/home/container/{}", file_name)
            } else {
                // We're in a subdirectory
                format!("/home/container/{}/{}", relative_input, file_name)
            };
            
            if is_directory {
                total_directories += 1;
            } else {
                total_files += 1;
            }
            
            files.push(FileInfo {
                name: file_name,
                path: normalized_path,
                is_directory,
                size,
                modified,

                mode,
                mode_bits,
                is_file,
                is_symlink,
                mimetype,
                created_at,
                modified_at,
            });
        }
        
        // Sort: directories first, then alphabetically
        files.sort_by(|a, b| {
            match (a.is_directory, b.is_directory) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            }
        });
        
        // Normalize the returned path to always show /home/container
        // Ensure the path always starts with /home/container
        let normalized_list_path = if path.is_empty() || path == "/" {
            "/home/container".to_string()
        } else if path.starts_with("/home/container") {
            path.to_string()
        } else {
            format!("/home/container/{}", path.trim_start_matches('/'))
        };
        
        Ok(DirectoryListing {
            path: normalized_list_path,
            files,
            total_files,
            total_directories,
        })
    }

    /// Read file content
    pub fn get_file_content(&self, container_id: &str, path: &str) -> anyhow::Result<String> {
        info!("Reading file {} in container {}", path, container_id);
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        if !host_path.exists() {
            return Err(anyhow::anyhow!("File does not exist"));
        }
        
        if !host_path.is_file() {
            return Err(anyhow::anyhow!("Path is not a file"));
        }
        
        let mut file = fs::File::open(&host_path)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        
        Ok(content)
    }

    /// Write file content
    pub fn write_file(&self, container_id: &str, path: &str, content: &str) -> anyhow::Result<()> {
        info!("Writing file {} in container {}", path, container_id);
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        // Create parent directories if they don't exist
        if let Some(parent) = host_path.parent() {
            fs::create_dir_all(parent)?;
        }
        
        let mut file = fs::File::create(&host_path)?;
        file.write_all(content.as_bytes())?;
        
        Ok(())
    }

    /// Write file content from bytes (for binary uploads)
    pub fn write_file_bytes(&self, container_id: &str, path: &str, content: &[u8]) -> anyhow::Result<()> {
        info!("Writing file (bytes) {} in container {}", path, container_id);
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        // Create parent directories if they don't exist
        if let Some(parent) = host_path.parent() {
            fs::create_dir_all(parent)?;
        }
        
        let mut file = fs::File::create(&host_path)?;
        file.write_all(content)?;
        
        Ok(())
    }

    /// Copy a file (byte-safe).
    ///
    /// This intentionally copies bytes on the host volume instead of reading as UTF-8.
    pub fn copy_file(&self, container_id: &str, source_path: &str, dest_path: &str) -> anyhow::Result<()> {
        info!("Copying {} to {} in container {}", source_path, dest_path, container_id);

        let source_host_path = self.get_volume_path(container_id, source_path)?;
        let dest_host_path = self.get_volume_path(container_id, dest_path)?;

        if !source_host_path.exists() {
            return Err(anyhow::anyhow!("Source path does not exist"));
        }

        if !source_host_path.is_file() {
            return Err(anyhow::anyhow!("Source path is not a file"));
        }

        if let Some(parent) = dest_host_path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::copy(&source_host_path, &dest_host_path)?;
        Ok(())
    }

    /// Create directory
    pub fn create_directory(&self, container_id: &str, path: &str) -> anyhow::Result<()> {
        info!("Creating directory {} in container {}", path, container_id);
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        fs::create_dir_all(&host_path)?;
        
        Ok(())
    }

    /// Delete file or directory
    pub fn delete(&self, container_id: &str, path: &str) -> anyhow::Result<()> {
        info!("Deleting {} in container {}", path, container_id);
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        if !host_path.exists() {
            return Err(anyhow::anyhow!("Path does not exist"));
        }
        
        if host_path.is_dir() {
            fs::remove_dir_all(&host_path)?;
        } else {
            fs::remove_file(&host_path)?;
        }
        
        Ok(())
    }

    /// Rename/move file
    pub fn rename(&self, container_id: &str, old_path: &str, new_path: &str) -> anyhow::Result<()> {
        info!("Renaming {} to {} in container {}", old_path, new_path, container_id);
        
        let old_host_path = self.get_volume_path(container_id, old_path)?;
        let new_host_path = self.get_volume_path(container_id, new_path)?;
        
        if !old_host_path.exists() {
            return Err(anyhow::anyhow!("Source path does not exist"));
        }
        
        // Create parent directories for destination if needed
        if let Some(parent) = new_host_path.parent() {
            fs::create_dir_all(parent)?;
        }
        
        fs::rename(&old_host_path, &new_host_path)?;
        
        Ok(())
    }

    /// Create tar archive from multiple sources
    pub fn create_archive(&self, container_id: &str, source_paths: &[String], archive_path: &str, compression: Option<&str>) -> anyhow::Result<String> {
        if source_paths.is_empty() {
            return Err(anyhow::anyhow!("No source paths provided"));
        }
        
        let archive_host = self.get_volume_path(container_id, archive_path)?;
        
        // Validate all source paths exist
        let source_hosts: Vec<PathBuf> = source_paths.iter()
            .map(|p| self.get_volume_path(container_id, p))
            .collect::<Result<Vec<_>, _>>()?;
        
        for (idx, source_host) in source_hosts.iter().enumerate() {
            if !source_host.exists() {
                return Err(anyhow::anyhow!("Source path does not exist: {}", source_paths[idx]));
            }
        }
        
        // Create parent directory for archive
        if let Some(parent) = archive_host.parent() {
            fs::create_dir_all(parent)?;
        }
        
        let file = fs::File::create(&archive_host)?;
        
        // Apply compression wrapper based on type
        let encoder: Box<dyn Write> = match compression {
            Some("gzip") => {
                use flate2::write::GzEncoder;
                use flate2::Compression;
                Box::new(GzEncoder::new(file, Compression::default()))
            }
            Some("bzip2") => {
                use bzip2::write::BzEncoder;
                use bzip2::Compression;
                Box::new(BzEncoder::new(file, Compression::default()))
            }
            _ => Box::new(file),
        };
        
        let mut tar = tar::Builder::new(encoder);
        
        // Add each source to the archive
        for source_host in source_hosts {
            if source_host.is_dir() {
                let dir_name = source_host.file_name()
                    .ok_or_else(|| anyhow::anyhow!("Invalid directory name"))?;
                tar.append_dir_all(dir_name, &source_host)?;
            } else {
                let mut file = fs::File::open(&source_host)?;
                let name = source_host.file_name()
                    .ok_or_else(|| anyhow::anyhow!("Invalid source file name"))?;
                tar.append_file(name, &mut file)?;
            }
        }
        
        tar.finish()?;
        
        Ok(format!("Archive created: {} ({} items)", archive_path, source_paths.len()))
    }

    /// Extract tar archive
    pub fn extract_archive(&self, container_id: &str, archive_path: &str, dest_path: &str) -> anyhow::Result<String> {
        let archive_host = self.get_volume_path(container_id, archive_path)?;
        let dest_host = self.get_volume_path(container_id, dest_path)?;
        
        if !archive_host.exists() {
            return Err(anyhow::anyhow!("Archive does not exist"));
        }
        
        // Create destination directory
        fs::create_dir_all(&dest_host)?;
        
        let file = fs::File::open(&archive_host)?;
        
        // Try to detect compression by file extension
        let archive_name = archive_path.to_lowercase();
        
        if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            use flate2::read::GzDecoder;
            let decoder = GzDecoder::new(file);
            let mut archive = tar::Archive::new(decoder);
            archive.unpack(&dest_host)?;
        } else if archive_name.ends_with(".tar.bz2") || archive_name.ends_with(".tbz2") {
            use bzip2::read::BzDecoder;
            let decoder = BzDecoder::new(file);
            let mut archive = tar::Archive::new(decoder);
            archive.unpack(&dest_host)?;
        } else {
            // Assume uncompressed tar
            let mut archive = tar::Archive::new(file);
            archive.unpack(&dest_host)?;
        }
        
        Ok(format!("Archive extracted to: {}", dest_path))
    }

    /// Create zip archive from multiple sources
    pub fn create_zip(&self, container_id: &str, source_paths: &[String], zip_path: &str) -> anyhow::Result<String> {
        if source_paths.is_empty() {
            return Err(anyhow::anyhow!("No source paths provided"));
        }
        
        let zip_host = self.get_volume_path(container_id, zip_path)?;
        
        // Validate all source paths exist
        let source_hosts: Vec<PathBuf> = source_paths.iter()
            .map(|p| self.get_volume_path(container_id, p))
            .collect::<Result<Vec<_>, _>>()?;
        
        for (idx, source_host) in source_hosts.iter().enumerate() {
            if !source_host.exists() {
                return Err(anyhow::anyhow!("Source path does not exist: {}", source_paths[idx]));
            }
        }
        
        // Create parent directory for zip
        if let Some(parent) = zip_host.parent() {
            fs::create_dir_all(parent)?;
        }
        
        let file = fs::File::create(&zip_host)?;
        let mut zip = zip::ZipWriter::new(file);
        
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        
        // Add each source to the zip
        for source_host in source_hosts {
            if source_host.is_dir() {
                let dir_name = source_host.file_name()
                    .ok_or_else(|| anyhow::anyhow!("Invalid directory name"))?
                    .to_string_lossy();
                zip.add_directory(dir_name.as_ref(), options)?;
                self.zip_dir_with_prefix(&mut zip, &source_host, &source_host, &dir_name, &options)?;
            } else {
                let name = source_host.file_name()
                    .ok_or_else(|| anyhow::anyhow!("Invalid source file name"))?;
                zip.start_file(name.to_string_lossy(), options)?;
                let mut f = fs::File::open(&source_host)?;
                std::io::copy(&mut f, &mut zip)?;
            }
        }
        
        zip.finish()?;
        
        Ok(format!("Zip created: {} ({} items)", zip_path, source_paths.len()))
    }
    
    fn zip_dir_with_prefix(&self, zip: &mut zip::ZipWriter<fs::File>, base: &PathBuf, current: &PathBuf, prefix: &str, options: &zip::write::FileOptions) -> anyhow::Result<()> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(base)
                .map_err(|_| anyhow::anyhow!("Invalid path"))?;
            let full_path = format!("{}/{}", prefix, relative.to_string_lossy());
            
            if path.is_dir() {
                zip.add_directory(&full_path, *options)?;
                self.zip_dir_with_prefix(zip, base, &path, prefix, options)?;
            } else {
                zip.start_file(&full_path, *options)?;
                let mut f = fs::File::open(&path)?;
                std::io::copy(&mut f, zip)?;
            }
        }
        
        Ok(())
    }

    /// Extract zip archive
    pub fn extract_zip(&self, container_id: &str, zip_path: &str, dest_path: &str) -> anyhow::Result<String> {
        let zip_host = self.get_volume_path(container_id, zip_path)?;
        let dest_host = self.get_volume_path(container_id, dest_path)?;
        
        if !zip_host.exists() {
            return Err(anyhow::anyhow!("Zip file does not exist"));
        }
        
        // Create destination directory
        fs::create_dir_all(&dest_host)?;
        
        let file = fs::File::open(&zip_host)?;
        let mut archive = zip::ZipArchive::new(file)?;
        
        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let outpath = dest_host.join(file.name());
            
            if file.is_dir() {
                fs::create_dir_all(&outpath)?;
            } else {
                if let Some(p) = outpath.parent() {
                    fs::create_dir_all(p)?;
                }
                let mut outfile = fs::File::create(&outpath)?;
                std::io::copy(&mut file, &mut outfile)?;
            }
        }
        
        Ok(format!("Zip extracted to: {}", dest_path))
    }

    /// Change file permissions (chmod)
    pub fn chmod(&self, container_id: &str, path: &str, permissions: &str) -> anyhow::Result<()> {
        use std::process::Command;
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        if !host_path.exists() {
            return Err(anyhow::anyhow!("Path does not exist"));
        }
        
        let output = Command::new("chmod")
            .arg(permissions)
            .arg(&host_path)
            .output()?;
        
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Failed to change permissions: {}", error));
        }
        
        Ok(())
    }

    /// Change file ownership (chown)
    pub fn chown(&self, container_id: &str, path: &str, owner: &str, group: Option<&str>) -> anyhow::Result<()> {
        use std::process::Command;
        
        let host_path = self.get_volume_path(container_id, path)?;
        
        if !host_path.exists() {
            return Err(anyhow::anyhow!("Path does not exist"));
        }
        
        let owner_group = if let Some(g) = group {
            format!("{}:{}", owner, g)
        } else {
            owner.to_string()
        };
        
        let output = Command::new("chown")
            .arg(&owner_group)
            .arg(&host_path)
            .output()?;
        
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Failed to change ownership: {}", error));
        }
        
        Ok(())
    }
}
