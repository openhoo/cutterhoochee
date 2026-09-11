use std::fmt;

#[derive(Debug)]
pub(crate) struct RuntimeError {
    message: String,
}

impl RuntimeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    #[cfg(target_os = "linux")]
    fn io(operation: &str, path: &std::path::Path, error: std::io::Error) -> Self {
        Self::new(format!("{operation} {}: {error}", path.to_string_lossy()))
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

#[cfg(not(target_os = "linux"))]
pub(crate) struct RuntimeGuard;

#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare() -> Result<RuntimeGuard, RuntimeError> {
    Ok(RuntimeGuard)
}

#[cfg(target_os = "linux")]
mod linux {
    use super::RuntimeError;
    use serde::Deserialize;
    use std::env;
    use std::ffi::OsStr;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    const MARKER_RELATIVE_PATH: &str = "usr/share/cutterhoochee/gtk-runtime.json";
    const GLYCIN_DATA_SUBPATH: &str = "glycin-loaders/2+/conf.d";
    const GLYCIN_TEMPLATE_SUBPATH: &str = "usr/share/glycin-loaders/2+/conf.d";
    const GLYCIN_LOADER_SUBPATH: &str = "usr/bin";
    const MIME_CACHE_RELATIVE_PATH: &str = "usr/share/mime/mime.cache";
    const WRAPPER_NAME: &str = "bwrap";
    const REAL_BWRAP_NAME: &str = "cutterhoochee-bwrap";
    const PRIVATE_DIR_PREFIX: &str = "cutterhoochee-glycin-";
    const PRIVATE_DIR_MODE: u32 = 0o700;
    const PRIVATE_FILE_MODE: u32 = 0o600;
    const MAX_MARKER_BYTES: u64 = 16 * 1024;
    const MAX_TEMPLATE_BYTES: u64 = 1024 * 1024;
    const REQUIRED_LOADERS: [&str; 2] = ["glycin-image-rs", "glycin-svg"];
    const KNOWN_LOADERS: [&str; 4] = ["glycin-image-rs", "glycin-svg", "glycin-heif", "glycin-jxl"];

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RuntimeMarker {
        version: u32,
        glycin_compat_version: String,
        loaders: Vec<String>,
    }

    #[derive(Debug)]
    struct BundleLayout {
        app_dir: PathBuf,
        loaders: Vec<String>,
    }

    pub(crate) struct RuntimeGuard {
        config: Option<PrivateConfig>,
    }

    impl RuntimeGuard {
        fn disabled() -> Self {
            Self { config: None }
        }

        fn cleanup(&mut self) {
            // GLYCIN_DATA_DIR intentionally remains active for the process lifetime.
            // Only the owned config tree is removed here; restoring a global environment
            // variable after GTK/Glycin threads have started would race those threads.
            self.config.take();
        }
    }

    impl Drop for RuntimeGuard {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    struct PrivateConfig {
        root: PathBuf,
    }

    impl Drop for PrivateConfig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    pub(crate) fn prepare() -> Result<RuntimeGuard, RuntimeError> {
        let Some(bundle) = discover_bundle()? else {
            return Ok(RuntimeGuard::disabled());
        };

        let config = prepare_filesystem(&bundle)?;
        if let Err(error) = activate_environment(&config.root, &bundle.app_dir.join("usr/bin")) {
            drop(config);
            return Err(error);
        }

        Ok(RuntimeGuard {
            config: Some(config),
        })
    }

    fn discover_bundle() -> Result<Option<BundleLayout>, RuntimeError> {
        let executable = env::current_exe().map_err(|error| {
            RuntimeError::io(
                "could not locate the current executable",
                Path::new("/proc/self/exe"),
                error,
            )
        })?;
        let executable = fs::canonicalize(&executable).map_err(|error| {
            RuntimeError::io(
                "could not resolve the current executable",
                &executable,
                error,
            )
        })?;
        let Some(bin_dir) = executable
            .parent()
            .filter(|path| path.file_name() == Some(OsStr::new("bin")))
        else {
            return Ok(None);
        };
        let Some(usr_dir) = bin_dir
            .parent()
            .filter(|path| path.file_name() == Some(OsStr::new("usr")))
        else {
            return Ok(None);
        };
        let Some(app_dir) = usr_dir.parent() else {
            return Ok(None);
        };
        let marker_path = app_dir.join(MARKER_RELATIVE_PATH);
        match fs::symlink_metadata(&marker_path) {
            Ok(_) => load_bundle(app_dir, marker_path).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RuntimeError::io(
                "could not inspect the packaged Glycin marker",
                &marker_path,
                error,
            )),
        }
    }

