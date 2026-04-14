use std::path::Path;

fn main() {
    // Ensure web/dist exists so rust-embed compiles even before the frontend is built.
    // Silently ignore errors — in CI the directory may already exist with restricted permissions.
    let dist = Path::new("../web/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(dist);
    }
}
