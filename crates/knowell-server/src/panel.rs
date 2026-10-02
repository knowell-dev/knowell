//! Panel static files: embedded at build time (`panel/dist`, or the
//! placeholder page when the panel was not built; see `build.rs`) or read
//! from a directory at run time.
//!
//! Hashed assets under `assets/` are cached for a year (`immutable`);
//! everything else, including `index.html`, is `no-cache`. The panel uses
//! hash routing, so there is no SPA fallback: unknown paths are 404.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};

use crate::error::ApiError;

/// Content Security Policy of panel documents: no inline scripts or styles,
/// no external resources, no framing.
pub(crate) const PANEL_CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";
const MAX_PATH_LEN: usize = 512;

#[cfg(knowell_panel_dist)]
static DIST: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../panel/dist");
// With a real panel build the placeholder is only used by tests.
#[cfg_attr(knowell_panel_dist, allow(dead_code))]
static PLACEHOLDER: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/panel-placeholder");

/// Where panel files are read from.
#[derive(Debug, Clone)]
pub(crate) enum PanelSource {
    /// Files compiled into the binary.
    Embedded {
        dir: &'static Dir<'static>,
        /// True when this is the "panel not built" page.
        placeholder: bool,
    },
    /// A canonicalized directory read at run time.
    Directory(PathBuf),
    /// No panel.
    Disabled,
}

impl PanelSource {
    /// The build's embedded panel (`panel/dist` or the placeholder).
    pub(crate) fn embedded() -> Self {
        #[cfg(knowell_panel_dist)]
        {
            Self::Embedded {
                dir: &DIST,
                placeholder: false,
            }
        }
        #[cfg(not(knowell_panel_dist))]
        {
            Self::placeholder()
        }
    }

    /// The placeholder page.
    #[cfg_attr(knowell_panel_dist, allow(dead_code))]
    pub(crate) fn placeholder() -> Self {
        Self::Embedded {
            dir: &PLACEHOLDER,
            placeholder: true,
        }
    }

    /// Short status for `/api/v1/health`: `(ok, detail)`.
    pub(crate) fn describe(&self) -> (bool, &'static str) {
        match self {
            Self::Embedded {
                placeholder: false, ..
            } => (true, "panel embedded in the binary"),
            Self::Embedded {
                placeholder: true, ..
            } => (
                false,
                "the panel was not built into this binary; serving the placeholder page",
            ),
            Self::Directory(_) => (true, "panel served from a directory"),
            Self::Disabled => (true, "panel disabled"),
        }
    }
}

/// Serves `uri_path` (the request path, still percent-encoded).
pub(crate) async fn serve(source: &PanelSource, method: &Method, uri_path: &str) -> Response {
    if matches!(source, PanelSource::Disabled) {
        return ApiError::not_found("the panel is disabled on this server").into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return ApiError::method_not_allowed(Some("GET, HEAD")).into_response();
    }
    let Some(rel) = asset_path(uri_path) else {
        return ApiError::not_found("the requested resource does not exist").into_response();
    };
    let bytes = match source {
        PanelSource::Embedded { dir, .. } => dir.get_file(&rel).map(|f| f.contents().to_vec()),
        PanelSource::Directory(root) => read_from_directory(root, &rel).await,
        PanelSource::Disabled => None,
    };
    match bytes {
        Some(bytes) => file_response(&rel, bytes),
        None => ApiError::not_found("the requested resource does not exist").into_response(),
    }
}

/// Maps a request path to a relative asset path, or `None` when it could
/// escape the panel root or is malformed. `/` maps to `index.html`.
pub(crate) fn asset_path(uri_path: &str) -> Option<String> {
    if uri_path.len() > MAX_PATH_LEN {
        return None;
    }
    let decoded = percent_decode(uri_path)?;
    let trimmed = decoded.strip_prefix('/').unwrap_or(&decoded);
    if trimmed.is_empty() {
        return Some("index.html".to_owned());
    }
    let mut segments = Vec::new();
    for segment in trimmed.split('/') {
        let bad = segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.chars().any(|c| {
                c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            });
        if bad {
            return None;
        }
        segments.push(segment);
    }
    Some(segments.join("/"))
}

