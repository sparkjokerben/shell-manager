//! Static files for the web UI.
//!
//! When `web/dist/index.html` exists at build time, `build.rs` sets the
//! `web_dist_present` cfg and the whole directory is embedded in the binary.
//! Without a frontend build the server still compiles and serves a placeholder
//! page, so a plain `cargo build` does not require Node.js.

#[cfg(web_dist_present)]
static DIST: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/web/dist");

const INDEX: &str = "index.html";

#[cfg(not(web_dist_present))]
const PLACEHOLDER: &[u8] = br#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>smw</title></head>
<body>
<h1>smw web UI is not built</h1>
<p>The server is running, but <code>web/dist/index.html</code> was missing when
<code>smw</code> was compiled.</p>
<p>Build the frontend first:</p>
<pre>cd web
npm install
npm run build</pre>
<p>then rebuild <code>smw</code>.</p>
</body>
</html>
"#;

/// Resolve a URL path to an embedded file and its MIME type.
///
/// `/` maps to `index.html`. Anything that could escape the embedded root is
/// rejected: `..` and `.` segments, backslashes, empty segments, drive letters
/// and UNC-style paths.
pub fn lookup(path: &str) -> Option<(&'static str, &'static [u8])> {
    let rel = normalize(path)?;

    #[cfg(web_dist_present)]
    {
        let file = DIST.get_file(rel)?;
        Some((mime_for(rel), file.contents()))
    }

    #[cfg(not(web_dist_present))]
    {
        if rel == INDEX {
            Some(("text/html; charset=utf-8", PLACEHOLDER))
        } else {
            None
        }
    }
}

/// Validate a URL path and return the path relative to the dist root.
fn normalize(path: &str) -> Option<&str> {
    if path.is_empty()
        || path.contains('\\')
        || path.contains('\0')
        || path.contains(':')
        || path.starts_with("//")
    {
        return None;
    }

    let rel = path.trim_start_matches('/');
    if rel.is_empty() {
        return Some(INDEX);
    }

    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
    }
    Some(rel)
}

/// MIME type for a file name, based on its extension.
fn mime_for(path: &str) -> &'static str {
    let ext = match path.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => String::new(),
    };
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "map" | "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types_cover_common_assets() {
        assert_eq!(mime_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(mime_for("app.css"), "text/css; charset=utf-8");
        assert_eq!(mime_for("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(mime_for("app.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(mime_for("logo.svg"), "image/svg+xml");
        assert_eq!(mime_for("logo.PNG"), "image/png");
        assert_eq!(mime_for("favicon.ico"), "image/x-icon");
        assert_eq!(mime_for("font.woff2"), "font/woff2");
        assert_eq!(mime_for("font.woff"), "font/woff");
        assert_eq!(mime_for("font.ttf"), "font/ttf");
        assert_eq!(mime_for("app.js.map"), "application/json");
        assert_eq!(mime_for("data.json"), "application/json");
        assert_eq!(mime_for("notes.txt"), "text/plain; charset=utf-8");
        assert_eq!(mime_for("mod.wasm"), "application/wasm");
        assert_eq!(mime_for("unknown.bin"), "application/octet-stream");
        assert_eq!(mime_for("no-extension"), "application/octet-stream");
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        let rejected = [
            "",
            "..",
            "../secret",
            "/../secret",
            "/a/../b",
            "./index.html",
            "/./index.html",
            "a\\b",
            "/a\\b",
            "//host/share",
            "C:/Windows/win.ini",
            "/a//b",
            "/a/",
            "index.html/",
        ];
        for path in rejected {
            assert!(
                lookup(path).is_none(),
                "path should be rejected: {:?}",
                path
            );
            assert!(
                normalize(path).is_none(),
                "normalize should reject: {:?}",
                path
            );
        }
    }

    #[test]
    fn root_maps_to_index() {
        assert_eq!(normalize("/"), Some(INDEX));
        assert_eq!(normalize(""), None);
        let (mime, body) = lookup("/").expect("index page");
        assert_eq!(mime, "text/html; charset=utf-8");
        assert!(!body.is_empty());
    }

    #[test]
    fn plain_relative_paths_are_accepted() {
        assert_eq!(normalize("index.html"), Some("index.html"));
        assert_eq!(normalize("/index.html"), Some("index.html"));
        assert_eq!(normalize("/assets/app.js"), Some("assets/app.js"));
        assert_eq!(normalize("assets/app.js"), Some("assets/app.js"));
    }

    #[cfg(not(web_dist_present))]
    #[test]
    fn placeholder_is_served_without_dist() {
        let text = std::str::from_utf8(PLACEHOLDER).expect("placeholder is UTF-8");
        assert!(text.contains("npm run build"));
        // Unknown assets still 404 rather than serving HTML for them.
        assert!(lookup("/assets/missing.js").is_none());
    }
}
