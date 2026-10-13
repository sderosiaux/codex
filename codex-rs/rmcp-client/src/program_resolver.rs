//! Platform-specific program resolution for MCP server execution.
//!
//! This module provides a unified interface for resolving executable paths
//! across different operating systems. The key challenge it addresses is that
//! Windows cannot execute script files (e.g., `.cmd`, `.bat`) directly through
//! `Command::new()` without their file extensions, while Unix systems handle
//! scripts natively through shebangs.
//!
//! The `resolve` function abstracts these platform differences:
//! - On Unix: Returns the program unchanged (OS handles script execution)
//! - On Windows: Uses the `which` crate to resolve full paths including extensions

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

/// Resolves a program to its executable path on Unix systems.
///
/// Unix systems handle PATH resolution and script execution natively through
/// the kernel's shebang (`#!`) mechanism, so this function simply returns
/// the program name unchanged.
#[cfg(unix)]
pub fn resolve(
    program: OsString,
    _env: &HashMap<OsString, OsString>,
    _cwd: &Path,
) -> std::io::Result<OsString> {
    Ok(program)
}

/// Resolves a program to its executable path on Windows systems.
///
/// Windows requires explicit file extensions for script execution. This function
/// uses the `which` crate to search the `PATH` environment variable and find
/// the full path to the executable, including necessary script extensions
/// (`.cmd`, `.bat`, etc.) defined in `PATHEXT`.
///
/// This enables tools like `npx`, `pnpm`, and `yarn` to work correctly on Windows
/// without requiring users to specify full paths or extensions in their configuration.
#[cfg(windows)]
pub fn resolve(
    program: OsString,
    env: &HashMap<OsString, OsString>,
    cwd: &Path,
) -> std::io::Result<OsString> {
    // Extract PATH from environment for search locations
    let search_path = env.iter().find_map(|(name, value)| {
        name.to_string_lossy()
            .eq_ignore_ascii_case("PATH")
            .then_some(value)
    });

    // Attempt resolution via which crate
    match which::which_in(&program, search_path, cwd) {
        Ok(resolved) => {
            tracing::debug!("Resolved {program:?} to {resolved:?}");
            Ok(resolved.into_os_string())
        }
        Err(e) => {
            tracing::debug!("Failed to resolve {program:?}: {e}. Using original path");
            // Fallback to original program - let Command::new() handle the error
            Ok(program)
        }
    }
}
