use lol_html::{element, rewrite_str, RewriteStrSettings};
use thiserror::Error;
use url::Url;

#[derive(Error, Debug)]
pub enum RewriteError {
    #[error("Invalid base URL: {0}")]
    InvalidUrl(String),
    #[error("Rewrite failed: {0}")]
    RewriteFailed(String),
}

pub fn rewrite_html(html: &str, base_url: &str) -> Result<String, RewriteError> {
    let base = Url::parse(base_url).map_err(|e| RewriteError::InvalidUrl(e.to_string()))?;

    let result = rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![
                element!("a[href]", |el| {
                    let href = el.get_attribute("href").unwrap_or_default();
                    match build_proxy_url(&href, &base) {
                        Some(proxy_url) => {
                            el.set_attribute("href", &proxy_url)?;
                        }
                    None => {
                        el.set_attribute("href", "#")?;
                    }
                    }
                    Ok(())
                }),
                element!("form[action]", |el| {
                    let action = el.get_attribute("action").unwrap_or_default();
                    match build_proxy_url(&action, &base) {
                        Some(proxy_url) => {
                            el.set_attribute("action", &proxy_url)?;
                            if el.get_attribute("method").is_none() {
                                el.set_attribute("method", "POST")?;
                            }
                        }
                        None => {
                            el.remove();
                        }
                    }
                    Ok(())
                }),
                element!("link[rel=stylesheet]", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("script", |el| {
                    el.remove();
                    Ok(())
                }),
            ],
            ..Default::default()
        },
    )
    .map_err(|e| RewriteError::RewriteFailed(e.to_string()))?;

    Ok(result)
}

fn build_proxy_url(url: &str, base: &Url) -> Option<String> {
    let trimmed = url.trim();

    if trimmed.is_empty()
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with("data:")
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with("tel:")
    {
        return None;
    }

    if trimmed.starts_with('#') {
        return Some(trimmed.to_string());
    }

    let resolved = base.join(trimmed).ok()?;

    let mut proxy_url = Url::parse("http://localhost/browse").ok()?;
    proxy_url
        .query_pairs_mut()
        .append_pair("url", resolved.as_str());

    let query = proxy_url.query()?;
    Some(format!("/browse?{}", query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_absolute_url() {
        let html = r#"<a href="https://example.com/page">Link</a>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(result.contains("/browse?url="));
        assert!(result.contains("example.com"));
    }

    #[test]
    fn test_rewrite_relative_url() {
        let html = r#"<a href="/other/page">Link</a>"#;
        let result = rewrite_html(html, "https://example.com/base").unwrap();
        assert!(result.contains("/browse?url="));
        assert!(result.contains("example.com%2Fother%2Fpage"));
    }

    #[test]
    fn test_strip_javascript_url() {
        let html = r#"<a href="javascript:alert(1)">Link</a>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.contains("javascript:"));
    }

    #[test]
    fn test_strip_mailto_url() {
        let html = r#"<a href="mailto:test@example.com">Email</a>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.contains("mailto:"));
    }

    #[test]
    fn test_preserve_fragment() {
        let html = r##"<a href="#section">Link</a>"##;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(result.contains("#section"));
    }

    #[test]
    fn test_rewrite_form_action() {
        let html = r#"<form action="/search"><input name="q"></form>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(result.contains("/browse?url="));
        assert!(result.contains("method=\"POST\""));
    }

    #[test]
    fn test_strip_scripts() {
        let html = r#"<script>alert(1)</script><p>content</p>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.contains("<script>"));
        assert!(result.contains("<p>content</p>"));
    }

    #[test]
    fn test_strip_stylesheet_links() {
        let html = r#"<link rel="stylesheet" href="style.css"><p>content</p>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.contains("stylesheet"));
        assert!(result.contains("<p>content</p>"));
    }

    #[test]
    fn test_resolve_relative_path() {
        let html = r#"<a href="../page">Link</a>"#;
        let result = rewrite_html(html, "https://example.com/dir/subdir/").unwrap();
        assert!(result.contains("/browse?url="));
        assert!(result.contains("example.com%2Fdir%2Fpage"));
    }
}
