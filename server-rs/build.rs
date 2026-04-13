use std::path::Path;

fn main() {
    // Ensure web/dist exists so rust-embed compiles even before the frontend is built.
    let dist = Path::new("../web/dist");
    if !dist.exists() {
        std::fs::create_dir_all(dist).expect("failed to create web/dist placeholder");
    }
}
