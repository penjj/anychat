use std::io::Read;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{AboutMetadata, CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{TrayIconBuilder, TrayIconEvent},
    Emitter, LogicalPosition, LogicalSize, Monitor, Manager, Webview, WebviewBuilder, WebviewUrl,
    webview::NewWindowResponse,
    WindowBuilder,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

mod tools;

use tools::{Tool, TOOLS};

const RAIL_LABEL: &str = "rail";
/// Separate window for the launch animation. It is transparent, covers the
/// whole screen and owns no chrome, so the app appears to materialise out of
/// the desktop instead of popping in as a rectangle.
const SPLASH_LABEL: &str = "splash";
/// Draggable rail bounds. The floor is the icon-only strip; below ICON_ONLY_BELOW
/// the labels are dropped rather than squeezed. The ceiling stops the sidebar
/// from eating the page on a small window.
const RAIL_W_MIN: f64 = 52.0;
const RAIL_W_MAX: f64 = 420.0;
const BAR_H: f64 = 40.0;
/// Width of the rail-page strip kept clear of the content webview so the drag
/// handle on the divider stays reachable. Native child views sit above the
/// parent page, so any overlap silently swallows pointer events.
const RESIZE_GRIP: f64 = 8.0;
/// Window corner radius. Kept next to the other chrome constants so the Rust
/// mask and the CSS `border-radius` are changed together, never separately.
const WINDOW_RADIUS: f64 = 20.0;

struct App {
    active: std::sync::Mutex<String>,
    /// Last DOM report from the rail, used by the smoke gate.
    rail: std::sync::Mutex<Option<String>>,
    /// Last DOM report from the settings overlay.
    settings_dom: std::sync::Mutex<Option<String>>,
    /// Whether the settings panel should be showing; the rail polls this on load.
    settings_open: std::sync::atomic::AtomicBool,
    /// Whether the active content webview is hidden behind the settings panel.
    content_hidden: std::sync::atomic::AtomicBool,
    /// Popup requests the content webviews made, and how many were honoured.
    /// Login buttons go through `window.open`; if these stay at zero the site
    /// never asked, and if they climb without a window the request was dropped.
    popups_seen: std::sync::atomic::AtomicUsize,
    popups_opened: std::sync::atomic::AtomicUsize,
    /// Set once the launch animation has handed over, so the main window is
    /// never revealed mid-animation.
    booted: std::sync::atomic::AtomicBool,
    /// Wall-clock instant each tool webview was last on screen. A hidden
    /// WKWebView keeps its whole render tree resident, so the only way to give
    /// the memory back is to destroy it -- and to do that fairly, we need to
    /// know how long each one has been sitting unused.
    last_shown: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// A rail context menu is showing. Blur auto-hide must not fire while it is
    /// up: the menu is a page element, and hiding the window would destroy it
    /// mid-click.
    menu_open: std::sync::atomic::AtomicBool,
    /// A summon arrived before the animation finished. Remembered instead of
    /// dropped: otherwise ⌥Space during startup would do nothing at all.
    wants_visible: std::sync::atomic::AtomicBool,
}

#[derive(Serialize, Deserialize, Clone)]
struct Layout {
    rail_width: f64,
    /// Page zoom for the tool webviews. AI chat UIs are wide three-column
    /// layouts, so 1.0 clipped them in a small window.
    #[serde(default = "default_zoom")]
    zoom: f64,
    /// Window size in logical points. Zero means "never saved", which is how a
    /// fresh install is told apart from one where the user chose a size.
    #[serde(default)]
    win_w: f64,
    #[serde(default)]
    win_h: f64,
    /// Pin-to-top is a user preference, not a transient window flag: without
    /// persisting it, every summon reset the window to unpinned and the button
    /// appeared to do nothing.
    #[serde(default)]
    always_on_top: bool,
    /// Colour theme: "auto" (follow the system), "light" or "dark". Stored as a
    /// string rather than an enum so a value written by a newer build does not
    /// fail to deserialize and reset the whole layout.
    #[serde(default = "default_theme")]
    theme: String,
}

fn default_theme() -> String {
    "auto".to_string()
}

fn default_zoom() -> f64 {
    0.8
}

impl Default for Layout {
    fn default() -> Self {
        // Start as the narrowest icon strip: the rail is a switcher, and the
        // labels are one drag away. ICON_ONLY_BELOW in app.js must stay above
        // this so the two agree on what "narrow" means.
        Layout {
            rail_width: 56.0,
            zoom: default_zoom(),
            win_w: 0.0,
            win_h: 0.0,
            always_on_top: false,
            theme: default_theme(),
        }
    }
}

/// Root for all app-owned state. `ANYCHAT_DATA_DIR` lets the gate run hermetically
/// instead of inheriting a developer's saved layout and icon cache.
fn data_root(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = match std::env::var("ANYCHAT_DATA_DIR") {
        Ok(custom) => PathBuf::from(custom),
        Err(_) => app.path().app_data_dir().ok()?,
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn layout_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = data_root(app)?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("layout.json"))
}

fn load_layout(app: &tauri::AppHandle) -> Layout {
    layout_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_layout(app: &tauri::AppHandle, layout: &Layout) {
    if let Some(path) = layout_path(app) {
        if let Ok(json) = serde_json::to_string_pretty(layout) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Whether the window is genuinely non-opaque. Used by the gate: `transparent`
/// in the config only permits alpha, it does not clear the background.
#[cfg(target_os = "macos")]
fn window_is_transparent(window: &tauri::Window) -> Option<(bool, bool)> {
    let ptr = window.ns_window().ok()?;
    unsafe {
        let ns_window: &objc2_app_kit::NSWindow = &*(ptr as *mut objc2_app_kit::NSWindow);
        // alphaComponent is what actually decides whether the resize flash is
        // visible: an opaque colour with alpha 0 still composites as clear.
        let alpha = ns_window.backgroundColor().alphaComponent();
        Some((!ns_window.isOpaque(), alpha < 0.01))
    }
}

/// Window size in logical points, which is the unit the layout math uses.
fn logical_window_size(app: &tauri::AppHandle) -> (f64, f64) {
    let Some(window) = app.get_window("main") else { return (0.0, 0.0) };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return (0.0, 0.0);
    };
    let logical: LogicalSize<f64> = size.to_logical(scale);
    (logical.width, logical.height)
}

fn rail_width(app: &tauri::AppHandle) -> f64 {
    load_layout(app).rail_width.clamp(RAIL_W_MIN, RAIL_W_MAX)
}

#[derive(Serialize, Clone)]
struct ToolView {
    id: &'static str,
    name: &'static str,
    group: &'static str,
    url: &'static str,
    new_chat: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    /// Times the user switched to this tool; the rail sorts by it, recency first.
    uses: u64,
    last_used: u64,
}


/* ---------------- favicons ---------------- */

/// Per-tool switch counters. Small JSON, written only when a tool is selected.
#[derive(Serialize, Deserialize, Clone, Default)]
struct UsageEntry {
    uses: u64,
    last_used: u64,
}

fn usage_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(data_root(app)?.join("usage.json"))
}

fn load_usage(app: &tauri::AppHandle) -> std::collections::HashMap<String, UsageEntry> {
    let Some(path) = usage_path(app) else { return Default::default() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn note_usage(app: &tauri::AppHandle, id: &str) {
    let mut usage = load_usage(app);
    let entry = usage.entry(id.to_string()).or_default();
    entry.uses += 1;
    entry.last_used = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(path) = usage_path(app) {
        if let Ok(json) = serde_json::to_string_pretty(&usage) {
            let _ = std::fs::write(path, json);
        }
    }
}

fn icon_cache_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = data_root(app)?.join("icons");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Cache-only lookup. Must stay fast: it runs on the main thread for every
/// state() call, so it never touches the network.
fn icon_cached(app: &tauri::AppHandle, id: &str) -> Option<String> {
    let file = icon_cache_path(app)?.join(format!("{id}.txt"));
    let cached = std::fs::read_to_string(file).ok()?;
    let cached = cached.trim().to_string();
    if cached.is_empty() {
        return None;
    }
    // Older builds wrote whatever came back, including HTML 404 pages, and the
    // cache was never revalidated: those entries stuck forever. Verify the bytes
    // now and treat a bad entry as a miss so it gets refetched.
    if !data_url_looks_like_icon(&cached) {
        return None;
    }
    Some(cached)
}

/// Checks the payload of a `data:` URL rather than trusting the stored MIME, since
/// the bug was a mislabelled HTML body saved as `image/x-icon`.
fn data_url_looks_like_icon(data_url: &str) -> bool {
    let Some((_, payload)) = data_url.split_once(",") else { return false };
    let Some(bytes) = base64_decode(payload.trim()) else { return false };
    looks_like_image(&bytes)
}

/// A stock Safari UA matching the running macOS major version.
fn safari_user_agent() -> &'static str {
    // Apple froze the platform token at 10_15_7 for compatibility, so a constant
    // is both accurate and cheaper than shelling out to sw_vers.
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15"
}

/// HTTP agent for icon fetches.
///
/// ureq prefers ALL_PROXY over HTTPS_PROXY when it auto-detects, and this host
/// sets ALL_PROXY to a socks5 URL while ureq is built without socks support:
/// every request then failed with a connect timeout while curl succeeded over
/// the http proxy. So pick an explicit HTTP proxy, ignoring socks entries.
fn icon_agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10));
    for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(raw) = std::env::var(key) {
            if raw.starts_with("socks") || raw.trim().is_empty() {
                continue;
            }
            if let Ok(proxy) = ureq::Proxy::new(&raw) {
                builder = builder.proxy(proxy);
                break;
            }
        }
    }
    builder.build()
}

/// Icons are cached as data URLs, so the cap bounds what lands in the DOM.
/// 48KB was far too small: real favicons run to a few hundred KB.
const ICON_MAX_BYTES: usize = 512 * 1024;

