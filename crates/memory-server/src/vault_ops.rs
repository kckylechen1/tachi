// vault_ops.rs — MCP tool handlers for Tachi Vault

use super::*;
use crate::vault_crypto as crypto;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::Utc;
use memory_core::vault::{VaultConfig, VaultEntry, VaultKeyRotation};
use serde_json::json;

fn default_secret_type() -> String {
    "api_key".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VaultInitParams {
    /// Master password for the vault
    pub password: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VaultUnlockParams {
    /// Master password for the vault
    pub password: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VaultSetParams {
    /// Secret name (e.g., "OPENAI_API_KEY")
    pub name: String,
    /// Secret value (will be encrypted)
    pub value: String,
    /// Secret type: api_key, oauth_token, json_blob, cookie, other
    #[serde(default = "default_secret_type")]
    pub secret_type: String,
    /// Optional human-readable description
    #[serde(default)]
    pub description: String,
    /// If true and secret name ends with _N pattern, set up key rotation
    #[serde(default)]
    pub enable_rotation: bool,
    /// Rotation strategy: round_robin, random, least_recently_used (default: round_robin)
    #[serde(default)]
    pub rotation_strategy: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VaultGetParams {
    /// Secret name to retrieve
    pub name: String,
    /// If true and this is a rotation prefix, auto-select next key
    #[serde(default)]
    pub auto_rotate: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VaultListParams {
    /// Optional filter by secret type
    #[serde(default)]
    pub secret_type: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VaultRemoveParams {
    /// Secret name to remove
    pub name: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VaultSetupRotationParams {
    /// Key prefix (e.g., "GEMINI_API_KEY" for GEMINI_API_KEY_1, GEMINI_API_KEY_2)
    pub prefix: String,
    /// Total number of keys in rotation
    pub total_keys: i64,
    /// Rotation strategy: round_robin, random, least_recently_used
    #[serde(default = "default_rotation_strategy")]
    pub strategy: String,
}

fn default_rotation_strategy() -> String {
    "round_robin".to_string()
}

fn normalize_rotation_strategy(s: &str) -> String {
    match s.to_ascii_lowercase().as_str() {
        "round_robin" | "round-robin" => "round_robin".to_string(),
        "random" => "random".to_string(),
        "least_recently_used" | "lru" | "least-recently-used" => "least_recently_used".to_string(),
        _ => "round_robin".to_string(),
    }
}

/// Check if vault is unlocked and return the key.
fn get_vault_key(server: &MemoryServer) -> Result<std::sync::RwLockReadGuard<'_, Option<[u8; 32]>>, String> {
    let key_guard = server
        .vault_key
        .read()
        .map_err(|e| format!("Failed to lock vault key: {e}"))?;
    if key_guard.is_none() {
        return Err("Vault is locked. Call vault_unlock first.".into());
    }
    Ok(key_guard)
}

/// Check if vault is initialized.
fn is_vault_initialized(server: &MemoryServer) -> Result<bool, String> {
    server
        .with_global_store(|store| store.vault_get_config().map_err(|e| e.to_string()).map_err(|e| e.to_string()))
        .map(|opt| opt.is_some())
}

pub(super) async fn handle_vault_init(
    server: &MemoryServer,
    params: VaultInitParams,
) -> Result<String, String> {
    // Check if already initialized
    if is_vault_initialized(server)? {
        return Err("Vault already initialized. Use vault_unlock to unlock.".into());
    }

    // Generate salt
    let salt = crypto::generate_salt();
    let salt_b64 = B64.encode(&salt);

    // Derive key
    let key = crypto::derive_key(&params.password, &salt)?;

    // Create verifier
    let verifier = crypto::create_verifier(&key)?;

    // Create config
    let now = Utc::now().to_rfc3339();
    let config = VaultConfig {
        salt: salt_b64,
        verifier,
        kdf_algorithm: "argon2id".to_string(),
        kdf_params: r#"{"m":65536,"t":3,"p":4}"#.to_string(),
        cipher: "aes-256-gcm".to_string(),
        created_at: now.clone(),
        updated_at: now,
    };

    // Save config
    server
        .with_global_store(|store| store.vault_set_config(&config).map_err(|e| e.to_string()).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to save vault config: {e}"))?;

    // Cache key
    let mut key_guard = server
        .vault_key
        .write()
        .map_err(|e| format!("Failed to lock vault key: {e}"))?;
    *key_guard = Some(key);

    let resp = json!({
        "initialized": true,
        "locked": false,
        "message": "Vault initialized and unlocked"
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_unlock(
    server: &MemoryServer,
    params: VaultUnlockParams,
) -> Result<String, String> {
    // Load config
    let config = server
        .with_global_store(|store| store.vault_get_config().map_err(|e| e.to_string()).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to load vault config: {e}"))?
        .ok_or("Vault not initialized. Call vault_init first.")?;

    // Decode salt
    let salt = base64::engine::general_purpose::STANDARD
        .decode(&config.salt)
        .map_err(|e| format!("Invalid salt in vault config: {e}"))?;

    // Derive key
    let key = crypto::derive_key(&params.password, &salt)?;

    // Verify password
    if !crypto::verify_password(&key, &config.verifier)? {
        return Err("Wrong password".into());
    }

    // Cache key
    let mut key_guard = server
        .vault_key
        .write()
        .map_err(|e| format!("Failed to lock vault key: {e}"))?;
    *key_guard = Some(key);

    let resp = json!({
        "unlocked": true
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_lock(server: &MemoryServer) -> Result<String, String> {
    let mut key_guard = server
        .vault_key
        .write()
        .map_err(|e| format!("Failed to lock vault key: {e}"))?;
    *key_guard = None;

    let resp = json!({
        "locked": true
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_set(
    server: &MemoryServer,
    params: VaultSetParams,
) -> Result<String, String> {
    // Validate name
    crypto::validate_secret_name(&params.name)?;

    // Check vault is unlocked
    let key_guard = get_vault_key(server)?;
    let key = key_guard.as_ref().unwrap();

    // Validate secret type
    let secret_type = match params.secret_type.to_ascii_lowercase().as_str() {
        "api_key" => "api_key",
        "oauth_token" | "oauth" => "oauth_token",
        "json_blob" | "json" => "json_blob",
        "cookie" => "cookie",
        _ => "other",
    };

    // Encrypt value
    let (encrypted_value, nonce) = crypto::encrypt(key, params.value.as_bytes())?;

    // Check if this is a new entry
    let is_new = !server
        .with_global_store(|store| store.vault_entry_exists(&params.name).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to check existing entry: {e}"))?;

    // Create entry
    let now = Utc::now().to_rfc3339();
    let entry = VaultEntry {
        name: params.name.clone(),
        encrypted_value,
        nonce,
        secret_type: secret_type.to_string(),
        description: params.description,
        created_at: if is_new { now.clone() } else { String::new() },
        updated_at: now,
        accessed_at: String::new(),
        access_count: 0,
    };

    // Save entry
    server
        .with_global_store(|store| store.vault_upsert_entry(&entry).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to save secret: {e}"))?;

    // Handle rotation setup if requested
    if params.enable_rotation {
        // Check if name matches pattern like KEY_1, KEY_2
        if let Some(pos) = params.name.rfind('_') {
            let suffix = &params.name[pos + 1..];
            if suffix.parse::<u32>().is_ok() {
                let prefix = &params.name[..pos];
                let strategy = normalize_rotation_strategy(
                    &params.rotation_strategy.unwrap_or_else(|| "round_robin".to_string()));
                
                // Count existing keys with this prefix
                let all_entries = server
                    .with_global_store(|store| store.vault_list_entries().map_err(|e| e.to_string()))
                    .map_err(|e| format!("Failed to list entries: {e}"))?;
                
                let matching_keys: Vec<_> = all_entries
                    .iter()
                    .filter(|e| {
                        if let Some(p) = e.name.rfind('_') {
                            let suf = &e.name[p + 1..];
                            e.name[..p] == *prefix && suf.parse::<u32>().is_ok()
                        } else {
                            false
                        }
                    })
                    .collect();
                
                let total_keys = matching_keys.len() as i64;
                
                // Update or create rotation record
                let rotation = VaultKeyRotation {
                    prefix: prefix.to_string(),
                    current_index: 1,
                    total_keys,
                    rotation_strategy: strategy,
                    created_at: Utc::now().to_rfc3339(),
                    updated_at: Utc::now().to_rfc3339(),
                };
                
                server
                    .with_global_store(|store| store.vault_set_rotation(&rotation).map_err(|e| e.to_string()))
                    .map_err(|e| format!("Failed to save rotation config: {e}"))?;
            }
        }
    }

    let resp = json!({
        "stored": true,
        "name": params.name,
        "secret_type": secret_type,
        "created": is_new
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_get(
    server: &MemoryServer,
    params: VaultGetParams,
) -> Result<String, String> {
    // Check vault is unlocked
    let key_guard = get_vault_key(server)?;
    let key = key_guard.as_ref().unwrap();

    let target_name = if params.auto_rotate {
        // Check if this is a rotation prefix
        if let Some(rotation) = server
            .with_global_store(|store| store.vault_get_rotation(&params.name).map_err(|e| e.to_string()))
            .map_err(|e| format!("Failed to check rotation: {e}"))?
        {
            // Get all keys for this prefix
            let all_entries = server
                .with_global_store(|store| store.vault_list_entries().map_err(|e| e.to_string()))
                .map_err(|e| format!("Failed to list entries: {e}"))?;
            
            let mut matching_keys: Vec<VaultEntry> = all_entries
                .into_iter()
                .filter(|e| {
                    if let Some(p) = e.name.rfind('_') {
                        let suf = &e.name[p + 1..];
                        e.name[..p] == rotation.prefix && suf.parse::<u32>().is_ok()
                    } else {
                        false
                    }
                })
                .collect();
            
            if matching_keys.is_empty() {
                return Err(format!("No keys found for rotation prefix '{}'", params.name));
            }

            // Select key based on rotation strategy
            let selected_key = match rotation.rotation_strategy.as_str() {
                "round_robin" => {
                    let idx = (rotation.current_index as usize - 1) % matching_keys.len();
                    
                    // Update current_index
                    let new_rotation = VaultKeyRotation {
                        current_index: (rotation.current_index % rotation.total_keys) + 1,
                        updated_at: Utc::now().to_rfc3339(),
                        ..rotation
                    };
                    server
                        .with_global_store(|store| store.vault_set_rotation(&new_rotation).map_err(|e| e.to_string()))
                        .map_err(|e| format!("Failed to update rotation: {e}"))?;
                    
                    matching_keys.get(idx).cloned()
                }
                "random" => {
                    use rand::Rng;
                    let idx = rand::thread_rng().gen_range(0..matching_keys.len());
                    matching_keys.get(idx).cloned()
                }
                "least_recently_used" => {
                    // Sort by access_count (ascending)
                    matching_keys.sort_by_key(|k| k.access_count);
                    matching_keys.first().cloned()
                }
                _ => matching_keys.first().cloned(),
            };

            selected_key.map(|k| k.name)
        } else {
            Some(params.name.clone())
        }
    } else {
        Some(params.name.clone())
    };

    let target_name = target_name.ok_or("No key selected")?;

    // Get entry
    let entry = server
        .with_global_store(|store| store.vault_get_entry(&target_name).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to get secret: {e}"))?
        .ok_or_else(|| format!("Secret not found: {}", target_name))?;

    // Decrypt value
    let decrypted = crypto::decrypt(key, &entry.encrypted_value, &entry.nonce)?;
    let value = String::from_utf8(decrypted)
        .map_err(|e| format!("Decrypted value is not valid UTF-8: {e}"))?;

    // Update access stats
    server
        .with_global_store(|store| store.vault_touch_entry(&target_name).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to update access stats: {e}"))?;

    let resp = json!({
        "name": entry.name,
        "value": value,
        "secret_type": entry.secret_type,
        "description": entry.description,
        "access_count": entry.access_count + 1,
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_list(
    server: &MemoryServer,
    params: VaultListParams,
) -> Result<String, String> {
    // Check vault is initialized
    if !is_vault_initialized(server)? {
        return Err("Vault not initialized. Call vault_init first.".into());
    }

    // List entries (no need to be unlocked - only metadata)
    let entries = if let Some(ref secret_type) = params.secret_type {
        server
            .with_global_store(|store| store.vault_list_entries_by_type(secret_type).map_err(|e| e.to_string()))
    } else {
        server
            .with_global_store(|store| store.vault_list_entries().map_err(|e| e.to_string()))
    }
    .map_err(|e| format!("Failed to list secrets: {e}"))?;

    let payload: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| {
            json!({
                "name": e.name,
                "secret_type": e.secret_type,
                "description": e.description,
                "created_at": e.created_at,
                "updated_at": e.updated_at,
                "access_count": e.access_count,
            })
        })
        .collect();

    let resp = json!({
        "count": payload.len(),
        "secrets": payload,
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_remove(
    server: &MemoryServer,
    params: VaultRemoveParams,
) -> Result<String, String> {
    // Check vault is unlocked
    let _key_guard = get_vault_key(server)?;

    let removed = server
        .with_global_store(|store| store.vault_delete_entry(&params.name).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to remove secret: {e}"))?;

    let resp = if removed {
        json!({
            "removed": true,
            "name": params.name
        })
    } else {
        json!({
            "removed": false,
            "error": format!("Secret not found: {}", params.name)
        })
    };
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_status(server: &MemoryServer) -> Result<String, String> {
    let initialized = is_vault_initialized(server)?;
    let locked = server
        .vault_key
        .read()
        .map_err(|e| format!("Failed to lock vault key: {e}"))?
        .is_none();
    let entry_count = if initialized {
        server
            .with_global_store(|store| store.vault_count_entries().map_err(|e| e.to_string()))
            .unwrap_or(0)
    } else {
        0
    };

    let resp = json!({
        "initialized": initialized,
        "locked": locked,
        "entry_count": entry_count,
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vault_setup_rotation(
    server: &MemoryServer,
    params: VaultSetupRotationParams,
) -> Result<String, String> {
    // Check vault is unlocked
    let _key_guard = get_vault_key(server)?;

    if params.total_keys < 2 {
        return Err("Rotation requires at least 2 keys".into());
    }

    let strategy = normalize_rotation_strategy(&params.strategy);

    // Verify keys exist
    let all_entries = server
        .with_global_store(|store| store.vault_list_entries().map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to list entries: {e}"))?;

    let mut found_keys = 0;
    for i in 1..=params.total_keys {
        let key_name = format!("{}_{}", params.prefix, i);
        if all_entries.iter().any(|e| e.name == key_name) {
            found_keys += 1;
        }
    }

    if found_keys < params.total_keys {
        return Err(format!(
            "Expected {} keys for prefix '{}', found {}. Please set all keys first.",
            params.total_keys, params.prefix, found_keys
        ));
    }

    // Create rotation record
    let now = Utc::now().to_rfc3339();
    let rotation = VaultKeyRotation {
        prefix: params.prefix.clone(),
        current_index: 1,
        total_keys: params.total_keys,
        rotation_strategy: strategy.clone(),
        created_at: now.clone(),
        updated_at: now,
    };

    server
        .with_global_store(|store| store.vault_set_rotation(&rotation).map_err(|e| e.to_string()))
        .map_err(|e| format!("Failed to save rotation config: {e}"))?;

    let resp = json!({
        "setup": true,
        "prefix": params.prefix,
        "total_keys": params.total_keys,
        "strategy": strategy,
    });
    serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))
}
