//! Browser-backed copies of recently opened drawings.
//!
//! A web file picker exposes bytes and a display name, not a reusable native
//! path. Keep the last-opened copy in the origin-private file system (OPFS) so
//! the Start page can reopen it without asking the user to pick it again.

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

// WKWebView's OPFS writable stream can terminate its Networking process.
// Embedded Metal uses native, atomic cache files for drawings AND thumbnails.
enum RecentDirectory {
    Desktop(js_sys::Function),
    Browser(web_sys::FileSystemDirectoryHandle),
}

async fn desktop_entry(
    bridge: &js_sys::Function,
    action: &str,
    key: &str,
    bytes: Option<&[u8]>,
) -> Result<JsValue, String> {
    let payload = bytes
        .map(|bytes| JsValue::from(js_sys::Uint8Array::from(bytes)))
        .unwrap_or(JsValue::UNDEFINED);
    let promise = bridge
        .call3(
            &JsValue::NULL,
            &JsValue::from_str(action),
            &JsValue::from_str(key),
            &payload,
        )
        .map_err(js_error)?;
    JsFuture::from(js_sys::Promise::resolve(&promise))
        .await
        .map_err(js_error)
}

async fn remove_entry(directory: &RecentDirectory, key: &str) -> Result<(), String> {
    match directory {
        RecentDirectory::Desktop(bridge) => {
            desktop_entry(bridge, "remove", key, None).await.map(|_| ())
        }
        RecentDirectory::Browser(directory) => JsFuture::from(directory.remove_entry(key))
            .await
            .map(|_| ())
            .map_err(js_error),
    }
}

struct OpenStoreState {
    latest_id: u64,
    latest_bytes: std::sync::Arc<[u8]>,
    active_writers: usize,
}

