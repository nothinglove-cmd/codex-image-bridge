use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use toml_edit::{value, DocumentMut, InlineTable, Item, Table, Value as TomlValue};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::{image, install};

pub const IMAGE_MODEL: &str = "gpt-image-2";
const PROVIDER_ID: &str = "comidea";
const PROVIDER_NAME: &str = "Comidea Image Service";
const STATE_VERSION: u32 = 2;
const LEGACY_STATE_VERSION: u32 = 1;
const LEGACY_STATE_FILE_NAME: &str = "model-config-state.json";
const STATE_DIRECTORY_NAME: &str = "model-config-states";
const MODEL_CACHE_FILE_NAME: &str = "models_cache.json";
const MODEL_CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const GLOBAL_STATE_FILE_NAME: &str = ".codex-global-state.json";
const GLOBAL_STATE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const PERSISTED_ATOMS_KEY: &str = "electron-persisted-atom-state";
const MODEL_PICKER_VIEW_KEY: &str = "composer-model-picker-menu-view-v1";
const ADVANCED_MODEL_PICKER_VIEW: &str = "advanced";
const MODEL_CACHE_MARKER_PREFIX: &str = "comidea-codex-image-bridge:";
const STALE_MODEL_CACHE_TIMESTAMP: &str = "1970-01-01T00:00:00Z";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransportMode {
    #[default]
    Auto,
    HttpsSse,
    WebSocket,
}

impl TransportMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（推荐）",
            Self::HttpsSse => "HTTPS/SSE",
            Self::WebSocket => "WebSocket",
        }
    }

    fn supports_websockets(self) -> bool {
        matches!(self, Self::WebSocket)
    }
}

pub struct ModelSettings {
    pub codex_home: PathBuf,
    pub config_path: PathBuf,
    pub auth_path: PathBuf,
    pub provider_id: String,
    pub server_url: String,
    pub api_key: String,
    pub image_model: String,
    pub image_model_enabled: bool,
    pub static_headers: BTreeMap<String, String>,
    pub env_headers: BTreeMap<String, String>,
    pub transport_mode: TransportMode,
    pub inherit_system_proxy: bool,
    pub managed: bool,
    pub revisions: ModelRevisions,
}

impl Drop for ModelSettings {
    fn drop(&mut self) {
        self.api_key.zeroize();
        zeroize_header_values(&mut self.static_headers);
    }
}

pub struct ModelConfiguration {
    pub server_url: String,
    pub api_key: String,
    pub image_model: String,
    pub image_generation_enabled: bool,
    pub static_headers: BTreeMap<String, String>,
    pub env_headers: BTreeMap<String, String>,
    pub transport_mode: TransportMode,
    pub inherit_system_proxy: bool,
}

