use std::path::PathBuf;

use anyhow::Result;
use log::{info, warn};
use walkdir::WalkDir;

#[derive(Debug, PartialEq)]
pub enum FileType {
    Dir,
    File,
}

#[derive(Debug)]
pub struct ViewFile {
    pub filetype: FileType,
    pub path: PathBuf,
}

pub fn generate_paths(filter_paths: Vec<PathBuf>) -> Result<Vec<ViewFile>> {
    let mut files = Vec::new();

    for filter in filter_paths {
        for entry in WalkDir::new(filter).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("Skipping unreadable entry: {e}");
                    continue;
                }
            };
            let efiletype = entry.file_type();

            let filetype = if efiletype.is_symlink() {
                info!("Symlink skipped: {}", entry.path().display());
                continue;
            } else if efiletype.is_file() {
                FileType::File
            } else if efiletype.is_dir() {
                FileType::Dir
            } else {
                info!("File skipped: {}", entry.path().display());
                continue;
            };

            files.push(ViewFile {
                filetype,
                path: entry.into_path(),
            });
        }
    }

    Ok(files)
}
