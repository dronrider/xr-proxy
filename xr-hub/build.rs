// Логика выбора решена в src/ui_dist.rs, тесты крейта гоняют её же.
include!("src/ui_dist.rs");

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dist = manifest_dir.join("admin-ui").join("dist");

    // Пока dist нет, следим за родительским каталогом: он меняется, когда
    // npm-сборка каталог создаёт.
    let watch = if dist.exists() { dist.clone() } else { manifest_dir.join("admin-ui") };
    println!("cargo:rerun-if-changed={}", watch.display());

    // С dev-ui страницы отдаются с диска, вшивать нечего, и релиз тоже вправе
    // собираться без npm.
    let release = std::env::var("PROFILE").as_deref() == Ok("release")
        && !cfg!(feature = "dev-ui");

    match pick(&manifest_dir, &out_dir, release, dist.join("index.html").exists()) {
        UiSource::Built(dir) => emit(&dir, false),
        UiSource::Placeholder(dir) => {
            write_placeholder(&dir).expect("не удалось положить заглушку admin UI");
            emit(&dir, true);
        }
        UiSource::Refuse(msg) => {
            println!("cargo:warning={msg}");
            panic!("{msg}");
        }
    }
}

fn emit(dir: &Path, placeholder: bool) {
    println!("cargo:rustc-env=XR_HUB_UI_DIR={}", dir.display());
    println!(
        "cargo:rustc-env=XR_HUB_UI_PLACEHOLDER={}",
        if placeholder { "1" } else { "0" }
    );
}