thread_local! {
    static OPEN_STORES: std::cell::RefCell<
        std::collections::HashMap<String, OpenStoreState>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

const RECENT_DIRECTORY: &str = "opencadstudio-recent";
const THUMBNAIL_MAGIC: &[u8; 4] = b"OCST";
const THUMBNAIL_MAX_DIM: u32 = 256;

pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub async fn store(name: &str, bytes: &[u8]) -> Result<(), String> {
    let directory = recent_directory(true).await?;
    write_entry(&directory, &cache_key(name), bytes).await?;

    if let Some(image) = dwg_thumbnailer::extract_bytes(bytes, THUMBNAIL_MAX_DIM) {
        let thumbnail = encode_thumbnail(image);
        // The drawing copy is the durable part of a recent entry. Thumbnail
        // caching must not make an otherwise successful open/save fail under
        // browser quota pressure.
        let _ = write_entry(&directory, &thumbnail_key(name), &thumbnail).await;
    } else {
        // An overwrite may replace a drawing that had a preview with one that
        // does not. Do not leave the old image attached to the new file.
        let _ = remove_entry(&directory, &thumbnail_key(name)).await;
    }
    Ok(())
}

pub async fn store_open(
    name: &str,
    bytes: std::sync::Arc<[u8]>,
    open_id: u64,
) -> Result<(), String> {
    let key = name.to_string();
    let (mut pending_id, mut pending_bytes) = OPEN_STORES.with(|stores| {
        let mut stores = stores.borrow_mut();
        let state = stores.entry(key.clone()).or_insert_with(|| OpenStoreState {
            latest_id: open_id,
            latest_bytes: std::sync::Arc::clone(&bytes),
            active_writers: 0,
        });
        state.active_writers = state.active_writers.saturating_add(1);
        if open_id > state.latest_id {
            state.latest_id = open_id;
            state.latest_bytes = bytes;
        }
        (state.latest_id, std::sync::Arc::clone(&state.latest_bytes))
    });
    loop {
        if let Err(error) = store(name, &pending_bytes).await {
            OPEN_STORES.with(|stores| {
                let mut stores = stores.borrow_mut();
                let remove = if let Some(state) = stores.get_mut(&key) {
                    state.active_writers = state.active_writers.saturating_sub(1);
                    state.active_writers == 0
                } else {
                    false
                };
                if remove {
                    stores.remove(&key);
                }
            });
            return Err(error);
        }

        let latest = OPEN_STORES.with(|stores| {
            let mut stores = stores.borrow_mut();
            let Some(state) = stores.get_mut(&key) else {
                return None;
            };
            if state.latest_id != pending_id {
                return Some((
                    state.latest_id,
                    std::sync::Arc::clone(&state.latest_bytes),
                ));
            }
            state.active_writers = state.active_writers.saturating_sub(1);
            if state.active_writers == 0 {
                stores.remove(&key);
            }
            None
        });
        match latest {
            Some((latest_id, latest_bytes)) => {
                pending_id = latest_id;
                pending_bytes = latest_bytes;
            }
            None => return Ok(()),
        }
    }
}

async fn write_entry(directory: &RecentDirectory, key: &str, bytes: &[u8]) -> Result<(), String> {
    let directory = match directory {
        RecentDirectory::Desktop(bridge) => {
            return desktop_entry(bridge, "write", key, Some(bytes))
                .await
                .map(|_| ())
        }
        RecentDirectory::Browser(directory) => directory,
    };
    let options = web_sys::FileSystemGetFileOptions::new();
    options.set_create(true);
    let handle = JsFuture::from(directory.get_file_handle_with_options(key, &options))
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::FileSystemFileHandle>()
        .map_err(js_error)?;
    let writable = JsFuture::from(handle.create_writable())
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::FileSystemWritableFileStream>()
        .map_err(js_error)?;
    for chunk in bytes.chunks(1024 * 1024) {
        let owned = js_sys::Uint8Array::from(chunk);
        let write = writable.write_with_js_u8_array(&owned).map_err(js_error)?;
        JsFuture::from(write).await.map_err(js_error)?;
    }
    JsFuture::from(writable.close()).await.map_err(js_error)?;
    Ok(())
}

pub async fn read(name: &str) -> Result<Vec<u8>, String> {
    let directory = recent_directory(false).await?;
    read_entry(&directory, &cache_key(name)).await
}

async fn read_entry(directory: &RecentDirectory, key: &str) -> Result<Vec<u8>, String> {
    let directory = match directory {
        RecentDirectory::Desktop(bridge) => {
            match desktop_entry(bridge, "read", key, None).await {
                Ok(buffer) => return Ok(js_sys::Uint8Array::new(&buffer).to_vec()),
                Err(error) if error.contains("CAD cache entry not found") => {
                    // Keep existing recent drawings recoverable. Legacy OPFS is
                    // read-only: migration writes exclusively through native IPC.
                    let legacy = browser_directory(false).await?;
                    let file = get_file(&legacy, key).await?;
                    let bytes = read_blob(file.as_ref()).await?;
                    let _ = desktop_entry(bridge, "write", key, Some(&bytes)).await;
                    return Ok(bytes);
                }
                Err(error) => return Err(error),
            }
        }
        RecentDirectory::Browser(directory) => directory,
    };
    let file = get_file(directory, key).await?;
    read_blob(file.as_ref()).await
}

async fn get_file(
    directory: &web_sys::FileSystemDirectoryHandle,
    key: &str,
) -> Result<web_sys::File, String> {
    let handle = JsFuture::from(directory.get_file_handle(key))
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::FileSystemFileHandle>()
        .map_err(js_error)?;
    JsFuture::from(handle.get_file())
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::File>()
        .map_err(js_error)
}

async fn read_blob(blob: &web_sys::Blob) -> Result<Vec<u8>, String> {
    let buffer = JsFuture::from(blob.array_buffer())
        .await
        .map_err(js_error)?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

pub async fn read_thumbnail(name: &str) -> Result<Option<Thumbnail>, String> {
    let directory = recent_directory(false).await?;
    match read_entry(&directory, &thumbnail_key(name)).await {
        Ok(bytes) => Ok(decode_thumbnail(bytes)),
        Err(_) => Ok(None),
    }
}

pub async fn remove(name: &str) -> Result<(), String> {
    let directory = recent_directory(false).await?;
    let drawing = remove_entry(&directory, &cache_key(name)).await;
    let _ = remove_entry(&directory, &thumbnail_key(name)).await;
    drawing.map(|_| ())
}

async fn recent_directory(create: bool) -> Result<RecentDirectory, String> {
    let window = web_sys::window().ok_or_else(|| "browser window unavailable".to_string())?;
    if let Ok(Some(parent)) = window.parent() {
        if let Ok(value) =
            js_sys::Reflect::get(parent.as_ref(), &JsValue::from_str("__METAL_RECENT_CAD"))
        {
            if let Some(bridge) = value.dyn_ref::<js_sys::Function>() {
                return Ok(RecentDirectory::Desktop(bridge.clone()));
            }
        }
        if js_sys::Reflect::has(parent.as_ref(), &JsValue::from_str("__TAURI_INTERNALS__"))
            .unwrap_or(false)
        {
            return Err("Metal native CAD cache bridge is unavailable".into());
        }
    }
    browser_directory(create)
        .await
        .map(RecentDirectory::Browser)
}

async fn browser_directory(create: bool) -> Result<web_sys::FileSystemDirectoryHandle, String> {
    let window = web_sys::window().ok_or_else(|| "browser window unavailable".to_string())?;
    let root = JsFuture::from(window.navigator().storage().get_directory())
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::FileSystemDirectoryHandle>()
        .map_err(js_error)?;
    let options = web_sys::FileSystemGetDirectoryOptions::new();
    options.set_create(create);
    JsFuture::from(root.get_directory_handle_with_options(RECENT_DIRECTORY, &options))
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::FileSystemDirectoryHandle>()
        .map_err(js_error)
}

/// Stable, short OPFS entry name. The original display name remains in
/// `AppConfig::recent`; only the browser-private cache uses this key.
fn cache_key(name: &str) -> String {
    format!("{}.cad", name_hash(name))
}

fn thumbnail_key(name: &str) -> String {
    format!("{}.thumb", name_hash(name))
}

fn name_hash(name: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn encode_thumbnail(image: dwg_thumbnailer::RgbaImage) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12 + image.as_raw().len());
    bytes.extend_from_slice(THUMBNAIL_MAGIC);
    bytes.extend_from_slice(&image.width().to_le_bytes());
    bytes.extend_from_slice(&image.height().to_le_bytes());
    bytes.extend_from_slice(image.as_raw());
    bytes
}

fn decode_thumbnail(bytes: Vec<u8>) -> Option<Thumbnail> {
    if bytes.get(..4)? != THUMBNAIL_MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?);
    let height = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?);
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    if width == 0
        || height == 0
        || width > THUMBNAIL_MAX_DIM
        || height > THUMBNAIL_MAX_DIM
        || bytes.len() != 12 + expected
    {
        return None;
    }
    Some(Thumbnail {
        width,
        height,
        rgba: bytes[12..].to_vec(),
    })
}

fn js_error(value: wasm_bindgen::JsValue) -> String {
    value
        .as_string()
        .unwrap_or_else(|| format!("browser storage error: {value:?}"))
}
