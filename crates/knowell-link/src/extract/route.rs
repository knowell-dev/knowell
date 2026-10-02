//! Routes derived from file paths (file-system routing conventions).

use crate::pack::RouteConvention;

const SCRIPT_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// The URL path a file serves under `convention`, or `None` when the file
/// is not a route file of that convention. Dynamic segments (`[id]`,
/// `[...slug]`, `[[...slug]]`) become `{}`.
pub(crate) fn route_for(convention: RouteConvention, path: &str) -> Option<String> {
    let (stem_path, extension) = path.rsplit_once('.')?;
    if !SCRIPT_EXTENSIONS.contains(&extension) {
        return None;
    }
    let parts: Vec<&str> = stem_path.split('/').collect();
    match convention {
        RouteConvention::NextjsApp => {
            let app = parts.iter().position(|p| *p == "app")?;
            let before_ok = parts.get(..app)?.iter().all(|p| *p == "src") || app == 0;
            if !before_ok || parts.last() != Some(&"route") {
                return None;
            }
            let inner = parts.get(app + 1..parts.len().saturating_sub(1))?;
            let mut segments = Vec::new();
            for part in inner {
                if part.starts_with('_') {
                    // Private folders are not routable.
                    return None;
                }
                if (part.starts_with('(') && part.ends_with(')')) || part.starts_with('@') {
                    continue;
                }
                segments.push(segment(part));
            }
            Some(format!("/{}", segments.join("/")))
        }
        RouteConvention::NextjsPages => {
            let pages = parts.iter().position(|p| *p == "pages")?;
            let before_ok = parts.get(..pages)?.iter().all(|p| *p == "src") || pages == 0;
            if !before_ok || parts.get(pages + 1) != Some(&"api") {
                return None;
            }
            let mut segments: Vec<String> =
                parts.get(pages + 1..)?.iter().map(|p| segment(p)).collect();
            if segments.last().map(String::as_str) == Some("index") {
                segments.pop();
            }
            Some(format!("/{}", segments.join("/")))
        }
    }
}

fn segment(part: &str) -> String {
    if part.starts_with('[') && part.ends_with(']') {
        "{}".to_owned()
    } else {
        part.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_router() {
        let r = |p| route_for(RouteConvention::NextjsApp, p);
        assert_eq!(
            r("app/api/orders/[orderId]/route.ts").as_deref(),
            Some("/api/orders/{}")
        );
        assert_eq!(
            r("src/app/(shop)/api/x/route.js").as_deref(),
            Some("/api/x")
        );
        assert_eq!(r("app/route.ts").as_deref(), Some("/"));
        assert_eq!(r("app/api/x/page.tsx"), None);
        assert_eq!(r("app/_lib/route.ts"), None);
        assert_eq!(r("lib/app/x/route.ts"), None);
        assert_eq!(r("app/x/route.md"), None);
    }

    #[test]
    fn pages_api() {
        let r = |p| route_for(RouteConvention::NextjsPages, p);
        assert_eq!(
            r("src/pages/api/reports/index.ts").as_deref(),
            Some("/api/reports")
        );
        assert_eq!(
            r("pages/api/files/[...slug].ts").as_deref(),
            Some("/api/files/{}")
        );
        assert_eq!(r("pages/account/settings.tsx"), None);
    }
}