impl Drop for ModelConfiguration {
    fn drop(&mut self) {
        self.api_key.zeroize();
        zeroize_header_values(&mut self.static_headers);
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionReport {
    pub normalized_url: String,
    pub models_status: Option<u32>,
    pub responses_status: Option<u32>,
    pub model_available: Option<bool>,
    pub usable: bool,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRevisions {
    config: FileRevision,
    auth: FileRevision,
    state: FileRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileRevision {
    existed: bool,
    sha256: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SavedModelSettings {
    pub server_url: String,
    pub image_model: String,
    pub image_model_enabled: bool,
}

#[derive(Clone, Debug)]
struct ModelPaths {
    config: PathBuf,
    auth: PathBuf,
    state: PathBuf,
    legacy_state: PathBuf,
    codex_home: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManagedModelState {
    version: u32,
    config_path: PathBuf,
    auth_path: PathBuf,
    original_config: FileSnapshot,
    original_auth: ProtectedFileSnapshot,
    installed_config_sha256: String,
    installed_auth_sha256: String,
    #[serde(default = "default_image_model")]
    image_model: String,
    #[serde(default)]
    original_model_picker_view: Option<JsonValueSnapshot>,
    #[serde(default)]
    transport_mode: TransportMode,
    #[serde(default = "default_inherit_system_proxy")]
    inherit_system_proxy: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyManagedModelState {
    version: u32,
    config_path: PathBuf,
    auth_path: PathBuf,
    original_config: FileSnapshot,
    original_auth: FileSnapshot,
    installed_config_sha256: String,
    installed_auth_sha256: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileSnapshot {
    existed: bool,
    bytes_base64: Option<String>,
    sha256: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProtectedFileSnapshot {
    existed: bool,
    protected_bytes_base64: Option<String>,
    sha256: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonValueSnapshot {
    existed: bool,
    value: Option<Value>,
}

struct SecretAuth(Map<String, Value>);

impl std::ops::Deref for SecretAuth {
    type Target = Map<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SecretAuth {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for SecretAuth {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            zeroize_json_value(value);
        }
    }
}

pub fn load_settings() -> Result<ModelSettings> {
    #[cfg(windows)]
    let _operation_lock = install::OperationLock::acquire()?;
    let paths = ModelPaths::discover()?;
    migrate_legacy_state(&paths)?;
    load_from_paths(&paths)
}

pub fn save_settings(
    configuration: &ModelConfiguration,
    expected_revisions: &ModelRevisions,
) -> Result<SavedModelSettings> {
    #[cfg(windows)]
    let _operation_lock = install::OperationLock::acquire()?;
    let paths = ModelPaths::discover()?;
    migrate_legacy_state(&paths)?;
    save_to_paths(&paths, configuration, Some(expected_revisions))
}

pub fn restore_managed_config() -> Result<bool> {
    #[cfg(windows)]
    let _operation_lock = install::OperationLock::acquire()?;
    let paths = ModelPaths::discover()?;
    migrate_legacy_state(&paths)?;
    restore_from_paths(&paths)
}

pub fn sync_model_cache() -> Result<bool> {
    #[cfg(windows)]
    let _operation_lock = install::OperationLock::acquire()?;
    let paths = ModelPaths::discover()?;
    migrate_legacy_state(&paths)?;
    sync_model_cache_file(&paths.codex_home.join(MODEL_CACHE_FILE_NAME))
}

pub fn ensure_advanced_model_picker() -> Result<bool> {
    #[cfg(windows)]
    let _operation_lock = install::OperationLock::acquire()?;
    let paths = ModelPaths::discover()?;
    migrate_legacy_state(&paths)?;
    if !paths.state.is_file() {
        return Ok(false);
    }
    let document = read_config(&paths.config)?;
    if !document
        .get("features")
        .and_then(|features| features.get("image_generation"))
        .and_then(toml_edit::Item::as_bool)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let path = paths.codex_home.join(GLOBAL_STATE_FILE_NAME);
    let snapshot = FileSnapshot::capture(&path)?;
    let Some(bytes) = updated_model_picker_state(&snapshot, ModelPickerPreference::Advanced, None)?
    else {
        return Ok(false);
    };
    atomic_write(&path, &bytes)?;
    Ok(true)
}

pub(crate) fn has_managed_configs() -> Result<bool> {
    let fix_root = install::fix_root()?;
    has_managed_configs_in(&fix_root)
}

fn has_managed_configs_in(fix_root: &Path) -> Result<bool> {
    if fix_root.join(LEGACY_STATE_FILE_NAME).is_file() {
        return Ok(true);
    }
    let directory = fix_root.join(STATE_DIRECTORY_NAME);
    if !directory.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|value| value.to_str()) == Some("json") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn test_connection(configuration: &ModelConfiguration) -> Result<ConnectionReport> {
    let server_url = normalize_server_url(&configuration.server_url)?;
    validate_api_key(&configuration.api_key)?;
    let image_model = validate_model_id(&configuration.image_model)?;
    validate_headers(&configuration.static_headers, &configuration.env_headers)?;
    test_connection_platform(
        &server_url,
        &configuration.api_key,
        image_model,
        &configuration.static_headers,
        &configuration.env_headers,
    )
}

pub fn preview_settings(
    configuration: &ModelConfiguration,
    current: &ModelSettings,
) -> Result<String> {
    let server_url = normalize_server_url(&configuration.server_url)?;
    let image_model = validate_model_id(&configuration.image_model)?;
    validate_headers(&configuration.static_headers, &configuration.env_headers)?;
    let previous_server = if current.server_url.is_empty() {
        "未配置"
    } else {
        &current.server_url
    };
    Ok(format!(
        "服务器地址\r\n  {previous_server}\r\n  -> {server_url}\r\n\r\nProvider\r\n  {} -> {PROVIDER_ID}\r\n\r\n图片模型 ID\r\n  {} -> {image_model}\r\n\r\n图片能力\r\n  {} -> {}\r\n\r\n传输模式\r\n  {} -> {}\r\n\r\nWindows 代理继承\r\n  {} -> {}\r\n\r\nHeader\r\n  静态 {} 项，环境变量 {} 项（内容不显示）\r\n\r\nAPI Key\r\n  将更新（内容不显示）\r\n\r\n当前文本模型不会被修改。确认保存吗？",
        current.provider_id,
        current.image_model,
        if current.image_model_enabled {
            "已启用"
        } else {
            "未启用"
        },
        if configuration.image_generation_enabled {
            "已启用"
        } else {
            "未启用"
        },
        current.transport_mode.label(),
        configuration.transport_mode.label(),
        if current.inherit_system_proxy {
            "已开启"
        } else {
            "已关闭"
        },
        if configuration.inherit_system_proxy {
            "已开启"
        } else {
            "已关闭"
        },
        configuration.static_headers.len(),
        configuration.env_headers.len()
    ))
}

impl ModelPaths {
    fn discover() -> Result<Self> {
        let codex_home = normalize_codex_home(&image::codex_home())?;
        let fix_root = install::fix_root()?;
        let state = fix_root
            .join(STATE_DIRECTORY_NAME)
            .join(format!("{}.json", codex_home_key(&codex_home)));
        Ok(Self {
            config: codex_home.join("config.toml"),
            auth: codex_home.join("auth.json"),
            state,
            legacy_state: fix_root.join(LEGACY_STATE_FILE_NAME),
            codex_home,
        })
    }
}

fn load_from_paths(paths: &ModelPaths) -> Result<ModelSettings> {
    let document = read_config(&paths.config)?;
    let auth = read_auth(&paths.auth)?;
    let requested_provider = document
        .get("model_provider")
        .and_then(toml_edit::Item::as_str);
    let providers = document.get("model_providers");
    let requested =
        requested_provider.and_then(|id| providers.and_then(|providers| providers.get(id)));
    let managed = providers.and_then(|providers| providers.get(PROVIDER_ID));
    let (provider_id, provider) = requested
        .map(|provider| (requested_provider.unwrap_or(PROVIDER_ID), Some(provider)))
        .unwrap_or((PROVIDER_ID, managed));
    let server_url = provider
        .and_then(|provider| provider.get("base_url"))
        .and_then(toml_edit::Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let static_headers = provider
        .and_then(|provider| provider.get("http_headers"))
        .map(|item| read_header_map(item, "http_headers"))
        .transpose()?
        .unwrap_or_default();
    let env_headers = provider
        .and_then(|provider| provider.get("env_http_headers"))
        .map(|item| read_header_map(item, "env_http_headers"))
        .transpose()?
        .unwrap_or_default();
    let api_key = auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let image_model_enabled = document
        .get("features")
        .and_then(|features| features.get("image_generation"))
        .and_then(toml_edit::Item::as_bool)
        == Some(true);
    let managed_state = if paths.state.is_file() {
        Some(read_state(&paths.state)?)
    } else {
        None
    };
    let image_model = managed_state
        .as_ref()
        .map(|state| state.image_model.clone())
        .unwrap_or_else(default_image_model);
    let transport_mode = managed_state
        .as_ref()
        .map(|state| state.transport_mode)
        .unwrap_or_else(|| {
            match provider
                .and_then(|provider| provider.get("supports_websockets"))
                .and_then(toml_edit::Item::as_bool)
            {
                Some(true) => TransportMode::WebSocket,
                Some(false) => TransportMode::HttpsSse,
                None => TransportMode::Auto,
            }
        });
    let inherit_system_proxy = managed_state
        .as_ref()
        .map(|state| state.inherit_system_proxy)
        .unwrap_or_else(|| {
            document
                .get("features")
                .and_then(|features| features.get("respect_system_proxy"))
                .and_then(toml_edit::Item::as_bool)
                .unwrap_or_else(default_inherit_system_proxy)
        });

    Ok(ModelSettings {
        codex_home: paths.codex_home.clone(),
        config_path: paths.config.clone(),
        auth_path: paths.auth.clone(),
        provider_id: provider_id.to_owned(),
        server_url,
        api_key,
        image_model,
        image_model_enabled,
        static_headers,
        env_headers,
        transport_mode,
        inherit_system_proxy,
        managed: managed_state.is_some(),
        revisions: ModelRevisions::capture(paths)?,
    })
}

fn save_to_paths(
    paths: &ModelPaths,
    configuration: &ModelConfiguration,
    expected_revisions: Option<&ModelRevisions>,
) -> Result<SavedModelSettings> {
    let server_url = normalize_server_url(&configuration.server_url)?;
    validate_api_key(&configuration.api_key)?;
    let image_model = validate_model_id(&configuration.image_model)?.to_owned();
    validate_headers(&configuration.static_headers, &configuration.env_headers)?;

    let previous_config = FileSnapshot::capture(&paths.config)?;
    let previous_auth = FileSnapshot::capture(&paths.auth)?;
    let previous_state = FileSnapshot::capture(&paths.state)?;
    let model_cache_path = paths.codex_home.join(MODEL_CACHE_FILE_NAME);
    let previous_model_cache = FileSnapshot::capture(&model_cache_path)?;
    let global_state_path = paths.codex_home.join(GLOBAL_STATE_FILE_NAME);
    let previous_global_state = FileSnapshot::capture(&global_state_path)?;
    let current_revisions =
        ModelRevisions::from_snapshots(&previous_config, &previous_auth, &previous_state);
    if let Some(expected) = expected_revisions {
        if expected != &current_revisions {
            bail!("model configuration changed after it was loaded; reload before saving");
        }
    }
    let mut document = parse_config_snapshot(&previous_config, &paths.config)?;
    let mut auth = parse_auth_snapshot(&previous_auth, &paths.auth)?;

    let providers = document
        .entry("model_providers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("model_providers must be a TOML table")?;
    let provider = providers
        .entry(PROVIDER_ID)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("model_providers.comidea must be a TOML table")?;
    provider["name"] = value(PROVIDER_NAME);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    provider["base_url"] = value(&server_url);
    provider["supports_websockets"] = value(configuration.transport_mode.supports_websockets());
    write_header_map(provider, "http_headers", &configuration.static_headers);
    write_header_map(provider, "env_http_headers", &configuration.env_headers);
    document["model_provider"] = value(PROVIDER_ID);
    let features = document
        .entry("features")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("features must be a TOML table")?;
    features["image_generation"] = value(configuration.image_generation_enabled);
    features["respect_system_proxy"] = value(configuration.inherit_system_proxy);
    auth.insert(
        "OPENAI_API_KEY".into(),
        Value::String(configuration.api_key.to_owned()),
    );

    let config_bytes = document.to_string().into_bytes();
    let mut auth_bytes = Zeroizing::new(serde_json::to_vec_pretty(&auth.0)?);
    auth_bytes.push(b'\n');
    let mut original = if paths.state.is_file() {
        let state = read_state(&paths.state)?;
        if state.config_path != paths.config || state.auth_path != paths.auth {
            bail!("CODEX_HOME changed after model configuration; restore the previous configuration first");
        }
        state
    } else {
        ManagedModelState {
            version: STATE_VERSION,
            config_path: paths.config.clone(),
            auth_path: paths.auth.clone(),
            original_config: previous_config.clone(),
            original_auth: ProtectedFileSnapshot::from_plain(&previous_auth)?,
            installed_config_sha256: String::new(),
            installed_auth_sha256: String::new(),
            image_model: image_model.clone(),
            original_model_picker_view: None,
            transport_mode: configuration.transport_mode,
            inherit_system_proxy: configuration.inherit_system_proxy,
        }
    };
    if original.original_model_picker_view.is_none() {
        original.original_model_picker_view =
            Some(capture_model_picker_view(&previous_global_state)?);
    }
    let state = ManagedModelState {
        installed_config_sha256: image::sha256(&config_bytes),
        installed_auth_sha256: image::sha256(&auth_bytes),
        image_model: image_model.clone(),
        transport_mode: configuration.transport_mode,
        inherit_system_proxy: configuration.inherit_system_proxy,
        ..original
    };
    let state_bytes = serde_json::to_vec_pretty(&state)?;
    let picker_preference = if configuration.image_generation_enabled {
        ModelPickerPreference::Advanced
    } else {
        ModelPickerPreference::Restore
    };
    let global_state_bytes = updated_model_picker_state(
        &previous_global_state,
        picker_preference,
        state.original_model_picker_view.as_ref(),
    )?;

    ModelRevisions::from_snapshots(&previous_config, &previous_auth, &previous_state)
        .verify(paths)
        .context("model configuration changed while preparing the save")?;

    let result = (|| {
        secure_state_directory(paths.state.parent().context("state has no parent")?)?;
        atomic_write(&paths.auth, &auth_bytes)?;
        atomic_write(&paths.config, &config_bytes)?;
        atomic_write(&paths.state, &state_bytes)?;
        sync_model_cache_file(&model_cache_path)?;
        if let Some(bytes) = global_state_bytes.as_ref() {
            atomic_write(&global_state_path, bytes)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = previous_auth.restore(&paths.auth);
        let _ = previous_config.restore(&paths.config);
        let _ = previous_state.restore(&paths.state);
        let _ = previous_model_cache.restore(&model_cache_path);
        let _ = previous_global_state.restore(&global_state_path);
        return Err(error);
    }

    Ok(SavedModelSettings {
        server_url,
        image_model,
        image_model_enabled: configuration.image_generation_enabled,
    })
}

fn restore_from_paths(paths: &ModelPaths) -> Result<bool> {
    if !paths.state.is_file() {
        return Ok(false);
    }
    let state = read_state(&paths.state)?;
    if state.config_path != paths.config || state.auth_path != paths.auth {
        bail!("managed model configuration belongs to a different CODEX_HOME");
    }
    verify_installed_file(&paths.config, &state.installed_config_sha256, "config.toml")?;
    verify_installed_file(&paths.auth, &state.installed_auth_sha256, "auth.json")?;

    let installed_config = FileSnapshot::capture(&paths.config)?;
    let installed_auth = FileSnapshot::capture(&paths.auth)?;
    let model_cache_path = paths.codex_home.join(MODEL_CACHE_FILE_NAME);
    let installed_model_cache = FileSnapshot::capture(&model_cache_path)?;
    let global_state_path = paths.codex_home.join(GLOBAL_STATE_FILE_NAME);
    let installed_global_state = FileSnapshot::capture(&global_state_path)?;
    let restored_global_state = updated_model_picker_state(
        &installed_global_state,
        ModelPickerPreference::Restore,
        state.original_model_picker_view.as_ref(),
    )?;
    let result = (|| {
        state.original_auth.restore(&paths.auth)?;
        state.original_config.restore(&paths.config)?;
        sync_model_cache_file(&model_cache_path)?;
        if let Some(bytes) = restored_global_state.as_ref() {
            atomic_write(&global_state_path, bytes)?;
        }
        fs::remove_file(&paths.state)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = installed_auth.restore(&paths.auth);
        let _ = installed_config.restore(&paths.config);
        let _ = installed_model_cache.restore(&model_cache_path);
        let _ = installed_global_state.restore(&global_state_path);
        return Err(error);
    }
    Ok(true)
}

fn sync_model_cache_file(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let metadata = fs::metadata(path)?;
    if metadata.len() > MODEL_CACHE_MAX_BYTES {
        bail!("Codex model cache exceeds the safe size limit");
    }
    let original = fs::read(path)?;
    let mut cache: Value =
        serde_json::from_slice(&original).context("invalid Codex model cache")?;
    let models = cache
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("Codex model cache has no models array")?;

    let original_len = models.len();
    models.retain(|model| !managed_model_cache_entry(model));
    if models.len() == original_len {
        return Ok(false);
    }
    cache["fetched_at"] = Value::String(STALE_MODEL_CACHE_TIMESTAMP.to_owned());

    let mut bytes = serde_json::to_vec_pretty(&cache)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)?;
    Ok(true)
}

fn managed_model_cache_entry(model: &Value) -> bool {
    model
        .get("comp_hash")
        .and_then(Value::as_str)
        .is_some_and(|hash| hash.starts_with(MODEL_CACHE_MARKER_PREFIX))
}

fn migrate_legacy_state(paths: &ModelPaths) -> Result<()> {
    if !paths.legacy_state.is_file() {
        return Ok(());
    }
    let bytes = fs::read(&paths.legacy_state)?;
    let version = serde_json::from_slice::<Value>(&bytes)?
        .get("version")
        .and_then(Value::as_u64)
        .context("legacy model state has no version")? as u32;
    let state = match version {
        LEGACY_STATE_VERSION => {
            let legacy: LegacyManagedModelState = serde_json::from_slice(&bytes)?;
            if legacy.version != LEGACY_STATE_VERSION {
                bail!(
                    "unsupported legacy model configuration state version {}",
                    legacy.version
                );
            }
            ManagedModelState {
                version: STATE_VERSION,
                config_path: legacy.config_path,
                auth_path: legacy.auth_path,
                original_config: legacy.original_config,
                original_auth: ProtectedFileSnapshot::from_plain(&legacy.original_auth)?,
                installed_config_sha256: legacy.installed_config_sha256,
                installed_auth_sha256: legacy.installed_auth_sha256,
                image_model: default_image_model(),
                original_model_picker_view: None,
                transport_mode: TransportMode::Auto,
                inherit_system_proxy: default_inherit_system_proxy(),
            }
        }
        STATE_VERSION => serde_json::from_slice(&bytes)?,
        _ => bail!("unsupported legacy model configuration state version {version}"),
    };
    let legacy_home = state
        .config_path
        .parent()
        .context("legacy model config path has no parent")?;
    if state.auth_path.parent() != Some(legacy_home) {
        bail!("legacy model state paths do not share a CODEX_HOME");
    }
    let legacy_home = normalize_codex_home(legacy_home)?;
    let state_directory = paths.state.parent().context("state has no parent")?;
    let target = state_directory.join(format!("{}.json", codex_home_key(&legacy_home)));
    if target.is_file() {
        return Ok(());
    }
    secure_state_directory(state_directory)?;
    atomic_write(&target, &serde_json::to_vec_pretty(&state)?)?;
    fs::remove_file(&paths.legacy_state).context("failed to remove migrated legacy model state")?;
    Ok(())
}

fn normalize_codex_home(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    match absolute.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(absolute),
        Err(error) => Err(error).context("failed to normalize CODEX_HOME"),
    }
}

fn codex_home_key(path: &Path) -> String {
    image::sha256(path.to_string_lossy().to_lowercase().as_bytes())
}

fn parse_config_snapshot(snapshot: &FileSnapshot, path: &Path) -> Result<DocumentMut> {
    let Some(bytes) = snapshot.bytes()? else {
        return Ok(DocumentMut::new());
    };
    String::from_utf8(bytes)
        .context("config.toml is not UTF-8")?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn parse_auth_snapshot(snapshot: &FileSnapshot, path: &Path) -> Result<SecretAuth> {
    let Some(bytes) = snapshot.bytes()? else {
        return Ok(SecretAuth(Map::new()));
    };
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .map(SecretAuth)
        .context("auth.json root must be a JSON object")
}

impl ModelRevisions {
    fn capture(paths: &ModelPaths) -> Result<Self> {
        Ok(Self {
            config: FileRevision::capture(&paths.config)?,
            auth: FileRevision::capture(&paths.auth)?,
            state: FileRevision::capture(&paths.state)?,
        })
    }

    fn from_snapshots(config: &FileSnapshot, auth: &FileSnapshot, state: &FileSnapshot) -> Self {
        Self {
            config: FileRevision::from_snapshot(config),
            auth: FileRevision::from_snapshot(auth),
            state: FileRevision::from_snapshot(state),
        }
    }

    fn verify(&self, paths: &ModelPaths) -> Result<()> {
        let current = Self::capture(paths)?;
        if &current != self {
            bail!("model configuration changed during the operation; refusing to overwrite it");
        }
        Ok(())
    }
}

impl FileRevision {
    fn capture(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                existed: false,
                sha256: None,
            });
        }
        if !path.is_file() {
            bail!(
                "configuration path is not a regular file: {}",
                path.display()
            );
        }
        Ok(Self {
            existed: true,
            sha256: Some(image::sha256(&fs::read(path)?)),
        })
    }

    fn from_snapshot(snapshot: &FileSnapshot) -> Self {
        Self {
            existed: snapshot.existed,
            sha256: snapshot.sha256.clone(),
        }
    }
}

impl ProtectedFileSnapshot {
    fn from_plain(snapshot: &FileSnapshot) -> Result<Self> {
        let Some(bytes) = snapshot.bytes()? else {
            return Ok(Self::default());
        };
        let bytes = Zeroizing::new(bytes);
        Ok(Self {
            existed: true,
            protected_bytes_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(protect_data(&bytes)?),
            ),
            sha256: snapshot.sha256.clone(),
        })
    }

    fn restore(&self, path: &Path) -> Result<()> {
        if !self.existed {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        let protected = base64::engine::general_purpose::STANDARD.decode(
            self.protected_bytes_base64
                .as_deref()
                .context("protected file snapshot has no content")?,
        )?;
        let bytes = Zeroizing::new(unprotect_data(&protected)?);
        if self.sha256.as_deref() != Some(image::sha256(&bytes).as_str()) {
            bail!("protected file snapshot hash mismatch");
        }
        atomic_write(path, &bytes)
    }
}

#[cfg(windows)]
fn protect_data(bytes: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes
            .len()
            .try_into()
            .context("secret snapshot is too large")?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let entropy_bytes = b"comidea.org/CodexImageFix/model-config/v2";
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_bytes.len() as u32,
        pbData: entropy_bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let succeeded = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("DPAPI protection failed");
    }
    let protected = unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        bytes
    };
    Ok(protected)
}

#[cfg(windows)]
fn unprotect_data(bytes: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes
            .len()
            .try_into()
            .context("protected snapshot is too large")?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let entropy_bytes = b"comidea.org/CodexImageFix/model-config/v2";
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_bytes.len() as u32,
        pbData: entropy_bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let succeeded = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("DPAPI unprotection failed");
    }
    let plain = unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        bytes
    };
    Ok(plain)
}

#[cfg(not(windows))]
fn protect_data(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}

#[cfg(not(windows))]
fn unprotect_data(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}

#[cfg(windows)]
fn secure_state_directory(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    fs::create_dir_all(path)?;
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
    let icacls = PathBuf::from(system_root)
        .join("System32")
        .join("icacls.exe");
    let domain = std::env::var("USERDOMAIN").context("USERDOMAIN is not set")?;
    let username = std::env::var("USERNAME").context("USERNAME is not set")?;
    let current_user = format!(r"{domain}\{username}:(OI)(CI)F");
    let output = Command::new(icacls)
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(current_user)
        .arg("/grant:r")
        .arg("*S-1-5-18:(OI)(CI)F")
        .arg("/grant:r")
        .arg("*S-1-5-32-544:(OI)(CI)F")
        .creation_flags(0x0800_0000)
        .output()?;
    if !output.status.success() {
        bail!("failed to secure model state directory with icacls");
    }
    Ok(())
}

#[cfg(not(windows))]
fn secure_state_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ModelPickerPreference {
    Advanced,
    Restore,
}

fn capture_model_picker_view(snapshot: &FileSnapshot) -> Result<JsonValueSnapshot> {
    let state = parse_global_state(snapshot)?;
    let value = state
        .get(PERSISTED_ATOMS_KEY)
        .and_then(Value::as_object)
        .and_then(|atoms| atoms.get(MODEL_PICKER_VIEW_KEY));
    Ok(JsonValueSnapshot {
        existed: value.is_some(),
        value: value.cloned(),
    })
}

fn updated_model_picker_state(
    snapshot: &FileSnapshot,
    preference: ModelPickerPreference,
    original: Option<&JsonValueSnapshot>,
) -> Result<Option<Vec<u8>>> {
    let previous_bytes = snapshot.bytes()?;
    let preserve_newline = previous_bytes
        .as_deref()
        .is_some_and(|bytes| bytes.ends_with(b"\n"));
    let mut state = parse_global_state(snapshot)?;
    let root = state
        .as_object_mut()
        .context("Codex global state root must be a JSON object")?;
    let atoms = root
        .entry(PERSISTED_ATOMS_KEY)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("Codex persisted atom state must be a JSON object")?;

    match preference {
        ModelPickerPreference::Advanced => {
            if atoms.get(MODEL_PICKER_VIEW_KEY).and_then(Value::as_str)
                == Some(ADVANCED_MODEL_PICKER_VIEW)
            {
                return Ok(None);
            }
            atoms.insert(
                MODEL_PICKER_VIEW_KEY.to_owned(),
                Value::String(ADVANCED_MODEL_PICKER_VIEW.to_owned()),
            );
        }
        ModelPickerPreference::Restore => {
            let Some(original) = original else {
                return Ok(None);
            };
            if atoms.get(MODEL_PICKER_VIEW_KEY).and_then(Value::as_str)
                != Some(ADVANCED_MODEL_PICKER_VIEW)
            {
                return Ok(None);
            }
            if original.existed {
                let value = original
                    .value
                    .clone()
                    .context("saved model picker preference has no value")?;
                atoms.insert(MODEL_PICKER_VIEW_KEY.to_owned(), value);
            } else {
                atoms.remove(MODEL_PICKER_VIEW_KEY);
            }
        }
    }

    let mut bytes = serde_json::to_vec(&state)?;
    if preserve_newline {
        bytes.push(b'\n');
    }
    if bytes.len() as u64 > GLOBAL_STATE_MAX_BYTES {
        bail!("Codex global state exceeds the safe size limit");
    }
    Ok(Some(bytes))
}

fn parse_global_state(snapshot: &FileSnapshot) -> Result<Value> {
    let Some(bytes) = snapshot.bytes()? else {
        return Ok(Value::Object(Map::new()));
    };
    if bytes.len() as u64 > GLOBAL_STATE_MAX_BYTES {
        bail!("Codex global state exceeds the safe size limit");
    }
    serde_json::from_slice(&bytes)
        .context("invalid Codex global state; model picker preference was not changed")
}

fn read_config(path: &Path) -> Result<DocumentMut> {
    if !path.is_file() {
        return Ok(DocumentMut::new());
    }
    fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn read_auth(path: &Path) -> Result<SecretAuth> {
    if !path.is_file() {
        return Ok(SecretAuth(Map::new()));
    }
    let value: Value = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .map(SecretAuth)
        .context("auth.json root must be a JSON object")
}

fn read_state(path: &Path) -> Result<ManagedModelState> {
    let state: ManagedModelState = serde_json::from_slice(&fs::read(path)?)?;
    if state.version != STATE_VERSION {
        bail!(
            "unsupported model configuration state version {}",
            state.version
        );
    }
    Ok(state)
}

fn verify_installed_file(path: &Path, expected_sha256: &str, label: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("managed {label} is missing"))?;
    if image::sha256(&bytes) != expected_sha256 {
        bail!("{label} changed after it was saved; refusing to overwrite newer changes");
    }
    Ok(())
}

fn default_image_model() -> String {
    IMAGE_MODEL.to_owned()
}

fn default_inherit_system_proxy() -> bool {
    true
}

pub fn proxy_inheritance_enabled() -> Result<bool> {
    let config_path = image::codex_home().join("config.toml");
    let document = read_config(&config_path)?;
    Ok(document
        .get("features")
        .and_then(|features| features.get("respect_system_proxy"))
        .and_then(toml_edit::Item::as_bool)
        .unwrap_or(false))
}

pub fn managed_websocket_cli_override() -> Result<Option<String>> {
    let config_path = image::codex_home().join("config.toml");
    let document = read_config(&config_path)?;
    Ok(managed_websocket_cli_override_from_document(&document))
}

fn managed_websocket_cli_override_from_document(document: &DocumentMut) -> Option<String> {
    if document
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        != Some(PROVIDER_ID)
    {
        return None;
    }
    let provider = document
        .get("model_providers")
        .and_then(|providers| providers.get(PROVIDER_ID))?;
    let supports_websockets = provider
        .get("supports_websockets")
        .and_then(toml_edit::Item::as_bool)
        .unwrap_or(false);
    Some(format!(
        "model_providers.{PROVIDER_ID}.supports_websockets={supports_websockets}"
    ))
}

pub fn parse_static_headers(value: &str) -> Result<BTreeMap<String, String>> {
    parse_header_json(value, false)
}

pub fn parse_env_headers(value: &str) -> Result<BTreeMap<String, String>> {
    parse_header_json(value, true)
}

pub fn format_headers_json(headers: &BTreeMap<String, String>) -> Result<String> {
    if headers.is_empty() {
        Ok(String::new())
    } else {
        serde_json::to_string(headers).context("failed to format Header configuration")
    }
}

fn parse_header_json(value: &str, environment: bool) -> Result<BTreeMap<String, String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(BTreeMap::new());
    }
    let parsed: BTreeMap<String, String> = serde_json::from_str(value)
        .context("Header must be a JSON object whose names and values are strings")?;
    if environment {
        validate_headers(&BTreeMap::new(), &parsed)?;
    } else {
        validate_headers(&parsed, &BTreeMap::new())?;
    }
    Ok(parsed)
}

fn read_header_map(item: &Item, label: &str) -> Result<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    match item {
        Item::Value(TomlValue::InlineTable(table)) => {
            for (name, value) in table.iter() {
                headers.insert(
                    name.to_owned(),
                    value
                        .as_str()
                        .with_context(|| format!("{label}.{name} must be a string"))?
                        .to_owned(),
                );
            }
        }
        Item::Table(table) => {
            for (name, value) in table.iter() {
                headers.insert(
                    name.to_owned(),
                    value
                        .as_str()
                        .with_context(|| format!("{label}.{name} must be a string"))?
                        .to_owned(),
                );
            }
        }
        _ => bail!("{label} must be a TOML table"),
    }
    Ok(headers)
}

fn write_header_map(table: &mut Table, key: &str, headers: &BTreeMap<String, String>) {
    if headers.is_empty() {
        table.remove(key);
        return;
    }
    let mut inline = InlineTable::new();
    for (name, value) in headers {
        inline.insert(name, TomlValue::from(value.as_str()));
    }
    table[key] = Item::Value(TomlValue::InlineTable(inline));
}

fn validate_model_id(value: &str) -> Result<&str> {
    let value = value.trim();
    if value.is_empty() {
        bail!("image model ID cannot be empty");
    }
    if value.len() > 256 || value.chars().any(char::is_control) {
        bail!("image model ID contains invalid characters or is too long");
    }
    Ok(value)
}

fn validate_headers(
    static_headers: &BTreeMap<String, String>,
    env_headers: &BTreeMap<String, String>,
) -> Result<()> {
    if static_headers.len() + env_headers.len() > 64 {
        bail!("too many custom Headers; the maximum is 64");
    }
    for (name, value) in static_headers {
        validate_header_name(name)?;
        if value.len() > 8192 || value.chars().any(char::is_control) {
            bail!("Header {name} has an invalid or excessively long value");
        }
    }
    for (name, variable) in env_headers {
        validate_header_name(name)?;
        let mut characters = variable.chars();
        if variable.len() > 256
            || !characters
                .next()
                .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            bail!("Header {name} has an invalid environment variable name");
        }
    }
    Ok(())
}

fn validate_header_name(name: &str) -> Result<()> {
    let reserved = ["authorization", "content-length", "host"];
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
        || reserved
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        bail!("invalid or reserved Header name: {name}");
    }
    Ok(())
}

fn zeroize_header_values(headers: &mut BTreeMap<String, String>) {
    for value in headers.values_mut() {
        value.zeroize();
    }
}

pub fn normalize_server_url(value: &str) -> Result<String> {
    let value = value.trim();
    let mut url = Url::parse(value).context("server address must be an absolute URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("server address must use HTTPS or HTTP");
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("server address has an invalid host or embedded credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("server address must not contain a query string or fragment");
    }
    if url.scheme() == "http" {
        let host = url.host_str().unwrap_or_default();
        if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
            bail!("API keys require HTTPS except for a local loopback server");
        }
    }
    let path = url.path().trim_end_matches('/');
    let version_segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .filter(|segment| segment.eq_ignore_ascii_case("v1"))
        .count();
    if version_segments > 1 {
        bail!("server address contains a duplicate /v1/v1 version path");
    }
    let version_is_final = path
        .rsplit('/')
        .next()
        .is_some_and(|segment| segment.eq_ignore_ascii_case("v1"));
    if version_segments == 1 && !version_is_final {
        bail!("the /v1 version segment must be the final server path segment");
    }
    let normalized_path = if path.is_empty() {
        "/v1".to_owned()
    } else if version_segments == 0 {
        format!("{path}/v1")
    } else {
        path.to_owned()
    };
    url.set_path(&normalized_path);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn validate_api_key(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("API Key cannot be empty");
    }
    if value.len() > 8192 || value.chars().any(char::is_control) {
        bail!("API Key contains invalid characters or is too long");
    }
    Ok(())
}

impl FileSnapshot {
    fn capture(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        if !path.is_file() {
            bail!("snapshot path is not a regular file: {}", path.display());
        }
        let bytes = fs::read(path)?;
        Ok(Self {
            existed: true,
            bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            sha256: Some(image::sha256(&bytes)),
        })
    }

    fn bytes(&self) -> Result<Option<Vec<u8>>> {
        if !self.existed {
            return Ok(None);
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(
            self.bytes_base64
                .as_deref()
                .context("file snapshot has no content")?,
        )?;
        if self.sha256.as_deref() != Some(image::sha256(&bytes).as_str()) {
            bail!("file snapshot hash mismatch");
        }
        Ok(Some(bytes))
    }

    fn restore(&self, path: &Path) -> Result<()> {
        if !self.existed {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        let bytes = self.bytes()?.context("file snapshot has no content")?;
        atomic_write(path, &bytes)
    }
}

impl Drop for FileSnapshot {
    fn drop(&mut self) {
        if let Some(bytes) = self.bytes_base64.as_mut() {
            bytes.zeroize();
        }
    }
}

fn zeroize_json_value(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Array(values) => {
            for value in values {
                zeroize_json_value(value);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                zeroize_json_value(value);
            }
        }
        _ => {}
    }
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> Result<()> {
    let parent = destination.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = destination.with_extension(format!("{}.{}.tmp", std::process::id(), counter));
    {
        let mut file = fs::File::options()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    replace_file(&temp, destination).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("atomic file replace failed");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).context("atomic file replace failed")
}

#[cfg(windows)]
fn test_connection_platform(
    server_url: &str,
    api_key: &str,
    image_model: &str,
    static_headers: &BTreeMap<String, String>,
    env_headers: &BTreeMap<String, String>,
) -> Result<ConnectionReport> {
    use std::{ffi::c_void, ptr::null_mut};
    use windows_sys::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
        WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
        WinHttpSendRequest, WinHttpSetTimeouts, INTERNET_DEFAULT_HTTPS_PORT,
        INTERNET_DEFAULT_HTTP_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    };

    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    struct HttpProbe {
        status: u32,
        body: Vec<u8>,
    }

    unsafe fn send_probe(
        session: *mut c_void,
        endpoint: &Url,
        method: &str,
        body: &[u8],
        headers: &[u16],
    ) -> Result<HttpProbe> {
        let host = wide(endpoint.host_str().context("server URL has no host")?);
        let path = wide(endpoint.path());
        let verb = wide(method);
        let secure = endpoint.scheme() == "https";
        let port = endpoint.port().unwrap_or(if secure {
            INTERNET_DEFAULT_HTTPS_PORT
        } else {
            INTERNET_DEFAULT_HTTP_PORT
        });
        let connection = Handle(WinHttpConnect(session, host.as_ptr(), port, 0));
        if connection.0.is_null() {
            return Err(classify_winhttp_error("连接服务器失败"));
        }
        let request = Handle(WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            if secure { WINHTTP_FLAG_SECURE } else { 0 },
        ));
        if request.0.is_null() {
            return Err(classify_winhttp_error("创建 HTTP 请求失败"));
        }
        let (body_pointer, body_length) = if body.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (
                body.as_ptr().cast_mut().cast::<c_void>(),
                body.len()
                    .try_into()
                    .context("HTTP request body is too large")?,
            )
        };
        if WinHttpSendRequest(
            request.0,
            headers.as_ptr(),
            u32::MAX,
            body_pointer,
            body_length,
            body_length,
            0,
        ) == 0
        {
            return Err(classify_winhttp_error("发送 HTTP 请求失败"));
        }
        if WinHttpReceiveResponse(request.0, null_mut()) == 0 {
            return Err(classify_winhttp_error("接收 HTTP 响应失败"));
        }
        let mut status = 0u32;
        let mut length = size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status as *mut u32).cast(),
            &mut length,
            null_mut(),
        ) == 0
        {
            return Err(classify_winhttp_error("读取 HTTP 状态码失败"));
        }
        let mut response_body = Vec::new();
        loop {
            let mut available = 0u32;
            if WinHttpQueryDataAvailable(request.0, &mut available) == 0 {
                return Err(classify_winhttp_error("读取 HTTP 响应长度失败"));
            }
            if available == 0 {
                break;
            }
            if response_body.len().saturating_add(available as usize) > 2 * 1024 * 1024 {
                bail!("HTTP response exceeds the 2 MiB diagnostic limit");
            }
            let offset = response_body.len();
            response_body.resize(offset + available as usize, 0);
            let mut read = 0u32;
            if WinHttpReadData(
                request.0,
                response_body[offset..].as_mut_ptr().cast(),
                available,
                &mut read,
            ) == 0
            {
                return Err(classify_winhttp_error("读取 HTTP 响应内容失败"));
            }
            response_body.truncate(offset + read as usize);
            if read == 0 {
                break;
            }
        }
        Ok(HttpProbe {
            status,
            body: response_body,
        })
    }

    let models_endpoint = Url::parse(&format!("{server_url}/models"))?;
    let responses_endpoint = Url::parse(&format!("{server_url}/responses"))?;
    let headers = build_probe_headers(api_key, static_headers, env_headers)?;
    unsafe {
        let agent = wide(concat!("CodexImageFix/", env!("CARGO_PKG_VERSION")));
        let session = Handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(classify_winhttp_error("初始化 WinHTTP 失败"));
        }
        if WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 8000) == 0 {
            return Err(classify_winhttp_error("设置 WinHTTP 超时失败"));
        }
        let models = send_probe(session.0, &models_endpoint, "GET", &[], &headers)?;
        let responses = send_probe(session.0, &responses_endpoint, "POST", b"{}", &headers)?;
        build_connection_report(
            server_url,
            image_model,
            models.status,
            &models.body,
            responses.status,
        )
    }
}

