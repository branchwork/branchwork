use axum::{http, response::IntoResponse};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../web/dist"]
#[allow(unused)]
struct Assets;

pub async fn serve_frontend(uri: http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(http::header::CONTENT_TYPE, mime.as_ref())],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => {
            // SPA fallback: serve index.html for unmatched routes
            match Assets::get("index.html") {
                Some(file) => (
                    [(http::header::CONTENT_TYPE, "text/html")],
                    file.data.into_owned(),
                )
                    .into_response(),
                None => (http::StatusCode::NOT_FOUND, "frontend not built").into_response(),
            }
        }
    }
}
