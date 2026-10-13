//! Path facts for configuration parsing. Native callers and callers resolving
//! another platform use the same resolver after fact capture.

use codex_utils_path_uri::LegacyAppPathStringError;
use codex_utils_path_uri::PathConvention;
use codex_utils_path_uri::PathUri;

/// The owning environment's path convention, base directory, and home for configuration.
#[derive(Clone, Debug)]
pub struct ConfigPathContext {
    convention: PathConvention,
    base_dir: Option<PathUri>,
    user_home_dir: Option<PathUri>,
}

impl ConfigPathContext {
    /// Supplies the grammar and directories of the environment owning the layer.
    /// Relative paths require a base; home-relative paths require a home directory.
    pub fn new(
        convention: PathConvention,
        base_dir: Option<PathUri>,
        user_home_dir: Option<PathUri>,
    ) -> Self {
        Self {
            convention,
            base_dir,
            user_home_dir,
        }
    }

    /// Returns the path grammar supplied by the owning environment.
    pub fn convention(&self) -> PathConvention {
        self.convention
    }

    /// Resolves configuration text to a URI using the supplied directory and home facts.
    pub fn resolve_path(&self, input: &str) -> Result<PathUri, LegacyAppPathStringError> {
        PathUri::resolve_config_path_uri(
            input,
            self.convention,
            self.base_dir.as_ref(),
            self.user_home_dir.as_ref(),
        )
    }

    /// Resolves against a supplied base using this context's grammar and home.
    pub fn resolve_against(
        &self,
        input: &str,
        base: &PathUri,
    ) -> Result<PathUri, LegacyAppPathStringError> {
        PathUri::resolve_config_path_uri(
            input,
            self.convention,
            Some(base),
            self.user_home_dir.as_ref(),
        )
    }
}
