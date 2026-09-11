use std::env;
use std::path::PathBuf;

fn main() {
    let target = env::var("TARGET").expect("Cargo must provide TARGET to the build script");
    println!("cargo:rustc-env=CUTTERHOOCHEE_TARGET_TRIPLE={target}");

    // Development and type generation are intentionally independent of local
    // sidecar staging. A release build, however, must never silently package an
    // app that falls back to PATH for Node or lacks the agent entrypoint.
    if env::var("PROFILE").as_deref() == Ok("release") {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let node_name = if target.contains("windows") {
            format!("node-{target}.exe")
        } else {
            format!("node-{target}")
        };
        let node_path = root.join("binaries").join(node_name);
        let agent_entrypoint = root.join("resources/agent/agent/dist/main.js");
        if !node_path.is_file() || !agent_entrypoint.is_file() {
            panic!(
                "release sidecars are incomplete; run `pnpm build:agent` and `pnpm prepare:sidecars` before packaging"
            );
        }
    }

    tauri_build::build();
}
