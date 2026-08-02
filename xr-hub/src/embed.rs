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

    // Каталог выбирает build.rs: собранный admin-ui/dist, а в отладочной
    // сборке без него заглушка из OUT_DIR (XR-238).
    #[derive(Embed)]
    #[folder = "$XR_HUB_UI_DIR"]
    struct Asset;

    pub const UI_PLACEHOLDER: bool = matches!(env!("XR_HUB_UI_PLACEHOLDER").as_bytes(), [b'1']);

    #[derive(Clone)]
    pub struct EmbeddedSpa {
        placeholder: bool,
    }

    impl EmbeddedSpa {
        pub fn new(placeholder: bool) -> Self {
            Self { placeholder }
        }
    }

    /// Вшита заглушка, а не UI: на любой запрос отвечаем отказом, чтобы хаб с
    /// пустой админкой нельзя было принять за рабочий.
    fn placeholder_response() -> Response<Body> {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "admin UI не собран: бинарь собран без admin-ui/dist. \
             Собрать: cd xr-hub/admin-ui && npm ci && npm run build\n",
        )
            .into_response()
    }

    impl tower::Service<Request<Body>> for EmbeddedSpa {
        type Response = Response<Body>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            if self.placeholder {
                return Box::pin(async { Ok(placeholder_response()) });
            }

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

    #[cfg(test)]
    mod tests {
        use super::*;
        use axum::body::to_bytes;
        use tower::Service;

        async fn get(spa: &mut EmbeddedSpa, path: &str) -> (StatusCode, String) {
            let req = Request::builder().uri(path).body(Body::empty()).unwrap();
            let resp = spa.call(req).await.unwrap();
            let status = resp.status();
            let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        }

        #[tokio::test]
        async fn placeholder_build_refuses_instead_of_serving_empty_ui() {
            let mut spa = EmbeddedSpa::new(true);

            for path in ["/", "/invites", "/assets/index.js"] {
                let (status, body) = get(&mut spa, path).await;
                assert_eq!(
                    status,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "заглушка обязана отказывать на {path}"
                );
                assert!(body.contains("npm run build"), "отказ молчит о сборке: {body}");
            }
        }
    }
}

#[cfg(not(feature = "dev-ui"))]
pub use embedded::UI_PLACEHOLDER;

#[cfg(not(feature = "dev-ui"))]
pub fn spa_service() -> Router {
    Router::new().fallback_service(embedded::EmbeddedSpa::new(UI_PLACEHOLDER))
}

#[cfg(feature = "dev-ui")]
pub const UI_PLACEHOLDER: bool = false;

#[cfg(feature = "dev-ui")]
pub fn spa_service() -> Router {
    use tower_http::services::{ServeDir, ServeFile};
    let serve = ServeDir::new("xr-hub/admin-ui/dist")
        .fallback(ServeFile::new("xr-hub/admin-ui/dist/index.html"));
    Router::new().fallback_service(serve)
}
