use std::fs::OpenOptions;
use std::io::Read;
use std::io::Result;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs;
use uuid::Uuid;

pub(crate) const INSTALLATION_ID_FILENAME: &str = "installation_id";

pub async fn resolve_installation_id(codex_home: &AbsolutePathBuf) -> Result<String> {
    let path = codex_home.join(INSTALLATION_ID_FILENAME);
    fs::create_dir_all(codex_home).await?;
    tokio::task::spawn_blocking(move || {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);

        #[cfg(unix)]
        {
            options.mode(0o644);
        }

        let mut file = options.open(&path)?;
        file.lock()?;

        #[cfg(unix)]
        {
            let metadata = file.metadata()?;
            let current_mode = metadata.permissions().mode() & 0o777;
            if current_mode != 0o644 {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o644);
                file.set_permissions(permissions)?;
            }
        }

        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let trimmed = contents.trim();
        if !trimmed.is_empty()
            && let Ok(existing) = Uuid::parse_str(trimmed)
        {
            return Ok(existing.to_string());
        }

        let installation_id = Uuid::new_v4().to_string();
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(installation_id.as_bytes())?;
        file.flush()?;
        file.sync_all()?;

        Ok(installation_id)
    })
    .await?
}