/// Fetch + cache one icon. Background threads only; this hits the network.
fn icon_fetch(app: &tauri::AppHandle, tool: &Tool) -> Option<String> {
    if let Some(cached) = icon_cached(app, tool.id) {
        return Some(cached);
    }
    let dir = icon_cache_path(app)?;
    let file = dir.join(format!("{}.txt", tool.id));
    let base = tool.url.parse::<tauri::Url>().ok()?;
    let origin = format!("{}://{}", base.scheme(), base.host_str()?);

    // Discover what the site actually declares. Guessing /favicon.ico fails for
    // most modern SPAs: they ship an SVG or a hashed /cdn/asset path instead.
    let mut candidates: Vec<String> = Vec::new();
    // Some sites (Claude, ChatGPT) reject non-browser user agents outright.
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
    // Ask for the site root, not the deep new-chat route: /new and /app are more
    // likely to bounce to a login page or render a bare SPA shell with no <link>.
    let home = format!("{origin}/");
    if let Ok(response) = icon_agent().get(&home)
        .set("User-Agent", ua)
        .set("Accept", "text/html,application/xhtml+xml")
        .timeout(std::time::Duration::from_secs(12))
        .call()
    {
        let mut raw = Vec::new();
        let _ = response.into_reader().take(512 * 1024).read_to_end(&mut raw);
        // Lossy: read_to_string discards the whole page on one bad byte, which
        // silently emptied the candidate list for otherwise reachable sites.
        let html = String::from_utf8_lossy(&raw).into_owned();
        for chunk in html.split('<') {
            let lower = chunk.to_lowercase();
            if !lower.starts_with("link") {
                continue;
            }
            let Some(rel) = extract_attr(chunk, "rel") else { continue };
            // rel is a space-separated token list and may carry suffixes such as
            // apple-touch-icon-precomposed; exact string matching missed those.
            let rel = rel.to_lowercase();
            let is_icon = rel.split_whitespace().any(|token| {
                token == "icon" || token == "shortcut" || token.starts_with("apple-touch-icon")
            });
            if !is_icon {
                continue;
            }
            if let Some(href) = extract_attr(chunk, "href") {
                let absolute = absolutize(&origin, &href);
                if !absolute.is_empty() && !candidates.contains(&absolute) {
                    candidates.push(absolute);
                }
            }
        }
    }
    // Third-party icon services as a last resort. Some hosts block direct
    // fetches (403/429) or are unreachable from this network entirely, yet a
    // public favicon mirror still has them.
    let host = base.host_str().unwrap_or("");
    if !host.is_empty() {
        for service in [
            format!("https://www.google.com/s2/favicons?domain={host}&sz=64"),
            format!("https://icons.duckduckgo.com/ip3/{host}.ico"),
        ] {
            if !candidates.contains(&service) {
                candidates.push(service);
            }
        }
    }

    // Fallbacks, in the order a browser would try them.
    for fallback in [
        format!("{origin}/favicon.ico"),
        format!("{origin}/favicon.svg"),
        format!("{origin}/apple-touch-icon.png"),
    ] {
        if !candidates.contains(&fallback) {
            candidates.push(fallback);
        }
    }

    for candidate in candidates {
        let Ok(response) = icon_agent().get(&candidate)
            .set("User-Agent", ua)
            .set("Referer", &origin)
            .set("Accept", "image/avif,image/webp,image/png,image/svg+xml,*/*")
            .timeout(std::time::Duration::from_secs(10))
            .call()
        else {
            continue;
        };
        let header_mime = response.header("content-type").unwrap_or("").to_string();
        let mut bytes = Vec::new();
        if response
            .into_reader()
            .take(ICON_MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() < 32
            || bytes.len() > ICON_MAX_BYTES
        {
            continue;
        }
        if !looks_like_image(&bytes) {
            continue;
        }
        let mime = classify_image(&bytes, &header_mime, &candidate);
        let data_url = format!("data:{};base64,{}", mime, base64_encode(&bytes));
        let _ = std::fs::write(&file, &data_url);
        return Some(data_url);
    }
    None
}

/// Pull one attribute out of a raw tag fragment, tolerating single or double quotes.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let needle = format!("{name}=");
    let at = lower.find(&needle)? + needle.len();
    let rest = &tag[at..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &rest[1..];
        let close = inner.find(quote)?;
        Some(inner[..close].to_string())
    } else {
        let end = rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

fn absolutize(origin: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with("//") {
        format!("https:{href}")
    } else if href.starts_with('/') {
        format!("{origin}{href}")
    } else if href.is_empty() || href.starts_with("data:") {
        String::new()
    } else {
        format!("{origin}/{href}")
    }
}

/// Sniff real image bytes. Sites return HTML 200s for missing icons, and a 404
/// page often contains `<svg` markup, so check the document root first.
fn looks_like_image(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || bytes.starts_with(&[0xFF, 0xD8])
        || bytes.starts_with(b"GIF8")
        || bytes.starts_with(&[0x00, 0x00, 0x01, 0x00])
        || bytes.starts_with(b"RIFF")
    {
        return true;
    }
    // Text formats: judge by the *root element*, not the first bytes. Real SVGs
    // routinely open with an XML declaration and an Adobe generator comment
    // before <svg>, and a 404 HTML page can also contain inline <svg> markup.
    // So skip declarations, comments and doctypes, then read the first tag.
    let probe = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_lowercase();
    let mut rest = probe.trim_start_matches('\u{feff}').trim_start();
    for _ in 0..8 {
        if let Some(after) = rest.strip_prefix("<?") {
            rest = after.split_once("?>").map(|(_, tail)| tail).unwrap_or("").trim_start();
        } else if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.split_once("-->").map(|(_, tail)| tail).unwrap_or("").trim_start();
        } else if let Some(after) = rest.strip_prefix("<!doctype") {
            rest = after.split_once('>').map(|(_, tail)| tail).unwrap_or("").trim_start();
        } else {
            break;
        }
    }
    rest.starts_with("<svg")
}

fn classify_image(bytes: &[u8], header: &str, url: &str) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        "image/x-icon"
    } else if bytes.iter().position(|b| !b.is_ascii_whitespace()).map(|i| bytes[i..].starts_with(b"<svg")).unwrap_or(false) {
        "image/svg+xml"
    } else if header.contains("svg") || url.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "image/png"
    }
}

/// Warm the cache in the background so the rail gets icons without blocking boot.
/// Each tool gets its own thread: 34 serial fetches at 4 s each is 2 minutes.
/// Fetches missing icons in one pass, with a small concurrency cap.
///
/// Deliberately no retry loop: a full round costs worst-case ~40s per tool, so
/// retrying inside a single launch just delays the ones that will succeed. The
/// cache is the retry mechanism -- whatever fails here is picked up next start.
fn prefetch_icons(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let queue = std::sync::Arc::new(std::sync::Mutex::new(TOOLS.iter().collect::<Vec<_>>()));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let queue = queue.clone();
            let app = app.clone();
            let done = done.clone();
            workers.push(std::thread::spawn(move || loop {
                let tool = { queue.lock().unwrap().pop() };
                let Some(tool) = tool else { break };
                if icon_fetch(&app, tool).is_some() {
                    let _ = app.emit_to(RAIL_LABEL, "icons-ready", ());
                }
                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
        let cached = TOOLS.iter().filter(|t| icon_cached(&app, t.id).is_some()).count();
        println!("icons: {cached} of {} cached", TOOLS.len());
    });
}


/// Minimal decoder mirroring `base64_encode`; only used to revalidate the cache.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for ch in input.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

#[derive(Serialize, Clone)]
struct StateDto {
    tools: Vec<ToolView>,
    active: String,
    active_name: String,
    new_chat: String,
    autostart: bool,
    shortcut_ok: bool,
    visible: bool,
    rail_width: f64,
    always_on_top: bool,
    settings_open: bool,
    icon_count: usize,
    zoom: f64,
    theme: String,
}

#[derive(Deserialize, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn partition_dir(app: &tauri::AppHandle, id: &str) -> Option<PathBuf> {
    let dir = data_root(app)?.join("partitions").join(id);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Stable 16-byte store id so macOS gives each tool its own WKWebsiteDataStore.
#[cfg(target_os = "macos")]
fn store_id(id: &str) -> [u8; 16] {
    // FNV-1a over the tool id, doubled to fill 16 bytes. Stability matters more
    // than collision resistance: it only has to separate our tools.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&h.to_be_bytes());
    out[8..].copy_from_slice(&h.rotate_left(32).to_be_bytes());
    out
}

fn build_tool_webview(app: &tauri::AppHandle, id: &str) -> Option<Webview> {
    let tool = tools::get(id)?;
    let label = format!("tool_{id}");
    if let Some(existing) = app.get_webview(&label) {
        return Some(existing);
    }
    let window = app.get_window("main")?;
    let url = tool.url.parse().ok()?;
    let mut builder = WebviewBuilder::new(&label, WebviewUrl::External(url));

    // Session storage, per platform. Never both on macOS: wry prefers the custom
    // data store and ignores `data_directory`, so mixing them looks like isolated
    // profiles while every tool quietly shares one store.
    #[cfg(target_os = "macos")]
    {
        builder = builder.data_store_identifier(store_id(id));
    }

    // Give the webview the same identity a real Safari presents. The WKWebView
    // default UA drops the "Version/x Safari/y" tail, which sites can spot; this
    // is honest -- we are a Safari engine -- and avoids gratuitous mismatches.
    builder = builder.user_agent(safari_user_agent());

    // Login flows (Google, GitHub, Apple, …) open a popup via `window.open`.
    // Tauri denies those by default, so the button appeared to do nothing at
    // all. Allow them: on macOS the popup shares the opener's configuration and
    // therefore its cookie jar, which is exactly what an OAuth handoff needs.
    //
    // Record the request: without a trace there is no way to tell "popup was
    // swallowed" from "the site never asked", and that ambiguity cost a full
    // debugging round on a login button that silently did nothing.
    // ⌘N has to work while the caret is inside this page. These are
    // cross-origin documents: the rail's keydown listener never fires for them,
    // and a menu accelerator does not apply either. Injecting a listener is the
    // only way, and it talks to a single whitelisted command.
    builder = builder.initialization_script(
        r#"document.addEventListener('keydown', function (e) {
             if (!(e.metaKey || e.ctrlKey) || e.shiftKey) return;
             if (e.key !== 'n' && e.key !== 'N') return;
             e.preventDefault();
             try { window.__TAURI_INTERNALS__.invoke('new_chat') } catch (_) {}
           }, true);
"#,
    );

    let counter = app.clone();
    builder = builder.on_new_window(move |url, _features| {
        use std::sync::atomic::Ordering::SeqCst;
        let state = counter.state::<App>();
        state.popups_seen.fetch_add(1, SeqCst);
        state.popups_opened.fetch_add(1, SeqCst);
        eprintln!("anychat: popup allowed {url}");
        NewWindowResponse::Allow
    });
    #[cfg(not(target_os = "macos"))]
    if let Some(dir) = partition_dir(app, id) {
        builder = builder.data_directory(dir);
    }
    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(rail_width(app), BAR_H),
            LogicalSize::new(800.0, 600.0),
        )
        .ok()?;
    let _ = webview.set_zoom(load_layout(app).zoom);
    let _ = webview.hide();
    Some(webview)
}

