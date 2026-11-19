//! Music is opened lazily by Minecraft. Missing music may be filled after Java
//! starts; textures, language, sound effects and other assets remain mandatory.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Mutex, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

struct Music { path: PathBuf, hash: String, size: u64 }
static QUEUE: Mutex<VecDeque<Music>> = Mutex::new(VecDeque::new());
static RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) fn is_music(name: &str) -> bool {
    name.starts_with("minecraft/sounds/music/") && name.ends_with(".ogg")
}

pub(crate) fn queue(path: PathBuf, hash: String, size: u64) {
    if let Ok(mut queue) = QUEUE.lock() {
        if !queue.iter().any(|m| m.path == path) { queue.push_back(Music { path, hash, size }); }
    }
}

struct Running { released: bool }
impl Drop for Running { fn drop(&mut self) { if !self.released { RUNNING.store(false, Ordering::SeqCst); } } }

pub(crate) fn start(app: AppHandle) {
    if RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() { return; }
    tauri::async_runtime::spawn(async move {
        let mut running = Running { released: false };
        let client = crate::bridge::binary_client();
        loop {
            let next = QUEUE.lock().ok().and_then(|mut q| {
                let next = q.pop_front();
                if next.is_none() {
                    RUNNING.store(false, Ordering::SeqCst);
                    running.released = true;
                }
                next
            });
            let Some(music) = next else { break };
            let cached = music.path.clone();
            let _ = tokio::task::spawn_blocking(move || crate::bridge::seed_cached_file(&cached)).await;
            let url = format!("https://resources.download.minecraft.net/{}/{}", &music.hash[..2], music.hash);
            match super::ranged::background(&client, &url, &music.path, &music.hash, music.size).await {
                Ok(_) => {
                    let remaining = QUEUE.lock().map(|q| q.len()).unwrap_or(0);
                    let _ = app.emit("background-music-progress", serde_json::json!({"remaining": remaining}));
                }
                Err(error) => {
                    eprintln!("[Music] Фоновая докачка: {error}");
                    if let Ok(mut queue) = QUEUE.lock() { queue.push_back(music); }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_music_can_be_deferred() {
        assert!(is_music("minecraft/sounds/music/game/end/alpha.ogg"));
        assert!(!is_music("minecraft/sounds/ambient/cave/cave1.ogg"));
        assert!(!is_music("minecraft/sounds/random/click.ogg"));
        assert!(!is_music("minecraft/lang/ru_ru.json"));
        assert!(!is_music("minecraft/textures/gui/title/minecraft.png"));
    }
}