    fn load_bundle(app_dir: &Path, marker_path: PathBuf) -> Result<BundleLayout, RuntimeError> {
        if !app_dir.is_absolute() {
            return Err(RuntimeError::new(
                "packaged Glycin AppDir must be an absolute path",
            ));
        }
        let expected_marker = app_dir.join(MARKER_RELATIVE_PATH);
        if marker_path != expected_marker {
            return Err(RuntimeError::new(format!(
                "packaged Glycin marker is outside the AppDir layout: {}",
                marker_path.display()
            )));
        }
        let metadata = fs::symlink_metadata(&marker_path).map_err(|error| {
            RuntimeError::io(
                "could not inspect the packaged Glycin marker",
                &marker_path,
                error,
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(RuntimeError::new(format!(
                "packaged Glycin marker is not a regular file: {}",
                marker_path.display()
            )));
        }
        if metadata.len() > MAX_MARKER_BYTES {
            return Err(RuntimeError::new(format!(
                "packaged Glycin marker is too large: {}",
                marker_path.display()
            )));
        }
        let marker_bytes = fs::read(&marker_path).map_err(|error| {
            RuntimeError::io(
                "could not read the packaged Glycin marker",
                &marker_path,
                error,
            )
        })?;
        let marker: RuntimeMarker = serde_json::from_slice(&marker_bytes).map_err(|error| {
            RuntimeError::new(format!(
                "packaged Glycin marker is invalid ({}): {error}",
                marker_path.display()
            ))
        })?;
        validate_marker(&marker, &marker_path)?;

        let bundle = BundleLayout {
            app_dir: app_dir.to_path_buf(),
            loaders: marker.loaders,
        };
        validate_resources(&bundle)?;
        Ok(bundle)
    }

    fn validate_marker(marker: &RuntimeMarker, marker_path: &Path) -> Result<(), RuntimeError> {
        if marker.version != 1 {
            return Err(RuntimeError::new(format!(
                "packaged Glycin marker has unsupported version {}: {}",
                marker.version,
                marker_path.display()
            )));
        }
        if marker.glycin_compat_version != "2+" {
            return Err(RuntimeError::new(format!(
                "packaged Glycin marker has unsupported compatibility version {:?}: {}",
                marker.glycin_compat_version,
                marker_path.display()
            )));
        }
        if marker.loaders.is_empty() {
            return Err(RuntimeError::new(format!(
                "packaged Glycin marker does not list any loaders: {}",
                marker_path.display()
            )));
        }
        for (index, loader) in marker.loaders.iter().enumerate() {
            if !KNOWN_LOADERS.contains(&loader.as_str()) {
                return Err(RuntimeError::new(format!(
                    "packaged Glycin marker lists unknown loader {:?}: {}",
                    loader,
                    marker_path.display()
                )));
            }
            if marker.loaders[..index]
                .iter()
                .any(|previous| previous == loader)
            {
                return Err(RuntimeError::new(format!(
                    "packaged Glycin marker lists loader {:?} more than once: {}",
                    loader,
                    marker_path.display()
                )));
            }
        }
        for required in REQUIRED_LOADERS {
            if !marker.loaders.iter().any(|loader| loader == required) {
                return Err(RuntimeError::new(format!(
                    "packaged Glycin marker is missing required loader {required:?}: {}",
                    marker_path.display()
                )));
            }
        }
        Ok(())
    }

    fn validate_resources(bundle: &BundleLayout) -> Result<(), RuntimeError> {
        ensure_regular_file(&bundle.app_dir.join(MIME_CACHE_RELATIVE_PATH), "MIME cache")?;
        ensure_executable(
            &bundle.app_dir.join("usr/bin").join(WRAPPER_NAME),
            "bubblewrap wrapper",
        )?;
        ensure_executable(
            &bundle.app_dir.join("usr/bin").join(REAL_BWRAP_NAME),
            "bundled bubblewrap executable",
        )?;

        for loader in &bundle.loaders {
            let template = bundle
                .app_dir
                .join(GLYCIN_TEMPLATE_SUBPATH)
                .join(format!("{loader}.conf"));
            ensure_regular_file(&template, "Glycin loader template")?;
            let executable = bundle.app_dir.join(GLYCIN_LOADER_SUBPATH).join(loader);
            ensure_executable(&executable, "Glycin loader executable")?;
        }
        Ok(())
    }

    fn ensure_regular_file(path: &Path, description: &str) -> Result<(), RuntimeError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            RuntimeError::io(&format!("could not inspect {description}"), path, error)
        })?;
        if !metadata.file_type().is_file() {
            return Err(RuntimeError::new(format!(
                "{description} is not a regular file: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn ensure_executable(path: &Path, description: &str) -> Result<(), RuntimeError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            RuntimeError::io(&format!("could not inspect {description}"), path, error)
        })?;
        if !metadata.file_type().is_file() {
            return Err(RuntimeError::new(format!(
                "{description} is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(RuntimeError::new(format!(
                "{description} is not executable: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn prepare_filesystem(bundle: &BundleLayout) -> Result<PrivateConfig, RuntimeError> {
        let root = create_private_directory()?;
        let config = PrivateConfig { root };
        match write_configs(bundle, &config.root) {
            Ok(()) => Ok(config),
            Err(error) => {
                drop(config);
                Err(error)
            }
        }
    }

    fn create_private_directory() -> Result<PathBuf, RuntimeError> {
        let base = env::temp_dir();
        for _ in 0..8 {
            let root = base.join(format!("{PRIVATE_DIR_PREFIX}{}", Uuid::new_v4().simple()));
            match fs::DirBuilder::new().mode(PRIVATE_DIR_MODE).create(&root) {
                Ok(()) => return Ok(root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(RuntimeError::io(
                        "could not create the private Glycin directory",
                        &root,
                        error,
                    ))
                }
            }
        }
        Err(RuntimeError::new(
            "could not allocate a unique private Glycin directory",
        ))
    }

    fn write_configs(bundle: &BundleLayout, root: &Path) -> Result<(), RuntimeError> {
        let destination_dir = root.join(GLYCIN_DATA_SUBPATH);
        fs::create_dir_all(&destination_dir).map_err(|error| {
            RuntimeError::io(
                "could not create the private Glycin config directory",
                &destination_dir,
                error,
            )
        })?;
        for loader in &bundle.loaders {
            let source = bundle
                .app_dir
                .join(GLYCIN_TEMPLATE_SUBPATH)
                .join(format!("{loader}.conf"));
            let metadata = fs::symlink_metadata(&source).map_err(|error| {
                RuntimeError::io(
                    "could not inspect the Glycin loader template",
                    &source,
                    error,
                )
            })?;
            if metadata.len() > MAX_TEMPLATE_BYTES {
                return Err(RuntimeError::new(format!(
                    "Glycin loader template is too large: {}",
                    source.display()
                )));
            }
            let template = fs::read_to_string(&source).map_err(|error| {
                RuntimeError::io("could not read the Glycin loader template", &source, error)
            })?;
            let executable = bundle.app_dir.join(GLYCIN_LOADER_SUBPATH).join(loader);
            let relocated = rewrite_execs(&template, loader, &executable)?;
            let destination = destination_dir.join(format!("{loader}.conf"));
            write_private_file(&destination, relocated.as_bytes())?;
        }
        Ok(())
    }

    fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), RuntimeError> {
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(PRIVATE_FILE_MODE)
                .open(path)?;
            file.write_all(bytes)?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(path);
            return Err(RuntimeError::io(
                "could not create the private Glycin config",
                path,
                error,
            ));
        }
        Ok(())
    }

    fn rewrite_execs(
        template: &str,
        loader: &str,
        executable: &Path,
    ) -> Result<String, RuntimeError> {
        if template.contains('\0') {
            return Err(RuntimeError::new(format!(
                "Glycin loader template {loader:?} contains a NUL byte"
            )));
        }
        let executable = executable.to_str().ok_or_else(|| {
            RuntimeError::new(format!(
                "Glycin loader executable path for {loader:?} is not valid UTF-8"
            ))
        })?;
        let replacement = escape_keyfile_value(executable);
        let mut output = String::with_capacity(template.len() + replacement.len());
        let mut in_group = false;
        let mut exec_count = 0usize;

        for raw_line in template.split_inclusive('\n') {
            let (line, ending) = if let Some(line) = raw_line.strip_suffix("\r\n") {
                (line, "\r\n")
            } else if let Some(line) = raw_line.strip_suffix('\n') {
                (line, "\n")
            } else {
                (raw_line, "")
            };
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                output.push_str(line);
                output.push_str(ending);
                continue;
            }
            if trimmed.starts_with('[') {
                if !trimmed.ends_with(']') || trimmed.len() <= 2 {
                    return Err(RuntimeError::new(format!(
                        "Glycin loader template {loader:?} has an invalid group header"
                    )));
                }
                in_group = true;
                output.push_str(line);
                output.push_str(ending);
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(RuntimeError::new(format!(
                    "Glycin loader template {loader:?} has an invalid key line"
                )));
            };
            if !in_group || key.trim().is_empty() {
                return Err(RuntimeError::new(format!(
                    "Glycin loader template {loader:?} has a key outside a group"
                )));
            }
            if key.trim() == "Exec" {
                let source_executable = Path::new(value.trim());
                if !source_executable.is_absolute()
                    || source_executable.file_name() != Some(OsStr::new(loader))
                {
                    return Err(RuntimeError::new(format!(
                        "Glycin loader template {loader:?} has an unexpected Exec value"
                    )));
                }
                output.push_str("Exec=");
                output.push_str(&replacement);
                output.push_str(ending);
                exec_count += 1;
            } else {
                output.push_str(line);
                output.push_str(ending);
            }
        }
        if exec_count == 0 {
            return Err(RuntimeError::new(format!(
                "Glycin loader template {loader:?} has no Exec entry"
            )));
        }
        Ok(output)
    }

    fn escape_keyfile_value(value: &str) -> String {
        let mut escaped = String::with_capacity(value.len());
        let mut parsing_leading_space = true;
        for character in value.chars() {
            match character {
                ' ' if parsing_leading_space => escaped.push_str("\\s"),
                '\t' if parsing_leading_space => escaped.push_str("\\t"),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\\' => {
                    escaped.push_str("\\\\");
                    parsing_leading_space = false;
                }
                character => {
                    escaped.push(character);
                    parsing_leading_space = false;
                }
            }
        }
        escaped
    }

    fn prepend_path(
        bin_dir: &Path,
        old_path: Option<&OsStr>,
    ) -> Result<std::ffi::OsString, RuntimeError> {
        if !bin_dir.is_absolute() {
            return Err(RuntimeError::new(
                "the packaged Glycin PATH prefix must be absolute",
            ));
        }
        let mut paths = Vec::new();
        paths.push(bin_dir.to_path_buf());
        if let Some(old_path) = old_path {
            paths.extend(env::split_paths(old_path));
        }
        env::join_paths(paths)
            .map_err(|_| RuntimeError::new("the existing PATH cannot be extended safely"))
    }

    fn activate_environment(config_root: &Path, bin_dir: &Path) -> Result<(), RuntimeError> {
        if !config_root.is_absolute() || !bin_dir.is_absolute() {
            return Err(RuntimeError::new(
                "the packaged Glycin runtime requires absolute paths",
            ));
        }
        let path = prepend_path(bin_dir, env::var_os("PATH").as_deref())?;
        // All fallible validation and path construction happens above. These are the
        // only process-wide mutations and happen before Builder::build initializes GTK.
        env::set_var("GLYCIN_DATA_DIR", config_root);
        env::set_var("PATH", path);
        Ok(())
    }
    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use super::*;
        use std::env;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use uuid::Uuid;

        const ALL_LOADERS: [&str; 4] =
            ["glycin-image-rs", "glycin-svg", "glycin-heif", "glycin-jxl"];

        struct Fixture {
            app_dir: PathBuf,
        }

        impl Fixture {
            fn new() -> Self {
                let app_dir = env::temp_dir().join(format!(
                    "cutterhoochee-glycin-fixture {}",
                    Uuid::new_v4().simple()
                ));
                fs::create_dir_all(app_dir.join(GLYCIN_TEMPLATE_SUBPATH))
                    .expect("template directory");
                fs::create_dir_all(app_dir.join(GLYCIN_LOADER_SUBPATH)).expect("loader directory");
                fs::create_dir_all(app_dir.join("usr/share/mime")).expect("MIME directory");
                fs::write(app_dir.join(MIME_CACHE_RELATIVE_PATH), b"mime-cache")
                    .expect("MIME cache");
                fs::create_dir_all(app_dir.join("usr/bin")).expect("bin directory");

                for loader in ALL_LOADERS {
                    let template = format!(
                        "[loader:image/{loader}]\nExec=/usr/lib/glycin-loaders/2+/{loader}\n"
                    );
                    fs::write(
                        app_dir
                            .join(GLYCIN_TEMPLATE_SUBPATH)
                            .join(format!("{loader}.conf")),
                        template,
                    )
                    .expect("template");
                    let loader_path = app_dir.join(GLYCIN_LOADER_SUBPATH).join(loader);
                    fs::write(&loader_path, b"loader").expect("loader");
                    fs::set_permissions(&loader_path, fs::Permissions::from_mode(0o755))
                        .expect("loader mode");
                }
                for name in [WRAPPER_NAME, REAL_BWRAP_NAME] {
                    let path = app_dir.join("usr/bin").join(name);
                    fs::write(&path, b"#!/bin/sh\n").expect("bubblewrap");
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                        .expect("bubblewrap mode");
                }

                let marker_path = app_dir.join(MARKER_RELATIVE_PATH);
                fs::create_dir_all(marker_path.parent().expect("marker parent"))
                    .expect("marker directory");
                fs::write(
                &marker_path,
                br#"{"version":1,"glycinCompatVersion":"2+","loaders":["glycin-image-rs","glycin-svg","glycin-heif","glycin-jxl"]}"#,
            )
            .expect("marker");
                Self { app_dir }
            }

            fn bundle(&self) -> BundleLayout {
                load_bundle(&self.app_dir, self.app_dir.join(MARKER_RELATIVE_PATH))
                    .expect("valid fixture bundle")
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.app_dir);
            }
        }