fn emit_state(app: &tauri::AppHandle) {
    let payload = state_dto(app);
    let _ = app.emit_to(RAIL_LABEL, "state", payload);
}


/// Show the window without activating the app or stealing keyboard focus.
/// `orderFrontRegardless` puts it on the user's current space but leaves the
/// active app untouched, which is the whole point of a ⌥Space summon.
#[cfg(target_os = "macos")]
/// Clip the window's content view to a rounded rectangle.
///
/// The rail page and the content webview are siblings inside the window, so a
/// CSS `border-radius` on the page rounds only its own box. Setting the mask on
/// the shared content view rounds everything, including native child views.
#[cfg(target_os = "macos")]
/// Clear the window's own backdrop so the desktop shows through.
///
/// `transparent(true)` in the builder only permits alpha; the window still has
/// an opaque NSWindow background, which is what painted a white sheet behind
/// the launch animation.
#[cfg(target_os = "macos")]
fn make_window_transparent(window: &tauri::Window) {
    let Ok(ptr) = window.ns_window() else { return };
    unsafe {
        let ns_window: &objc2_app_kit::NSWindow = &*(ptr as *mut objc2_app_kit::NSWindow);
        ns_window.setOpaque(false);
        ns_window.setBackgroundColor(Some(&objc2_app_kit::NSColor::clearColor()));
    }
}

#[cfg(target_os = "macos")]
fn round_window_corners(window: &tauri::Window, radius: f64) -> bool {
    make_window_transparent(window);
    let Ok(ptr) = window.ns_window() else { return false };
    unsafe {
        let ns_window: &objc2_app_kit::NSWindow = &*(ptr as *mut objc2_app_kit::NSWindow);

        // `decorations(false)` clears the style mask, which also drops
        // NSWindowStyleMaskResizable -- the window looked fixed-size because
        // AppKit had no resize edges left to drag. Put the bit back.
        ns_window.setStyleMask(
            ns_window.styleMask() | objc2_app_kit::NSWindowStyleMask::Resizable,
        );

        let Some(content) = ns_window.contentView() else { return false };
        content.setWantsLayer(true);
        match content.layer() {
            Some(layer) => {
                layer.setCornerRadius(radius);
                layer.setMasksToBounds(true);
                // Read it back: a silent no-op here would leave square corners
                // and there is no other way to notice.
                (layer.cornerRadius() - radius).abs() < 0.5 && layer.masksToBounds()
            }
            None => false,
        }
    }
}