/// Strict percent-decoding to UTF-8; `None` for malformed escapes or bytes.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = hex(*bytes.get(i + 1)?)?;
            let lo = hex(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

async fn read_from_directory(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let mut path = root.to_path_buf();
    for segment in rel.split('/') {
        path.push(segment);
    }
    // Symlinks inside the directory must not lead outside it.
    let real = tokio::fs::canonicalize(&path).await.ok()?;
    if !real.starts_with(root) || !tokio::fs::metadata(&real).await.ok()?.is_file() {
        return None;
    }
    tokio::fs::read(&real).await.ok()
}

fn file_response(rel: &str, bytes: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(rel).first_or_octet_stream();
    let essence = mime.essence_str();
    let needs_charset = mime.type_() == mime_guess::mime::TEXT
        || essence == "application/javascript"
        || essence == "application/json"
        || essence == "image/svg+xml";
    let content_type = if needs_charset {
        format!("{essence}; charset=utf-8")
    } else {
        essence.to_owned()
    };
    let cache = if rel.starts_with("assets/") {
        IMMUTABLE
    } else {
        NO_CACHE
    };
    let mut response = (StatusCode::OK, Body::from(bytes)).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        headers.insert(CONTENT_TYPE, value);
    }
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(PANEL_CSP));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_paths() {
        assert_eq!(asset_path("/").as_deref(), Some("index.html"));
        assert_eq!(asset_path("").as_deref(), Some("index.html"));
        assert_eq!(asset_path("/index.html").as_deref(), Some("index.html"));
        assert_eq!(
            asset_path("/assets/index-BE8dBo-G.js").as_deref(),
            Some("assets/index-BE8dBo-G.js")
        );
        assert_eq!(asset_path("/a%20b.txt").as_deref(), Some("a b.txt"));
        for bad in [
            "/..",
            "/../secret",
            "/assets/../../etc/passwd",
            "/%2e%2e/x",
            "/%2E%2E%2Fx",
            "/a//b",
            "/assets/",
            "/./index.html",
            "/a\\b",
            "/%5c..%5cx",
            "/c:/windows",
            "/%00",
            "/%zz",
            "/%e9",
            "/a%0ab",
        ] {
            assert_eq!(asset_path(bad), None, "{bad}");
        }
        assert_eq!(asset_path(&format!("/{}", "a".repeat(600))), None);
    }

    #[tokio::test]
    async fn placeholder_is_served_with_headers() {
        let source = PanelSource::placeholder();
        let response = serve(&source, &Method::GET, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(headers[CACHE_CONTROL], NO_CACHE);
        assert_eq!(headers[CONTENT_SECURITY_POLICY], PANEL_CSP);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("was not built"));
        // The placeholder must satisfy the CSP itself.
        assert!(!text.contains("<script") && !text.contains("style="));

        let head = serve(&source, &Method::HEAD, "/index.html").await;
        assert_eq!(head.status(), StatusCode::OK);
        let missing = serve(&source, &Method::GET, "/assets/nope.js").await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let traversal = serve(&source, &Method::GET, "/../Cargo.toml").await;
        assert_eq!(traversal.status(), StatusCode::NOT_FOUND);
        let post = serve(&source, &Method::POST, "/").await;
        assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
        let (ok, _) = source.describe();
        assert!(!ok);
    }

    #[tokio::test]
    async fn directory_source_serves_files_and_blocks_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("dist");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("index.html"), "<!doctype html><title>t</title>").unwrap();
        std::fs::write(root.join("assets").join("app-1a2b.js"), "export {};").unwrap();
        std::fs::write(root.join("assets").join("app-1a2b.css"), "body{}").unwrap();
        std::fs::write(tmp.path().join("outside.txt"), "secret").unwrap();
        let source = PanelSource::Directory(std::fs::canonicalize(&root).unwrap());

        let js = serve(&source, &Method::GET, "/assets/app-1a2b.js").await;
        assert_eq!(js.status(), StatusCode::OK);
        let ct = js.headers()[CONTENT_TYPE].to_str().unwrap().to_owned();
        assert!(
            ct.contains("javascript") && ct.ends_with("charset=utf-8"),
            "{ct}"
        );
        assert_eq!(js.headers()[CACHE_CONTROL], IMMUTABLE);
        let css = serve(&source, &Method::GET, "/assets/app-1a2b.css").await;
        assert_eq!(css.headers()[CONTENT_TYPE], "text/css; charset=utf-8");
        let index = serve(&source, &Method::GET, "/").await;
        assert_eq!(index.headers()[CACHE_CONTROL], NO_CACHE);
        for escape in ["/../outside.txt", "/%2e%2e/outside.txt", "/assets"] {
            let r = serve(&source, &Method::GET, escape).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{escape}");
        }
    }

    #[tokio::test]
    async fn disabled_panel_is_not_found() {
        let r = serve(&PanelSource::Disabled, &Method::GET, "/").await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
