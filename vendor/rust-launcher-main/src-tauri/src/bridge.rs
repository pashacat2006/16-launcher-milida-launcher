//! In-process adapter for a Millida UI. Credentials are supplied by its vault,
//! stay in memory, and are never written to 16Launcher's account files.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::Ordering;

use serde_json::json;
use sha1::{Digest, Sha1};
use tauri::AppHandle;

use crate::infra::fs_atomic::write_atomic;
use crate::services::game::{install, versions};

#[derive(Default)]
struct Roots {
    data: PathBuf,
    game: PathBuf,
    cache: Option<PathBuf>,
    profiles: HashMap<String, PathBuf>,
}

static ROOTS: OnceLock<Mutex<Roots>> = OnceLock::new();
pub static LAUNCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn initialize(data: &Path, game: &Path) -> Result<(), String> {
    let roots = ROOTS.get_or_init(|| Mutex::new(Roots::default()));
    let mut roots = roots.lock().map_err(|_| "16: paths lock poisoned")?;
    if !roots.data.as_os_str().is_empty() && (roots.data != data || roots.game != game) {
        return Err("Нельзя менять папку движка 16 во время работы приложения".into());
    }
    std::fs::create_dir_all(data).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(game).map_err(|e| e.to_string())?;
    roots.data = data.into();
    roots.game = game.into();
    Ok(())
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    ROOTS.get()?.lock().ok().map(|r| r.data.clone()).filter(|p| !p.as_os_str().is_empty())
}
pub(crate) fn game_dir() -> Option<PathBuf> {
    ROOTS.get()?.lock().ok().map(|r| r.game.clone()).filter(|p| !p.as_os_str().is_empty())
}
pub(crate) fn profile_dir(id: &str) -> Option<PathBuf> {
    ROOTS.get()?.lock().ok()?.profiles.get(id).cloned()
}
pub(crate) fn metadata(id: &str, file: &str) -> Option<PathBuf> {
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') { return None; }
    Some(data_dir()?.join("profiles").join(id).join(file))
}

pub fn cache_source(source: &Path) {
    if let Some(roots) = ROOTS.get() {
        if let Ok(mut roots) = roots.lock() { roots.cache = source.canonicalize().ok(); }
    }
}

/// Reuse only the shared game cache. Never read mods, saves or account files.
/// Copy atomically: a downloader replacing its copy cannot change Millida's file.
pub(crate) fn seed_cached_file(dest: &Path) -> bool {
    use std::path::Component;
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if dest.exists() { return false; }
    let Some((game, source)) = ROOTS.get().and_then(|m| m.lock().ok())
        .and_then(|r| Some((r.game.clone(), r.cache.clone()?))) else { return false };
    let Ok(rel) = dest.strip_prefix(&game) else { return false };
    if rel.components().any(|p| !matches!(p, Component::Normal(_))) { return false; }
    let Some(first) = rel.components().next() else { return false };
    let mut from = if rel.components().count() == 1 && rel.extension().is_some_and(|e| e == "jar") {
        // Fabric/Quilt's 16 installer stores the vanilla client at game root;
        // Millida stores it beneath versions/<id>/<id>.jar.
        let Some(version) = rel.file_stem() else { return false };
        source.join("versions").join(version).join(rel)
    } else {
        if !["libraries", "assets", "versions"].iter().any(|s| first.as_os_str() == *s) { return false; }
        source.join(rel)
    };
    if !from.is_file() && first.as_os_str() == "versions" {
        if let Some(stem) = from.file_stem().and_then(|s| s.to_str()).map(str::to_owned) {
            if from.extension().is_some_and(|e| e == "json") { from.set_file_name(format!("{stem}-meta.json")); }
        }
    }
    let Ok(from) = from.canonicalize() else { return false };
    if !from.starts_with(&source) || !from.is_file() { return false; }
    let Some(parent) = dest.parent() else { return false };
    if std::fs::create_dir_all(parent).is_err() { return false; }
    let temp = parent.join(format!(".millida16-seed-{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let copied = std::fs::copy(from, &temp).is_ok() && std::fs::rename(&temp, dest).is_ok();
    let _ = std::fs::remove_file(&temp);
    copied
}

pub struct Profile<'a> {
    pub name: &'a str,
    pub game_version: &'a str,
    pub loader: &'a str,
    pub loader_version: Option<&'a str>,
    pub directory: &'a Path,
    pub ram_mb: u32,
    pub jvm_args: &'a str,
    pub java_path: Option<&'a Path>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub fn register(profile: Profile<'_>) -> Result<String, String> {
    let id = format!("millida-{:x}", Sha1::digest(profile.name.as_bytes()));
    let roots = ROOTS.get().ok_or("16: adapter is not initialized")?;
    roots.lock().map_err(|_| "16: paths lock poisoned")?.profiles.insert(id.clone(), profile.directory.into());
    let config_path = metadata(&id, "config.json").ok_or("16: profile path")?;
    let previous: serde_json::Value = std::fs::read(&config_path).ok()
        .and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let config = json!({
        "id": id, "name": profile.name, "game_version": profile.game_version,
        "loader": profile.loader, "loader_version": profile.loader_version,
        "created_at": previous["created_at"].as_u64().unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()),
        "play_time_seconds": previous["play_time_seconds"].as_u64().unwrap_or(0),
        "last_played_at": previous["last_played_at"].as_u64()
    });
    write_atomic(&config_path, serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?)?;
    let settings = json!({"ram_mb": profile.ram_mb,
        "java_settings": {"java_path": profile.java_path.map(|p| p.to_string_lossy()),
            "use_custom_jvm_args": !profile.jvm_args.is_empty(), "jvm_args": profile.jvm_args},
        "resolution_width": profile.width.filter(|w| *w > 0),
        "resolution_height": profile.height.filter(|h| *h > 0),
        "close_launcher_on_game_start": false, "show_console_on_launch": false});
    write_atomic(&metadata(&id, "settings.json").ok_or("16: settings path")?, serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?)?;
    write_atomic(&crate::app::paths::selected_profile_path()?, serde_json::to_vec(&json!({"id": id})).map_err(|e| e.to_string())?)?;
    Ok(id)
}

