use std::path::Path;
use std::process::Command;

/// Build the `web/` frontend bundle so `include_str!("../web/dist/automations.js")`
/// in `main.rs` has something to embed. Skipped when `FUSEBOX_SKIP_WEB_BUILD=1`
/// (CI/release pipelines that pre-build the bundle), and skipped silently when
/// `npm` is missing — in that case we expect `web/dist/automations.js` to
/// already exist, and `include_str!` will fail loudly if it doesn't.
fn main() {
    let cargo_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let web_dir = Path::new(&cargo_dir).join("web");

    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/vite.config.ts");
    println!("cargo:rerun-if-changed=web/tsconfig.json");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FUSEBOX_SKIP_WEB_BUILD");

    if std::env::var("FUSEBOX_SKIP_WEB_BUILD").is_ok() {
        eprintln!("FUSEBOX_SKIP_WEB_BUILD=1, skipping frontend build");
        return;
    }

    if which("npm").is_none() {
        eprintln!("npm not found in PATH; skipping frontend build");
        return;
    }

    let node_modules = web_dir.join("node_modules");
    if !node_modules.exists() {
        eprintln!("running `npm install` in web/");
        let status = Command::new("npm")
            .arg("install")
            .arg("--no-audit")
            .arg("--no-fund")
            .current_dir(&web_dir)
            .status()
            .expect("failed to invoke npm install");
        if !status.success() {
            panic!("npm install failed in web/");
        }
    }

    eprintln!("running `npm run build` in web/");
    let status = Command::new("npm")
        .arg("run")
        .arg("build")
        .current_dir(&web_dir)
        .status()
        .expect("failed to invoke npm run build");
    if !status.success() {
        panic!("npm run build failed in web/");
    }
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
