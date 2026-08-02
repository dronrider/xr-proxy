// Общий код build.rs и тестов: build.rs подключает этот файл через include!,
// поэтому решение о том, что вшивать вместо несобранного admin-ui/dist,
// проверяется обычными тестами крейта.

use std::path::{Path, PathBuf};

/// Каталог заглушки внутри OUT_DIR.
pub const PLACEHOLDER_DIR: &str = "admin-ui-placeholder";

/// Что отдать rust-embed под вшивание.
#[derive(Debug, PartialEq, Eq)]
pub enum UiSource {
    /// Собранный админ-UI.
    Built(PathBuf),
    /// UI не собран, но сборка отладочная: вшиваем заглушку, чтобы крейт
    /// компилировался в свежем чекауте без npm.
    Placeholder(PathBuf),
    /// UI не собран, сборка релизная: вшивать нечего, отказываемся.
    Refuse(String),
}

pub fn refuse_message() -> String {
    "admin UI не собран: нет xr-hub/admin-ui/dist/index.html. \
     Собрать: cd xr-hub/admin-ui && npm ci && npm run build"
        .to_string()
}

pub fn pick(
    manifest_dir: &Path,
    out_dir: &Path,
    release: bool,
    dist_index_exists: bool,
) -> UiSource {
    if dist_index_exists {
        UiSource::Built(manifest_dir.join("admin-ui").join("dist"))
    } else if release {
        UiSource::Refuse(refuse_message())
    } else {
        UiSource::Placeholder(out_dir.join(PLACEHOLDER_DIR))
    }
}

/// Кладёт в каталог одну страницу. Её текст в браузере видит тот, кто поднял
/// хаб отладочной сборкой без npm, поэтому страница называет причину и команду.
pub fn write_placeholder(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("index.html"), PLACEHOLDER_PAGE)
}

pub const PLACEHOLDER_PAGE: &str = "<!doctype html>\n\
<meta charset=\"utf-8\">\n\
<title>xr-hub: admin UI не собран</title>\n\
<h1>Admin UI не собран</h1>\n\
<p>Этот бинарь собран без admin-ui/dist, внутри него только заглушка.\n\
Собрать UI: <code>cd xr-hub/admin-ui &amp;&amp; npm ci &amp;&amp; npm run build</code>,\n\
затем пересобрать xr-hub. Релизная сборка такой заглушки не допускает.</p>\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (PathBuf, PathBuf) {
        (PathBuf::from("/src/xr-hub"), PathBuf::from("/out"))
    }

    #[test]
    fn debug_build_without_dist_gets_placeholder() {
        let (manifest, out) = dirs();
        assert_eq!(
            pick(&manifest, &out, false, false),
            UiSource::Placeholder(PathBuf::from("/out").join(PLACEHOLDER_DIR))
        );
    }

    #[test]
    fn release_build_without_dist_refuses_and_names_npm() {
        let (manifest, out) = dirs();
        match pick(&manifest, &out, true, false) {
            UiSource::Refuse(msg) => {
                assert!(msg.contains("npm run build"), "нет команды сборки: {msg}");
                assert!(msg.contains("admin-ui/dist"), "нет пути до UI: {msg}");
            }
            other => panic!("релиз без UI обязан отказываться, получили {other:?}"),
        }
    }

    #[test]
    fn built_dist_wins_in_both_profiles() {
        let (manifest, out) = dirs();
        let built = UiSource::Built(PathBuf::from("/src/xr-hub/admin-ui/dist"));
        assert_eq!(pick(&manifest, &out, false, true), built);
        assert_eq!(pick(&manifest, &out, true, true), built);
    }

    #[test]
    fn placeholder_page_lands_on_disk_and_names_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join(PLACEHOLDER_DIR);
        write_placeholder(&dir).unwrap();

        let page = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(page.contains("npm run build"), "страница молчит о сборке: {page}");
    }
}