        #[test]
        fn relocation_rewrites_every_exec_to_the_current_appdir() {
            let fixture = Fixture::new();
            let bundle = fixture.bundle();
            let config = prepare_filesystem(&bundle).expect("private config");
            let config_path = config
                .root
                .join(GLYCIN_DATA_SUBPATH)
                .join("glycin-image-rs.conf");
            let contents = fs::read_to_string(config_path).expect("relocated config");
            let executable = fixture
                .app_dir
                .join(GLYCIN_LOADER_SUBPATH)
                .join("glycin-image-rs");
            assert!(contents.contains(&format!("Exec={}", executable.display())));
            assert!(!contents.contains("/usr/lib/glycin-loaders/2+/glycin-image-rs"));
        }

        #[test]
        fn missing_loader_resource_is_rejected_before_config_creation() {
            let fixture = Fixture::new();
            let missing = fixture
                .app_dir
                .join(GLYCIN_LOADER_SUBPATH)
                .join("glycin-image-rs");
            fs::remove_file(&missing).expect("remove loader");
            let error = load_bundle(&fixture.app_dir, fixture.app_dir.join(MARKER_RELATIVE_PATH))
                .expect_err("missing loader must fail");
            assert!(error.to_string().contains("glycin-image-rs"));
        }

        #[test]
        fn keyfile_escape_matches_glib_string_rules() {
            assert_eq!(
                escape_keyfile_value(" \tfoo\\bar\n\r"),
                "\\s\\tfoo\\\\bar\\n\\r"
            );
            assert_eq!(escape_keyfile_value("path with spaces"), "path with spaces");
        }

        #[test]
        fn private_config_is_removed_when_runtime_guard_is_dropped() {
            let fixture = Fixture::new();
            let bundle = fixture.bundle();
            let config = prepare_filesystem(&bundle).expect("private config");
            let root = config.root.clone();
            assert_eq!(
                fs::metadata(&root).expect("root").permissions().mode() & 0o777,
                0o700
            );
            drop(config);
            assert!(!root.exists());
        }

        #[test]
        fn malformed_marker_is_rejected_instead_of_using_host_loaders() {
            let fixture = Fixture::new();
            let marker = fixture.app_dir.join(MARKER_RELATIVE_PATH);
            fs::write(
            &marker,
            br#"{"version":1,"glycinCompatVersion":"2+","loaders":["glycin-image-rs","glycin-svg","host-loader"]}"#,
        )
        .expect("malformed marker");
            let error = load_bundle(&fixture.app_dir, marker).expect_err("marker must fail");
            assert!(error.to_string().contains("unknown loader"));
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{prepare, RuntimeGuard};