/// Deliberately has no Serialize/Debug: session secrets must not reach the UI or logs.
pub struct LaunchAuth {
    pub nick: String,
    pub uuid: String,
    pub token: String,
    pub user_type: String,
    pub xuid: String,
    pub jvm_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub fn pid() -> u32 {
    crate::services::game::state::GAME_PROCESS_PID.load(Ordering::SeqCst) as u32
}
pub fn cancel() { crate::services::game::state::CANCEL_DOWNLOAD.store(true, Ordering::SeqCst); }

pub fn binary_client() -> reqwest::Client {
    crate::infra::http::http_client_for_binary_download(false)
}

pub fn continue_downloads(app: AppHandle) {
    crate::services::game::runtime::deferred::start(app);
}

pub async fn prepare(app: AppHandle, version: &str, loader: &str, build: Option<&str>) -> Result<String, String> {
    crate::services::game::state::CANCEL_DOWNLOAD.store(false, Ordering::SeqCst);
    seed_cached_file(&crate::app::paths::versions_dir()?.join(version).join(format!("{version}.json")));
    let key = format!("{:x}", Sha1::digest(format!("{version}|{loader}|{}", build.unwrap_or("")).as_bytes()));
    let stamp = data_dir().ok_or("16: adapter is not initialized")?.join("installed").join(format!("{key}.json"));
    if let Ok(bytes) = std::fs::read(&stamp) {
        if let Ok(id) = serde_json::from_slice::<String>(&bytes) {
            let dir = crate::app::paths::versions_dir()?.join(&id);
            if dir.join(format!("{id}.json")).is_file() || dir.join("profile.json").is_file() { return Ok(id); }
        }
    }
    let pin = build.filter(|s| !s.trim().is_empty());
    let id = match loader {
        "fabric" => {
            let builds = if pin.is_none() { versions::fetch_fabric_loaders(version.into()).await? } else { Vec::new() };
            let selected = pin.map(str::to_owned).or_else(|| builds.first().map(|b| b.version.clone())).ok_or("Нет версии Fabric")?;
            install::install_fabric(app, version.into(), selected).await?
        }
        "quilt" => install::install_quilt(app, version.into(), pin.map(str::to_owned)).await?,
        "forge" => {
            let builds = if pin.is_none() { versions::fetch_forge_builds_for_game(version.into()).await? } else { Vec::new() };
            let selected = pin.map(str::to_owned).or_else(|| builds.first().map(|b| b.version.clone())).ok_or("Нет версии Forge")?;
            let full = if selected.starts_with(&format!("{version}-")) { selected } else { format!("{version}-{selected}") };
            let id = format!("{version}-forge-{}", full.trim_start_matches(&format!("{version}-")));
            install::install_forge(app, id.clone(), format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{full}/forge-{full}-installer.jar")).await?;
            id
        }
        "neoforge" => {
            let builds = if pin.is_none() { versions::fetch_neoforge_builds_for_game(version.into()).await? } else { Vec::new() };
            let selected = pin.map(str::to_owned).or_else(|| builds.first().map(|b| b.version.clone())).ok_or("Нет версии NeoForge")?;
            let id = format!("{version}-neoforge-{selected}");
            install::install_neoforge(app, id.clone()).await?;
            id
        }
        "vanilla" => {
            if crate::services::game::core::read_local_version(&crate::app::paths::versions_dir()?, version).is_some() {
                install::install_local_version(app, version.into()).await?;
            } else {
                let all = versions::fetch_all_versions().await?;
                let entry = all.into_iter().find(|v| v.id == version).ok_or("Версия Minecraft не найдена")?;
                install::install_version(app, version.into(), entry.url).await?;
            }
            version.into()
        }
        _ => return Err(format!("Движок 16 не поддерживает загрузчик {loader}")),
    };
    write_atomic(&stamp, serde_json::to_vec(&id).map_err(|e| e.to_string())?)?;
    Ok(id)
}

pub async fn launch(app: AppHandle, version_id: String, server: Option<String>, world: Option<String>, auth: LaunchAuth) -> Result<(), String> {
    crate::services::game::launcher::launch::launch_game_impl(app, version_id, None, server, world, Some(auth)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::profile::{InstanceConfig, InstanceSettings};

    #[test]
    fn adoption_preserves_profile_contents_and_isolates_engine_metadata() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("millida16-test-{}-{unique}", std::process::id()));
        let original = root.join("existing-millida-profile");
        let data = root.join("engine16");
        std::fs::create_dir_all(original.join("saves")).unwrap();
        std::fs::write(original.join("config.json"), b"original mod config").unwrap();
        std::fs::write(original.join("saves/world.dat"), b"world contents").unwrap();
        initialize(&data, &data.join("game")).unwrap();
        let id = register(Profile {
            name: "Food 1.21.1", game_version: "1.21.1", loader: "fabric", loader_version: Some("0.16.14"),
            directory: &original, ram_mb: 4096, jvm_args: "-XX:+UseG1GC", java_path: None,
            width: Some(1280), height: Some(720),
        }).unwrap();
        assert_eq!(crate::services::game::profiles::selected_instance_dir(), Some(original.clone()));
        let config_path = crate::app::paths::instance_config_path(&id).unwrap();
        assert!(config_path.starts_with(&data));
        let config: InstanceConfig = serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
        assert_eq!(config.loader_version.as_deref(), Some("0.16.14"));
        let (_, settings): (String, InstanceSettings) = crate::services::game::profiles::load_selected_instance_settings().unwrap().unwrap();
        assert_eq!(settings.ram_mb, Some(4096));
        assert_eq!(settings.java_settings.unwrap().jvm_args.as_deref(), Some("-XX:+UseG1GC"));
        assert_eq!(settings.resolution_width, Some(1280));
        assert_eq!(std::fs::read(original.join("config.json")).unwrap(), b"original mod config");
        assert_eq!(std::fs::read(original.join("saves/world.dat")).unwrap(), b"world contents");
        assert!(!data.join("profile.json").exists(), "account credentials must not be persisted by adoption");
        assert!(!data.join("accounts.json").exists());
        let cache = root.join("original-cache");
        std::fs::create_dir_all(cache.join("assets/objects/ab")).unwrap();
        std::fs::write(cache.join("assets/objects/ab/abcdef"), b"cached asset").unwrap();
        std::fs::create_dir_all(cache.join("versions/1.21.1")).unwrap();
        std::fs::write(cache.join("versions/1.21.1/1.21.1.jar"), b"cached vanilla client").unwrap();
        std::fs::write(cache.join("accounts.json"), b"must stay private").unwrap();
        cache_source(&cache);
        let adopted = data.join("game/assets/objects/ab/abcdef");
        assert!(seed_cached_file(&adopted));
        let client = data.join("game/1.21.1.jar");
        assert!(seed_cached_file(&client));
        assert_eq!(std::fs::read(client).unwrap(), b"cached vanilla client");
        assert!(!seed_cached_file(&data.join("game/accounts.json")));
        std::fs::write(&adopted, b"engine replaces its copy").unwrap();
        assert_eq!(std::fs::read(cache.join("assets/objects/ab/abcdef")).unwrap(), b"cached asset");
        assert!(initialize(&root.join("other"), &root.join("other/game")).is_err());
        // Only this uniquely created test directory is removed.
        std::fs::remove_dir_all(root).unwrap();
    }
}