#[cfg(not(windows))]
fn test_connection_platform(
    _server_url: &str,
    _api_key: &str,
    _image_model: &str,
    _static_headers: &BTreeMap<String, String>,
    _env_headers: &BTreeMap<String, String>,
) -> Result<ConnectionReport> {
    bail!("connection testing is only supported on Windows")
}

#[cfg(windows)]
fn build_probe_headers(
    api_key: &str,
    static_headers: &BTreeMap<String, String>,
    env_headers: &BTreeMap<String, String>,
) -> Result<Zeroizing<Vec<u16>>> {
    let mut headers = Zeroizing::new(format!(
        "Authorization: Bearer {api_key}\r\nAccept: application/json\r\nContent-Type: application/json\r\n"
    ));
    for (name, value) in static_headers {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(value);
        headers.push_str("\r\n");
    }
    for (name, variable) in env_headers {
        let value = Zeroizing::new(std::env::var(variable).with_context(|| {
            format!("Header {name} requires environment variable {variable}, but it is not set")
        })?);
        if value.len() > 8192 || value.chars().any(char::is_control) {
            bail!("Header {name} environment variable has an invalid or excessively long value");
        }
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(&value);
        headers.push_str("\r\n");
    }
    Ok(Zeroizing::new(wide(&headers)))
}

