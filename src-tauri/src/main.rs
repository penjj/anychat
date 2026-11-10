// Without this the release build links as a console application on Windows, so
// a black terminal window opens alongside the app on every launch -- a
// subsystem flag cannot be set from the bundle config, only from the crate
// root. Debug builds keep the console on purpose: that is where the
// diagnostics go.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
use tauri_plugin_updater::UpdaterExt;

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
    /// Last self-report each tool page made about whether it actually painted.
    ///
    /// `Webview` exposes no visibility query, so the host cannot ask "is this
    /// child view on screen". The page itself is the only witness: it reports a
    /// rendered body, and the host pairs that with what it believes it showed.
    /// A page that loaded but reports no paint is the white-screen case, and
    /// nothing in the process would otherwise notice it.
    paint: std::sync::Mutex<std::collections::HashMap<String, String>>,
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
    /// A new version that has been found, and whether its payload is already on
    /// disk waiting for a relaunch. Shared with the rail so the banner reflects
    /// the real state rather than a copy that can drift.
    update_ready: std::sync::Mutex<Option<ReadyUpdate>>,
    /// The rail has mounted and laid itself out. Recorded so handover can tell
    /// "the app is usable" from "the launch film has finished" -- the splash
    /// ends on the second, and the first only means it will not be wasted.
    ready: std::sync::atomic::AtomicBool,
    /// Verified payload from the background download. Kept here, not on disk,
    /// so a relaunch that the user dismisses leaves nothing stale to clean up.
    pending_bytes: std::sync::Mutex<Option<Vec<u8>>>,
}

/// A downloaded update waiting for a relaunch.
#[derive(Serialize, Clone)]
struct ReadyUpdate {
    version: String,
    /// False while the payload is still being fetched. The banner appears at
    /// the moment a version is found so the user is not surprised later.
    downloaded: bool,
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
    /// Summon accelerator in Tauri's string form, e.g. "Alt+Space". Empty means
    /// "never set", which is what lets the platform default below take over.
    #[serde(default)]
    shortcut: String,
    /// Where a link that leaves the current site should open.
    ///
    /// "browser" keeps the tool webview on its own site: without it, clicking a
    /// citation mid-conversation navigated the chat away and the conversation
    /// was still there, one back-press from being lost. "app" keeps everything
    /// inside, which is what a kiosk-style setup wants.
    #[serde(default = "default_external_links")]
    external_links: String,
    /// Whether to look for a newer release shortly after launch. On by default:
    /// an app installed from a download link has no other way to learn that it
    /// is behind. Nothing is downloaded or installed without the user clicking
    /// through, so the check itself carries no risk.
    #[serde(default = "default_true")]
    auto_check_update: bool,
}

fn default_true() -> bool {
    true
}

/// Open links that leave the site in the system browser.
fn default_external_links() -> String {
    "browser".to_string()
}

fn default_theme() -> String {
    "auto".to_string()
}

/// The default summon accelerator per platform.
///
/// macOS keeps ⌥Space: it is the system-wide spotlight-free gap, and ⌘Space is
/// already Spotlight. Windows uses Ctrl+Alt+Space -- Win+Space switches the
/// keyboard layout, and a bare Alt+Space opens the window's system menu, so both
/// obvious choices are taken by the OS.
fn default_shortcut() -> String {
    if cfg!(target_os = "macos") {
        "Alt+Space".to_string()
    } else {
        "Ctrl+Alt+Space".to_string()
    }
}

/// Every accelerator worth offering in the UI, in the order the picker shows.
/// Kept as a constant so the menu, the settings panel and the docs cannot drift.
const SHORTCUT_CHOICES: &[(&str, &str)] = &[
    ("Alt+Space", "⌥ Space / Alt+Space"),
    ("Ctrl+Alt+Space", "Ctrl + Alt + Space"),
    ("Alt+Shift+Space", "⌥ ⇧ Space / Alt+Shift+Space"),
    ("Ctrl+Shift+Space", "Ctrl + Shift + Space"),
    ("Super+Shift+A", "⌘ ⇧ A / Win+Shift+A"),
];

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
            shortcut: String::new(),
            auto_check_update: true,
            external_links: default_external_links(),
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
    /// The accelerator in force, so the settings panel shows the real binding
    /// rather than a guess, and offers the platform default when unset.
    shortcut: String,
    visible: bool,
    rail_width: f64,
    always_on_top: bool,
    settings_open: bool,
    icon_count: usize,
    zoom: f64,
    theme: String,
    /// The running build's version, from the bundle rather than from a config
    /// read at startup: this is what the settings panel labels itself with, and
    /// a version that disagreed with the installed app would make every
    /// "up to date" message questionable.
    version: String,
    /// Mirrors the saved preference so the checkbox reflects the setting that
    /// actually governs the next launch, not a transient toggle position.
    auto_check_update: bool,
    /// "browser" or "app": where links leaving the current site go.
    external_links: String,
    /// The version waiting to be applied, if any. Null when there is none, so
    /// the banner is driven by state rather than by a separate event that could
    /// be missed during a reload.
    update_ready: Option<ReadyUpdate>,
}

