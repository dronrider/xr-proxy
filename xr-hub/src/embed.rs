use axum::Router;

#[cfg(not(feature = "dev-ui"))]
mod embedded {
    use axum::body::Body;
    use axum::http::{Request, Response, StatusCode, header};
    use axum::response::IntoResponse;
    use rust_embed::Embed;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Embed)]
    #[folder = "admin-ui/dist"]
    struct Asset;

    #[derive(Clone)]
    pub struct EmbeddedSpa;

    impl tower::Service<Request<Body>> for EmbeddedSpa {
        type Response = Response<Body>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            let path = req.uri().path().trim_start_matches('/');
            let path = if path.is_empty() { "index.html" } else { path };

            let response = if let Some(file) = Asset::get(path) {
                let mime = mime_guess::from_path(path).first_or_octet_stream();
                Response::builder()
                    .header(header::CONTENT_TYPE, mime.as_ref())
                    .body(Body::from(file.data.to_vec()))
                    .unwrap()
            } else if let Some(index) = Asset::get("index.html") {
                // SPA fallback: serve index.html for unmatched routes.
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from(index.data.to_vec()))
                    .unwrap()
            } else {
                (StatusCode::NOT_FOUND, "not found").into_response()
            };

            Box::pin(async { Ok(response) })
        }
    }
}

#[cfg(not(feature = "dev-ui"))]
pub fn spa_service() -> Router {
    Router::new().fallback_service(embedded::EmbeddedSpa)
}

#[cfg(feature = "dev-ui")]
pub fn spa_service() -> Router {
    use tower_http::services::{ServeDir, ServeFile};
    let serve = ServeDir::new("xr-hub/admin-ui/dist")
        .fallback(ServeFile::new("xr-hub/admin-ui/dist/index.html"));
    Router::new().fallback_service(serve)
}
