//! Content-addressed Minecraft assets: bounded parallel requests and persistent
//! completed chunks. A cancelled launch must not discard a nearly finished song.
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use std::time::Duration;

use futures_util::{StreamExt, TryStreamExt};
use reqwest::{Client, header::{ACCEPT_ENCODING, CONTENT_RANGE, RANGE}};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::services::game::{core::sha1_hex_of_file, state::CANCEL_DOWNLOAD};

pub(crate) const CHUNK: u64 = 1 << 20;
static CONNECTIONS: Semaphore = Semaphore::const_new(16);
static NEXT: AtomicU64 = AtomicU64::new(0);
type Progress = Arc<dyn Fn(u64) + Send + Sync>;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

async fn until_cancelled(follow_launch: bool) {
    if !follow_launch { std::future::pending::<()>().await; }
    loop {
        if CANCEL_DOWNLOAD.load(Ordering::SeqCst) { return; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn scratch(path: &Path) -> Scratch {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".incoming-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    Scratch(name.into())
}

async fn part_attempt(client: &Client, url: &str, path: &Path, start: u64, length: u64, size: u64, timeout: Duration) -> Result<(), String> {
    let _permit = CONNECTIONS.acquire().await.map_err(|e| e.to_string())?;
    let end = start + length - 1;
    let response = client.get(url).header(RANGE, format!("bytes={start}-{end}"))
        .header(ACCEPT_ENCODING, "identity").timeout(timeout).send().await.map_err(|e| e.to_string())?;
    let expected = format!("bytes {start}-{end}/{size}");
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || response.headers().get(CONTENT_RANGE).and_then(|h| h.to_str().ok()).map(str::trim) != Some(expected.as_str()) {
        return Err(format!("Сервер не подтвердил диапазон {expected}: HTTP {}", response.status()));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if bytes.len() as u64 + chunk.len() as u64 > length { return Err("Слишком большой ответ на запрос части файла".into()); }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() as u64 != length { return Err("Часть ресурса получена не полностью".into()); }
    let tmp = scratch(path);
    tokio::fs::write(&tmp.0, bytes).await.map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp.0, path).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn part(client: &Client, urls: &[String], path: &Path, start: u64, length: u64, size: u64, timeout: Duration, follow_launch: bool) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if follow_launch && CANCEL_DOWNLOAD.load(Ordering::SeqCst) { return Err("Загрузка отменена пользователем".into()); }
        let url = &urls[attempt % urls.len()];
        let result = tokio::select! {
            result = part_attempt(client, url, path, start, length, size, timeout) => result,
            _ = until_cancelled(follow_launch) => return Err("Загрузка отменена пользователем".into()),
        };
        match result {
            Ok(()) => { crate::services::game::mirrors::note_success(url); return Ok(()); }
            Err(error) => {
                crate::services::game::mirrors::note_failure(url);
                last = error;
            }
        }
    }
    Err(format!("Не удалось скачать часть ресурса {start}: {last}. Готовые части сохранены для докачки"))
}

pub(crate) async fn download(client: &Client, url: &str, path: &Path, hash: &str, size: u64, progress: Progress) -> Result<u64, String> {
    download_from(client, &crate::services::game::mirrors::candidates(url), path, hash, size, Duration::from_secs(20), true, progress).await
}

pub(crate) async fn background(client: &Client, url: &str, path: &Path, hash: &str, size: u64) -> Result<u64, String> {
    download_from(client, &crate::services::game::mirrors::candidates(url), path, hash, size, Duration::from_secs(45), false, Arc::new(|_| {})).await
}

async fn download_from(client: &Client, urls: &[String], path: &Path, hash: &str, size: u64, timeout: Duration, follow_launch: bool, progress: Progress) -> Result<u64, String> {
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || size == 0 || urls.is_empty() {
        return Err("Некорректное описание ресурса Minecraft".into());
    }
    if path.is_file() && sha1_hex_of_file(path).await? == hash { progress(size); return Ok(0); }
    let parent = path.parent().ok_or("Не задана папка ресурса")?;
    let parts = parent.join(format!(".{hash}-{size}.parts"));
    tokio::fs::create_dir_all(&parts).await.map_err(|e| e.to_string())?;
    let count = size.div_ceil(CHUNK);
    let mut missing = Vec::new();
    for i in 0..count {
        let start = i * CHUNK;
        let length = CHUNK.min(size - start);
        let part_path = parts.join(format!("{i}.part"));
        if tokio::fs::metadata(&part_path).await.is_ok_and(|m| m.is_file() && m.len() == length) {
            progress(length);
        } else { missing.push((part_path, start, length)); }
    }
    let transferred: u64 = missing.iter().map(|(_, _, length)| *length).sum();
    futures_util::stream::iter(missing.into_iter().map(|(part_path, start, length)| {
        let progress = progress.clone();
        async move {
            part(client, urls, &part_path, start, length, size, timeout, follow_launch).await?;
            progress(length);
            Ok::<(), String>(())
        }
    })).buffer_unordered(4).try_collect::<Vec<_>>().await?;
    if follow_launch && CANCEL_DOWNLOAD.load(Ordering::SeqCst) { return Err("Загрузка отменена пользователем".into()); }
    let tmp = scratch(path);
    let mut output = tokio::fs::File::create(&tmp.0).await.map_err(|e| e.to_string())?;
    for i in 0..count {
        let mut input = tokio::fs::File::open(parts.join(format!("{i}.part"))).await.map_err(|e| e.to_string())?;
        tokio::io::copy(&mut input, &mut output).await.map_err(|e| e.to_string())?;
    }
    output.flush().await.map_err(|e| e.to_string())?;
    drop(output);
    if sha1_hex_of_file(&tmp.0).await? != hash {
        // Only our known chunk names are invalidated; unrelated files stay put.
        for i in 0..count { let _ = tokio::fs::remove_file(parts.join(format!("{i}.part"))).await; }
        return Err("SHA1 ресурса Minecraft не совпал. Повреждённые части сброшены".into());
    }
    tokio::fs::rename(&tmp.0, path).await.map_err(|e| e.to_string())?;
    for i in 0..count { let _ = tokio::fs::remove_file(parts.join(format!("{i}.part"))).await; }
    let _ = tokio::fs::remove_dir(parts).await;
    Ok(transferred)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, atomic::AtomicBool};
    use tokio::io::AsyncReadExt;
    use sha1::{Digest, Sha1};

    async fn server(data: Arc<Vec<u8>>, stalled: Arc<AtomicBool>, corrupt: bool) -> (String, Arc<Mutex<Vec<u64>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/asset", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (data, stalled, log) = (data.clone(), stalled.clone(), log.clone());
                tokio::spawn(async move {
                    let mut buf = [0; 8192];
                    let n = socket.read(&mut buf).await.unwrap();
                    let text = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    let range = text.lines().find_map(|l| l.strip_prefix("range: bytes=")).unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let (start, end): (u64,u64) = (start.trim().parse().unwrap(), end.trim().parse().unwrap());
                    log.lock().unwrap().push(start);
                    if start > 0 && stalled.load(Ordering::SeqCst) { tokio::time::sleep(Duration::from_secs(5)).await; return; }
                    let mut bytes = data[start as usize..=end as usize].to_vec();
                    if corrupt { bytes[0] ^= 1; }
                    let headers = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len(), bytes.len());
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(&bytes).await;
                });
            }
        });
        (url, requests, handle)
    }

    fn root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mc16-range-{label}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
    }

    #[tokio::test]
    async fn cancellation_preserves_completed_chunks_and_resume_verifies_whole_file() {
        let data = Arc::new(vec![37; (CHUNK * 2 + 137) as usize]);
        let hash = format!("{:x}", Sha1::digest(data.as_slice()));
        let stalled = Arc::new(AtomicBool::new(true));
        let (url, requests, server) = server(data.clone(), stalled.clone(), false).await;
        let root = root("resume"); let path = root.join(&hash);
        let ready = Arc::new(tokio::sync::Notify::new()); let signal = ready.clone();
        let urls = vec![url]; let client = Client::builder().no_proxy().build().unwrap();
        let (c, u, p, h) = (client.clone(), urls.clone(), path.clone(), hash.clone());
        let size = data.len() as u64;
        let run = tokio::spawn(async move { download_from(&c, &u, &p, &h, size, Duration::from_secs(10), true, Arc::new(move |_| signal.notify_one())).await });
        tokio::time::timeout(Duration::from_secs(2), ready.notified()).await.unwrap();
        run.abort(); let _ = run.await;
        stalled.store(false, Ordering::SeqCst);
        download_from(&client, &urls, &path, &hash, size, Duration::from_secs(2), true, Arc::new(|_| {})).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), *data);
        assert_eq!(requests.lock().unwrap().iter().filter(|&&start| start == 0).count(), 1, "A completed chunk must not be downloaded again");
        server.abort(); std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn corrupt_parts_never_replace_an_asset() {
        let data = Arc::new(vec![19; CHUNK as usize]);
        let hash = format!("{:x}", Sha1::digest(data.as_slice()));
        let (url, _, server) = server(data.clone(), Arc::new(AtomicBool::new(false)), true).await;
        let root = root("corrupt"); let path = root.join(&hash);
        let result = download_from(&Client::builder().no_proxy().build().unwrap(), &[url], &path, &hash, data.len() as u64, Duration::from_secs(2), true, Arc::new(|_| {})).await;
        assert!(result.unwrap_err().contains("SHA1")); assert!(!path.exists());
        server.abort(); std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "Manual preparation of public Minecraft assets; requires MILLIDA_ASSET_GAME_ROOT"]
    async fn prepare_millida_assets() {
        use crate::services::game::{core::file_len_matches, version_types::AssetIndexJson};
        let root = PathBuf::from(std::env::var_os("MILLIDA_ASSET_GAME_ROOT").expect("game cache path required"));
        assert!(root.ends_with(Path::new("engine16").join("game")));
        crate::bridge::initialize(root.parent().unwrap(), &root).unwrap();
        let index: AssetIndexJson = serde_json::from_slice(&std::fs::read(root.join("assets/indexes/17.json")).unwrap()).unwrap();
        let mut seen = std::collections::HashSet::new();
        let jobs: Vec<_> = index.objects.into_values().filter(|a| seen.insert(a.hash.clone())).filter_map(|a| {
            let path = root.join("assets/objects").join(&a.hash[..2]).join(&a.hash);
            (!file_len_matches(&path, a.size)).then_some((a, path))
        }).collect();
        let total: u64 = jobs.iter().map(|(a,_)| a.size).sum();
        println!("Missing public assets: {}; {:.1} MiB", jobs.len(), total as f64 / 1048576.0);
        let done = Arc::new(AtomicU64::new(0)); let start = std::time::Instant::now();
        let client = crate::bridge::binary_client();
        futures_util::stream::iter(jobs.into_iter().map(|(asset,path)| {
            let (client, done) = (client.clone(), done.clone());
            async move {
                let url = format!("https://resources.download.minecraft.net/{}/{}", &asset.hash[..2], asset.hash);
                let progress = done.clone();
                download_from(&client, &crate::services::game::mirrors::candidates(&url), &path, &asset.hash, asset.size, Duration::from_secs(45), false, Arc::new(move |n| { progress.fetch_add(n,Ordering::SeqCst); })).await?;
                println!("Asset SHA1 verified; {:.1}/{:.1} MiB", done.load(Ordering::SeqCst) as f64 / 1048576.0, total as f64 / 1048576.0);
                Ok::<(),String>(())
            }
        })).buffer_unordered(2).try_collect::<Vec<_>>().await.unwrap();
        println!("All missing assets verified in {:.1}s", start.elapsed().as_secs_f64());
    }
}
