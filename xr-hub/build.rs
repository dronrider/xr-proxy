fn main() {
    // In release mode without dev-ui feature, require admin-ui/dist/index.html.
    #[cfg(not(feature = "dev-ui"))]
    {
        let profile = std::env::var("PROFILE").unwrap_or_default();
        if profile == "release" {
            let dist = std::path::Path::new("admin-ui/dist/index.html");
            if !dist.exists() {
                println!(
                    "cargo:warning=admin-ui/dist/index.html not found. \
                     Run: cd xr-hub/admin-ui && npm ci && npm run build"
                );
                panic!(
                    "Admin UI not built. Run: cd xr-hub/admin-ui && npm ci && npm run build"
                );
            }
        }
    }
}
