use headless_chrome::{Browser, Tab};
use std::time::Instant;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BrowserError {
    #[error("Failed to launch browser: {0}")]
    LaunchFailed(String),
    #[error("Failed to navigate: {0}")]
    NavigationFailed(String),
    #[error("Failed to extract content: {0}")]
    ExtractionFailed(String),
    #[error("Browser crashed - please try again")]
    BrowserCrashed,
}

pub struct BrowserInstance {
    _browser: Browser,
    tab: Arc<Tab>,
    _chrome: Child,
}

const CHROME_ARGS: &[&str] = &[
    "--headless",
    "--remote-debugging-port=0",
    "--disable-dev-shm-usage",
    "--disable-gpu",
    "--disable-software-rasterizer",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-extensions",
    "--disable-setuid-sandbox",
    "--no-sandbox",
    "--disable-crash-reporter",
    "--disable-breakpad",
    "--disable-features=VizDisplayCompositor,Vulkan,OnDeviceModel",
    "--disable-vulkan",
    "--disable-accelerated-2d-canvas",
    "--disable-accelerated-video-decode",
];

impl BrowserInstance {
    pub fn new(callsign: &str) -> Result<Self, BrowserError> {
        let safe_id: String = callsign.chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        let session_dir = format!("/tmp/chrome-{}", safe_id);

        // Create session directory with secure permissions (0o700)
        if !std::path::Path::new(&session_dir).exists() {
            if let Err(e) = std::fs::create_dir(&session_dir) {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    eprintln!("[BROWSER] Warning: could not create session dir: {}", e);
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700)) {
                    eprintln!("[BROWSER] Warning: could not set session dir permissions: {}", e);
                }
            }
        }

        let chromium_path = std::env::var("CHROMIUM_PATH")
            .unwrap_or_else(|_| "/bin/chromium".to_string());

        eprintln!("[BROWSER] Launching Chrome at {}", chromium_path);

        let mut child = Command::new(&chromium_path)
            .args(CHROME_ARGS)
            .arg(format!("--user-data-dir={}", session_dir))
            .env("BREAKPAD_DUMP_LOCATION", &session_dir)
            .env("HOME", "/tmp")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BrowserError::LaunchFailed(format!("Failed to spawn {}: {}", chromium_path, e)))?;

        let stderr = child.stderr.take()
            .ok_or_else(|| BrowserError::LaunchFailed("Could not capture Chrome stderr".to_string()))?;

        let (tx, rx) = mpsc::channel::<String>();

        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut url_sent = false;
            for line in reader.lines().flatten() {
                eprintln!("[CHROME] {}", line);
                if !url_sent {
                    if let Some(url) = line.strip_prefix("DevTools listening on ") {
                        let _ = tx.send(url.trim().to_string());
                        url_sent = true;
                    }
                }
            }
            eprintln!("[CHROME] stderr closed (Chrome exited or crashed)");
        });

        let ws_url = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(url) => url,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                return Err(BrowserError::LaunchFailed(
                    "Chrome did not output DevTools URL within 30 seconds".to_string()
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BrowserError::LaunchFailed(
                    "Chrome exited before outputting DevTools URL".to_string()
                ));
            }
        };

        eprintln!("[BROWSER] Chrome ready, connecting to {}", ws_url);

        let browser = Browser::connect_with_timeout(ws_url, Duration::from_secs(120))
            .map_err(|e| BrowserError::LaunchFailed(e.to_string()))?;

        eprintln!("[BROWSER] Connected to Chrome DevTools");

        eprintln!("[BROWSER] Waiting for Chrome renderer to stabilize...");
        let deadline = Instant::now() + Duration::from_secs(120);
        let tab = loop {
            if let Some(tab) = browser.get_tabs().lock().unwrap().first().cloned() {
                break tab;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                return Err(BrowserError::LaunchFailed(
                    "Chrome renderer did not stabilize within 120 seconds".to_string()
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        };

        eprintln!("[BROWSER] Chrome renderer ready");
        tab.set_default_timeout(Duration::from_secs(15));

        Ok(Self { _browser: browser, tab, _chrome: child })
    }

    pub fn fetch_page(&self, url: &str) -> Result<String, BrowserError> {
        eprintln!("[BROWSER] Fetching: {}", url);

        self.tab.navigate_to(url)
            .map_err(|e| BrowserError::NavigationFailed(e.to_string()))?;

        if let Err(e) = self.tab.wait_until_navigated() {
            eprintln!("[BROWSER] Navigation timeout ({}), attempting extraction anyway", e);
            std::thread::sleep(Duration::from_secs(3));
        }

        eprintln!("[BROWSER] Page loaded: {}", url);
        extract_html(&self.tab)
    }
}

impl Drop for BrowserInstance {
    fn drop(&mut self) {
        eprintln!("[BROWSER] Shutting down Chrome");
        let _ = self._chrome.kill();
    }
}

const JS_SCRUB_HTML: &str = r#"
(async function() {
    const FALLBACK_CSS = `body{font-family:sans-serif;max-width:40em;margin:0 auto;padding:1em;line-height:1.5}a{color:#06c}h1,h2,h3{margin:1em 0 .5em}table{border-collapse:collapse}td,th{border:1px solid #ccc;padding:.3em}img{display:none}`;
    const MAX_SIZE = 32768;

    const stylesheets = Array.from(document.querySelectorAll('link[rel="stylesheet"]'));
    const cssTexts = [];
    
    for (const link of stylesheets) {
        try {
            const response = await fetch(link.href);
            const css = await response.text();
            const cleaned = css.replace(/url\([^)]*\)/g, '');
            cssTexts.push(cleaned);
            link.remove();
        } catch (e) {
        }
    }
    
    if (cssTexts.length > 0) {
        const style = document.createElement('style');
        style.textContent = cssTexts.join('\n');
        document.head.appendChild(style);
    }
    
    const heavySelectors = ['script', 'iframe', 'video', 'audio', 'canvas', 'svg', 'object', 'embed', 'noscript', 'template'];
    for (const sel of heavySelectors) {
        document.querySelectorAll(sel).forEach(el => el.remove());
    }
    
    document.querySelectorAll('img').forEach(img => {
        const alt = img.alt || img.src.split('/').pop() || 'image';
        const text = document.createTextNode(`[image: ${alt}]`);
        img.parentNode.replaceChild(text, img);
    });
    
    document.querySelectorAll('*').forEach(el => {
        Array.from(el.attributes).forEach(attr => {
            if (attr.name.startsWith('on')) {
                el.removeAttribute(attr.name);
            }
        });
    });
    
    let html = document.documentElement.outerHTML;
    
    if (html.length > MAX_SIZE) {
        document.querySelectorAll('style').forEach(el => el.remove());
        document.querySelectorAll('*').forEach(el => {
            el.removeAttribute('class');
            el.removeAttribute('id');
            el.removeAttribute('style');
        });
        const style = document.createElement('style');
        style.textContent = FALLBACK_CSS;
        document.head.appendChild(style);
        html = document.documentElement.outerHTML;
    }
    
    return html;
})()
"#;

fn extract_html(tab: &Arc<Tab>) -> Result<String, BrowserError> {
    let result = tab.evaluate(JS_SCRUB_HTML, false)
        .map_err(|e| BrowserError::ExtractionFailed(e.to_string()))?;

    result.value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .ok_or_else(|| BrowserError::ExtractionFailed("No HTML returned".to_string()))
}
