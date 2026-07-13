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
                // Server already strips these in JS_SCRUB_HTML, but defense in depth
                // so the client cannot be backdoored by trusting the server alone.
                //
                // Inline <style> is intentionally NOT stripped: it costs a few
                // dozen bytes and lets author styling reach the reader instead
                // of the client's chrome swallowing the page. External sheets
                // are still gone (link[rel=stylesheet]) so no extra fetches.
                // CSP on the browse page keeps style-src to 'unsafe-inline'
                // and blocks the URL-based CSS exfil vectors (@import, url()
                // requesting anything other than data:).
                element!("script, link[rel=stylesheet], iframe, frame, frameset, object, embed, applet, noscript, base", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("meta[http-equiv]", |el| {
                    let eq = el.get_attribute("http-equiv").unwrap_or_default();
                    if eq.eq_ignore_ascii_case("refresh") || eq.eq_ignore_ascii_case("content-security-policy") {
                        el.remove();
                    }
                    Ok(())
                }),
                // Drop every on*= event handler attribute and any javascript: URLs.
                element!("*", |el| {
                    let attr_names: Vec<String> = el
                        .attributes()
                        .iter()
                        .map(|a| a.name())
                        .collect();
                    for name in attr_names {
                        if name.starts_with("on") {
                            el.remove_attribute(&name);
                            continue;
                        }
                        if matches!(name.as_str(), "src" | "href" | "action" | "formaction" | "background" | "poster") {
                            if let Some(v) = el.get_attribute(&name) {
                                if v.trim_start().to_ascii_lowercase().starts_with("javascript:") {
                                    el.remove_attribute(&name);
                                }
                            }
                        }
                        // style="..." is kept: CSP allows 'unsafe-inline'
                        // for style-src, and it's typically what makes the
                        // fetched page look like itself instead of the
                        // client shell.
                    }
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

    #[test]
    fn test_strip_iframe_and_object() {
        let html = r#"<iframe src="https://evil"></iframe><object data="x"></object><p>ok</p>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.contains("<iframe"));
        assert!(!result.contains("<object"));
        assert!(result.contains("<p>ok</p>"));
    }

    #[test]
    fn test_strip_event_handlers() {
        let html = r#"<p onclick="alert(1)" onerror="bad()">hi</p>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.contains("onclick"));
        assert!(!result.contains("onerror"));
        assert!(result.contains("hi"));
    }

    #[test]
    fn test_preserve_inline_style_and_style_attr() {
        // Author styling is intentionally allowed through so pages render as
        // themselves rather than in the client's chrome palette. External
        // stylesheets are still gone (verified by test_strip_stylesheet_links).
        let html = r#"<style>body{color:red}</style><p style="color:blue">x</p>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(result.contains("<style>"));
        assert!(result.contains("style=\"color:blue\""));
        assert!(result.contains("x"));
    }

    #[test]
    fn test_strip_meta_refresh() {
        let html = r#"<meta http-equiv="refresh" content="0;url=https://evil"><p>x</p>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.to_lowercase().contains("refresh"));
        assert!(result.contains("<p>x</p>"));
    }

    #[test]
    fn test_strip_javascript_src() {
        let html = r#"<img src="javascript:alert(1)" alt="x"><p>ok</p>"#;
        let result = rewrite_html(html, "https://example.com").unwrap();
        assert!(!result.to_lowercase().contains("javascript:"));
        assert!(result.contains("<p>ok</p>"));
    }
}
