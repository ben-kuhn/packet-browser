use fantoccini::{wd::Capabilities, Client, ClientBuilder};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::runtime::Runtime;

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
    client: Option<Client>,
    _geckodriver: Child,
    _session_dir: tempfile::TempDir,
    runtime: Runtime,
}

impl BrowserInstance {
    pub fn new(callsign: &str) -> Result<Self, BrowserError> {
        let safe_id: String = callsign
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();

        // Unguessable per-instance profile root, atomic with 0700 perms.
        let session_tmp = tempfile::Builder::new()
            .prefix(&format!("firefox-{}-", safe_id))
            .tempdir_in("/tmp")
            .map_err(|e| BrowserError::LaunchFailed(format!("Failed to create session dir: {}", e)))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(
                session_tmp.path(),
                std::fs::Permissions::from_mode(0o700),
            ) {
                eprintln!("[BROWSER] Warning: could not set session dir permissions: {}", e);
            }
        }

        let runtime = Runtime::new()
            .map_err(|e| BrowserError::LaunchFailed(format!("create tokio runtime: {}", e)))?;

        // Probe for a free port for geckodriver. Brief race window with another
        // process binding it; acceptable inside an isolated container.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .map_err(|e| BrowserError::LaunchFailed(format!("port probe: {}", e)))?;
            let p = listener
                .local_addr()
                .map_err(|e| BrowserError::LaunchFailed(e.to_string()))?
                .port();
            drop(listener);
            p
        };

        let geckodriver_path = std::env::var("GECKODRIVER_PATH")
            .unwrap_or_else(|_| "/bin/geckodriver".to_string());
        let firefox_path = std::env::var("FIREFOX_PATH")
            .unwrap_or_else(|_| "/bin/firefox".to_string());

        eprintln!("[BROWSER] Launching geckodriver at {} on port {}", geckodriver_path, port);

        let geckodriver = Command::new(&geckodriver_path)
            .arg("--port")
            .arg(port.to_string())
            .arg("--binary")
            .arg(&firefox_path)
            .arg("--profile-root")
            .arg(session_tmp.path())
            // Confine geckodriver/Firefox file accesses to the temp dir.
            .env("HOME", session_tmp.path())
            .env("MOZ_HEADLESS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| BrowserError::LaunchFailed(format!("spawn geckodriver: {}", e)))?;

        // Wait for the WebDriver port to accept connections.
        let webdriver_url = format!("http://127.0.0.1:{}", port);
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_err() {
            if Instant::now() >= deadline {
                let mut child = geckodriver;
                let _ = child.kill();
                return Err(BrowserError::LaunchFailed(
                    "geckodriver did not start listening within 30s".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        eprintln!("[BROWSER] Geckodriver ready, creating session");

        let mut caps: Capabilities = serde_json::Map::new();
        caps.insert(
            "moz:firefoxOptions".to_string(),
            serde_json::json!({
                "binary": firefox_path,
                "args": ["-headless"],
                "prefs": {
                    "browser.cache.disk.enable": false,
                    "browser.cache.memory.enable": true,
                    "media.autoplay.default": 5,
                    // Block image network loads; the sanitizer drops <img> anyway.
                    "permissions.default.image": 2,
                    // Disable telemetry, updates, marketing pings.
                    "datareporting.healthreport.uploadEnabled": false,
                    "toolkit.telemetry.enabled": false,
                    "app.update.enabled": false,
                    "browser.shell.checkDefaultBrowser": false,
                    "browser.startup.homepage_override.mstone": "ignore",
                    "browser.contentblocking.category": "strict",
                    "network.cookie.cookieBehavior": 5,
                    // Surface stub PDF viewer rather than launching anything.
                    "pdfjs.disabled": true
                }
            }),
        );

        let client = runtime
            .block_on(async {
                ClientBuilder::native()
                    .capabilities(caps)
                    .connect(&webdriver_url)
                    .await
            })
            .map_err(|e| BrowserError::LaunchFailed(format!("connect WebDriver: {}", e)))?;

        let timeouts = fantoccini::wd::TimeoutConfiguration::new(
            Some(Duration::from_secs(15)),
            Some(Duration::from_secs(15)),
            Some(Duration::from_secs(15)),
        );
        runtime
            .block_on(async { client.update_timeouts(timeouts).await })
            .map_err(|e| BrowserError::LaunchFailed(format!("set timeouts: {}", e)))?;

        eprintln!("[BROWSER] Session ready");

        Ok(Self {
            client: Some(client),
            _geckodriver: geckodriver,
            _session_dir: session_tmp,
            runtime,
        })
    }

    pub fn fetch_page(&self, url: &str) -> Result<String, BrowserError> {
        let client = self.client.as_ref().ok_or(BrowserError::BrowserCrashed)?;
        self.runtime.block_on(async {
            eprintln!("[BROWSER] Fetching: {}", url);

            client.goto(url).await.map_err(|e| {
                let s = e.to_string();
                if s.contains("session deleted") || s.contains("invalid session id") {
                    BrowserError::BrowserCrashed
                } else {
                    BrowserError::NavigationFailed(s)
                }
            })?;

            // JS_SCRUB_HTML is an async IIFE returning a Promise. WebDriver's
            // synchronous execute() can't await it directly, so wrap with the
            // async-script callback convention.
            let wrapped = format!(
                "const cb = arguments[arguments.length - 1]; ({}).then(cb).catch(e => cb('__SCRUB_ERROR__' + (e && e.message ? e.message : e)));",
                JS_SCRUB_HTML
            );
            let value = client
                .execute_async(&wrapped, vec![])
                .await
                .map_err(|e| {
                    let s = e.to_string();
                    if s.contains("session deleted") || s.contains("invalid session id") {
                        BrowserError::BrowserCrashed
                    } else {
                        BrowserError::ExtractionFailed(s)
                    }
                })?;

            let html = value
                .as_str()
                .ok_or_else(|| BrowserError::ExtractionFailed("No HTML returned".to_string()))?;

            if let Some(rest) = html.strip_prefix("__SCRUB_ERROR__") {
                return Err(BrowserError::ExtractionFailed(rest.to_string()));
            }

            Ok(html.to_string())
        })
    }
}

impl Drop for BrowserInstance {
    fn drop(&mut self) {
        eprintln!("[BROWSER] Shutting down Firefox");
        // Close the WebDriver session politely if we still have a Client.
        // close() consumes the Client, so take it out of self.
        if let Some(client) = self.client.take() {
            let _ = self.runtime.block_on(async move { client.close().await });
        }
        let _ = self._geckodriver.kill();
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
