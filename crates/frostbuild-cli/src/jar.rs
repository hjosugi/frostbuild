use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub fn pack(
    workspace: &Path,
    input: &Path,
    output: &Path,
    main_class: Option<&str>,
) -> Result<usize> {
    let input = workspace_path(workspace, input, "input")?;
    let output = workspace_path(workspace, output, "output")?;
    let metadata = std::fs::symlink_metadata(&input)
        .with_context(|| format!("JAR input {} does not exist", input.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("JAR input {} must be a real directory", input.display());
    }
    if output.starts_with(&input) {
        bail!("JAR output must not be inside its input directory");
    }

    let mut files = Vec::new();
    collect_files(&input, &input, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temp = output.with_extension(format!("jar.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let result = write_archive(&temp, &files, main_class);
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
    Ok(files.len())
}

fn workspace_path(workspace: &Path, path: &Path, label: &str) -> Result<PathBuf> {
    let text = path
        .to_str()
        .with_context(|| format!("non-UTF-8 JAR {label} path is not supported"))?;
    let relative = frostbuild_core::paths::validate_rel_path(text)
        .with_context(|| format!("invalid JAR {label} path"))?;
    Ok(workspace.join(relative))
}

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            bail!("JAR input contains symlink {}", path.display());
        }
        if file_type.is_dir() {
            collect_files(root, &path, files)?;
        } else if file_type.is_file() {
            let name = path
                .strip_prefix(root)
                .expect("collected JAR path stays under its root")
                .to_string_lossy()
                .replace('\\', "/");
            if !name.eq_ignore_ascii_case("META-INF/MANIFEST.MF") {
                files.push((name, path));
            }
        }
    }
    Ok(())
}

fn write_archive(
    destination: &Path,
    files: &[(String, PathBuf)],
    main_class: Option<&str>,
) -> Result<()> {
    let file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
        .unix_permissions(0o644);

    archive.start_file("META-INF/MANIFEST.MF", options)?;
    archive.write_all(b"Manifest-Version: 1.0\r\nCreated-By: FrostBuild\r\n")?;
    if let Some(main_class) = main_class {
        validate_main_class(main_class)?;
        write_manifest_header(&mut archive, "Main-Class", main_class)?;
    }
    archive.write_all(b"\r\n")?;

    for (name, path) in files {
        archive
            .start_file(name, options)
            .with_context(|| format!("failed to add JAR entry {name:?}"))?;
        let mut source =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        std::io::copy(&mut source, &mut archive)
            .with_context(|| format!("failed to add {}", path.display()))?;
    }
    archive.finish()?;
    Ok(())
}

fn validate_main_class(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 65_535
        || !value.split('.').all(|component| {
            let mut chars = component.chars();
            chars.next().is_some_and(is_java_identifier_start) && chars.all(is_java_identifier_part)
        })
    {
        bail!("main class must be a non-empty Java binary name");
    }
    Ok(())
}

fn is_java_identifier_start(character: char) -> bool {
    character == '_' || character == '$' || character.is_alphabetic()
}

fn is_java_identifier_part(character: char) -> bool {
    is_java_identifier_start(character) || character.is_numeric()
}

fn write_manifest_header(output: &mut impl Write, name: &str, value: &str) -> std::io::Result<()> {
    // The JAR specification caps a physical manifest line at 72 bytes. Keep
    // content to 70 so CRLF also fits under that cap, and prefix continuation
    // lines with the required single space.
    const CONTENT_BYTES: usize = 70;
    let prefix = format!("{name}: ");
    let first = utf8_prefix(value, CONTENT_BYTES - prefix.len());
    output.write_all(prefix.as_bytes())?;
    output.write_all(&value.as_bytes()[..first])?;
    output.write_all(b"\r\n")?;
    let mut cursor = first;
    while cursor < value.len() {
        let length = utf8_prefix(&value[cursor..], CONTENT_BYTES - 1);
        let end = cursor + length;
        output.write_all(b" ")?;
        output.write_all(&value.as_bytes()[cursor..end])?;
        output.write_all(b"\r\n")?;
        cursor = end;
    }
    Ok(())
}

fn utf8_prefix(value: &str, max_bytes: usize) -> usize {
    if value.len() <= max_bytes {
        return value.len();
    }
    value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|&index| index <= max_bytes)
        .last()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn jar_is_sorted_deterministic_and_can_name_a_main_class() {
        let root = std::env::temp_dir().join(format!("frost-pack-jar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("classes/pkg")).unwrap();
        std::fs::write(root.join("classes/pkg/Z.class"), b"z").unwrap();
        std::fs::write(root.join("classes/pkg/A.class"), b"a").unwrap();

        assert_eq!(
            pack(
                &root,
                Path::new("classes"),
                Path::new("out/app.jar"),
                Some("pkg.A"),
            )
            .unwrap(),
            2
        );
        let first = std::fs::read(root.join("out/app.jar")).unwrap();
        pack(
            &root,
            Path::new("classes"),
            Path::new("out/app.jar"),
            Some("pkg.A"),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("out/app.jar")).unwrap(), first);

        let file = File::open(root.join("out/app.jar")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(
            (0..archive.len())
                .map(|index| archive.by_index(index).unwrap().name().to_string())
                .collect::<Vec<_>>(),
            ["META-INF/MANIFEST.MF", "pkg/A.class", "pkg/Z.class",]
        );
        let mut manifest = String::new();
        archive
            .by_name("META-INF/MANIFEST.MF")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        assert!(manifest.contains("Main-Class: pkg.A\r\n"), "{manifest:?}");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn long_main_class_uses_spec_compliant_continuation_lines() {
        let name = format!("pkg.{}", "VeryLongClassName".repeat(8));
        let mut manifest = Vec::new();
        write_manifest_header(&mut manifest, "Main-Class", &name).unwrap();
        let text = String::from_utf8(manifest).unwrap();
        assert!(
            text.split_terminator("\r\n").all(|line| line.len() <= 70),
            "{text:?}"
        );
        assert!(text.lines().skip(1).all(|line| line.starts_with(' ')));
    }
}
