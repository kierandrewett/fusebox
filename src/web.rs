use axum::Json;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{Html, Response};

pub(crate) const SWITCH_SOUND_BYTES: &[u8] =
    include_bytes!("../assets/348224__tbrook__switch-light-06.wav");
pub(crate) const APP_BUNDLE_JS: &str = include_str!("../web/dist/app.js");

pub(crate) async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub(crate) async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub(crate) async fn switch_sound() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(SWITCH_SOUND_BYTES))
        .expect("static switch sound response should be valid")
}

pub(crate) async fn app_bundle() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(APP_BUNDLE_JS))
        .expect("static app bundle response should be valid")
}

pub(crate) async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

pub(crate) const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta name="theme-color" content="#201d19" id="theme-color">
    <title>Fusebox</title>
    <script>
        (() => {
            try {
                const storedTheme = localStorage.getItem("fusebox-theme");
                const systemTheme = window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "classic";
                document.documentElement.dataset.theme = storedTheme ?? systemTheme;
            } catch (_error) {
                document.documentElement.dataset.theme = "classic";
            }
        })();
    </script>
</head>
<body>
    <div id="app-root"></div>
    <script src="/assets/app.js" type="module"></script>
</body>
</html>
"##;
