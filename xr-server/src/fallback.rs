/// HTTP fallback response: when the server receives a non-protocol connection
/// (e.g., a DPI probe or web browser), it responds with a generic HTTP page
/// to masquerade as a regular web server.

const DEFAULT_RESPONSE_BODY: &str = r#"<!DOCTYPE html>
<html><head><title>Welcome</title></head>
<body><h1>It works!</h1><p>The server is running.</p></body></html>"#;

/// Build an HTTP response from a file or use the default.
pub fn build_fallback_response(response_file: Option<&str>) -> Vec<u8> {
    let body = if let Some(path) = response_file {
        std::fs::read_to_string(path).unwrap_or_else(|e| {
            tracing::warn!("Failed to read fallback file {}: {}, using default", path, e);
            DEFAULT_RESPONSE_BODY.to_string()
        })
    } else {
        DEFAULT_RESPONSE_BODY.to_string()
    };

    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Server: nginx\r\n\
         \r\n\
         {}",
        body.len(),
        body
    )
    .into_bytes()
}
