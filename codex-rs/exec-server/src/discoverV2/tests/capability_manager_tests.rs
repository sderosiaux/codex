//! Covers snapshot caching, shared initialization, cancellation retry, and cached warnings.

use super::*;
use crate::discover_v2::capability_locations::tests::request;
use crate::discover_v2::capability_locations::tests::write;
use pretty_assertions::assert_eq;

async fn setup() -> anyhow::Result<(
    tempfile::TempDir,
    CapabilityLocationRequest,
    Arc<CapabilityManager>,
)> {
    let directory = tempfile::tempdir()?;
    write(
        directory.path(),
        "codex/skills/demo/SKILL.md",
        "---\nname: demo\ndescription: original\n---\nbody never cached",
    )?;
    let request = request(&std::fs::canonicalize(directory.path())?)?;
    let file_system = LocalFileSystem::new();
    let manager = CapabilityManager::new(file_system);
    manager.prewarm_locations(request.clone()).await?;
    capability_watchers::tests::stop_listener(&manager).await?;
    Ok((directory, request, manager))
}

#[tokio::test]
async fn discovery_responses_are_cached_per_location_snapshot() -> anyhow::Result<()> {
    let (_directory, request, manager) = setup().await?;
    let first = manager.get_or_refresh_discovery(request.clone()).await?;
    assert_eq!(first.skills.len(), 1);
    assert!(Arc::ptr_eq(
        &first,
        &manager.get_or_refresh_discovery(request.clone()).await?
    ));

    // A refreshed snapshot reloads lazily, but previously returned responses stay valid.
    manager.refresh_locations().await;
    let reloaded = manager.get_or_refresh_discovery(request).await?;
    assert_eq!(reloaded, first);
    assert!(!Arc::ptr_eq(&reloaded, &first));
    Ok(())
}

#[test]
fn cancelled_initialization_retries_and_warning_responses_are_cached() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?
        .block_on(async {
            let (directory, request, manager) = setup().await?;
            let snapshot = manager
                .location_snapshot()
                .await
                .ok_or_else(|| anyhow::anyhow!("missing snapshot"))?;
            // Hold the only blocking worker so discovery's filesystem read cannot finish early.
            // Dropping the sender also releases the worker if an assertion unwinds.
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let (started, ready) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = wait.recv();
            });
            ready.await?;
            let mut first = Box::pin(manager.get_or_refresh_discovery(request.clone()));
            assert!(futures::poll!(&mut first).is_pending());
            let mut second = Box::pin(manager.get_or_refresh_discovery(request.clone()));
            assert!(futures::poll!(&mut second).is_pending());
            assert!(snapshot.discovery.get().is_none());
            // Cancel the initializer; the waiting request must be able to initialize instead.
            drop(first);
            drop(release);
            blocker.await?;
            let original = second.await?;
            assert_eq!(original.skills.len(), 1);
            assert!(Arc::ptr_eq(
                &original,
                &manager.get_or_refresh_discovery(request.clone()).await?,
            ));

            write(
                directory.path(),
                "codex/skills/demo/SKILL.md",
                "invalid skill metadata",
            )?;
            // Invalidation replaces the snapshot instead of overwriting its initialized response.
            manager.refresh_locations().await;
            let (partial, concurrent) = tokio::try_join!(
                manager.get_or_refresh_discovery(request.clone()),
                manager.get_or_refresh_discovery(request.clone()),
            )?;
            assert!(Arc::ptr_eq(&partial, &concurrent));
            assert!(partial.skills.is_empty());
            assert!(!partial.warnings.is_empty());
            assert!(Arc::ptr_eq(
                &partial,
                &manager.get_or_refresh_discovery(request.clone()).await?
            ));

            write(
                directory.path(),
                "codex/skills/demo/SKILL.md",
                "---\nname: demo\ndescription: recovered\n---\n",
            )?;
            assert!(Arc::ptr_eq(
                &partial,
                &manager.get_or_refresh_discovery(request.clone()).await?
            ));
            manager.refresh_locations().await;
            let recovered = manager.get_or_refresh_discovery(request.clone()).await?;
            assert!(recovered.warnings.is_empty());
            assert_eq!(
                recovered
                    .skills
                    .iter()
                    .map(|skill| skill.description.as_str())
                    .collect::<Vec<_>>(),
                vec!["recovered"]
            );
            assert!(Arc::ptr_eq(
                &recovered,
                &manager.get_or_refresh_discovery(request).await?
            ));
            Ok(())
        })
}
