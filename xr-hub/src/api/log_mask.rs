//! Маскировка секретов в URI для собственного лога хаба (XR-198).
//!
//! Инвайт-токен по замыслу стоит в пути (`/invite/<токен>`, ссылку кодирует
//! QR и открывает браузер), а share-токен едет в query `?token=`. Инвариант
//! «токены не логируем» поэтому держится не форматом ссылок, а каждым
//! писателем лога: request-спан хаба берёт URI отсюда, а фронт nginx
//! маскирует те же места своим `map` (`deploy/nginx-token-mask.conf`).
//! Правила у обоих одни: сегмент пути после `invite` или `invites` и значение
//! параметра `token` заменяются на [`MASK`].

/// Чем заменяется секрет. Та же строка стоит в конфиге nginx, по ней оператор
/// узнаёт в логе замаскированный запрос.
pub const MASK: &str = "<masked>";

/// Сегменты пути, за которыми идёт токен.
const TOKEN_PARENTS: [&str; 2] = ["invite", "invites"];

/// Параметр query, чьё значение это токен.
const TOKEN_PARAM: &str = "token";

/// URI без секретов: путь с замаскированным сегментом после `invite`/`invites`,
/// query с замаскированным `token=`. Остальное проходит как есть.
pub fn mask_uri(uri: &str) -> String {
    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (uri, None),
    };
    let mut out = mask_path(path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(&mask_query(q));
    }
    out
}

fn mask_path(path: &str) -> String {
    let mut prev = "";
    let mut segments = Vec::new();
    for seg in path.split('/') {
        if !seg.is_empty() && TOKEN_PARENTS.contains(&prev) {
            segments.push(MASK);
        } else {
            segments.push(seg);
        }
        prev = seg;
    }
    segments.join("/")
}

fn mask_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if key == TOKEN_PARAM => format!("{key}={MASK}"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "AbCdEf0123456789012345";

    #[test]
    fn invite_token_in_path_is_masked() {
        assert_eq!(mask_uri("/invite/AbCdEf0123456789012345"), "/invite/<masked>");
        assert_eq!(
            mask_uri("/api/v1/invite/AbCdEf0123456789012345/claim"),
            "/api/v1/invite/<masked>/claim"
        );
        assert_eq!(
            mask_uri("/api/v1/admin/invites/AbCdEf0123456789012345"),
            "/api/v1/admin/invites/<masked>"
        );
    }

    #[test]
    fn share_token_in_query_is_masked_among_other_params() {
        assert_eq!(mask_uri("/W/web?token=BLOB"), "/W/web?token=<masked>");
        assert_eq!(
            mask_uri("/W/file/a/b.md?x=1&token=BLOB&y=2"),
            "/W/file/a/b.md?x=1&token=<masked>&y=2"
        );
    }

    #[test]
    fn unrelated_uris_pass_through() {
        for uri in [
            "/healthz",
            "/api/v1/shares",
            "/api/v1/invite",
            "/api/v1/admin/invites",
            "/W/web?tokens=1&t=token",
            "/static/token=/x",
        ] {
            assert_eq!(mask_uri(uri), uri, "маскировка тронула безобидный URI");
        }
    }

    #[test]
    fn masked_uri_never_carries_the_token() {
        for uri in [
            format!("/invite/{TOKEN}"),
            format!("/api/v1/invite/{TOKEN}/shares"),
            format!("/W/web?token={TOKEN}"),
            format!("/W/file/x?token={TOKEN}&a=b"),
        ] {
            let masked = mask_uri(&uri);
            assert!(!masked.contains(TOKEN), "токен остался в {masked}");
            assert!(masked.contains(MASK), "нет отметки маскировки в {masked}");
        }
    }
}
