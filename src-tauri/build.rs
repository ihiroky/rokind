use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let env_path = manifest_dir
        .parent()
        .expect("src-tauri directory should have a parent")
        .join(".env");

    println!("cargo:rerun-if-changed={}", env_path.display());

    let mut client_id = None;
    let mut client_secret = None;
    let mut source = format!("build-time .env not found: {}", env_path.display());

    if env_path.is_file() {
        match dotenvy::from_path_iter(&env_path) {
            Ok(iter) => {
                for item in iter.flatten() {
                    match item.0.as_str() {
                        "GOOGLE_CLIENT_ID" => client_id = Some(item.1),
                        "GOOGLE_CLIENT_SECRET" => client_secret = Some(item.1),
                        _ => {}
                    }
                }
                source = format!("embedded from {}", env_path.display());
            }
            Err(error) => {
                source = format!(
                    "build-time .env load error at {}: {error}",
                    env_path.display()
                );
            }
        }
    }

    if let Some(value) = client_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        println!("cargo:rustc-env=GOOGLE_CLIENT_ID={value}");
    }

    if let Some(value) = client_secret
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        println!("cargo:rustc-env=GOOGLE_CLIENT_SECRET={value}");
    }

    println!("cargo:rustc-env=ROKIND_OAUTH_CONFIG_SOURCE={source}");

    tauri_build::build()
}