#[derive(Deserialize, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// One profile directory for every tool, so cookies and storage are shared the
/// way they are in a browser. Private tabs are not modelled here: isolation
/// between tools was the previous behaviour and it broke single sign-on.
fn shared_profile_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = data_root(app)?.join("profile");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A private user-data directory for the rail, beside the tools' store.
///
/// Distinct from `profile/` on purpose: two webviews cannot share one user-data
/// directory through separate environments, and the rail needs nothing that is
/// stored there.
#[cfg(not(target_os = "macos"))]
fn rail_profile_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = data_root(app)?.join("rail-profile");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A private user-data directory for the splash window on Windows.
///
/// The rail keeps Tauri's default directory and the splash gets this one, so
/// the two webviews created during `setup` never open the same one. They are
/// the only pair that exists before the event loop runs, which is why only they
/// need separating.
#[cfg(windows)]
fn splash_profile_dir() -> PathBuf {
    // Unique per process, not one shared name. The gate launches the app twice
    // in a row -- once to time the handover, once to run the checks -- and
    // kills the first between them. A fixed directory then still looks busy to
    // the second launch and it fails exactly like the collision this avoids.
    let dir = std::env::temp_dir().join(format!(
        "anychat-splash-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// The one WKWebsiteDataStore every tool shares.
///
/// This used to be hashed per tool, which gave each its own cookie jar. The
/// result was a sign-in per tool for anything that authenticates centrally:
/// three Google tools meant three Google sign-ins, and each of them landed in
/// a store the others could not see. Sharing one jar matches what a browser
/// does and is what "log in once" requires.
///
/// The cost is real and is the reason per-tool jars existed: two accounts on
/// the same site cannot be held at once. That is a browser's tradeoff too, and
/// it is the one worth making -- the common case is a single account across
/// several tools, not two accounts on one site.
///
/// The value is arbitrary but must never change between builds: it names an
/// on-disk store, and a new value would strand every existing sign-in.
#[cfg(target_os = "macos")]
fn store_id(_id: &str) -> [u8; 16] {
    *b"anychat-shared01"
}

/// The one WebView2 environment every content webview shares on Windows.
///
/// Each `add_child` without this calls `CreateCoreWebView2EnvironmentWithOptions`
/// against the same user-data directory, and the second one to do so fails with
/// `HRESULT(0x800700AA)` — "The requested resource is in use". That is the
/// reported white screen: the first chat works, every later one is a child view
/// whose environment never initialised, so the window shows nothing.
///
/// macOS has no equivalent: WKWebView shares a `WKWebsiteDataStore` instead, and
/// several WKWebViews coexisting is normal.
#[cfg(windows)]
mod win_env {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment;
    use windows_core::Interface;

    /// The live environment, once any webview has been created.
    ///
    /// A COM interface is not `Send`, and Tauri hands it out only inside
    /// `with_webview`, so it cannot simply be kept in a `static`. The address it
    /// carries is stored instead -- `as_raw()` gives the interface pointer, which
    /// is stable for the life of the process because WebView2 keeps an
    /// environment alive as long as any controller refers to it.
    ///
    /// Taking the address of the *wrapper* rather than the interface is the
    /// mistake to avoid: `&env as *const _ as usize` stores a stack address that
    /// dies with the call. That read back as whatever happened to occupy the
    /// slot -- on the runner, a UTF-16 fragment -- and aborted with "misaligned
    /// pointer dereference" before the window existed.
    static ENV: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    pub fn publish(env: &ICoreWebView2Environment) {
        // First writer wins. Later children hold the same environment, so a
        // second store would be the same pointer anyway.
        let _ = ENV.compare_exchange(
            0,
            env.as_raw() as usize,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// How many content views have been created in this process.
    static CONTENT_VIEWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    pub fn content_views() -> usize {
        CONTENT_VIEWS.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn count_content_view() {
        CONTENT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn has_environment() -> bool {
        ENV.load(std::sync::atomic::Ordering::SeqCst) != 0
    }

    pub fn get() -> Option<ICoreWebView2Environment> {
        let addr = ENV.load(std::sync::atomic::Ordering::SeqCst);
        if addr == 0 {
            return None;
        }
        // Borrowed, then owned: `to_owned` is what raises the reference count.
        // `from_raw` would take ownership of a reference nobody created, so
        // each call would consume the one the previous handle owned. The first
        // webview to join would work and every later one would wrap freed
        // memory -- creation reports success and the page never navigates,
        // which reads as a white screen rather than as a crash.
        let raw = addr as *mut std::ffi::c_void;
        unsafe { ICoreWebView2Environment::from_raw_borrowed(&raw).map(|e| e.to_owned()) }
    }
}

/// Records the environment of a freshly created webview so later children reuse
/// it instead of initialising a second one against the same directory.
#[cfg(windows)]
fn remember_environment(webview: &Webview) {
    let _ = webview.with_webview(|platform| {
        win_env::publish(&platform.environment());
    });
}

/// Joins the shared environment when one exists; the first child has none and
/// creates it.
#[cfg(windows)]
fn with_shared_environment<R: tauri::Runtime>(
    builder: WebviewBuilder<R>,
) -> WebviewBuilder<R> {
    match win_env::get() {
        Some(env) => builder.with_environment(env),
        None => builder,
    }
}

/// Blocks until a content view has published its environment, or gives up.
///
/// The environment is published from a background thread precisely so asking
/// for it cannot re-enter webview creation. That makes publication arrive a
/// moment later than the child that triggers it, so a second tool opened right
/// away would find nothing, build its own environment over the same directory,
/// and fail with ERROR_BUSY -- the original defect. Waiting closes that window.
///
/// Bounded, because no environment ever appearing means the first child failed,
/// and then the caller should proceed and let the failure surface as it is.
#[cfg(windows)]
fn wait_for_environment(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if win_env::has_environment() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    win_env::has_environment()
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
    #[cfg(windows)]
    {
        // The first content view has nobody to wait for; every later one waits
        // for the environment its predecessor publishes on a background thread.
        if win_env::content_views() > 0 {
            let _ = wait_for_environment(std::time::Duration::from_secs(3));
        }
        win_env::count_content_view();
        builder = with_shared_environment(builder);
    }

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
    // Diagnostic override: a blank UA means "use the engine default", which is
    // the fastest way to tell a site that refuses Safari-UA script from a view
    // that never loaded at all.
    if std::env::var("ANYCHAT_DEFAULT_UA").is_ok() {
        // leave the default
    } else {
        builder = builder.user_agent(safari_user_agent());
    }

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
    // ⌘N, and the link policy.
    //
    // Links are intercepted here rather than through a navigation handler
    // because the decision is per-click and needs the target: a citation inside
    // a chat must not navigate the chat away, while the site's own routes must
    // keep working. Same-registrable-domain links are left entirely alone, so
    // the SPA routing of every site is unaffected.
    // Paint witness. `Webview` has no visibility query, so the only way to tell
    // a loaded-but-blank page from a rendered one is to ask the document. The
    // white-screen regression is exactly this: the view is on screen, the title
    // loads, and nothing is painted because the compositor never attached it.
    let paint_script = format!(
        r#"(function () {{
             var label = '{label}';
             function painted() {{
               var b = document.body;
               if (!b) return {{ ok: false, why: 'no-body' }};
               var text = (b.innerText || '').trim().length;
               var kids = b.children ? b.children.length : 0;
               var r = b.getBoundingClientRect();
               return {{
                 ok: kids > 0 && r.width > 0 && r.height > 0,
                 why: 'kids=' + kids + ' text=' + text + ' w=' + Math.round(r.width) +
                      ' h=' + Math.round(r.height),
               }};
             }}
             function report() {{
               var p = painted();
               try {{
                 window.__TAURI_INTERNALS__.invoke('tool_paint', {{
                   label: label, painted: p.ok, detail: p.why,
                 }});
               }} catch (err) {{
                 try {{
                   window.__TAURI_INTERNALS__.invoke('tool_paint', {{
                     label: label, painted: false,
                     detail: 'invoke-failed ' + (err && err.message || err),
                   }});
                 }} catch (_) {{}}
               }}
             }}
             // Report once the document has settled, and again after the load
             // event so a page that paints late is not called blank too soon.
             if (document.readyState === 'complete') report();
             else window.addEventListener('load', report, {{ once: true }});
             setTimeout(report, 2500);
           }})();
"#,
        label = label
    );
    builder = builder.initialization_script(paint_script);

    builder = builder.initialization_script(
        r#"document.addEventListener('keydown', function (e) {
             if (!(e.metaKey || e.ctrlKey) || e.shiftKey) return;
             if (e.key !== 'n' && e.key !== 'N') return;
             e.preventDefault();
             try { window.__TAURI_INTERNALS__.invoke('new_chat') } catch (_) {}
           }, true);

           // Set by the host once the saved preference is known. Defaults to
           // leaving the page alone, so a webview that loads before the flag
           // arrives behaves exactly as it did before this existed.
           window.__ANYCHAT_EXTERNAL__ = window.__ANYCHAT_EXTERNAL__ || 'app';

           function registrable(host) {
             var parts = String(host || '').split('.');
             return parts.slice(-2).join('.');
           }

           document.addEventListener('click', function (e) {
             if (e.defaultPrevented || e.button !== 0) return;
             if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
             var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
             if (!a) return;
             var href = a.getAttribute('href') || '';
             if (!href || href.charAt(0) === '#') return;
             if (a.hasAttribute('download') || a.target === '_blank') return;
             var url;
             try { url = new URL(href, location.href) } catch (_) { return; }
             if (url.protocol !== 'http:' && url.protocol !== 'https:') return;
             // Same site: let the page handle its own routing.
             if (registrable(url.hostname) === registrable(location.hostname)) return;
             if (window.__ANYCHAT_EXTERNAL__ !== 'browser') return;
             e.preventDefault();
             e.stopPropagation();
             try { window.__TAURI_INTERNALS__.invoke('open_external', { url: url.href }) } catch (_) {}
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
    // Same store for every tool, matching the macOS branch. A per-tool
    // directory here would silently reintroduce the per-tool sign-in that the
    // shared identifier removes there.
    #[cfg(not(target_os = "macos"))]
    if let Some(dir) = shared_profile_dir(app) {
        builder = builder.data_directory(dir);
    }
    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(rail_width(app), BAR_H),
            LogicalSize::new(800.0, 600.0),
        )
        .ok()?;
    #[cfg(windows)]
    {
        // Everything below runs off the creation frame. Asking a webview that
        // is still being created for its URL blocks: `url()` goes through the
        // same channel as creation and is queued behind the frame that has not
        // returned yet. That single call was the whole stall -- the child
        // existed, nothing printed after it, and the window stayed white.
        let late = webview.clone();
        let report_label = label.clone();
        std::thread::spawn(move || {
            // Long enough for creation to finish and the browser to start.
            std::thread::sleep(std::time::Duration::from_millis(600));
            // Publishing first: every later content view waits for it.
            remember_environment(&late);
            let initial = late
                .url()
                .map(|u| u.to_string())
                .unwrap_or_else(|e| format!("err:{e}"));
            println!("TOOLURL {report_label} initial={initial}");
            if initial == "about:blank" {
                // The controller can exist before the browser behind it, and a
                // navigation issued in that window is dropped without an error.
                if let Ok(target) = tools::get(id).map(|t| t.url).unwrap_or_default().parse() {
                    match late.navigate(target) {
                        Ok(()) => println!("TOOLURL {report_label} reissued"),
                        Err(e) => println!("TOOLURL {report_label} reissue-err {e}"),
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2500));
            println!(
                "TOOLURL {report_label} settled={:?}",
                late.url().map(|u| u.to_string()).unwrap_or_else(|e| format!("err:{e}"))
            );
        });
    }
    let _ = webview.set_zoom(load_layout(app).zoom);
    // A tool created after the preference changed must start with it, not with
    // the conservative default the script assumes.
    apply_external_links_to(app, &webview, &load_layout(app).external_links);
    // Not hidden here: a hidden WKWebView can stall its own load, and a heavy
    // SPA that never finishes loading renders as a white page forever. The
    // caller (`select_tool`) shows or hides it once it knows which case it is.
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
/// Wall time since the process started.
///
/// The clock has to begin before the first caller, or every measurement is
/// relative to whenever it happened to be asked: the splash used to report
/// "opening at 0ms" on a launch that had already taken hundreds.
/// Anchor for every startup timestamp, started at the first line of main.
///
/// Lazily initialised on first read would anchor on the reader, not on the
/// process: the splash's own "opening at 0ms" line was measuring itself.
static BOOT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn boot_ms() -> u128 {
    BOOT.get_or_init(std::time::Instant::now).elapsed().as_millis()
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
    // Its own user-data directory, not the shared one. On Windows a webview
    // created during `setup` cannot join an existing environment: joining goes
    // through `with_webview`, which posts to the event loop, and the event loop
    // is not running yet. Two webviews over one directory then fail with
    // ERROR_BUSY, which is the white screen. The splash shares nothing with
    // anyone -- no cookies, no storage, no sign-in -- so a private directory
    // costs nothing and removes the conflict.
    #[cfg(windows)]
    let webview_builder = WebviewBuilder::new(
        SPLASH_LABEL,
        WebviewUrl::App("splash.html".into()),
    )
    .data_directory(splash_profile_dir());
    #[cfg(not(windows))]
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

/// Closes the splash and reveals the main window.
///
/// Idempotent: whichever signal arrives first wins, and the other is a no-op.
/// There are two because either can be the real one depending on the machine --
/// a fast start is ready before the animation's exit beat, a cold one is not.
fn hand_over_from_splash(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering::SeqCst;
    // The window must be shown before the splash goes, or one frame is
    // composited with neither. The swap also makes this the only call that acts.
    if app.state::<App>().booted.swap(true, SeqCst) {
        return;
    }
    eprintln!("anychat: handing over at {}ms", boot_ms());
    if let Some(w) = app.get_window("main") {
        if let (Ok(p), Ok(sz), Ok(sc)) = (w.outer_position(), w.outer_size(), w.scale_factor()) {
            eprintln!(
                "anychat: window at {:.0},{:.0} size {:.0}x{:.0}",
                p.x as f64 / sc, p.y as f64 / sc,
                sz.width as f64 / sc, sz.height as f64 / sc
            );
            if let Ok(ms) = w.available_monitors() {
                for m in &ms {
                    let mp = m.position();
                    let msz = m.size();
                    eprintln!(
                        "anychat: monitor at {:.0},{:.0} size {:.0}x{:.0}",
                        mp.x as f64 / sc, mp.y as f64 / sc,
                        msz.width as f64 / sc, msz.height as f64 / sc
                    );
                }
            }
        }
    }
    if let Some(splash) = app.get_window(SPLASH_LABEL) {
        let _ = splash.close();
    }
    // A window that starts off-screen would be revealed nowhere. The clamp is
    // applied here as well as in show_window because this path reveals the
    // window directly, without going through that function.
    if let Some(window) = app.get_window("main") {
        clamp_to_screens(&window);
    }
    if std::env::var("ANYCHAT_DEMO_SHOW").is_ok()
        || app.state::<App>().wants_visible.load(SeqCst)
    {
        show_window(app);
    }
}

/// Called by the splash when its animation ends. This is now the only thing
/// that ends the launch, so the film always plays in full.
#[tauri::command]
fn splash_done(app: tauri::AppHandle) {
    eprintln!(
        "anychat: splash finished at {}ms (rail ready={})",
        boot_ms(),
        app.state::<App>()
            .ready
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    hand_over_from_splash(&app);
}

/// Pulls the window back onto a visible screen if it is fully or mostly off.
///
/// Dragging is handled by the OS (`startDragging`), which happily lets a window
/// be moved past the edge until nothing is grabbable. The window then still
/// shows on summon, so it looks alive while being unreachable -- and nothing in
/// the app could bring it back, because every control lives inside it.
///
/// Only acted on when no usable part is on screen: a window deliberately parked
/// half over an edge must stay where the user put it.
fn clamp_to_screens(window: &tauri::Window) {
    let Ok(pos) = window.outer_position() else { return };
    let Ok(size) = window.outer_size() else { return };
    let Ok(scale) = window.scale_factor() else { return };
    let Ok(monitors) = window.available_monitors() else { return };
    if monitors.is_empty() {
        return;
    }

    let wx = pos.x as f64 / scale;
    let wy = pos.y as f64 / scale;
    let ww = size.width as f64 / scale;
    let wh = size.height as f64 / scale;
    // A sliver is not "visible": the pointer has to be able to land on the
    // header, and a few pixels at the edge is not a target.
    const MIN_VISIBLE: f64 = 80.0;

    let on_any = monitors.iter().any(|m| {
        let mp = m.position();
        let ms = m.size();
        let mx = mp.x as f64 / scale;
        let my = mp.y as f64 / scale;
        let mw = ms.width as f64 / scale;
        let mh = ms.height as f64 / scale;
        let vis_w = (wx + ww).min(mx + mw) - wx.max(mx);
        let vis_h = (wy + wh).min(my + mh) - wy.max(my);
        vis_w >= MIN_VISIBLE && vis_h >= MIN_VISIBLE
    });
    if on_any {
        return;
    }

    // Centre on the primary monitor. Not the nearest edge: the point is to
    // guarantee every control is reachable, and an edge nudge can leave the
    // window still mostly clipped on a small display.
    let Some(primary) = monitors.first() else { return };
    let mp = primary.position();
    let ms = primary.size();
    let mx = mp.x as f64 / scale;
    let my = mp.y as f64 / scale;
    let mw = ms.width as f64 / scale;
    let mh = ms.height as f64 / scale;

    let x = mx + ((mw - ww) / 2.0).max(0.0);
    let y = my + ((mh - wh) / 2.0).max(0.0);
    eprintln!("anychat: window was off-screen at {wx:.0},{wy:.0}; recentring to {x:.0},{y:.0}");
    let _ = window.set_position(LogicalPosition::new(x, y));
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_window("main") {
        if window.is_visible().unwrap_or(false) {
            emit_state(app);
            return;
        }
        // Before revealing it: a window summoned at an off-screen coordinate is
        // indistinguishable from one that did not appear at all.
        clamp_to_screens(&window);
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

/// Records a tool page's own claim about whether it painted.
///
/// Reporting the *label* rather than trusting the active id matters: after a
/// switch the previous page can be the last one to speak, and attributing its
/// report to the new tool would hide exactly the failure worth catching.
#[tauri::command]
fn tool_paint(app: tauri::AppHandle, label: String, painted: bool, detail: String) {
    println!("PAINT {label} {painted} {detail}");
    if let Ok(mut map) = app.state::<App>().paint.lock() {
        map.insert(label, format!("{painted} {detail}"));
    }
}

/// The rail reports what it actually rendered, so the gate can assert the DOM.
#[tauri::command]
fn rail_ready(app: tauri::AppHandle, report: String) {
    println!("RAIL {report}");
    *app.state::<App>().rail.lock().unwrap() = Some(report);
    // Readiness alone does not end the splash. It used to, which killed the
    // animation about a fifth of the way in and left the screen empty: the
    // window is hidden until handover, so closing the splash early shows
    // nothing at all rather than something sooner. The film is deliberately
    // three seconds; what it must never do is outlast readiness by much, so
    // this only records that the app is ready and lets the splash's own end
    // decide.
    app.state::<App>()
        .ready
        .store(true, std::sync::atomic::Ordering::SeqCst);
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
        shortcut_ok: app.global_shortcut().is_registered(active_shortcut(app).as_str()),
        shortcut: active_shortcut(app),
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
        version: app.package_info().version.to_string(),
        auto_check_update: load_layout(app).auto_check_update,
        external_links: load_layout(app).external_links,
        update_ready: app.state::<App>().update_ready.lock().unwrap().clone(),
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

/// Registers the summon accelerator, replacing whatever was bound before.
///
/// `register` alone only reserves the key -- it installs no callback, so the
/// shortcut looked bound while doing nothing. `on_shortcut` does both.
fn bind_shortcut(app: &tauri::AppHandle, accel: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    // Rebinding must not leave the previous accelerator live, or the old keys
    // would keep summoning the window alongside the new ones.
    let _ = gs.unregister_all();
    gs.on_shortcut(accel.parse::<Shortcut>().map_err(|e| e.to_string())?, |app, _s, event| {
        // Fire once per press; the release event would toggle straight back.
        if event.state() != ShortcutState::Pressed {
            return;
        }
        toggle_window(app);
    })
    .map_err(|e| format!("{accel} 已被其他应用占用 ({e})"))
}

/// The accelerator currently in force: the saved value unless it is unset.
fn active_shortcut(app: &tauri::AppHandle) -> String {
    let saved = load_layout(app).shortcut;
    if saved.is_empty() {
        default_shortcut()
    } else {
        saved
    }
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

    // An upper bound as well as a lower one. Dragging the grip used to grow the
    // window without limit, and a window taller than the display puts its own
    // header off the top of the screen -- there is then no way to grab it, and
    // the size is persisted, so a restart restores the same unreachable frame.
    //
    // The limit is the largest display's usable area rather than a constant:
    // the window must fit on *some* screen, not on an imagined one.
    let (max_w, max_h) = window
        .available_monitors()
        .ok()
        .filter(|m| !m.is_empty())
        .map(|monitors| {
            monitors.iter().fold((0.0_f64, 0.0_f64), |(mw, mh), m| {
                let s = m.size();
                let sc = m.scale_factor() as f64;
                (
                    mw.max(s.width as f64 / sc),
                    mh.max(s.height as f64 / sc),
                )
            })
        })
        .unzip();
    let max_w = max_w.filter(|v| *v > MIN_W).unwrap_or(f64::INFINITY);
    let max_h = max_h.filter(|v| *v > MIN_H).unwrap_or(f64::INFINITY);


    if corner.contains('w') {
        let nw = (w - dx).clamp(MIN_W, max_w);
        x += w - nw;
        w = nw;
    } else if corner.contains('e') {
        w = (w + dx).clamp(MIN_W, max_w);
    }
    if corner.contains('n') {
        let nh = (h - dy).clamp(MIN_H, max_h);
        y += h - nh;
        h = nh;
    } else if corner.contains('s') {
        h = (h + dy).clamp(MIN_H, max_h);
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
    // ⌘N also arrives from a tool page, where the rail's own handler never runs.
    // Announcing here means the acknowledgement appears however the shortcut was
    // pressed, rather than only when it started in the rail.
    let _ = app.emit_to(RAIL_LABEL, "new-chat", ());
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
/// Changes the summon accelerator.
///
/// Rebinding is only committed once the new keys are actually held: saving
/// first would leave the user with no working shortcut if the pick was already
/// taken by another app.
#[tauri::command]
fn set_shortcut(app: tauri::AppHandle, shortcut: String) -> Result<String, String> {
    let wanted = shortcut.trim().to_string();
    let restore = active_shortcut(&app);

    if let Err(why) = bind_shortcut(&app, &wanted) {
        // Put the old binding back so the window stays reachable.
        let _ = bind_shortcut(&app, &restore);
        return Err(why);
    }

    let mut layout = load_layout(&app);
    layout.shortcut = wanted.clone();
    save_layout(&app, &layout);
    emit_state(&app);
    Ok(wanted)
}

/// What the settings panel shows about available updates.
#[derive(Serialize)]
struct UpdateInfo {
    available: bool,
    version: String,
    current: String,
    notes: String,
}

/// Where release assets live, without a trailing slash.
const RELEASE_BASE: &str = "https://github.com/penjj/anychat/releases/latest/download";

/// Canonical manifest. Everything else is a mirror of what lives here.
const UPDATE_ENDPOINT: &str = concat!(
    "https://github.com/penjj/anychat/releases/latest/download",
    "/latest.json"
);

/// Builds the updater, honouring the endpoint overrides when set.
///
/// The overrides exist so the whole flow -- check, download, verify, install,
/// restart -- can be exercised against a locally served manifest without
/// publishing a release. They are environment variables rather than config
/// fields so they cannot be set by accident in a shipped build.
///
/// Endpoints are tried in order, so a reachable mirror ahead of the release
/// host is used without any change to the build.
///
/// GitHub releases are slow or unreachable from mainland networks. The updater
/// already walks the list on failure, so the configured host stays last: with a
/// responsive first entry the check finishes even when the original would hang.
fn update_endpoints() -> Vec<String> {
    let mut list = Vec::new();

    // Development override wins outright: pointing at a local manifest must
    // test exactly that manifest, not a mirror that happens to answer.
    if let Ok(endpoint) = std::env::var("ANYCHAT_UPDATE_ENDPOINT") {
        if !endpoint.is_empty() {
            return endpoint
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    // Mirrors first. A mirror serves the same signed latest.json, so a wrong or
    // malicious mirror cannot install anything: the signature embeds the
    // version and the payload is verified against the compiled-in key.
    if let Ok(mirrors) = std::env::var("ANYCHAT_UPDATE_MIRRORS") {
        list.extend(
            mirrors
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    if let Ok(mirror) = std::env::var("ANYCHAT_UPDATE_MIRROR") {
        if !mirror.is_empty() {
            list.push(mirror);
        }
    }

    // Proxy entries come in matched pairs: the manifest at index i is fetched
    // through proxy i, and its platform URLs are rewritten to the same proxy.
    // Fetching a manifest through a proxy while its download URL still named
    // github.com would only move the stall to the payload, not remove it.
    for (i, prefix) in GITHUB_PROXIES.iter().enumerate() {
        if prefix.is_empty() {
            continue;
        }
        list.push(format!(
            "{prefix}{RELEASE_BASE}/latest-mirror{}.json",
            i + 1
        ));
    }

    // Direct last: it is the reference implementation, and it stays in the list
    // so updating keeps working when a proxy goes offline.
    list.push(UPDATE_ENDPOINT.to_string());
    list
}

/// Reverse proxies, index-aligned with the `latest-mirrorN.json` assets.
///
/// These are third-party relays: they can disappear without notice, so the
/// release host stays in the list and the count stays small. An empty entry is
/// skipped, which is how a dead proxy is retired without renumbering the rest.
const GITHUB_PROXIES: &[&str] = &["https://ghproxy.net/", "https://ghfast.top/"];

fn build_updater(app: &tauri::AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let mut urls = Vec::new();
    for raw in update_endpoints() {
        match raw.parse() {
            Ok(url) => urls.push(url),
            // A typo in one mirror must not disable updating altogether.
            Err(_) => eprintln!("updater: ignoring bad endpoint {raw}"),
        }
    }
    if urls.is_empty() {
        return Err("no usable update endpoint".to_string());
    }

    app.updater_builder()
        .endpoints(urls)
        .map_err(|e| e.to_string())?
        // Without a bound, an unreachable host leaves the check spinning for
        // minutes. Ten seconds is enough for a working mirror and short enough
        // to move on.
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Finds a newer release and fetches its payload in the background.
///
/// Fetching ahead of time is what makes the banner's button a relaunch rather
/// than a download: the user is told a version is ready, and clicking gives
/// them that version immediately. Nothing is installed without that click --
/// swapping the binary underneath someone mid-sentence is not a thing an app
/// should do on its own.
///
/// A failure at any step is silent. There is no useful action for the user to
/// take about a check that did not reach the network, and an app that reports
/// every failed background attempt is an app people turn updates off in.
async fn auto_update(app: tauri::AppHandle) {
    let info = match check_update(app.clone()).await {
        Ok(info) => info,
        Err(why) => {
            println!("PROBE autocheck failed {why}");
            return;
        }
    };
    if !info.available {
        println!("PROBE autocheck current version={}", info.current);
        return;
    }
    println!(
        "PROBE autocheck available version={} current={}",
        info.version, info.current
    );

    // Announce first, then download. The banner is the user's cue that
    // something is happening; waiting for the payload would leave the check
    // invisible for however long 4 MB takes on a bad connection.
    {
        let state = app.state::<App>();
        *state.update_ready.lock().unwrap() = Some(ReadyUpdate {
            version: info.version.clone(),
            downloaded: false,
        });
    }
    emit_state(&app);

    let updater = match build_updater(&app) {
        Ok(u) => u,
        Err(why) => {
            println!("PROBE autoupdate failed {why}");
            return;
        }
    };
    let update = match updater.check().await {
        Ok(Some(u)) => u,
        _ => {
            println!("PROBE autoupdate failed recheck");
            return;
        }
    };
    // Not installed here: only the bytes are kept, and install_update applies
    // them when the user asks to relaunch.
    match update.download(|_, _| {}, || {}).await {
        Ok(bytes) => {
            let state = app.state::<App>();
            state.pending_bytes.lock().unwrap().replace(bytes);
            if let Some(ready) = state.update_ready.lock().unwrap().as_mut() {
                ready.downloaded = true;
            }
            emit_state(&app);
            println!(
                "PROBE autoupdate ready version={}",
                info.version
            );
        }
        Err(why) => println!("PROBE autoupdate failed {why}"),
    }
}

/// Looks for a newer release without downloading it.
///
/// Returns `available: false` rather than an error when the check cannot be
/// performed (offline, no key configured, endpoint unreachable): the settings
/// panel should say "cannot check right now", not look broken.
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let updater = match build_updater(&app) {
        Ok(u) => u,
        Err(e) => {
            return Ok(UpdateInfo {
                available: false,
                version: String::new(),
                current,
                notes: format!("更新器不可用：{e}"),
            })
        }
    };

    match updater.check().await {
        Ok(Some(update)) => Ok(UpdateInfo {
            available: true,
            version: update.version.clone(),
            current,
            notes: update.body.clone().unwrap_or_default(),
        }),
        Ok(None) => Ok(UpdateInfo {
            available: false,
            version: String::new(),
            current,
            notes: "已是最新版本".to_string(),
        }),
        Err(e) => Ok(UpdateInfo {
            available: false,
            version: String::new(),
            current,
            notes: format!("检查失败：{e}"),
        }),
    }
}

/// Downloads and installs the pending update, then restarts into it.
///
/// Progress goes to the rail as `update-progress` events so the panel can show
/// a bar; the restart happens here rather than in the page, because the page is
/// destroyed as part of it.
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    let updater = build_updater(&app)?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("没有可用更新".to_string());
    };

    // The background fetch may already hold the verified payload, in which
    // case installing is immediate. Re-downloading here would make the banner's
    // promise -- "ready, click to relaunch" -- false.
    let pending = app.state::<App>().pending_bytes.lock().unwrap().take();
    if let Some(bytes) = pending {
        update.install(&bytes).map_err(|e| e.to_string())?;
        emit_state(&app);
        app.restart();
    }

    let progress_app = app.clone();
    let mut downloaded: u64 = 0;
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = progress_app.emit_to(
                    RAIL_LABEL,
                    "update-progress",
                    serde_json::json!({ "downloaded": downloaded, "total": total }),
                );
            },
            || {
                // Called once the payload is on disk, before it is applied.
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    // The new binary only takes effect on relaunch.
    app.restart();
}

/// Called after a drag finishes.
///
/// The OS drag is fire-and-forget: it ends wherever the pointer was released,
/// including past the edge of every display. Checking here means the window
/// cannot be parked somewhere unreachable in the first place, rather than
/// relying on the next summon to notice.
#[tauri::command]
fn clamp_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_window("main") {
        clamp_to_screens(&window);
    }
}

#[tauri::command]
fn set_external_links(app: tauri::AppHandle, mode: String) -> Result<String, String> {
    let mode = match mode.as_str() {
        "browser" | "app" => mode,
        other => return Err(format!("unknown external_links: {other}")),
    };
    let mut layout = load_layout(&app);
    layout.external_links = mode.clone();
    save_layout(&app, &layout);
    apply_external_links(&app, &mode);
    emit_state(&app);
    Ok(mode)
}

/// Pushes the link policy into every live tool webview.
///
/// The flag lives in the page, not in the host's head: the click happens in a
/// cross-origin document the host never sees, so the only way to change the
/// behaviour for an already-open tool is to rewrite that document's global.
/// Applied to all of them, including hidden ones -- a switch made now has to
/// hold when the user returns to a tool they left open.
fn apply_external_links(app: &tauri::AppHandle, mode: &str) {
    for tool in tools::TOOLS {
        if let Some(w) = app.get_webview(&format!("tool_{}", tool.id)) {
            apply_external_links_to(app, &w, mode);
        }
    }
}

/// Single-webview form, used both by the broadcast and at creation time.
fn apply_external_links_to(_app: &tauri::AppHandle, webview: &Webview, mode: &str) {
    let js = format!(
        "window.__ANYCHAT_EXTERNAL__ = {}",
        serde_json::to_string(mode).unwrap_or_else(|_| "\"app\"".into())
    );
    let _ = webview.eval(&js);
}

#[tauri::command]
fn set_auto_check_update(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let mut layout = load_layout(&app);
    layout.auto_check_update = enabled;
    save_layout(&app, &layout);
    emit_state(&app);
    Ok(enabled)
}

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
/// Runs one probe command. Shared by both input paths so a command behaves
/// identically whether it arrived on stdin or through the tailed file.
fn run_probe_command(h: tauri::AppHandle, cmd: String) {
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
                                let apply = w.set_size(LogicalSize::new(parts[0], parts[1]));
                                if let Err(e) = &apply {
                                    println!("PROBE resize-set-err {e}");
                                }
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                // Report what the window actually became, not what
                                // was requested: a hidden or constrained window can
                                // silently ignore set_size, which made the resize
                                // check fail with a stale geometry reading.
                                let got = w
                                    .inner_size()
                                    .map(|v| format!("{}x{}", v.width, v.height))
                                    .unwrap_or_else(|_| "?".into());
                                let shown = w.is_visible().unwrap_or(false);
                                println!(
                                    "PROBE resized={}x{} actual={got} visible={shown}",
                                    parts[0], parts[1]
                                );
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
                    "title" => {
                        let active = h.state::<App>().active.lock().unwrap().clone();
                        if let Some(w) = h.get_webview(&format!("tool_{active}")) {
                            println!("PROBE title {:?}", w.url().map(|u| u.to_string()));
                        }
                    }
                    "show" => { show_window(&h); }
                    "hide" => { if let Some(w) = h.get_window("main") { let _ = w.hide(); } }
                    // Whether the injected policy is present in the live page.
                    // eval() cannot return a value to the host, so the report
                    // comes back through the page title on the next read.
                    "linkpolicy" => {
                        let active = h.state::<App>().active.lock().unwrap().clone();
                        match h.get_webview(&format!("tool_{active}")) {
                            Some(w) => {
                                let probe = "document.title = 'LP:' + (window.__ANYCHAT_EXTERNAL__ || 'missing')";
                                match w.eval(probe) {
                                    Ok(()) => println!("PROBE linkpolicy dispatched"),
                                    Err(e) => println!("PROBE linkpolicy err {e}"),
                                }
                            }
                            None => println!("PROBE linkpolicy no_webview"),
                        }
                    }
                    cmd if cmd.starts_with("external_links ") => {
                        let mode = cmd["external_links ".len()..].trim().to_string();
                        match set_external_links(h.clone(), mode) {
                            Ok(m) => println!("PROBE external_links_set {m}"),
                            Err(e) => println!("PROBE external_links_err {e}"),
                        }
                    }
                    "state" => {
                        let dto = state_dto(&h);
                        let (win_w, win_h) = logical_window_size(&h);
                        println!("PROBE state icons={} open={} rail={} win={}x{}",
                            dto.icon_count, dto.settings_open, dto.rail_width, win_w, win_h);
                        println!(
                            "PROBE external_links={}",
                            dto.external_links
                        );
                        // The banner is rendered by the rail page from this
                        // state, so the version here is what the user is
                        // actually being offered.
                        match &dto.update_ready {
                            Some(r) => println!(
                                "PROBE update_ready version={} downloaded={}",
                                r.version, r.downloaded
                            ),
                            None => println!("PROBE update_ready none"),
                        }
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
                    // Exercises the update path without the settings panel, so
                    // the flow can be driven from a script.
                    "check-update" => {
                        let probe_app = h.clone();
                        let handle = h.clone();
                        tauri::async_runtime::spawn(async move {
                            match check_update(probe_app).await {
                                Ok(info) => println!(
                                    "PROBE update available={} version={} current={} notes={}",
                                    info.available, info.version, info.current, info.notes
                                ),
                                Err(e) => println!("PROBE update-err {e}"),
                            }
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                            drop(handle);
                        });
                    }
                    // Downloads and verifies without installing: proves the
                    // signature accepts the payload while leaving the running
                    // bundle untouched, which is what a smoke test needs.
                    // The endpoint order decides which host is tried first, so
                    // it is worth asserting directly rather than inferring it
                    // from whichever one happens to answer.
                    "update-endpoints" => {
                        for url in update_endpoints() {
                            println!("PROBE endpoint {url}");
                        }
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    "download-update" => {
                        let probe_app = h.clone();
                        tauri::async_runtime::spawn(async move {
                            let result = async {
                                let updater = build_updater(&probe_app)?;
                                let update = updater
                                    .check()
                                    .await
                                    .map_err(|e| e.to_string())?
                                    .ok_or_else(|| "no update".to_string())?;
                                let version = update.version.clone();
                                let bytes = update
                                    .download(|_chunk, _total| {}, || {})
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>((version, bytes.len()))
                            }
                            .await;
                            match result {
                                Ok((v, n)) => println!(
                                    "PROBE download-ok version={v} bytes={n} signature=valid"
                                ),
                                Err(e) => println!("PROBE download-err {e}"),
                            }
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        });
                    }
                    "install-update" => {
                        let probe_app = h.clone();
                        tauri::async_runtime::spawn(async move {
                            match install_update(probe_app).await {
                                Ok(()) => println!("PROBE install started"),
                                Err(e) => println!("PROBE install-err {e}"),
                            }
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        });
                    }
                    "quit" => { h.exit(0); }
                    other => println!("PROBE unknown {other}"),
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn start_probe(app: tauri::AppHandle) {
    use std::io::BufRead;

    let handle = app.clone();

    // Commands may arrive through a file instead of stdin.
    //
    // Git Bash on Windows has no `mkfifo`, and feeding a plain file to stdin is
    // broken in a subtler way: the app reads to EOF and exits, so the driver
    // cannot write the next command. Tailing an append-only file keeps the
    // driver dumb and the ordering exact, and it works on every platform the
    // gate runs on.
    if let Ok(path) = std::env::var("ANYCHAT_PROBE_FILE") {
        std::thread::spawn(move || {
            let mut offset: u64 = 0;
            let mut pending = String::new();
            loop {
                // Read only what was appended since the last pass. Re-reading
                // the whole file would replay every command forever.
                if let Ok(mut f) = std::fs::File::open(&path) {
                    use std::io::{Read, Seek, SeekFrom};
                    if f.seek(SeekFrom::Start(offset)).is_ok() {
                        let mut buf = String::new();
                        if let Ok(n) = f.read_to_string(&mut buf) {
                            offset += n as u64;
                            pending.push_str(&buf);
                        }
                    }
                }
                while let Some(idx) = pending.find('\n') {
                    let line = pending[..idx].trim().to_string();
                    pending.drain(..=idx);
                    if line.is_empty() {
                        continue;
                    }
                    let h = handle.clone();
                    let _ = handle.run_on_main_thread(move || run_probe_command(h, line));
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
        });
        return;
    }

    let stdin = std::io::stdin();
    std::thread::spawn(move || {
        for line in stdin.lock().lines().flatten() {
            let cmd = line.trim().to_string();
            if cmd.is_empty() {
                continue;
            }
            let h = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                println!("PROBE tick {cmd}");
                run_probe_command(h, cmd);
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
        // Printed as it happens, not only in the summary at the end. Written to
        // a file, stdout is block-buffered, so a gate that wedges never flushes
        // and CI shows an empty log -- which is exactly how the first Windows
        // run failed: no output at all, and therefore no clue.
        println!("CHECK {name} {}", if ok { "pass" } else { "fail" });
        let _ = std::io::stdout().flush();
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
        // An invisible window cannot take the keyboard, and Windows reports a
        // hidden window as focused anyway. The property that matters is that a
        // *visible* window did not steal focus -- that is what the user feels.
        let focused = w.is_focused().unwrap_or(true);
        check(
            "window-not-focused",
            !visible || !focused || std::env::var("ANYCHAT_DEMO_SHOW").is_ok(),
            format!("visible={visible} focused={focused}"),
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

    // global shortcut: the effective binding, whatever the platform default is
    let accel = active_shortcut(app);
    let shortcut_ok = app.global_shortcut().is_registered(accel.as_str());
    check("shortcut-registered", shortcut_ok, accel.clone());

    // Rebinding is the part that can silently break: the store must end up with
    // the new accelerator *and* the OS must actually own it afterwards, so the
    // check looks at both.
    let alternate = SHORTCUT_CHOICES
        .iter()
        .map(|(v, _)| *v)
        .find(|v| *v != accel.as_str())
        .unwrap_or("Alt+Space");
    match set_shortcut(app.clone(), alternate.to_string()) {
        Ok(applied) => {
            let saved = load_layout(app).shortcut;
            let live = app.global_shortcut().is_registered(applied.as_str());
            check(
                "shortcut-rebind",
                applied == alternate && saved == alternate && live,
                format!("want={alternate} applied={applied} saved={saved} live={live}"),
            );
        }
        Err(why) => check("shortcut-rebind", false, why),
    }
    // Leave the original in place so later checks see the normal binding.
    set_shortcut(app.clone(), accel.clone()).ok();

    // The update check must answer rather than fail when the updater is not
    // configured: the settings panel showing "cannot check" is correct, a
    // rejected command is not.
    check(
        "version-reported",
        !app.package_info().version.to_string().is_empty(),
        app.package_info().version.to_string(),
    );

    // One shared profile, not one directory per tool. The previous check
    // asserted the opposite and would now pass a build that had silently gone
    // back to per-tool jars, which is exactly the regression worth catching:
    // it looks like a working app and costs a sign-in per tool.
    let shared = shared_profile_dir(app);
    check(
        "shared-profile-dir",
        shared.as_ref().is_some_and(|d| d.is_dir()),
        shared
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    // Two different tools must resolve to the same identifier, which is what
    // makes their cookies co-visible.
    #[cfg(target_os = "macos")]
    {
        let a = TOOLS.first().map(|t| store_id(t.id));
        let b = TOOLS.get(1).map(|t| store_id(t.id));
        check(
            "shared-store-identifier",
            a.is_some() && a == b,
            format!("{a:?} vs {b:?}"),
        );
    }

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
    // Wait for the navigation to land, not for a fixed delay. On a hosted
    // runner the webview is still initialising 1.5s later and `url()` still
    // reports about:blank, so the comparison below ran against a page that had
    // not moved yet and failed for a reason unrelated to the shortcut.
    let marker = "google.com";
    let mut before = String::new();
    for _ in 0..60 {
        before = app.get_webview(&other_label)
            .and_then(|w| w.url().ok())
            .map(|u| u.to_string())
            .unwrap_or_default();
        if before.contains(marker) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let _ = new_chat(app.clone());
    let expected = tools::get(other_tool).map(|t| t.new_chat).unwrap_or("");
    // Same reason: wait for the URL the shortcut asks for rather than assuming
    // how long the navigation takes on this machine.
    let mut after = String::new();
    for _ in 0..60 {
        after = app.get_webview(&other_label)
            .and_then(|w| w.url().ok())
            .map(|u| u.to_string())
            .unwrap_or_default();
        if !expected.is_empty() && after.contains(expected) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
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
    // Wait for a report that describes a rendered rail, not merely for the
    // first report. The page reports as soon as it lays out, which can be
    // before the tool list has arrived from the host: that snapshot says
    // `tools:0`, and taking it would report a correct UI as a broken one. Every
    // later state change provokes another report, so this converges.
    let mut dom = String::new();
    for _ in 0..60 {
        if let Some(v) = app.state::<App>().rail.lock().unwrap().clone() {
            let has_tools = serde_json::from_str::<serde_json::Value>(&v)
                .ok()
                .and_then(|p| p.get("tools").and_then(|t| t.as_u64()))
                .is_some_and(|n| n > 0);
            dom = v;
            if has_tools {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // Parsed, not string-matched. The literal `"pinned":10` depended on the
    // serialiser's spacing and on the saved usage file: a clean data directory
    // sorts differently, so the same, correct UI reported a different number and
    // the check failed for a reason that was not a defect.
    let mut rail_ok = false;
    let mut rail_detail = String::from("no rail report");
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&dom) {
        let tools = parsed.get("tools").and_then(|v| v.as_u64()).unwrap_or(0);
        let pinned = parsed.get("pinned").and_then(|v| v.as_u64()).unwrap_or(0);
        let dup = parsed.get("duplicateTools").and_then(|v| v.as_u64()).unwrap_or(1);
        // Every tool appears exactly once, either pinned or in its group, and
        // the pinned set is non-empty. That is the invariant; the exact split
        // depends on usage history, which a fresh runner does not have.
        rail_ok = tools == TOOLS.len() as u64 && dup == 0 && pinned > 0;
        rail_detail = format!("tools={tools} expected={} pinned={pinned} dup={dup}", TOOLS.len());
    }
    check("rail-rendered", rail_ok, rail_detail);
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
    // Anchor the startup clock before anything else: see BOOT.
    BOOT.get_or_init(std::time::Instant::now);
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(App {
            active: std::sync::Mutex::new(
                TOOLS.first().map(|t| t.id.to_string()).unwrap_or_default(),
            ),
            rail: std::sync::Mutex::new(None),
            settings_dom: std::sync::Mutex::new(None),
            settings_open: std::sync::atomic::AtomicBool::new(false),
            content_hidden: std::sync::atomic::AtomicBool::new(false),
            paint: std::sync::Mutex::new(std::collections::HashMap::new()),
            popups_seen: std::sync::atomic::AtomicUsize::new(0),
            popups_opened: std::sync::atomic::AtomicUsize::new(0),
            booted: std::sync::atomic::AtomicBool::new(false),
            last_shown: std::sync::Mutex::new(std::collections::HashMap::new()),
            menu_open: std::sync::atomic::AtomicBool::new(false),
            wants_visible: std::sync::atomic::AtomicBool::new(false),
            update_ready: std::sync::Mutex::new(None),
            ready: std::sync::atomic::AtomicBool::new(false),
            pending_bytes: std::sync::Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            list_tools,
            rail_ready,
            tool_paint,
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
            set_auto_check_update,
            set_external_links,
            clamp_window,
            set_theme,
            set_shortcut,
            check_update,
            install_update,
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

            let mut rail = WebviewBuilder::new(RAIL_LABEL, WebviewUrl::App("index.html".into()))
                .initialization_script("window.__ANYCHAT__ = { hidden: true };");
            // Its own directory, not Tauri's default. The default is the app
            // data directory, which is not governed by ANYCHAT_DATA_DIR, so two
            // launches in a row -- the gate does exactly that -- open the same
            // one and the second fails with ERROR_BUSY while the first is still
            // being torn down. Keeping it under `data_root` makes every webview
            // in the process follow the same isolation rule.
            #[cfg(not(target_os = "macos"))]
            if let Some(dir) = rail_profile_dir(app.handle()) {
                rail = rail.data_directory(dir);
            }
            window.add_child(
                rail,
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(980.0, 760.0),
            )?;
            // Deliberately not registered as the shared environment. An
            // environment is bound to the user-data directory it was created
            // over, and the rail keeps Tauri's default while content views use
            // `profile/`. Handing the rail's environment to a content view makes
            // WebView2 reject the mismatch with ERROR_BUSY -- the same code as
            // the directory collision, which is why it looked like the fix had
            // not applied. The tools share among themselves instead.

            apply_rail_width(&handle, rail_width(&handle));
            // Icons are I/O; start them only after the window and rail exist.
            let icon_handle = handle.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                prefetch_icons(icon_handle);
            });

            // A newer release is announced, never installed behind the user's
            // back: the check runs once shortly after launch, and finding one
            // only puts a mark on the settings row. Delayed past the splash and
            // the icon prefetch, since neither the user nor the network should
            // be asked to do three things at once.
            if load_layout(&handle).auto_check_update {
                let updater = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    println!("PROBE autocheck start");
                    tauri::async_runtime::block_on(auto_update(updater));
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                });
            }

            build_app_menu(&handle)?;
            build_tray(&handle)?;

            if let Err(why) = bind_shortcut(&handle, &active_shortcut(&handle)) {
                eprintln!("anychat: {why}");
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
