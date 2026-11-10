//! The web UI: static files built into the binary, served at `/ui/`.
//! They need no token to load; every API call they make sends one.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
};

const INDEX: &str = include_str!("../ui/index.html");
const FILES: [(&str, &str, &str); 4] = [
    ("app.js", "text/javascript; charset=utf-8", include_str!("../ui/app.js")),
    ("app.css", "text/css; charset=utf-8", include_str!("../ui/app.css")),
    ("viewer.js", "text/javascript; charset=utf-8", include_str!("../ui/viewer.js")),
    ("icon.svg", "image/svg+xml", include_str!("../ui/icon.svg")),
];

fn page(content_type: &'static str, body: &'static str) -> Response {
    // Small files, changed only by a new bridge build: always revalidate.
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

pub async fn to_ui() -> Redirect {
    Redirect::to("/ui/")
}

pub async fn index() -> Response {
    page("text/html; charset=utf-8", INDEX)
}

pub async fn asset(Path(file): Path<String>) -> Response {
    match FILES.iter().find(|(name, _, _)| *name == file) {
        Some((_, ct, body)) => page(ct, body),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn index_loads_its_assets() {
        for (name, _, body) in super::FILES {
            assert!(!body.is_empty(), "{name} is empty");
            if name != "icon.svg" {
                assert!(super::INDEX.contains(name), "index.html does not load {name}");
            }
        }
    }

    use axum::{body::to_bytes, extract::Path, http::{header, StatusCode}, response::IntoResponse};

    #[tokio::test]
    async fn the_root_redirects_to_the_ui() {
        let res = super::to_ui().await.into_response();
        assert!(res.status().is_redirection());
        assert_eq!(res.headers()[header::LOCATION], "/ui/");
    }

    #[tokio::test]
    async fn the_index_is_html_that_always_revalidates() {
        let res = super::index().await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-cache");
        let body = to_bytes(res.into_body(), 1 << 24).await.unwrap();
        assert_eq!(body, super::INDEX.as_bytes());
    }

    #[tokio::test]
    async fn each_asset_has_its_type_and_others_are_not_found() {
        for (name, ct, body) in super::FILES {
            let res = super::asset(Path(name.to_string())).await;
            assert_eq!(res.status(), StatusCode::OK, "{name}");
            assert_eq!(res.headers()[header::CONTENT_TYPE], ct);
            assert_eq!(to_bytes(res.into_body(), 1 << 24).await.unwrap(), body.as_bytes());
        }
        for missing in ["index.html", "../main.rs", "app.js.map", ""] {
            assert_eq!(super::asset(Path(missing.to_string())).await.status(), StatusCode::NOT_FOUND, "{missing}");
        }
    }
}
