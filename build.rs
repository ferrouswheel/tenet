use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Emit a build timestamp so the web client can detect when the binary was rebuilt.
    // Without any cargo:rerun-if-changed directives, Cargo re-runs this script whenever
    // any source file in the package changes, so BUILD_TIMESTAMP reflects every rebuild.
    let build_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_secs();
    println!("cargo:rustc-env=BUILD_TIMESTAMP={build_timestamp}");

    // Build the web assets
    build_web_assets();
}

fn build_web_assets() {
    let src_dir = Path::new("web/src");
    let dist_dir = Path::new("web/dist");

    // Create dist directory if it doesn't exist
    if !dist_dir.exists() {
        fs::create_dir_all(dist_dir).expect("Failed to create web/dist directory");
    }

    // Read source files
    let html_template =
        fs::read_to_string(src_dir.join("index.html")).expect("Failed to read web/src/index.html");
    let styles =
        fs::read_to_string(src_dir.join("styles.css")).expect("Failed to read web/src/styles.css");
    let scripts =
        fs::read_to_string(src_dir.join("app.js")).expect("Failed to read web/src/app.js");

    // Replace placeholders
    let output = html_template
        .replace("{{STYLES}}", &styles)
        .replace("{{SCRIPTS}}", &scripts);

    // Write output
    fs::write(dist_dir.join("index.html"), output).expect("Failed to write web/dist/index.html");
}
