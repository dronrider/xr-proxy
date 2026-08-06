//! Страницы входа и отказов, вшитые в бинарь (LLD-38 п. 2.2, п. 2.4).
//!
//! Ни одного внешнего запроса: браузер приходит на публикацию, которой может и
//! не быть на связи, и страница обязана нарисоваться сама. Стилей ровно
//! столько, чтобы форма читалась на телефоне и в тёмной теме.
//!
//! На странице входа прямым текстом сказано, что трафик расшифровывается на
//! сервере входа (п. 3.1): браузерный путь это доверенный посредник, и это
//! обещание не прячется в доке.

const STYLE: &str = "\
:root{color-scheme:light dark}\
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
font:16px/1.5 -apple-system,Segoe UI,Roboto,sans-serif;background:#f6f7f9;color:#16181d}\
main{width:min(92vw,26rem);background:#fff;border-radius:14px;padding:1.75rem;\
box-shadow:0 1px 3px rgba(0,0,0,.12)}\
h1{margin:0 0 .25rem;font-size:1.25rem}\
p{margin:.5rem 0;color:#5a6070}\
label{display:block;margin:.9rem 0 .25rem;font-size:.9rem;color:#5a6070}\
input{width:100%;box-sizing:border-box;padding:.6rem .7rem;font-size:1rem;\
border:1px solid #ccd2dd;border-radius:9px;background:#fff;color:inherit}\
button{margin-top:1.1rem;width:100%;padding:.65rem;font-size:1rem;border:0;border-radius:9px;\
background:#2f6df6;color:#fff;cursor:pointer}\
.host{font-weight:600;color:#16181d}\
.err{margin-top:.75rem;padding:.55rem .7rem;border-radius:9px;background:#fdecec;color:#a32020}\
.note{margin-top:1.1rem;font-size:.82rem;color:#8a8f9c}\
@media(prefers-color-scheme:dark){body{background:#15171c;color:#e8eaf0}\
main{background:#1d2027;box-shadow:none}p,label,.note{color:#98a0b0}\
input{background:#15171c;border-color:#333846;color:#e8eaf0}.host{color:#e8eaf0}\
.err{background:#3a1d1d;color:#ffb3b3}}\
";

/// Экранирование того, что попадает в страницу из чужих рук (имя хоста, текст
/// отказа хаба). Имя публикации проверено как DNS-метка, но страница не должна
/// зависеть от того, кто её позвал.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Страница входа для публикации. `error` рисуется, когда пароль не подошёл
/// или вход упёрся в задержку.
pub fn login(publication: &str, error: Option<&str>) -> String {
    let err = match error {
        Some(text) => format!("<div class=\"err\">{}</div>", esc(text)),
        None => String::new(),
    };
    format!(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>Вход: {name}</title><style>{STYLE}</style></head><body><main>\
<h1>Вход</h1><p>Публикация <span class=\"host\">{name}</span></p>\
<form method=\"post\" action=\"/.xr-web/login\">\
<label for=\"u\">Логин</label><input id=\"u\" name=\"username\" autocomplete=\"username\" autofocus>\
<label for=\"p\">Пароль</label><input id=\"p\" name=\"password\" type=\"password\" \
autocomplete=\"current-password\">\
<button type=\"submit\">Войти</button></form>{err}\
<p class=\"note\">Трафик расшифровывается на сервере входа: это доверенный посредник, \
а не сквозное шифрование до вашей машины.</p>\
</main></body></html>",
        name = esc(publication),
        err = err,
    )
}

/// Страница отказа: заголовок и названная причина. Молчаливой пятисотки быть
/// не должно, иначе отказ канала неотличим от отказа приложения.
pub fn failure(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{t}</title><style>{STYLE}</style></head><body><main>\
<h1>{t}</h1><p>{d}</p></main></body></html>",
        t = esc(title),
        d = esc(detail),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_is_self_contained_and_names_the_publication() {
        let page = login("dash", None);
        assert!(page.contains("dash"));
        assert!(page.contains("action=\"/.xr-web/login\""));
        assert!(
            page.contains("расшифровывается на сервере входа"),
            "обещание доверенного посредника обязано стоять на странице входа"
        );
        // Никаких внешних адресов: страница рисуется у машины, которая может
        // быть отрезана от всего, кроме этого фронта.
        assert!(!page.contains("http://"), "{page}");
        assert!(!page.contains("//cdn"), "{page}");
        assert!(!page.contains("<script"), "скриптов на странице входа нет");
        assert!(!page.contains("class=\"err\""), "без отказа плашки ошибки нет");
        assert!(login("dash", Some("неверный пароль")).contains("неверный пароль"));
    }

    #[test]
    fn markup_from_outside_is_escaped() {
        let page = failure("Машина не на связи", "<img src=x onerror=alert(1)>");
        assert!(!page.contains("<img"), "чужой текст обязан быть экранирован: {page}");
        assert!(page.contains("&lt;img"));
    }
}