#[cfg(windows)]
fn classify_winhttp_error(action: &str) -> anyhow::Error {
    let error = std::io::Error::last_os_error();
    let code = error.raw_os_error().unwrap_or_default();
    let category = match code {
        12002 => "网络超时",
        12007 => "DNS 解析失败",
        12029..=12031 => "服务器连接失败",
        12037 | 12038 | 12045 | 12175 => "TLS 证书或安全协商失败",
        12044 => "服务器要求客户端证书",
        12166 | 12167 | 12180 => "系统代理配置失败",
        _ => "WinHTTP 网络错误",
    };
    anyhow::Error::msg(format!("{category}：{action}（错误码 {code}）"))
}

fn build_connection_report(
    server_url: &str,
    image_model: &str,
    models_status: u32,
    models_body: &[u8],
    responses_status: u32,
) -> Result<ConnectionReport> {
    let unauthorized = matches!(models_status, 401 | 403) || matches!(responses_status, 401 | 403);
    let model_available = if (200..300).contains(&models_status) {
        Some(models_contains_model(models_body, image_model)?)
    } else {
        None
    };
    let responses_available =
        (200..300).contains(&responses_status) || matches!(responses_status, 400 | 409 | 422 | 429);
    let usable = !unauthorized && responses_available && model_available != Some(false);
    let summary = if unauthorized {
        format!("鉴权失败 · /models {models_status} · /responses {responses_status}")
    } else if !responses_available {
        format!("Responses API 不可用 · HTTP {responses_status}")
    } else if model_available == Some(false) {
        format!("服务器可连接，但未找到模型 {image_model}")
    } else if model_available.is_none() {
        format!("Responses API 可用 · /models HTTP {models_status}，模型存在性未确认")
    } else if responses_status == 429 {
        "连接与模型验证通过，但服务当前限流".to_owned()
    } else {
        format!("连接、鉴权、Responses API 与模型 {image_model} 均通过")
    };
    Ok(ConnectionReport {
        normalized_url: server_url.to_owned(),
        models_status: Some(models_status),
        responses_status: Some(responses_status),
        model_available,
        usable,
        summary,
    })
}

