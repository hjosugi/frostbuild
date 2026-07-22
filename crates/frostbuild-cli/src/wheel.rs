use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

struct Entry {
    name: String,
    bytes: Vec<u8>,
}

pub fn pack(
    workspace: &Path,
    input: &Path,
    distribution: &str,
    version: &str,
    output: &Path,
) -> Result<usize> {
    let input = workspace_path(workspace, input, "input")?;
    let output = workspace_path(workspace, output, "output")?;
    let metadata = std::fs::symlink_metadata(&input)
        .with_context(|| format!("wheel input {} does not exist", input.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("wheel input {} must be a real directory", input.display());
    }
    if output.starts_with(&input) {
        bail!("wheel output must not be inside its input directory");
    }

    let normalized_distribution = normalize_distribution(distribution)?;
    validate_version(version)?;
    let expected_filename = format!("{normalized_distribution}-{version}-py3-none-any.whl");
    if output.file_name().and_then(|name| name.to_str()) != Some(expected_filename.as_str()) {
        bail!("wheel output must end with {expected_filename:?}");
    }

    let mut entries = Vec::new();
    collect_files(&input, &input, &mut entries)?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    if entries.is_empty() {
        bail!("wheel input {} contains no files", input.display());
    }

    let dist_info = format!("{normalized_distribution}-{version}.dist-info");
    let metadata = format!("Metadata-Version: 2.1\nName: {distribution}\nVersion: {version}\n\n");
    entries.push(Entry {
        name: format!("{dist_info}/METADATA"),
        bytes: metadata.into_bytes(),
    });
    entries.push(Entry {
        name: format!("{dist_info}/WHEEL"),
        bytes: format!(
            "Wheel-Version: 1.0\nGenerator: FrostBuild {}\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
            env!("CARGO_PKG_VERSION")
        )
        .into_bytes(),
    });
    let source_count = entries.len() - 2;

    // RECORD is last physically, as recommended by the wheel specification.
    let record_name = format!("{dist_info}/RECORD");
    let mut record = String::new();
    for entry in &entries {
        let digest = Sha256::digest(&entry.bytes);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        record.push_str(&csv_field(&entry.name));
        record.push_str(",sha256=");
        record.push_str(&encoded);
        record.push(',');
        record.push_str(&entry.bytes.len().to_string());
        record.push('\n');
    }
    record.push_str(&csv_field(&record_name));
    record.push_str(",,\n");
    entries.push(Entry {
        name: record_name,
        bytes: record.into_bytes(),
    });

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temp = output.with_extension(format!("whl.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let result = write_archive(&temp, &entries);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if output.exists() {
        std::fs::remove_file(&output)
            .with_context(|| format!("failed to replace {}", output.display()))?;
    }
    std::fs::rename(&temp, &output)
        .with_context(|| format!("failed to publish {}", output.display()))?;
    Ok(source_count)
}

fn workspace_path(workspace: &Path, path: &Path, label: &str) -> Result<PathBuf> {
    let text = path
        .to_str()
        .with_context(|| format!("non-UTF-8 wheel {label} path is not supported"))?;
    let relative = frostbuild_core::paths::validate_rel_path(text)
        .with_context(|| format!("invalid wheel {label} path"))?;
    Ok(workspace.join(relative))
}

fn collect_files(root: &Path, directory: &Path, entries: &mut Vec<Entry>) -> Result<()> {
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            bail!("wheel input contains symlink {}", path.display());
        }
        if file_type.is_dir() {
            if entry.file_name() == "__pycache__" {
                continue;
            }
            collect_files(root, &path, entries)?;
        } else if file_type.is_file() {
            if path.extension().is_some_and(|extension| extension == "pyc") {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .expect("collected wheel path stays under its root");
            let name = archive_name(relative)?;
            let bytes = std::fs::read(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            entries.push(Entry { name, bytes });
        }
    }
    Ok(())
}

fn archive_name(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!("invalid wheel entry path {}", path.display());
        };
        let component = component.to_str().with_context(|| {
            format!("non-UTF-8 wheel entry {} is not supported", path.display())
        })?;
        if component.contains(['\r', '\n']) {
            bail!("wheel entry contains a line break: {}", path.display());
        }
        parts.push(component);
    }
    Ok(parts.join("/"))
}

fn normalize_distribution(value: &str) -> Result<String> {
    if value.is_empty() || !value.is_ascii() {
        bail!("wheel distribution must be a non-empty ASCII Python distribution name");
    }
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() {
            result.push((byte as char).to_ascii_lowercase());
        } else if matches!(byte, b'-' | b'_' | b'.') && !result.is_empty() {
            if !result.ends_with('_') {
                result.push('_');
            }
        } else {
            bail!("invalid character in wheel distribution {value:?}");
        }
    }
    if result.ends_with('_') {
        bail!("wheel distribution must end with an ASCII letter or digit");
    }
    Ok(result)
}

fn validate_version(value: &str) -> Result<()> {
    if value.is_empty()
        || !value.split('.').all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
                && (component == "0" || !component.starts_with('0'))
        })
    {
        bail!("wheel version must be a normalized numeric Python release such as 1.2.3");
    }
    Ok(())
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn write_archive(destination: &Path, entries: &[Entry]) -> Result<()> {
    let file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
        .unix_permissions(0o644);
    for entry in entries {
        archive
            .start_file(&entry.name, options)
            .with_context(|| format!("failed to add wheel entry {:?}", entry.name))?;
        archive.write_all(&entry.bytes)?;
    }
    archive.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn wheel_is_deterministic_and_record_hashes_every_entry() {
        let root = std::env::temp_dir().join(format!("frost-pack-wheel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/demo")).unwrap();
        std::fs::write(root.join("src/demo/__init__.py"), b"VALUE = 42\n").unwrap();
        std::fs::write(root.join("src/demo/data.txt"), b"data\n").unwrap();
        let output = Path::new("dist/demo_pkg-1.2.3-py3-none-any.whl");

        assert_eq!(
            pack(&root, Path::new("src"), "Demo.Pkg", "1.2.3", output).unwrap(),
            2
        );
        let first = std::fs::read(root.join(output)).unwrap();
        pack(&root, Path::new("src"), "Demo.Pkg", "1.2.3", output).unwrap();
        assert_eq!(std::fs::read(root.join(output)).unwrap(), first);

        let file = File::open(root.join(output)).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let record_name = "demo_pkg-1.2.3.dist-info/RECORD";
        let mut record = String::new();
        archive
            .by_name(record_name)
            .unwrap()
            .read_to_string(&mut record)
            .unwrap();
        assert!(record.ends_with(&format!("{record_name},,\n")));
        for name in [
            "demo/__init__.py",
            "demo/data.txt",
            "demo_pkg-1.2.3.dist-info/METADATA",
            "demo_pkg-1.2.3.dist-info/WHEEL",
        ] {
            let mut bytes = Vec::new();
            archive
                .by_name(name)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            let hash =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&bytes));
            assert!(
                record.contains(&format!("{name},sha256={hash},{}\n", bytes.len())),
                "{record:?}"
            );
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn distribution_names_follow_wheel_filename_normalization() {
        assert_eq!(
            normalize_distribution("Demo.Package-name").unwrap(),
            "demo_package_name"
        );
        assert!(normalize_distribution("bad/").is_err());
        assert!(validate_version("1.2.3").is_ok());
        assert!(validate_version("1.02.3").is_err());
        assert!(validate_version("1.0-bad").is_err());
    }
}