#[cfg(target_os = "macos")]
fn order_front_without_activating(window: &tauri::Window) {
    use objc2::rc::Retained;
    use objc2_app_kit::NSWindow;
    let Ok(ptr) = window.ns_window() else { return };
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ns_window` returns the live NSWindow backing this Tauri window;
    // we only call a documented ordering selector on it.
    unsafe {
        let ns_window: &NSWindow = &*(ptr as *const NSWindow);
        ns_window.orderFrontRegardless();
        let _ = Retained::retain(ns_window as *const _ as *mut NSWindow);
    }
}

#[cfg(not(target_os = "macos"))]
fn order_front_without_activating(_window: &tauri::Window) {}

/// Build the full-screen transparent splash window.
///
/// It sits above the main window (which is still hidden), plays the animation,
/// then closes itself. Nothing here loads app state: it is a single canvas, so
/// it can paint before the rail or any tool webview exists.
/// Milliseconds since process start, for timing the launch animation.
fn boot_ms() -> u128 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let t0 = START.get_or_init(std::time::Instant::now);
    t0.elapsed().as_millis()
}

fn build_splash(app: &tauri::AppHandle, monitor: Option<Monitor>) -> tauri::Result<()> {
    eprintln!("anychat: splash opening at {}ms", boot_ms());
    let (pos, size) = match monitor {
        Some(m) => {
            let scale = m.scale_factor();
            let p: LogicalPosition<f64> = m.position().to_logical(scale);
            let s: LogicalSize<f64> = m.size().to_logical(scale);
            (p, s)
        }
        None => (LogicalPosition::new(0.0, 0.0), LogicalSize::new(1280.0, 800.0)),
    };

    let window = WindowBuilder::new(app, SPLASH_LABEL)
        .title("AnyChat")
        .position(pos.x, pos.y)
        .inner_size(size.width, size.height)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .focused(false)
        .build()?;

    #[cfg(target_os = "macos")]
    {
        // Only the backdrop matters here: the splash has no rounded edge of its
        // own, and forcing a mask on it would clip nothing useful.
        make_window_transparent(&window);
    }

    // `background_color` is what makes wry disable `drawsBackground` on the
    // WKWebView. A child webview does not inherit the window's transparency, so
    // without this the animation sits on a solid white sheet no matter how clear
    // the window itself is.
    let webview_builder = WebviewBuilder::new(SPLASH_LABEL, WebviewUrl::App("splash.html".into()))
        .background_color(tauri::window::Color(0, 0, 0, 0))
        // Belt and braces: the stylesheet already clears every layer, but the
        // webview's own backdrop is painted outside the document's reach, so
        // the root colour is forced here too.
        .initialization_script(
            r#"document.documentElement.style.background = 'transparent';
               if (document.body) document.body.style.background = 'transparent';"#,
        );
    let webview = window.add_child(
        webview_builder,
        LogicalPosition::new(0.0, 0.0),
        LogicalSize::new(size.width, size.height),
    )?;

    // WKWebView paints its own under-page backdrop, between the page and the
    // window. That layer is what made the splash look milky even though the
    // window itself was clear; this is the supported way to drop it.
    #[cfg(target_os = "macos")]
    {
        let _ = webview.with_webview(|w| {
            let ptr = w.inner();
            if ptr.is_null() {
                eprintln!("anychat: splash webview handle is null");
                return;
            }
            unsafe {
                let wk: &objc2_web_kit::WKWebView =
                    &*(ptr as *const objc2_web_kit::WKWebView);
                wk.setUnderPageBackgroundColor(None);
                wk.setUnderPageBackgroundColor(None);
                // Read back the private flag wry relies on. If it is still true
                // the white sheet is coming from the webview, not the window.
            }
        });
    }
    // Verify the backdrops are actually cleared; a silent failure here is what
    // makes the animation look milky instead of immersive.
    #[cfg(target_os = "macos")]
    {
        if let Ok(ptr) = window.ns_window() {
            unsafe {
                let ns: &objc2_app_kit::NSWindow = &*(ptr as *mut objc2_app_kit::NSWindow);
                let alpha = ns.backgroundColor().alphaComponent();
                eprintln!(
                    "anychat: splash opaque={} bgAlpha={alpha}",
                    ns.isOpaque()
                );
            }
        }
    }
    let _ = window.show();
    Ok(())
}

/// Reported once by the splash so the intro's density is a fact in the log
/// rather than something to eyeball. Cheap, and it makes a sparse word
/// diagnosable from a boot log alone.
#[tauri::command]
fn splash_stats(points: u32, step: u32, size: f64) {
    eprintln!(
        "anychat: splash glyph points={points} step={step} font={:.0}",
        size
    );
}

/// Called by the splash once its animation has finished. Closing the splash
/// first and only then revealing the main window avoids a frame where both are
/// composited on top of each other.
#[tauri::command]
fn splash_done(app: tauri::AppHandle) {
    eprintln!("anychat: splash done, handing over at {}ms", boot_ms());
    if let Some(splash) = app.get_window(SPLASH_LABEL) {
        let _ = splash.close();
    }
    if app.state::<App>().booted.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if std::env::var("ANYCHAT_DEMO_SHOW").is_ok() || app.state::<App>().wants_visible.load(std::sync::atomic::Ordering::SeqCst) {
        show_window(&app);
    }
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_window("main") {
        if window.is_visible().unwrap_or(false) {
            emit_state(app);
            return;
        }
        // Pin state is the user's choice and survives a summon. It used to be
        // cleared here, which made the pin button look broken: every ⌥Space
        // quietly undid it. Restore the persisted value instead.
        let _ = window.set_always_on_top(load_layout(app).always_on_top);
        // `window.show()` activates the app and takes keyboard focus. On macOS
        // the ordering selector has to do the work instead: the window becomes
        // visible without becoming key, which is the whole point of a summon.
        #[cfg(target_os = "macos")]
        {
            order_front_without_activating(&window);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = window.show();
        }
        emit_state(app);
    }
}

fn toggle_window(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering::SeqCst;
    // Before the animation hands over there is no main window to show yet.
    // Remember the request so the summon is not silently swallowed.
    if !app.state::<App>().booted.load(SeqCst) {
        app.state::<App>().wants_visible.store(true, SeqCst);
        return;
    }
    if let Some(window) = app.get_window("main") {
        match window.is_visible() {
            Ok(true) => {
                let _ = window.hide();
                emit_state(app);
            }
            _ => show_window(app),
        }
    }
}

#[tauri::command]
fn list_tools() -> Vec<Tool> {
    TOOLS.to_vec()
}

/// The rail reports what it actually rendered, so the gate can assert the DOM.
#[tauri::command]
fn rail_ready(app: tauri::AppHandle, report: String) {
    println!("RAIL {report}");
    *app.state::<App>().rail.lock().unwrap() = Some(report);
}

#[tauri::command]
fn settings_ready(app: tauri::AppHandle, report: String) {
    println!("SETTINGS {report}");
    *app.state::<App>().settings_dom.lock().unwrap() = Some(report);
}

fn state_dto(app: &tauri::AppHandle) -> StateDto {
    let active = app.state::<App>().active.lock().unwrap().clone();
    let tool = tools::get(&active);
    let usage = load_usage(app);
    StateDto {
        tools: TOOLS
            .iter()
            .map(|t| {
                let entry = usage.get(t.id).cloned().unwrap_or_default();
                ToolView {
                    id: t.id,
                    name: t.name,
                    group: t.group,
                    url: t.url,
                    new_chat: t.new_chat,
                    icon: icon_cached(app, t.id),
                    uses: entry.uses,
                    last_used: entry.last_used,
                }
            })
            .collect(),
        active: active.clone(),
        active_name: tool.map(|t| t.name.to_string()).unwrap_or_default(),
        new_chat: tool.map(|t| t.new_chat.to_string()).unwrap_or_default(),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        shortcut_ok: app.global_shortcut().is_registered("Alt+Space"),
        visible: app
            .get_window("main")
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false),
        rail_width: rail_width(app),
        always_on_top: app
            .get_window("main")
            .and_then(|w| w.is_always_on_top().ok())
            .unwrap_or(false),
        settings_open: app.state::<App>().settings_open.load(std::sync::atomic::Ordering::SeqCst),
        icon_count: TOOLS.iter().filter(|t| icon_cached(app, t.id).is_some()).count(),
        zoom: load_layout(app).zoom,
        theme: load_layout(app).theme,
    }
}

#[tauri::command]
fn state(app: tauri::AppHandle) -> StateDto {
    state_dto(&app)
}

/// The rail webview always spans the whole window: it draws the sidebar *and* the
/// top bar, and the content webview is positioned on top of its right side. Only
/// the CSS width of the sidebar changes; this function just re-asserts full size.
fn apply_rail_width(app: &tauri::AppHandle, _width: f64) {
    let Some(rail) = app.get_webview(RAIL_LABEL) else { return };
    let Some(window) = app.get_window("main") else { return };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else { return };
    let logical: LogicalSize<f64> = size.to_logical(scale);
    let _ = rail.set_position(LogicalPosition::new(0.0, 0.0));
    let _ = rail.set_size(LogicalSize::new(logical.width, logical.height));
}

/// Re-lay the rail and the active content webview after the window changed size.
///
/// Nothing did this before, so dragging the window edge left the rail at its old
/// size and the content webview floating mid-window -- the window was resizable
/// only in name. Called from the `Resized` event.
fn relayout_all(app: &tauri::AppHandle) {
    apply_rail_width(app, rail_width(app));
    // While settings is up the content webview must stay hidden; re-showing it
    // here would repaint it over the panel.
    let settings_open = app.state::<App>().settings_open.load(std::sync::atomic::Ordering::SeqCst);
    let active = app.state::<App>().active.lock().unwrap().clone();
    if let Some(webview) = app.get_webview(&format!("tool_{active}")) {
        if settings_open {
            let _ = webview.hide();
        } else {
            let _ = resume_tool(app, &webview);
        }
    }
    let _ = app.emit_to(RAIL_LABEL, "rail-layout", load_layout(app));
}

/// Applies the saved zoom to a webview (or all of them when id is None).
#[tauri::command]
fn set_zoom(app: tauri::AppHandle, zoom: f64, id: Option<String>) -> Result<Layout, String> {
    let clamped = zoom.clamp(0.5, 1.5);
    let mut layout = load_layout(&app);
    layout.zoom = clamped;
    save_layout(&app, &layout);
    let targets: Vec<String> = match id {
        Some(one) => vec![format!("tool_{one}")],
        None => TOOLS.iter().map(|t| format!("tool_{}", t.id)).collect(),
    };
    for label in targets {
        if let Some(webview) = app.get_webview(&label) {
            let _ = webview.set_zoom(clamped);
        }
    }
    Ok(layout)
}

/// Width chosen by dragging the divider.
#[tauri::command]
fn set_rail_width(app: tauri::AppHandle, width: f64) -> Result<Layout, String> {
    let clamped = width.clamp(RAIL_W_MIN, RAIL_W_MAX);
    let mut layout = load_layout(&app);
    layout.rail_width = clamped;
    save_layout(&app, &layout);
    apply_rail_width(&app, clamped);
    let _ = app.emit_to(RAIL_LABEL, "rail-layout", layout.clone());
    Ok(layout)
}

#[tauri::command]
fn layout(app: tauri::AppHandle, rect: Rect) {
    let active = app.state::<App>().active.lock().unwrap().clone();
    if let Some(webview) = app.get_webview(&format!("tool_{active}")) {
        let _ = webview.set_position(LogicalPosition::new(rect.x, rect.y));
        let _ = webview.set_size(LogicalSize::new(
            rect.width.max(1.0),
            rect.height.max(1.0),
        ));
    }
}

/// Position a content webview over the rail's right-hand area and show it.
/// Shared by tool switching and by closing settings, which has to restore
/// whatever was hidden behind the panel.
fn resume_tool(app: &tauri::AppHandle, webview: &Webview) -> Result<(), String> {
    if let Some(window) = app.get_window("main") {
        let size = window.inner_size().map_err(|e| e.to_string())?;
        let scale = window.scale_factor().map_err(|e| e.to_string())?;
        let logical: LogicalSize<f64> = size.to_logical(scale);
        // The rail page reserves the grip strip itself (#stage has a left
        // margin equal to RESIZE_GRIP), so the rect reported by `layout` and
        // this fallback both already exclude it. Only the rail width is added.
        let rail = rail_width(app);
        let _ = webview.set_position(LogicalPosition::new(rail, BAR_H));
        let _ = webview.set_size(LogicalSize::new(
            (logical.width - rail).max(1.0),
            (logical.height - BAR_H).max(1.0),
        ));
    }
    let _ = webview.set_zoom(load_layout(app).zoom);
    let _ = webview.show();
    // Stamp the moment it went on screen; reclaim_idle measures idleness from
    // here rather than from persisted usage, which only changes on a switch.
    if let Some(id) = webview.label().strip_prefix("tool_") {
        app.state::<App>()
            .last_shown
            .lock()
            .unwrap()
            .insert(id.to_string(), std::time::Instant::now());
    }
    Ok(())
}

/// How long a tool webview may sit hidden before it is destroyed.
///
/// A hidden WKWebView is not idle: its render tree, DOM, JS heap and decoded
/// images all stay resident, so ten opened tools cost ten times one tool. This
/// is the dominant memory term in practice -- chatgpt.com alone settles around
/// 1.5 GB once a long conversation is in the DOM.
const IDLE_RECLAIM_SECS: u64 = 600;

/// Destroy every tool webview except the active one that has been unused for
/// at least `idle_secs`.
///
/// Reclaiming is safe because `build_tool_webview` recreates on demand: the
/// next switch back reloads the page. The cost is a reload, which is exactly
/// the trade being made -- memory now, time later.
fn reclaim_idle(app: &tauri::AppHandle, idle_secs: u64) -> usize {
    let active = app.state::<App>().active.lock().unwrap().clone();
    let now = std::time::Instant::now();

    // Snapshot under the lock, act outside it: closing a webview can re-enter
    // this side of the code, and a held mutex would deadlock the caller.
    let state = app.state::<App>();
    let victims: Vec<String> = {
        let shown = state.last_shown.lock().unwrap();
        tools::TOOLS
            .iter()
            .map(|t| t.id.to_string())
            .filter(|id| *id != active)
            .filter(|id| app.get_webview(&format!("tool_{id}")).is_some())
            .filter(|id| {
                shown
                    .get(id)
                    .map(|at| now.duration_since(*at).as_secs() >= idle_secs)
                    // No timestamp means it was never shown since launch; treat
                    // it as reclaimable rather than pinning it forever.
                    .unwrap_or(true)
            })
            .collect()
    };

    for id in &victims {
        if let Some(webview) = app.get_webview(&format!("tool_{id}")) {
            let _ = webview.close();
        }
        app.state::<App>().last_shown.lock().unwrap().remove(id);
    }

    if !victims.is_empty() {
        eprintln!("anychat: reclaimed {} idle webview(s): {}", victims.len(), victims.join(", "));
    }
    victims.len()
}

/// Hide the content webview so a rail-drawn popup can be seen.
///
/// The content area is a *native child view* stacked above the rail's page, not
/// a DOM box, so no z-index can lift a menu over it -- the menu rendered
/// underneath and looked like it never opened. Settings already solves this by
/// hiding the content view; the context menu has the same problem and takes the
/// same way out.
#[tauri::command]
fn menu_open(app: tauri::AppHandle) {
    if app.state::<App>().settings_open.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // Suppresses blur auto-hide: the menu lives in the page, so hiding the
    // window would take it away mid-interaction.
    app.state::<App>().menu_open.store(true, std::sync::atomic::Ordering::SeqCst);
    let active = app.state::<App>().active.lock().unwrap().clone();
    if let Some(webview) = app.get_webview(&format!("tool_{active}")) {
        app.state::<App>().content_hidden.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = webview.hide();
    }
}

/// Restore the content webview once the menu closes. Only when settings is not
/// the thing keeping it hidden: closing a menu while the panel is open must not
/// paint the page back over it.
#[tauri::command]
fn menu_close(app: tauri::AppHandle) {
    app.state::<App>().menu_open.store(false, std::sync::atomic::Ordering::SeqCst);
    if app.state::<App>().settings_open.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let active = app.state::<App>().active.lock().unwrap().clone();
    if let Some(webview) = app.get_webview(&format!("tool_{active}")) {
        let _ = resume_tool(&app, &webview);
        app.state::<App>().content_hidden.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Start an interactive window resize from a corner or edge.
///
/// A `decorations(false)` window has its standard resize borders stripped by
/// macOS, and the script-injected corner radius clips what is left, so the
/// native corners became a target of a few pixels. This drives the geometry
/// from the rail instead: the pointer position decides the new frame, and the
/// window follows.
///
/// `corner` is one of nw/ne/sw/se/n/s/e/w. The anchor is the opposite corner,
/// which stays put while the dragged one moves.
#[tauri::command]
fn resize_window(app: tauri::AppHandle, corner: String, dx: f64, dy: f64) -> Result<(), String> {
    let window = app.get_window("main").ok_or("no window")?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let pos = window.outer_position().map_err(|e| e.to_string())?;
    let size = window.outer_size().map_err(|e| e.to_string())?;

    let mut x = pos.x as f64 / scale;
    let mut y = pos.y as f64 / scale;
    let mut w = size.width as f64 / scale;
    let mut h = size.height as f64 / scale;

    const MIN_W: f64 = 420.0;
    const MIN_H: f64 = 320.0;

    if corner.contains('w') {
        let nw = (w - dx).max(MIN_W);
        x += w - nw;
        w = nw;
    } else if corner.contains('e') {
        w = (w + dx).max(MIN_W);
    }
    if corner.contains('n') {
        let nh = (h - dy).max(MIN_H);
        y += h - nh;
        h = nh;
    } else if corner.contains('s') {
        h = (h + dy).max(MIN_H);
    }

    window
        .set_size(LogicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    window
        .set_position(LogicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;

    let mut layout = load_layout(&app);
    layout.win_w = w;
    layout.win_h = h;
    save_layout(&app, &layout);
    Ok(())
}

/// Toggle pin-to-top from the rail.
///
/// The rail is a child webview, not a window: `getCurrentWindow()` there does
/// not address the OS window, and `core:window:allow-set-always-on-top` is not
/// granted, so the old front-end call was rejected and the failure swallowed by
/// a bare catch. Doing it here keeps one owner for window state and lets the
/// choice be persisted.
#[tauri::command]
fn set_pin(app: tauri::AppHandle, pinned: bool) -> Result<bool, String> {
    let window = app.get_window("main").ok_or("no window")?;
    window
        .set_always_on_top(pinned)
        .map_err(|e| e.to_string())?;
    let mut layout = load_layout(&app);
    layout.always_on_top = pinned;
    save_layout(&app, &layout);
    let state = window.is_always_on_top().unwrap_or(pinned);
    emit_state(&app);
    Ok(state)
}

/// True when the app owns another visible window besides `main` -- a login
/// popup, for instance. Blur auto-hide consults this so an OAuth handoff does
/// not close the window that launched it.
#[cfg(target_os = "macos")]
fn any_other_window_open(app: &tauri::AppHandle) -> bool {
    app.webview_windows()
        .iter()
        .any(|(label, w)| label != "main" && label != SPLASH_LABEL && w.is_visible().unwrap_or(false))
}

/// Shared cleanup for every way the window can be hidden, so a restored
/// summon never lands on a half-hidden window with its content view missing.
fn on_window_hidden(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering::SeqCst;
    app.state::<App>().menu_open.store(false, SeqCst);
    emit_state(app);
}

/// Reclaim one tool's webview, refusing the active one: destroying what the
/// user is looking at would blank the window with no way back but a reload.
#[tauri::command]
fn reclaim_tool(app: tauri::AppHandle, id: String) -> Result<bool, String> {
    let active = app.state::<App>().active.lock().unwrap().clone();
    if id == active {
        return Err("active".into());
    }
    let Some(webview) = app.get_webview(&format!("tool_{id}")) else {
        return Ok(false);
    };
    webview.close().map_err(|e| e.to_string())?;
    app.state::<App>().last_shown.lock().unwrap().remove(&id);
    eprintln!("anychat: reclaimed webview tool_{id}");
    Ok(true)
}

/// Reclaim everything idle. Exposed so the rail can offer "free memory" without
/// the caller needing to know the idle rule.
#[tauri::command]
fn reclaim_all_idle(app: tauri::AppHandle, idle_secs: Option<u64>) -> usize {
    reclaim_idle(&app, idle_secs.unwrap_or(IDLE_RECLAIM_SECS))
}

/// Which tool webviews are currently resident, for the rail to show what can be
/// freed. Without this the UI cannot distinguish "never opened" from "open and
/// costing memory".
#[tauri::command]
fn resident_tools(app: tauri::AppHandle) -> Vec<String> {
    tools::TOOLS
        .iter()
        .map(|t| t.id.to_string())
        .filter(|id| app.get_webview(&format!("tool_{id}")).is_some())
        .collect()
}

#[tauri::command]
fn select_tool(app: tauri::AppHandle, id: String) -> Result<(), String> {
    if tools::get(&id).is_none() {
        return Err(format!("unknown tool: {id}"));
    }
    let previous = {
        let state = app.state::<App>();
        let mut active = state.active.lock().unwrap();
        let previous = active.clone();
        *active = id.clone();
        previous
    };

    if previous != id {
        if let Some(webview) = app.get_webview(&format!("tool_{previous}")) {
            let _ = webview.hide();
        }
    }

    note_usage(&app, &id);
    let webview = build_tool_webview(&app, &id).ok_or("failed to create webview")?;
    // Keep the content hidden while settings are open; showing it here would
    // paint the native child view straight over the panel.
    if app.state::<App>().settings_open.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = webview.hide();
    } else {
        resume_tool(&app, &webview)?;
    }
    emit_state(&app);
    Ok(())
}

#[tauri::command]
fn new_chat(app: tauri::AppHandle) -> Result<(), String> {
    let active = app.state::<App>().active.lock().unwrap().clone();
    let tool = tools::get(&active).ok_or("unknown tool")?;
    let url = tool.new_chat.parse().map_err(|e| format!("{e}"))?;
    let webview = build_tool_webview(&app, &active).ok_or("no webview")?;
    webview.navigate(url).map_err(|e| e.to_string())?;
    emit_state(&app);
    Ok(())
}

#[tauri::command]
fn nav(app: tauri::AppHandle, action: String) -> Result<(), String> {
    let active = app.state::<App>().active.lock().unwrap().clone();
    let webview = app
        .get_webview(&format!("tool_{active}"))
        .ok_or("no webview")?;
    match action.as_str() {
        "reload" => webview.reload().map_err(|e| e.to_string()),
        "back" => webview.eval("history.back()").map_err(|e| e.to_string()),
        "forward" => webview.eval("history.forward()").map_err(|e| e.to_string()),
        other => Err(format!("unknown action: {other}")),
    }
}

#[tauri::command]
fn toggle(app: tauri::AppHandle) {
    toggle_window(&app);
}

/// Settings is drawn by the rail webview, which spans the whole window.
///
/// The content webview is a *native child view* stacked above the rail's page,
/// so no z-index in the rail can cover it -- the panel was rendering underneath
/// and looked like it never opened. Hiding the active content webview is the
/// only reliable way to reveal it.
#[tauri::command]
fn open_settings(app: tauri::AppHandle) -> Result<StateDto, String> {
    app.state::<App>().settings_open.store(true, std::sync::atomic::Ordering::SeqCst);
    let active = app.state::<App>().active.lock().unwrap().clone();
    if let Some(webview) = app.get_webview(&format!("tool_{active}")) {
        app.state::<App>().content_hidden.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = webview.hide();
    }
    Ok(state_dto(&app))
}

#[tauri::command]
fn close_settings(app: tauri::AppHandle) -> Result<StateDto, String> {
    app.state::<App>().settings_open.store(false, std::sync::atomic::Ordering::SeqCst);
    let active = app.state::<App>().active.lock().unwrap().clone();
    if let Some(webview) = app.get_webview(&format!("tool_{active}")) {
        app.state::<App>().content_hidden.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = resume_tool(&app, &webview);
    }
    Ok(state_dto(&app))
}

/// True when this build is running from a cargo target directory rather than an
/// installed bundle.
///
/// Autostart registers the *current executable path*, so a dev run writes
/// `target/debug/anychat` into the LaunchAgent. Both builds share the same
/// label, so a dev run silently overwrites the installed app's entry -- and the
/// next rebuild deletes that path, which reads back as "disabled". That is why
/// the switch never stayed on.
fn is_dev_build() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let p = exe.to_string_lossy();
    p.contains("/target/debug/") || p.contains("/target/release/")
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    if is_dev_build() {
        return Err("dev".into());
    }
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    result.map_err(|e| e.to_string())?;
    emit_state(&app);
    manager.is_enabled().map_err(|e| e.to_string())
}

/// Sets the colour theme. `auto` follows the OS, which CSS resolves through
/// prefers-color-scheme, so no listener is needed here.
#[tauri::command]
fn set_theme(app: tauri::AppHandle, theme: String) -> Result<String, String> {
    let theme = match theme.as_str() {
        "light" | "dark" | "auto" => theme,
        other => return Err(format!("unknown theme: {other}")),
    };
    let mut layout = load_layout(&app);
    layout.theme = theme.clone();
    save_layout(&app, &layout);
    // The rail page owns the styling; the settings page is the same webview,
    // so re-applying here keeps both in sync without a reload.
    apply_theme(&app, &theme);
    emit_state(&app);
    Ok(theme)
}

/// Pushes the chosen theme onto every rail page. `auto` is passed through
/// untouched: the stylesheet decides what it means on this machine.
fn apply_theme(app: &tauri::AppHandle, theme: &str) {
    let js = format!(
        "document.documentElement.setAttribute('data-theme', {})",
        serde_json::to_string(theme).unwrap_or_else(|_| "\"auto\"".into())
    );
    if let Some(w) = app.get_webview(RAIL_LABEL) {
        let _ = w.eval(&js);
    }
}

#[tauri::command]
fn open_external(app: tauri::AppHandle, url: Option<String>) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    // Fall back to the live address when the caller passes nothing: the page's
    // own URL is the only thing that reflects where the user actually is.
    let target = url.filter(|u| !u.trim().is_empty()).or_else(|| {
        let active = app.state::<App>().active.lock().unwrap().clone();
        app.get_webview(&format!("tool_{active}"))
            .and_then(|w| w.url().ok())
            .map(|u| u.to_string())
    });
    let target = target.ok_or("no url to open")?;
    app.opener()
        .open_url(target, None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn current_url(app: tauri::AppHandle) -> Option<String> {
    let active = app.state::<App>().active.lock().unwrap().clone();
    app.get_webview(&format!("tool_{active}"))
        .and_then(|w| w.url().ok())
        .map(|u| u.to_string())
}



/// Persistence probe: an initialization script runs inside whatever origin the
/// tool loads, writes or reads a marker there, and parks the result in the URL
/// hash so the main process can read it without an eval callback.
fn persist_script(write: bool) -> String {
    if write {
        r#"try {
  localStorage.setItem('__anychat_probe__', 'v1');
  document.cookie = '__anychat_probe__=v1; path=/; max-age=86400';
} catch (e) {}
setTimeout(function () {
  var out = 'ls=' + (function(){ try { return localStorage.getItem('__anychat_probe__') } catch(e) { return 'ERR' } })()
    + ' ck=' + (document.cookie.indexOf('__anychat_probe__=v1') >= 0);
  try { history.replaceState(null, '', location.pathname + location.search + '#' + encodeURIComponent(out)) } catch (e) {}
}, 2500);"#.to_string()
    } else {
        r#"setTimeout(function () {
  var out = 'ls=' + (function(){ try { return localStorage.getItem('__anychat_probe__') } catch(e) { return 'ERR' } })()
    + ' ck=' + (document.cookie.indexOf('__anychat_probe__=v1') >= 0);
  try { history.replaceState(null, '', location.pathname + location.search + '#' + encodeURIComponent(out)) } catch (e) {}
}, 2500);"#.to_string()
    }
}

fn run_persist_test(app: &tauri::AppHandle, write: bool) {
    let active = app.state::<App>().active.lock().unwrap().clone();
    let label = format!("tool_{active}");
    let Some(webview) = app.get_webview(&label) else {
        println!("PERSIST error=no-webview");
        app.exit(2);
        return;
    };
    let tool = tools::get(&active);
    let url = tool.map(|t| t.url).unwrap_or("https://example.com/");
    // Reload with the probe script attached for this run only.
    let _ = webview.eval(&format!(
        "window.__anychat_persist_write = {};",
        if write { "true" } else { "false" }
    ));
    let script = persist_script(write);
    let _ = webview.eval(&script);

    let mut got = String::new();
    for _ in 0..32 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let current = webview.url().map(|u| u.to_string()).unwrap_or_default();
        if let Some(fragment) = current.split('#').nth(1) {
            if fragment.contains("ls=") {
                got = fragment.replace("%20", " ");
                break;
            }
        }
    }
    println!("PERSIST mode={} store={} url={} result={}", if write { "write" } else { "read" }, label, url, got);
    app.exit(0);
}

/// Headless self-check: asserts the things that can silently break, prints JSON, exits.
/// Runs the gate without freezing the UI thread.
///
/// `run_smoke` must issue window/webview calls on the main thread, but it also
/// sleeps between steps. Sleeping on the main thread starves the rail's IPC and
/// timers, so any check that needs the page to answer would fail. Waiting happens
/// on this plain thread instead; the gate itself is one main-thread task.

/// Live probe: keeps the event loop free, prints every rail/settings report, and
/// executes gate commands from stdin. Used by scripts/probe.sh.
fn start_probe(app: tauri::AppHandle) {
    use std::io::BufRead;

    let stdin = std::io::stdin();
    let handle = app.clone();
    std::thread::spawn(move || {
        for line in stdin.lock().lines().flatten() {
            let cmd = line.trim().to_string();
            if cmd.is_empty() {
                continue;
            }
            let h = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                println!("PROBE tick {cmd}");
                match cmd.as_str() {
                    "settings.open" => { let _ = open_settings(h.clone()); }
                    "settings.close" => { let _ = close_settings(h.clone()); }
                    // Report what "open externally" would launch, without
                    // actually launching it -- the point is the address, and
                    // spawning a browser mid-gate is noise.
                    // Navigate the active tool somewhere specific, then let the
                    // gate assert that "open externally" tracks it.
                    cmd if cmd.starts_with("goto ") => {
                        let target = cmd[5..].trim().to_string();
                        let active = h.state::<App>().active.lock().unwrap().clone();
                        if let Some(w) = h.get_webview(&format!("tool_{active}")) {
                            match target.parse() {
                                Ok(u) => { let _ = w.navigate(u); println!("PROBE goto={target}"); }
                                Err(e) => println!("PROBE goto-err {e}"),
                            }
                        }
                    }
                    "open_target" => {
                        let active = h.state::<App>().active.lock().unwrap().clone();
                        let live = h.get_webview(&format!("tool_{active}"))
                            .and_then(|w| w.url().ok())
                            .map(|u| u.to_string())
                            .unwrap_or_default();
                        println!("PROBE open_target={live}");
                    }
                    // Drive a real window resize so the gate can prove the rail
                    // and the content webview follow it.
                    cmd if cmd.starts_with("resize ") => {
                        let parts: Vec<f64> = cmd[7..]
                            .split_whitespace()
                            .filter_map(|v| v.parse().ok())
                            .collect();
                        if parts.len() == 2 {
                            if let Some(w) = h.get_window("main") {
                                let _ = w.set_size(LogicalSize::new(parts[0], parts[1]));
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                println!("PROBE resized={}x{}", parts[0], parts[1]);
                            }
                        } else {
                            println!("PROBE resize-err bad-args");
                        }
                    }
                    // Reclaim probes. The whole point of this feature is that
                    // memory actually comes back, so the gate needs to observe
                    // the webview count, not just that the call returned.
                    cmd if cmd.starts_with("select ") => {
                        let id = cmd[7..].trim().to_string();
                        match select_tool(h.clone(), id.clone()) {
                            Ok(()) => println!("PROBE selected={id}"),
                            Err(e) => println!("PROBE select-err {e}"),
                        }
                    }
                    "resident" => {
                        let ids: Vec<String> = tools::TOOLS
                            .iter()
                            .map(|t| t.id.to_string())
                            .filter(|id| h.get_webview(&format!("tool_{id}")).is_some())
                            .collect();
                        println!("PROBE resident={} {}", ids.len(), ids.join(","));
                    }
                    "reclaim_idle" => {
                        let n = reclaim_idle(&h, 0);
                        println!("PROBE reclaimed={n}");
                    }
                    "rss" => {
                        let ids: Vec<String> = tools::TOOLS
                            .iter()
                            .map(|t| t.id.to_string())
                            .filter(|id| h.get_webview(&format!("tool_{id}")).is_some())
                            .collect();
                        println!("PROBE resident_after={} {}", ids.len(), ids.join(","));
                    }
                    cmd if cmd.starts_with("rail.width") => {
                        let w: f64 = cmd[10..].parse().unwrap_or(208.0);
                        let _ = set_rail_width(h.clone(), w);
                    }
                    "show" => { show_window(&h); }
                    "hide" => { if let Some(w) = h.get_window("main") { let _ = w.hide(); } }
                    "state" => {
                        let dto = state_dto(&h);
                        let (win_w, win_h) = logical_window_size(&h);
                        println!("PROBE state icons={} open={} rail={} win={}x{}",
                            dto.icon_count, dto.settings_open, dto.rail_width, win_w, win_h);
                        if let Some(w) = h.get_window("main") {
                            #[cfg(target_os = "macos")]
                            if let Some((non_opaque, clear_bg)) = window_is_transparent(&w) {
                                println!("PROBE window transparent={non_opaque} clearBg={clear_bg}");
                            }
                        }
                        let st = h.state::<App>();
                        println!("PROBE popups seen={} opened={}",
                            st.popups_seen.load(std::sync::atomic::Ordering::SeqCst),
                            st.popups_opened.load(std::sync::atomic::Ordering::SeqCst));
                    }
                    // Fire a real popup from the active content webview to prove
                    // window.open is no longer swallowed.
                    "popup" => {
                        let id = h.state::<App>().active.lock().unwrap().clone();
                        if let Some(w) = h.get_webview(&format!("tool_{id}")) {
                            match w.eval("window.open('https://example.com/','_blank')") {
                                Ok(()) => println!("PROBE popup=dispatched"),
                                Err(e) => println!("PROBE popup-err {e}"),
                            }
                        } else {
                            println!("PROBE popup-err no-webview");
                        }
                    }
                    cmd if cmd.starts_with("zoom") => {
                        let v: f64 = cmd[4..].parse().unwrap_or(0.8);
                        match set_zoom(h.clone(), v, None) {
                            Ok(layout) => println!("PROBE zoom={}", layout.zoom),
                            Err(e) => println!("PROBE zoom-err {e}"),
                        }
                    }
                    "quit" => { h.exit(0); }
                    other => println!("PROBE unknown {other}"),
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
            });
        }
        // stdin closed: keep running until told to quit.
    });
}

fn run_smoke_async(app: tauri::AppHandle) {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = app.clone();
    let finished = done.clone();
    let _ = app.run_on_main_thread(move || {
        run_smoke(&handle, &finished);
    });
    while !done.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn run_smoke(app: &tauri::AppHandle, done: &std::sync::atomic::AtomicBool) {
    use std::io::Write;

    let mut results: Vec<(String, bool, String)> = Vec::new();
    let mut check = |name: &str, ok: bool, detail: String| {
        results.push((name.to_string(), ok, detail));
    };

    // Theme round-trip: the command must persist the choice and push it onto
    // the page. A UI-only switch would look right and reset on every launch,
    // which is the failure this checks for.
    for want in ["light", "dark", "auto"] {
        let got = set_theme(app.clone(), want.to_string()).unwrap_or_default();
        let saved = load_layout(app).theme;
        check(
            "theme-round-trip",
            got == want && saved == want,
            format!("want={want} got={got} saved={saved}"),
        );
    }
    set_theme(app.clone(), "auto".to_string()).ok();

    // tool inventory
    check("tool-inventory", !TOOLS.is_empty(), format!("{} tools", TOOLS.len()));
    let groups: Vec<&str> = TOOLS.iter().map(|t| t.group).collect();
    let mut uniq: Vec<&str> = groups.clone();
    uniq.sort();
    uniq.dedup();
    check("group-count", uniq.len() == 3, format!("{:?}", uniq));

    // window exists, hidden by default, not always-on-top, not focused
    let win = app.get_window("main");
    check("window-exists", win.is_some(), String::new());
    if let Some(w) = &win {
        check("window-not-always-on-top", !w.is_always_on_top().unwrap_or(true), String::new());
        let visible = w.is_visible().unwrap_or(true);
        check(
            "window-hidden-by-default",
            !visible || std::env::var("ANYCHAT_DEMO_SHOW").is_ok(),
            format!("visible={visible}"),
        );
        check(
            "window-not-focused",
            !w.is_focused().unwrap_or(true) || std::env::var("ANYCHAT_DEMO_SHOW").is_ok(),
            format!("focused={}", w.is_focused().unwrap_or(true)),
        );
        let size = w.inner_size().map(|s| (s.width, s.height)).unwrap_or((0, 0));
        check("window-size", size.0 >= 560 && size.1 >= 420, format!("{size:?}"));
    }

    // Summoning must not activate us: frontmost app stays someone else.
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSApplication;
        // Force our app out of the foreground first, so the assertion is real:
        // an already-frontmost app would pass no matter what we do.
        if let Some(w) = &win {
            let _ = w.hide();
        }
        unsafe {
            let ns = NSApplication::sharedApplication(objc2::MainThreadMarker::new_unchecked());
            ns.deactivate();
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        let before = unsafe {
            NSApplication::sharedApplication(objc2::MainThreadMarker::new_unchecked()).isActive()
        };

        show_window(app);
        std::thread::sleep(std::time::Duration::from_millis(700));
        let after = unsafe {
            NSApplication::sharedApplication(objc2::MainThreadMarker::new_unchecked()).isActive()
        };
        let visible = win.as_ref().and_then(|w| w.is_visible().ok()).unwrap_or(false);
        let focused = win.as_ref().and_then(|w| w.is_focused().ok()).unwrap_or(true);
        // As an Accessory app there is no Dock tile and the process stays the
        // "active" app, so `isActive()` cannot observe a focus steal. The
        // invariant that still matters -- and that the user feels -- is that the
        // summoned window never takes keyboard focus.
        check(
            "summon-does-not-steal-focus",
            visible && !focused,
            format!("visible={visible} activeBefore={before} activeAfter={after} focused={focused}"),
        );
        let on_top = win
            .as_ref()
            .and_then(|w| w.is_always_on_top().ok())
            .unwrap_or(true);
        check("summon-not-on-top", !on_top, format!("alwaysOnTop={on_top}"));
    }

    // global shortcut
    let shortcut_ok = app.global_shortcut().is_registered("Alt+Space");
    check("shortcut-alt-space", shortcut_ok, String::new());

    // every partition dir can be created and is distinct
    let mut dirs = Vec::new();
    for tool in TOOLS {
        if let Some(d) = partition_dir(app, tool.id) {
            dirs.push(d.display().to_string());
        }
    }
    let mut unique_dirs = dirs.clone();
    unique_dirs.sort();
    unique_dirs.dedup();
    check(
        "per-tool-partition-dirs",
        dirs.len() == TOOLS.len() && unique_dirs.len() == TOOLS.len(),
        format!("{} dirs, {} unique", dirs.len(), unique_dirs.len()),
    );

    // Pick a non-default tool from the registry instead of naming one site: the
    // inventory is editable and a removed entry must not break the whole gate.
    let default_tool = TOOLS.first().map(|t| t.id).unwrap_or("");
    let other_tool = TOOLS
        .iter()
        .find(|t| t.id != default_tool)
        .map(|t| t.id)
        .unwrap_or(default_tool);
    let other_label = format!("tool_{other_tool}");

    let handled = select_tool(app.clone(), other_tool.to_string());
    check("select-tool", handled.is_ok(), format!("{handled:?}"));
    check("webview-created", app.get_webview(&other_label).is_some(), other_label.clone());

    // Switching back must reuse the same webview, not spawn a second one.
    let first_url = app.get_webview(&format!("tool_{default_tool}"))
        .map(|w| w.url().map(|u| u.to_string()).unwrap_or_default());
    let _ = select_tool(app.clone(), default_tool.to_string());
    let again_url = app.get_webview(&format!("tool_{default_tool}"))
        .map(|w| w.url().map(|u| u.to_string()).unwrap_or_default());
    check(
        "webview-reused-on-return",
        first_url.is_some() && first_url == again_url,
        format!("first={first_url:?} again={again_url:?}"),
    );
    let _ = select_tool(app.clone(), other_tool.to_string());

    // ⌘N navigates the active webview to the tool's new-chat route.
    // Navigate somewhere else first, otherwise "before == after" proves nothing.
    if let Some(w) = app.get_webview(&other_label) {
        if let Ok(other) = "https://www.google.com/".parse() {
            let _ = w.navigate(other);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let before = app.get_webview(&other_label)
        .and_then(|w| w.url().ok())
        .map(|u| u.to_string())
        .unwrap_or_default();
    let _ = new_chat(app.clone());
    std::thread::sleep(std::time::Duration::from_millis(1800));
    let after = app.get_webview(&other_label)
        .and_then(|w| w.url().ok())
        .map(|u| u.to_string())
        .unwrap_or_default();
    let expected = tools::get(other_tool).map(|t| t.new_chat).unwrap_or("");
    check(
        "cmd-n-new-chat",
        after.contains(expected.trim_end_matches('/')) || after == expected,
        format!("before={before} after={after} expected={expected}"),
    );

    // Settings is reachable, covers the window, and reports its own DOM.
    // Settings is a view inside the rail page driven by a command reply, not by
    // events. What is provable from the main process is: the command flips the
    // flag, the rail reports the panel markup, and closing clears the flag.
    open_settings(app.clone()).ok();
    check("settings-flag-set",
        app.state::<App>().settings_open.load(std::sync::atomic::Ordering::SeqCst),
        String::new());
    // The panel lives in the rail page, which the native content webview covers.
    // Unless that child view is hidden the panel is unreachable no matter what
    // z-index it uses -- the bug where settings "did not open".
    check("settings-reveals-panel",
        app.state::<App>().content_hidden.load(std::sync::atomic::Ordering::SeqCst),
        format!("content_hidden={}", app.state::<App>().content_hidden.load(std::sync::atomic::Ordering::SeqCst)));

    let mut rail_dom = String::new();
    for _ in 0..40 {
        if let Some(v) = app.state::<App>().rail.lock().unwrap().clone() {
            rail_dom = v;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    check("settings-panel-markup-present",
        rail_dom.contains("\"settingsPanel\":true"),
        rail_dom.clone());

    // Rendering the panel requires the page to answer, which cannot happen while
    // this gate occupies the main thread. scripts/probe.sh asserts that path.

    close_settings(app.clone()).ok();
    check("settings-closed",
        !app.state::<App>().settings_open.load(std::sync::atomic::Ordering::SeqCst)
            && !app.state::<App>().content_hidden.load(std::sync::atomic::Ordering::SeqCst),
        String::new());

    // autostart round-trip through the OS
    let manager = app.autolaunch();
    let on = manager.enable().is_ok() && manager.is_enabled().unwrap_or(false);
    let off = manager.disable().is_ok() && !manager.is_enabled().unwrap_or(true);
    check("autostart-round-trip", on && off, format!("enable={on} disable={off}"));

    // Rail DOM: showing the window (without focusing) gives WKWebView a live
    // surface; a hidden window can defer layout and never run the probe.
    if let Some(w) = &win {
        let _ = w.show();
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Rail DOM is reported by the rail itself through IPC: the only way to prove
    // both the DOM and the command channel in one shot.
    let mut dom = String::new();
    for _ in 0..20 {
        if let Some(v) = app.state::<App>().rail.lock().unwrap().clone() {
            dom = v;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // 常用 is prepended to the three tool groups, so the count is 4 and the
    // caption order matters more than the number itself.
    let groups_ok = dom.contains("\"pinned\":10") && dom.contains("\"duplicateTools\":0");
    check(
        "rail-rendered",
        dom.contains(&format!("\"tools\":{}", TOOLS.len())) && groups_ok,
        dom.clone(),
    );
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&dom) {
        let w = parsed.get("slot").and_then(|s| s.get("w")).and_then(|v| v.as_i64()).unwrap_or(0);
        let h = parsed.get("slot").and_then(|s| s.get("h")).and_then(|v| v.as_i64()).unwrap_or(0);
        check("slot-has-area", w > 200 && h > 200, format!("{w}x{h}"));
        check(
            "active-tool-marked",
            parsed.get("active").and_then(|v| v.as_str()).is_some(),
            dom.clone(),
        );
    }

    // Rail width persists and clamps; dragging is the only way to change it now.
    let setw = set_rail_width(app.clone(), 300.0).unwrap_or_default();
    check("rail-width-clamped-and-saved",
        (setw.rail_width - 300.0).abs() < 0.5 && (rail_width(app) - 300.0).abs() < 0.5,
        format!("width={}", setw.rail_width));
    let too_narrow = set_rail_width(app.clone(), 50.0).unwrap_or_default();
    check("rail-width-min-enforced",
        too_narrow.rail_width >= RAIL_W_MIN,
        format!("width={}", too_narrow.rail_width));
    let too_wide = set_rail_width(app.clone(), 9999.0).unwrap_or_default();
    check("rail-width-max-enforced",
        too_wide.rail_width <= RAIL_W_MAX,
        format!("width={}", too_wide.rail_width));
    let _ = set_rail_width(app.clone(), 208.0);
    std::thread::sleep(std::time::Duration::from_millis(400));

    let failed: Vec<&str> = results.iter().filter(|(_, ok, _)| !ok).map(|(n, _, _)| n.as_str()).collect();
    let json = serde_json::json!({
        "ok": failed.is_empty(),
        "failures": failed,
        "steps": results.iter().map(|(n, ok, d)| serde_json::json!({"name": n, "pass": ok, "detail": d})).collect::<Vec<_>>(),
    });
    println!("SMOKE {}", serde_json::to_string_pretty(&json).unwrap());
    let _ = std::io::stdout().flush();
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    app.exit(if failed.is_empty() { 0 } else { 1 });
}

fn build_app_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    let handle = app.clone();

    let about = PredefinedMenuItem::about(app, None, Some(AboutMetadata::default()))?;
    let hide = MenuItem::with_id(app, "hide", "隐藏窗口", true, Some("CmdOrCtrl+H"))?;
    let quit = MenuItem::with_id(app, "quit", "退出 AnyChat", true, Some("CmdOrCtrl+Q"))?;
    let app_menu = Submenu::with_items(
        app,
        "AnyChat",
        true,
        &[&about, &PredefinedMenuItem::separator(app)?, &hide, &PredefinedMenuItem::separator(app)?, &quit],
    )?;

    let new_chat_item = MenuItem::with_id(app, "session.new", "新会话", true, Some("CmdOrCtrl+N"))?;
    let reload = MenuItem::with_id(app, "session.reload", "重新加载", true, Some("CmdOrCtrl+R"))?;
    let session_menu = Submenu::with_items(
        app,
        "会话",
        true,
        &[&new_chat_item, &reload, &PredefinedMenuItem::separator(app)?],
    )?;

    let edit_menu = Submenu::with_items(
        app,
        "编辑",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;

    let menu = Menu::with_items(app, &[&app_menu, &session_menu, &edit_menu])?;
    app.set_menu(menu)?;

    app.on_menu_event(move |app, event| match event.id().as_ref() {
        "session.new" => {
            let _ = new_chat(app.clone());
        }
        "session.reload" => {
            let _ = nav(app.clone(), "reload".to_string());
        }
        "hide" => {
            if let Some(w) = app.get_window("main") {
                let _ = w.hide();
            }
            emit_state(app);
        }
        "quit" => app.exit(0),
        _ => {}
    });

    let _ = handle;
    Ok(())
}

fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "toggle", "显示 / 隐藏  (⌥Space)", true, None::<&str>)?;
    let new_chat_item = MenuItem::with_id(app, "new", "新会话  (⌘N)", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机自启",
        true,
        app.autolaunch().is_enabled().unwrap_or(false),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&show, &new_chat_item, &sep, &autostart, &sep, &quit],
    )?;

    TrayIconBuilder::with_id("anychat")
        .icon(app.default_window_icon().cloned().unwrap())
        .icon_as_template(true)
        .tooltip("AnyChat — ⌥Space")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" => toggle_window(app),
            "new" => {
                show_window(app);
                let _ = new_chat(app.clone());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { .. } = event {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(App {
            active: std::sync::Mutex::new(
                TOOLS.first().map(|t| t.id.to_string()).unwrap_or_default(),
            ),
            rail: std::sync::Mutex::new(None),
            settings_dom: std::sync::Mutex::new(None),
            settings_open: std::sync::atomic::AtomicBool::new(false),
            content_hidden: std::sync::atomic::AtomicBool::new(false),
            popups_seen: std::sync::atomic::AtomicUsize::new(0),
            popups_opened: std::sync::atomic::AtomicUsize::new(0),
            booted: std::sync::atomic::AtomicBool::new(false),
            last_shown: std::sync::Mutex::new(std::collections::HashMap::new()),
            menu_open: std::sync::atomic::AtomicBool::new(false),
            wants_visible: std::sync::atomic::AtomicBool::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            list_tools,
            rail_ready,
            splash_stats,
            splash_done,
            set_pin,
            resize_window,
            menu_open,
            menu_close,
            reclaim_tool,
            reclaim_all_idle,
            resident_tools,
            settings_ready,
            state,
            layout,
            select_tool,
            new_chat,
            nav,
            toggle,
            set_rail_width,
            set_zoom,
            open_settings,
            close_settings,
            set_autostart,
            set_theme,
            open_external,
            current_url
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Menu-bar app: drop the Dock tile and the app menu so AnyChat behaves
            // like a summonable utility rather than a document app.
            #[cfg(target_os = "macos")]
            {
                use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
                let mtm = objc2::MainThreadMarker::new()
                    .expect("setup runs on the main thread");
                NSApplication::sharedApplication(mtm)
                    .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            }

            // Idle sweeper. A background thread rather than a timer tied to the
            // event loop: reclaiming is a plain close() call and needs no window.
            // One tick a minute is enough for a ten-minute budget.
            {
                let sweeper = handle.clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    reclaim_idle(&sweeper, IDLE_RECLAIM_SECS);
                });
            }

            let saved = load_layout(&handle);
            // 820x600 rather than 980x760: the window is a summonable utility,
            // and the old default filled a laptop screen for what is mostly a
            // list of tools plus one embedded page.
            let (init_w, init_h) = if saved.win_w > 200.0 && saved.win_h > 200.0 {
                (saved.win_w, saved.win_h)
            } else {
                (820.0, 600.0)
            };
            let window = WindowBuilder::new(app, "main")
                .title("AnyChat")
                .inner_size(init_w, init_h)
                .center()
                .min_inner_size(420.0, 320.0)
                .visible(false)
                .focused(false)
                .always_on_top(false)
                .resizable(true)
                // No native title bar at all: the system traffic lights used to
                // float over our header. The rail header now draws its own
                // close/pin buttons, so the window is undecorated.
                .decorations(false)
                .transparent(true);
            #[cfg(target_os = "macos")]
            let window = window.hidden_title(true);
            let window = window.build()?;

            // 40px corners. The content webview is a native child view stacked
            // over the rail, so rounding the HTML alone leaves square corners
            // showing through: the window's layer has to clip them.
            // Rounding is done by clipping the NSWindow's layer, which only
            // exists on macOS; elsewhere the platform's own decorations decide
            // the silhouette and there is nothing to report.
            #[cfg(target_os = "macos")]
            {
                if !round_window_corners(&window, WINDOW_RADIUS) {
                    eprintln!("anychat: window corners could not be rounded");
                }
            }

            let rail = WebviewBuilder::new(RAIL_LABEL, WebviewUrl::App("index.html".into()))
                .initialization_script("window.__ANYCHAT__ = { hidden: true };");
            window.add_child(
                rail,
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(980.0, 760.0),
            )?;

            apply_rail_width(&handle, rail_width(&handle));
            // Icons are I/O; start them only after the window and rail exist.
            let icon_handle = handle.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                prefetch_icons(icon_handle);
            });

            build_app_menu(&handle)?;
            build_tray(&handle)?;

            // `register` alone only reserves the accelerator -- it installs no
            // callback, so the key looked bound while doing nothing at all.
            // `on_shortcut` both registers and wires the handler.
            let shortcut = Shortcut::new(Some(Modifiers::ALT), Code::Space);
            let registered = handle
                .global_shortcut()
                .on_shortcut(shortcut, |app, _shortcut, event| {
                    // Fire once per press; the release event would toggle back.
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    toggle_window(app);
                })
                .is_ok();
            if !registered {
                eprintln!("anychat: ⌥Space is taken by another app; use the tray menu");
            }

            // First tool is created eagerly so the window has content the moment
            // it is summoned, but the window itself stays hidden until asked.
            let first = TOOLS[0].id.to_string();
            let _ = select_tool(handle.clone(), first);

            // Launch animation, unless this is a headless gate run: the tests
            // must not race a splash window that is about to close itself.
            let skip_splash = std::env::var("ANYCHAT_SMOKE").is_ok()
                || std::env::var("ANYCHAT_PROBE").is_ok()
                || std::env::var("ANYCHAT_NO_SPLASH").is_ok();
            if !skip_splash {
                let monitor = handle
                    .get_window("main")
                    .and_then(|w| w.current_monitor().ok().flatten());
                if let Err(e) = build_splash(&handle, monitor) {
                    eprintln!("anychat: splash failed, showing window directly: {e}");
                    handle
                        .state::<App>()
                        .booted
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    if std::env::var("ANYCHAT_DEMO_SHOW").is_ok() {
                        show_window(&handle);
                    }
                }
            } else {
                // No splash: behave exactly as it did before it existed.
                handle
                    .state::<App>()
                    .booted
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if std::env::var("ANYCHAT_DEMO_SHOW").is_ok() {
                    show_window(&handle);
                }
            }
            if let Ok(mode) = std::env::var("ANYCHAT_PERSIST") {
                let handle = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(5000));
                    let inner = handle.clone();
                    let write = mode == "write";
                    let _ = handle.run_on_main_thread(move || run_persist_test(&inner, write));
                });
            }
            if std::env::var("ANYCHAT_PROBE").is_ok() {
                start_probe(handle.clone());
            }
            if std::env::var("ANYCHAT_SMOKE").is_ok() {
                let handle = handle.clone();
                // Run off the main thread: run_smoke sleeps a lot, and sleeping
                // on the main thread starves the rail's IPC and its timers, which
                // is exactly what used to make the settings report never arrive.
                // run_smoke drives window APIs, which must run on the main thread,
                // but it also sleeps. Pump the event loop between phases instead of
                // blocking it: spawn a thread that posts one step at a time.
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(4000));
                    run_smoke_async(handle);
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    // Closing hides; the process (and every session) stays resident.
                    api.prevent_close();
                    let _ = window.hide();
                }
                // The child webviews are positioned absolutely, so they do not
                // follow the window on their own.
                // Auto-hide when the window stops being the active one.
                //
                // For a summonable utility this is the expected behaviour: click
                // away and it gets out of the way, no ⌘W needed. It is also the
                // riskiest event to act on, so three things suppress it -- an
                // open context menu, an open settings panel, and any window the
                // app just opened on the user's behalf (a login popup). Hiding
                // in those cases would make the app appear to have crashed.
                tauri::WindowEvent::Focused(false) => {
                    // Headless gate runs resize and focus the window on purpose;
                    // auto-hiding under them invalidates every geometry check.
                    if std::env::var("ANYCHAT_PROBE").is_ok()
                        || std::env::var("ANYCHAT_SMOKE").is_ok()
                    {
                        return;
                    }
                    let app = window.app_handle().clone();
                    // Deferred: focus bounces while a native popup or a menu is
                    // being torn down, and hiding on the first blur would close
                    // the window the user is still working in.
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        // The window may have regained focus in the meantime.
                        let Some(w) = app.get_window("main") else { return };
                        if w.is_focused().unwrap_or(false) || !w.is_visible().unwrap_or(false) {
                            return;
                        }
                        let st = app.state::<App>();
                        if st.settings_open.load(std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        if st.menu_open.load(std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        // A popup counts while it is on screen: the login window
                        // is the focused window then, and hiding the opener
                        // would strand the flow.
                        #[cfg(target_os = "macos")]
                        {
                            if any_other_window_open(&app) {
                                return;
                            }
                        }
                        let _ = w.hide();
                        on_window_hidden(&app);
                    });
                }
                tauri::WindowEvent::Resized(_) => {
                    let app = window.app_handle();
                    relayout_all(app);
                    // Remember the window size the same way the rail width is
                    // remembered, so a restart opens where the user left it.
                    let (w, h) = logical_window_size(app);
                    if w > 0.0 && h > 0.0 {
                        let mut layout = load_layout(app);
                        layout.win_w = w;
                        layout.win_h = h;
                        save_layout(app, &layout);
                    }
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running AnyChat");
}
