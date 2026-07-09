use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use super::resolve::ResolvedFetchRequest;
use super::ArchiveFormat;

pub(super) fn detect_archive_format(
    request: &ResolvedFetchRequest,
) -> Result<ArchiveFormat, String> {
    match request.archive_format {
        ArchiveFormat::Auto => auto_archive_format(&request.cache_path),
        other => Ok(other),
    }
}

pub(super) fn auto_archive_format(path: &Path) -> Result<ArchiveFormat, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tar.gz") {
        Ok(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar.xz") {
        Ok(ArchiveFormat::TarXz)
    } else if name.ends_with(".tar.zst") || name.ends_with(".tzst") {
        Ok(ArchiveFormat::TarZst)
    } else if name.ends_with(".zip") {
        Ok(ArchiveFormat::Zip)
    } else if name.ends_with(".zst") {
        Ok(ArchiveFormat::Zst)
    } else if name.ends_with(".xz") {
        Ok(ArchiveFormat::Xz)
    } else if name.ends_with(".7z") {
        Ok(ArchiveFormat::SevenZip)
    } else {
        Ok(ArchiveFormat::None)
    }
}

pub(super) fn extract_archive(
    request: &ResolvedFetchRequest,
    expanded_path: &Path,
) -> Result<(), String> {
    match detect_archive_format(request)? {
        ArchiveFormat::None => {
            copy_file(&request.cache_path, expanded_path).map_err(|e| e.to_string())
        }
        ArchiveFormat::Zst => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let mut decoder = ruzstd::StreamingDecoder::new(input).map_err(|e| e.to_string())?;
            write_decoded_to_file(&mut decoder, expanded_path).map_err(|e| e.to_string())
        }
        ArchiveFormat::Xz => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            if let Some(parent) = expanded_path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut output = File::create(expanded_path).map_err(|e| e.to_string())?;
            let mut input = io::BufReader::new(input);
            lzma_rs::xz_decompress(&mut input, &mut output).map_err(|e| e.to_string())
        }
        ArchiveFormat::Zip => extract_zip(&request.cache_path, expanded_path),
        ArchiveFormat::TarGz => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let decoder = flate2::read::GzDecoder::new(input);
            extract_tar(decoder, expanded_path)
        }
        ArchiveFormat::TarXz => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let mut decoded = Vec::new();
            let mut input = io::BufReader::new(input);
            lzma_rs::xz_decompress(&mut input, &mut decoded).map_err(|e| e.to_string())?;
            extract_tar(io::Cursor::new(decoded), expanded_path)
        }
        ArchiveFormat::TarZst => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let decoder = ruzstd::StreamingDecoder::new(input).map_err(|e| e.to_string())?;
            extract_tar(decoder, expanded_path)
        }
        ArchiveFormat::SevenZip => extract_7z(&request.cache_path, expanded_path),
        ArchiveFormat::Auto => Err("archive format auto-detection failed".to_string()),
    }
}

pub(super) fn extract_7z(archive_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let base = destination.to_path_buf();
    sevenz_rust::decompress_file_with_extract_fn(
        archive_path,
        destination,
        move |entry, reader, _default_dest| {
            let relative = Path::new(entry.name());
            let out_path = safe_join(&base, relative).map_err(std::io::Error::other)?;
            if entry.is_directory() {
                fs::create_dir_all(&out_path)?;
                return Ok(true);
            }
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = File::create(&out_path)?;
            io::copy(reader, &mut output)?;
            output.flush()?;
            Ok(true)
        },
    )
    .map_err(|e| e.to_string())
}

pub(super) fn write_decoded_to_file(reader: &mut dyn Read, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = File::create(destination)?;
    io::copy(reader, &mut output)?;
    output.flush()?;
    Ok(())
}

pub(super) fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    Ok(())
}

pub(super) fn extract_zip(archive_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let file = File::open(archive_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry
            .enclosed_name()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| format!("unsafe zip entry: {}", entry.name()))?;
        let out_path = safe_join(destination, &name)?;
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(mode) = entry.unix_mode() {
            if (mode & 0o170000) == 0o120000 {
                return Err(format!(
                    "zip symlink entries are not allowed: {}",
                    entry.name()
                ));
            }
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = File::create(&out_path).map_err(|e| e.to_string())?;
        io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(super) fn extract_tar<R: Read>(reader: R, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|e| e.to_string())?;
    for item in entries {
        let mut entry = item.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        let out_path = safe_join(destination, &path)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(format!(
                "tar link entries are not allowed: {}",
                path.display()
            ));
        }
        if entry_type.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = File::create(&out_path).map_err(|e| e.to_string())?;
        io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(super) fn safe_join(base: &Path, entry: &Path) -> Result<PathBuf, String> {
    if entry.is_absolute() {
        return Err(format!(
            "absolute archive entry is not allowed: {}",
            entry.display()
        ));
    }
    let mut clean = PathBuf::new();
    for component in entry.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return Err(format!("unsafe archive entry: {}", entry.display())),
        }
    }
    Ok(base.join(clean))
}

pub(super) fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        fs::remove_file(path).map_err(|e| e.to_string())
    }
}
