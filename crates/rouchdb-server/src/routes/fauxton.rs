use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "fauxton/"]
struct FauxtonAssets;

/// GET /_utils/*path — serve embedded Fauxton static files.
///
/// Falls back to index.html for SPA routing. If fauxton directory is empty,
/// serves a placeholder page.
pub async fn fauxton(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    serve_fauxton(&path)
}

/// GET /_utils — redirect to /_utils/
pub async fn fauxton_root() -> Response {
    serve_fauxton("index.html")
}

fn serve_fauxton(path: &str) -> Response {
    // Try the exact path first
    if let Some(file) = FauxtonAssets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.as_ref())],
            file.data.to_vec(),
        )
            .into_response();
    }

    // SPA fallback: serve index.html
    if let Some(file) = FauxtonAssets::get("index.html") {
        let mime = mime_guess::from_path("index.html").first_or_octet_stream();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.as_ref())],
            file.data.to_vec(),
        )
            .into_response();
    }

    // Fauxton not installed — show placeholder
    Html(
        r#"<!DOCTYPE html>
<html>
<head><title>RouchDB</title></head>
<body style="font-family: sans-serif; max-width: 600px; margin: 80px auto; text-align: center;">
    <h1>Fauxton not installed</h1>
    <p>Run <code>bash scripts/download-fauxton.sh</code> to download Fauxton, then rebuild.</p>
    <p>The RouchDB HTTP API is still available at the root URL.</p>
</body>
</html>"#
            .to_string(),
    )
    .into_response()
}
