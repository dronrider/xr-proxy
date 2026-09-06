//! Стенд для `deploy/nginx-token-mask.conf` (XR-198): nginx на машине сборки
//! нет, поэтому регулярки `map` читаются из самого файла и гоняются крейтом
//! `regex` по образцам URL. Захваты в файле записаны в форме `(?P<имя>...)`,
//! общей для PCRE и `regex`. Эмулируется порядок nginx: срабатывает первая
//! совпавшая регулярка, в значение подставляются захваты, без совпадения
//! берётся `default`.

use std::path::PathBuf;

use regex::Regex;

const MASK: &str = "<masked>";
const TOKEN: &str = "AbCdEf0123456789012345";

fn conf_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/nginx-token-mask.conf")
}

fn conf() -> String {
    std::fs::read_to_string(conf_path()).expect("deploy/nginx-token-mask.conf читается")
}

/// Одна строка `"~regex"  "value";` внутри `map`.
struct Rule {
    re: Regex,
    value: String,
}

/// Правила блока `map $source $target { ... }` в порядке записи.
fn map_rules(source: &str, target: &str) -> Vec<Rule> {
    let text = conf();
    let header = format!("map {source} {target} {{");
    let start = text
        .find(&header)
        .unwrap_or_else(|| panic!("в конфиге нет блока `{header}`"));
    let body = &text[start + header.len()..];
    // Первая же `}` стоит внутри `${pre}`, конец блока это скобка в начале строки.
    let end = body.find("\n}").expect("блок map закрыт");
    let mut rules = Vec::new();
    for line in body[..end].lines() {
        let line = line.trim();
        if !line.starts_with("\"~") {
            continue;
        }
        let mut parts = line.trim_end_matches(';').split('"').filter(|s| !s.trim().is_empty());
        let pattern = parts.next().unwrap().trim_start_matches('~');
        let value = parts.next().unwrap().to_string();
        let re = Regex::new(pattern).unwrap_or_else(|e| panic!("регулярка `{pattern}`: {e}"));
        rules.push(Rule { re, value });
    }
    assert!(!rules.is_empty(), "в блоке `{header}` нет ни одной регулярки");
    rules
}

/// Что напишет nginx в переменную-цель для данного значения источника.
fn apply(rules: &[Rule], input: &str) -> String {
    for rule in rules {
        if let Some(caps) = rule.re.captures(input) {
            let mut out = rule.value.clone();
            for name in rule.re.capture_names().flatten() {
                let got = caps.name(name).map(|m| m.as_str()).unwrap_or("");
                out = out.replace(&format!("${{{name}}}"), got);
            }
            return out;
        }
    }
    input.to_string()
}

#[test]
fn request_uri_masks_invite_in_path_and_token_in_query() {
    let rules = map_rules("$request_uri", "$xr_masked_uri");
    let cases = [
        (format!("/invite/{TOKEN}"), "/invite/<masked>".to_string()),
        (format!("/api/v1/invite/{TOKEN}/claim"), "/api/v1/invite/<masked>/claim".to_string()),
        (format!("/api/v1/invite/{TOKEN}/shares"), "/api/v1/invite/<masked>/shares".to_string()),
        (format!("/api/v1/admin/invites/{TOKEN}"), "/api/v1/admin/invites/<masked>".to_string()),
        (format!("/W/web?token={TOKEN}"), "/W/web?token=<masked>".to_string()),
        (
            format!("/W/file/a/b.md?x=1&token={TOKEN}&y=2"),
            "/W/file/a/b.md?x=1&token=<masked>&y=2".to_string(),
        ),
    ];
    for (input, want) in cases {
        let got = apply(&rules, &input);
        assert_eq!(got, want, "маскировка {input}");
        assert!(!got.contains(TOKEN));
    }
}

#[test]
fn request_uri_leaves_unrelated_paths_alone() {
    let rules = map_rules("$request_uri", "$xr_masked_uri");
    for uri in [
        "/healthz",
        "/api/v1/shares",
        "/api/v1/invite",
        "/api/v1/admin/invites",
        "/W/web?tokens=1&t=token",
        "/W/manifest?if_none_match=abc",
    ] {
        assert_eq!(apply(&rules, uri), uri, "безобидный URI тронут");
    }
}

#[test]
fn referer_with_token_is_masked_too() {
    let rules = map_rules("$http_referer", "$xr_masked_referer");
    let page = format!("http://agent.lan:8543/W/web?token={TOKEN}");
    assert_eq!(apply(&rules, &page), "http://agent.lan:8543/W/web?token=<masked>");
    let invite = format!("https://hub.example.com/invite/{TOKEN}");
    assert_eq!(apply(&rules, &invite), "https://hub.example.com/invite/<masked>");
    assert_eq!(apply(&rules, "-"), "-");
    assert_eq!(apply(&rules, "https://hub.example.com/"), "https://hub.example.com/");
}

/// Сам формат обязан брать маскированные переменные: `$request` и
/// `$http_referer` в нём вернули бы секреты обратно в лог.
#[test]
fn log_format_uses_masked_variables_only() {
    let text = conf();
    let start = text.find("log_format xr_masked").expect("есть log_format xr_masked");
    let fmt = &text[start..];
    let fmt = &fmt[..fmt.find(';').expect("log_format закрыт")];
    assert!(fmt.contains("$xr_masked_uri"), "{fmt}");
    assert!(fmt.contains("$xr_masked_referer"), "{fmt}");
    assert!(!fmt.contains("$request "), "сырой $request в формате: {fmt}");
    assert!(!fmt.contains("$request\""), "сырой $request в формате: {fmt}");
    assert!(!fmt.contains("$request_uri"), "сырой $request_uri в формате: {fmt}");
    assert!(!fmt.contains("$http_referer"), "сырой $http_referer в формате: {fmt}");
    assert!(text.contains(MASK));
}
