// Adapted from path-absolutize 3.1.1:
// Copyright (c) 2018 magiclen.org (Ron Li)
// Licensed under the MIT License.
//
// Keep this implementation local so explicit-base normalization can be
// infallible for `AbsolutePathBuf::resolve_path_against_base` and
// `AbsolutePathBuf::join`; only current-working-directory lookup remains
// fallible.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn absolutize(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(normalize_path(path));
    }

    Ok(absolutize_from(path, &std::env::current_dir()?))
}

pub(super) fn absolutize_from(path: &Path, base_path: &Path) -> PathBuf {
    normalize_path(&path_with_base(path, base_path))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

#[cfg(not(windows))]
fn path_with_base(path: &Path, base_path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_path.join(path)
    }
}

#[cfg(windows)]
fn path_with_base(path: &Path, base_path: &Path) -> PathBuf {
    if path.is_absolute() || path.has_root() {
        return base_path.join(path);
    }

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return base_path.join(path);
    };

    let mut path = PathBuf::new();
    path.push(prefix.as_os_str());

    if components.clone().next().is_none() {
        path.push(std::path::MAIN_SEPARATOR_STR);
        return path;
    }

    let skip_base_prefix = matches!(base_path.components().next(), Some(Component::Prefix(_)));
    for component in base_path
        .components()
        .skip(usize::from(skip_base_prefix))
        .chain(components)
    {
        path.push(component.as_os_str());
    }
    path
}