fn models_contains_model(body: &[u8], image_model: &str) -> Result<bool> {
    let document: Value = serde_json::from_slice(body)
        .context("models API returned invalid JSON; response content was not logged")?;
    let models = document
        .get("data")
        .or_else(|| document.get("models"))
        .and_then(Value::as_array)
        .context("models API JSON has no data/models array")?;
    Ok(models.iter().any(|model| {
        model
            .get("id")
            .or_else(|| model.get("slug"))
            .and_then(Value::as_str)
            == Some(image_model)
    }))
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn test_paths(name: &str) -> (PathBuf, ModelPaths) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-image-model-{name}-{unique}"));
        let paths = paths_for_home(&root, "codex");
        (root, paths)
    }

    fn paths_for_home(root: &Path, home_name: &str) -> ModelPaths {
        let codex_home = root.join(home_name);
        fs::create_dir_all(&codex_home).unwrap();
        let codex_home = normalize_codex_home(&codex_home).unwrap();
        ModelPaths {
            config: codex_home.join("config.toml"),
            auth: codex_home.join("auth.json"),
            state: root
                .join("install")
                .join(STATE_DIRECTORY_NAME)
                .join(format!("{}.json", codex_home_key(&codex_home))),
            legacy_state: root.join("install").join(LEGACY_STATE_FILE_NAME),
            codex_home,
        }
    }

    fn configuration(server_url: &str, api_key: &str, enabled: bool) -> ModelConfiguration {
        ModelConfiguration {
            server_url: server_url.to_owned(),
            api_key: api_key.to_owned(),
            image_model: IMAGE_MODEL.to_owned(),
            image_generation_enabled: enabled,
            static_headers: BTreeMap::new(),
            env_headers: BTreeMap::new(),
            transport_mode: TransportMode::Auto,
            inherit_system_proxy: true,
        }
    }

    #[test]
    fn model_picker_preference_is_reversible_and_preserves_state() {
        let (root, paths) = test_paths("model-picker-preference");
        let global_state_path = paths.codex_home.join(GLOBAL_STATE_FILE_NAME);
        fs::write(
            &global_state_path,
            br#"{"unrelated":{"kept":true},"electron-persisted-atom-state":{"composer-model-picker-menu-view-v1":"simple","other":"value"}}"#,
        )
        .unwrap();
        let original_file = FileSnapshot::capture(&global_state_path).unwrap();
        let original_view = capture_model_picker_view(&original_file).unwrap();

        let advanced =
            updated_model_picker_state(&original_file, ModelPickerPreference::Advanced, None)
                .unwrap()
                .unwrap();
        fs::write(&global_state_path, advanced).unwrap();
        let advanced_file = FileSnapshot::capture(&global_state_path).unwrap();
        assert!(
            updated_model_picker_state(&advanced_file, ModelPickerPreference::Advanced, None)
                .unwrap()
                .is_none()
        );

        let restored = updated_model_picker_state(
            &advanced_file,
            ModelPickerPreference::Restore,
            Some(&original_view),
        )
        .unwrap()
        .unwrap();
        let restored: Value = serde_json::from_slice(&restored).unwrap();
        assert_eq!(
            restored[PERSISTED_ATOMS_KEY][MODEL_PICKER_VIEW_KEY],
            "simple"
        );
        assert_eq!(restored[PERSISTED_ATOMS_KEY]["other"], "value");
        assert_eq!(restored["unrelated"]["kept"], true);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn model_picker_restore_does_not_override_new_user_choice() {
        let (root, paths) = test_paths("model-picker-user-choice");
        let global_state_path = paths.codex_home.join(GLOBAL_STATE_FILE_NAME);
        fs::write(
            &global_state_path,
            br#"{"electron-persisted-atom-state":{"composer-model-picker-menu-view-v1":"custom"}}"#,
        )
        .unwrap();
        let snapshot = FileSnapshot::capture(&global_state_path).unwrap();
        let original = JsonValueSnapshot {
            existed: true,
            value: Some(Value::String("simple".to_owned())),
        };
        assert!(updated_model_picker_state(
            &snapshot,
            ModelPickerPreference::Restore,
            Some(&original)
        )
        .unwrap()
        .is_none());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn model_cache_sync_removes_legacy_managed_model_only() {
        let (root, paths) = test_paths("model-cache");
        let cache_path = paths.codex_home.join(MODEL_CACHE_FILE_NAME);
        let original = serde_json::json!({
            "fetched_at": "2026-01-01T00:00:00Z",
            "etag": "official-etag",
            "client_version": "1.0.0",
            "models": [{
                "slug": "gpt-text",
                "display_name": "GPT Text",
                "description": "Official text model",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1,
                "comp_hash": "official-hash",
                "input_modalities": ["text", "image"]
            }, {
                "slug": IMAGE_MODEL,
                "display_name": "GPT Image 2",
                "description": "Legacy managed image model",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 0,
                "comp_hash": format!("{MODEL_CACHE_MARKER_PREFIX}{IMAGE_MODEL}"),
                "input_modalities": ["text", "image"]
            }]
        });
        fs::write(&cache_path, serde_json::to_vec(&original).unwrap()).unwrap();

        assert!(sync_model_cache_file(&cache_path).unwrap());
        assert!(!sync_model_cache_file(&cache_path).unwrap());
        let cleaned: Value = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        assert_eq!(cleaned["fetched_at"], STALE_MODEL_CACHE_TIMESTAMP);
        assert_eq!(cleaned["models"].as_array().unwrap().len(), 1);
        assert_eq!(cleaned["models"][0], original["models"][0]);
        assert_eq!(cleaned["etag"], "official-etag");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saves_image_model_without_overwriting_unrelated_configuration() {
        let (root, paths) = test_paths("save");
        fs::create_dir_all(&paths.codex_home).unwrap();
        fs::write(
            &paths.config,
            "model = \"gpt-5.4\"\nnotify = [\"existing\"]\n[features]\ngoals = true\n",
        )
        .unwrap();
        fs::write(&paths.auth, br#"{"existing":"value"}"#).unwrap();

        let saved = save_to_paths(
            &paths,
            &configuration("https://api.comidea.org/v1/", "secret-key", true),
            None,
        )
        .unwrap();
        assert_eq!(saved.server_url, "https://api.comidea.org/v1");
        let config = fs::read_to_string(&paths.config).unwrap();
        assert!(config.contains("notify = [\"existing\"]"));
        assert!(config.contains("goals = true"));
        assert!(config.contains("model = \"gpt-5.4\""));
        assert!(!config.contains("model = \"gpt-image-2\""));
        assert!(config.contains("image_generation = true"));
        assert!(config.contains("supports_websockets = false"));
        assert!(config.contains("respect_system_proxy = true"));
        assert!(config.contains("[model_providers.comidea]"));
        let auth: Value = serde_json::from_slice(&fs::read(&paths.auth).unwrap()).unwrap();
        assert_eq!(auth["existing"], "value");
        assert_eq!(auth["OPENAI_API_KEY"], "secret-key");

        assert!(restore_from_paths(&paths).unwrap());
        assert_eq!(
            fs::read_to_string(&paths.config).unwrap(),
            "model = \"gpt-5.4\"\nnotify = [\"existing\"]\n[features]\ngoals = true\n"
        );
        let auth: Value = serde_json::from_slice(&fs::read(&paths.auth).unwrap()).unwrap();
        assert_eq!(auth, serde_json::json!({"existing": "value"}));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loads_existing_active_provider_and_image_model() {
        let (root, paths) = test_paths("legacy-provider");
        fs::create_dir_all(&paths.codex_home).unwrap();
        fs::write(
            &paths.config,
            "model_provider = \"custom\"\nmodel = \"gpt-image-2\"\n\
             [features]\nimage_generation = true\n\
             [model_providers.custom]\nbase_url = \"https://legacy.example/v1\"\n",
        )
        .unwrap();
        fs::write(&paths.auth, br#"{"OPENAI_API_KEY":"legacy-key"}"#).unwrap();

        let loaded = load_from_paths(&paths).unwrap();
        assert_eq!(loaded.provider_id, "custom");
        assert_eq!(loaded.server_url, "https://legacy.example/v1");
        assert_eq!(loaded.api_key, "legacy-key");
        assert!(loaded.image_model_enabled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saving_disabled_model_migrates_provider_and_disables_image_generation() {
        let (root, paths) = test_paths("disabled-model");
        fs::create_dir_all(&paths.codex_home).unwrap();
        fs::write(
            &paths.config,
            "model_provider = \"custom\"\nmodel = \"gpt-image-2\"\n\
             [features]\nimage_generation = true\n\
             [model_providers.custom]\nbase_url = \"https://legacy.example/v1\"\n",
        )
        .unwrap();

        save_to_paths(
            &paths,
            &configuration("https://api.comidea.org/v1", "new-key", false),
            None,
        )
        .unwrap();
        let loaded = load_from_paths(&paths).unwrap();
        assert_eq!(loaded.provider_id, PROVIDER_ID);
        assert_eq!(loaded.server_url, "https://api.comidea.org/v1");
        assert!(!loaded.image_model_enabled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_refuses_to_overwrite_newer_user_changes() {
        let (root, paths) = test_paths("conflict");
        save_to_paths(
            &paths,
            &configuration("https://api.comidea.org/v1", "secret-key", true),
            None,
        )
        .unwrap();
        fs::write(&paths.config, "model = \"user-change\"\n").unwrap();
        assert!(restore_from_paths(&paths).is_err());
        assert!(paths.state.is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_refuses_changes_made_after_ui_load() {
        let (root, paths) = test_paths("optimistic-concurrency");
        fs::write(&paths.config, "model = \"initial\"\n").unwrap();
        fs::write(&paths.auth, br#"{"OPENAI_API_KEY":"initial-key"}"#).unwrap();
        let loaded = load_from_paths(&paths).unwrap();
        fs::write(&paths.config, "model = \"external-change\"\n").unwrap();

        let error = save_to_paths(
            &paths,
            &configuration("https://api.comidea.org/v1", "new-key", true),
            Some(&loaded.revisions),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("reload before saving"));
        assert_eq!(
            fs::read_to_string(&paths.config).unwrap(),
            "model = \"external-change\"\n"
        );
        assert!(!paths.state.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn state_encrypts_original_auth_snapshot_with_dpapi() {
        let (root, paths) = test_paths("dpapi-state");
        fs::write(
            &paths.auth,
            br#"{"OPENAI_API_KEY":"original-secret","other":"value"}"#,
        )
        .unwrap();

        save_to_paths(
            &paths,
            &configuration("https://api.comidea.org/v1", "replacement-secret", true),
            None,
        )
        .unwrap();

        let state_text = fs::read_to_string(&paths.state).unwrap();
        assert!(!state_text.contains("original-secret"));
        assert!(!state_text.contains("replacement-secret"));
        assert!(!state_text.contains("T1BFTkFJX0FQSV9LRVk"));
        assert!(restore_from_paths(&paths).unwrap());
        let restored: Value = serde_json::from_slice(&fs::read(&paths.auth).unwrap()).unwrap();
        assert_eq!(restored["OPENAI_API_KEY"], "original-secret");
        assert_eq!(restored["other"], "value");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multiple_codex_homes_are_managed_independently() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-image-model-multi-home-{unique}"));
        let paths_a = paths_for_home(&root, "home-a");
        let paths_b = paths_for_home(&root, "home-b");
        fs::write(&paths_a.config, "model = \"home-a-original\"\n").unwrap();
        fs::write(&paths_b.config, "model = \"home-b-original\"\n").unwrap();

        save_to_paths(
            &paths_a,
            &configuration("https://a.example/v1", "home-a-key", true),
            None,
        )
        .unwrap();
        save_to_paths(
            &paths_b,
            &configuration("https://b.example/v1", "home-b-key", true),
            None,
        )
        .unwrap();

        assert_ne!(paths_a.state, paths_b.state);
        assert!(paths_a.state.is_file());
        assert!(paths_b.state.is_file());
        assert!(restore_from_paths(&paths_a).unwrap());
        assert_eq!(
            fs::read_to_string(&paths_a.config).unwrap(),
            "model = \"home-a-original\"\n"
        );
        assert!(paths_b.state.is_file());
        assert!(fs::read_to_string(&paths_b.config)
            .unwrap()
            .contains("https://b.example/v1"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn migrates_legacy_plaintext_state_to_per_home_dpapi_state() {
        let (root, paths) = test_paths("legacy-migration");
        fs::create_dir_all(paths.legacy_state.parent().unwrap()).unwrap();
        fs::write(&paths.config, "model = \"installed\"\n").unwrap();
        fs::write(&paths.auth, br#"{"OPENAI_API_KEY":"installed-secret"}"#).unwrap();
        let original_auth = FileSnapshot {
            existed: true,
            bytes_base64: Some(
                base64::engine::general_purpose::STANDARD
                    .encode(br#"{"OPENAI_API_KEY":"legacy-original-secret"}"#),
            ),
            sha256: Some(image::sha256(
                br#"{"OPENAI_API_KEY":"legacy-original-secret"}"#,
            )),
        };
        let legacy = LegacyManagedModelState {
            version: LEGACY_STATE_VERSION,
            config_path: paths.config.clone(),
            auth_path: paths.auth.clone(),
            original_config: FileSnapshot::default(),
            original_auth,
            installed_config_sha256: image::sha256(&fs::read(&paths.config).unwrap()),
            installed_auth_sha256: image::sha256(&fs::read(&paths.auth).unwrap()),
        };
        fs::write(
            &paths.legacy_state,
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        migrate_legacy_state(&paths).unwrap();

        assert!(!paths.legacy_state.exists());
        assert!(paths.state.is_file());
        let state_text = fs::read_to_string(&paths.state).unwrap();
        assert!(!state_text.contains("legacy-original-secret"));
        assert!(restore_from_paths(&paths).unwrap());
        let restored: Value = serde_json::from_slice(&fs::read(&paths.auth).unwrap()).unwrap();
        assert_eq!(restored["OPENAI_API_KEY"], "legacy-original-secret");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_insecure_remote_server_urls() {
        assert!(normalize_server_url("http://api.example.com/v1").is_err());
        assert_eq!(
            normalize_server_url("http://127.0.0.1:8080/v1/").unwrap(),
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            normalize_server_url("https://api.example.com").unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            normalize_server_url("https://api.example.com/openai").unwrap(),
            "https://api.example.com/openai/v1"
        );
        assert!(normalize_server_url("https://api.example.com/v1/v1").is_err());
        assert!(normalize_server_url("https://api.example.com/v1/extra").is_err());
    }

    #[test]
    fn preview_never_contains_api_key() {
        let (root, paths) = test_paths("preview");
        fs::write(
            &paths.config,
            "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://old.example/v1\"\n",
        )
        .unwrap();
        fs::write(&paths.auth, br#"{"OPENAI_API_KEY":"preview-secret-key"}"#).unwrap();
        let settings = load_from_paths(&paths).unwrap();

        let mut configuration = configuration("https://new.example/v1", "replacement-secret", true);
        configuration.image_model = "custom-image-2".to_owned();
        configuration.transport_mode = TransportMode::HttpsSse;
        configuration.inherit_system_proxy = false;
        let preview = preview_settings(&configuration, &settings).unwrap();

        assert!(preview.contains("https://old.example/v1"));
        assert!(preview.contains("https://new.example/v1"));
        assert!(preview.contains("custom -> comidea"));
        assert!(preview.contains("自动（推荐） -> HTTPS/SSE"));
        assert!(preview.contains("已开启 -> 已关闭"));
        assert!(!preview.contains("preview-secret-key"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transport_modes_and_proxy_inheritance_round_trip() {
        for (suffix, mode, inherit_proxy, supports_websockets) in [
            ("auto", TransportMode::Auto, true, false),
            ("https", TransportMode::HttpsSse, false, false),
            ("websocket", TransportMode::WebSocket, true, true),
        ] {
            let (root, paths) = test_paths(&format!("transport-{suffix}"));
            let mut configuration = configuration("https://api.comidea.org/v1", "secret-key", true);
            configuration.transport_mode = mode;
            configuration.inherit_system_proxy = inherit_proxy;

            save_to_paths(&paths, &configuration, None).unwrap();
            let loaded = load_from_paths(&paths).unwrap();
            assert_eq!(loaded.transport_mode, mode);
            assert_eq!(loaded.inherit_system_proxy, inherit_proxy);

            let document = read_config(&paths.config).unwrap();
            assert_eq!(
                document["model_providers"][PROVIDER_ID]["supports_websockets"].as_bool(),
                Some(supports_websockets)
            );
            assert_eq!(
                document["features"]["respect_system_proxy"].as_bool(),
                Some(inherit_proxy)
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn managed_runtime_websocket_override_defaults_old_config_to_https() {
        let old_managed: DocumentMut =
            "model_provider = \"comidea\"\n[model_providers.comidea]\nbase_url = \"https://api.example/v1\"\n"
                .parse()
                .unwrap();
        assert_eq!(
            managed_websocket_cli_override_from_document(&old_managed).as_deref(),
            Some("model_providers.comidea.supports_websockets=false")
        );

        let websocket: DocumentMut =
            "model_provider = \"comidea\"\n[model_providers.comidea]\nsupports_websockets = true\n"
                .parse()
                .unwrap();
        assert_eq!(
            managed_websocket_cli_override_from_document(&websocket).as_deref(),
            Some("model_providers.comidea.supports_websockets=true")
        );

        let other: DocumentMut =
            "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://api.example/v1\"\n"
                .parse()
                .unwrap();
        assert!(managed_websocket_cli_override_from_document(&other).is_none());
    }

    #[test]
    fn managed_state_without_network_fields_uses_compatible_defaults() {
        let (root, paths) = test_paths("network-state-defaults");
        save_to_paths(
            &paths,
            &configuration("https://api.comidea.org/v1", "secret-key", true),
            None,
        )
        .unwrap();
        let mut state: Value = serde_json::from_slice(&fs::read(&paths.state).unwrap()).unwrap();
        let object = state.as_object_mut().unwrap();
        object.remove("transportMode");
        object.remove("inheritSystemProxy");
        let state: ManagedModelState = serde_json::from_value(state).unwrap();
        assert_eq!(state.transport_mode, TransportMode::Auto);
        assert!(state.inherit_system_proxy);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn custom_headers_round_trip_through_official_provider_fields() {
        let (root, paths) = test_paths("headers");
        let mut configuration = configuration("https://api.example.com", "secret-key", true);
        configuration.image_model = "custom-image-2".to_owned();
        configuration
            .static_headers
            .insert("X-Tenant".to_owned(), "tenant-secret".to_owned());
        configuration
            .env_headers
            .insert("X-Region".to_owned(), "CODEX_REGION".to_owned());

        save_to_paths(&paths, &configuration, None).unwrap();
        let loaded = load_from_paths(&paths).unwrap();

        assert_eq!(loaded.server_url, "https://api.example.com/v1");
        assert_eq!(loaded.image_model, "custom-image-2");
        assert_eq!(loaded.static_headers["X-Tenant"], "tenant-secret");
        assert_eq!(loaded.env_headers["X-Region"], "CODEX_REGION");
        let config = fs::read_to_string(&paths.config).unwrap();
        assert!(config.contains("http_headers"));
        assert!(config.contains("env_http_headers"));
        assert!(!config.contains("model = \"custom-image-2\""));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_header_json_without_accepting_injection() {
        let headers = parse_static_headers(r#"{"X-Tenant":"tenant-a"}"#).unwrap();
        assert_eq!(headers["X-Tenant"], "tenant-a");
        assert!(parse_static_headers(r#"{"Authorization":"secret"}"#).is_err());
        assert!(parse_static_headers("{\"X-Test\":\"bad\\nvalue\"}").is_err());
        assert!(parse_env_headers(r#"{"X-Test":"BAD-VARIABLE"}"#).is_err());
    }

    #[test]
    fn connection_report_accepts_missing_models_route_when_responses_works() {
        let report =
            build_connection_report("https://api.example.com/v1", IMAGE_MODEL, 404, b"{}", 400)
                .unwrap();
        assert!(report.usable);
        assert_eq!(report.model_available, None);

        let missing = build_connection_report(
            "https://api.example.com/v1",
            IMAGE_MODEL,
            200,
            br#"{"data":[{"id":"another-model"}]}"#,
            400,
        )
        .unwrap();
        assert!(!missing.usable);
        assert_eq!(missing.model_available, Some(false));

        let unauthorized =
            build_connection_report("https://api.example.com/v1", IMAGE_MODEL, 401, b"{}", 401)
                .unwrap();
        assert!(!unauthorized.usable);
    }

    #[test]
    fn managed_state_scan_keeps_other_codex_homes() {
        let (root, paths) = test_paths("state-scan");
        let state_directory = paths.state.parent().unwrap();
        fs::create_dir_all(state_directory).unwrap();
        let first = state_directory.join("first.json");
        let second = state_directory.join("second.json");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        assert!(has_managed_configs_in(root.join("install").as_path()).unwrap());
        fs::remove_file(first).unwrap();
        assert!(has_managed_configs_in(root.join("install").as_path()).unwrap());
        fs::remove_file(second).unwrap();
        assert!(!has_managed_configs_in(root.join("install").as_path()).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
