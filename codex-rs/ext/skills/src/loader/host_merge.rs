use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_exec_server::ExecutorFileSystem;
use codex_protocol::protocol::Product;
use codex_protocol::protocol::SkillScope;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;
use futures::StreamExt;
use tokio::sync::OnceCell;
use tokio::sync::Semaphore;

use super::HostSkillRoot;
use super::MAX_CONCURRENT_ROOT_SCANS;
use super::host::HostSkillRootSnapshot;
use super::load_host_skill_root;
use crate::SkillLoadOutcome;
use crate::host_service::RequestSkillRootSnapshots;
use crate::host_service::config_skill_root_cache_key;

pub(crate) async fn load_and_merge_host_skill_roots(
    roots: Vec<HostSkillRoot>,
    root_scan_slots: &Semaphore,
    restriction_product: Option<Product>,
    request_root_snapshots: Option<&RequestSkillRootSnapshots>,
) -> SkillLoadOutcome {
    let mut indexed_snapshots = futures::stream::iter(roots.into_iter().enumerate())
        .map(|(root_index, root)| async move {
            let request_snapshot = request_root_snapshots.map(|snapshots| {
                Arc::clone(
                    snapshots
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .entry(config_skill_root_cache_key(&root))
                        .or_insert_with(|| Arc::new(OnceCell::new())),
                )
            });
            let load = async {
                let _root_scan_slot = root_scan_slots
                    .acquire()
                    .await
                    .unwrap_or_else(|_| unreachable!());
                load_host_skill_root(root).await
            }
            .boxed();
            let snapshot = if let Some(snapshot) = request_snapshot {
                snapshot.get_or_init(|| load).await.clone()
            } else {
                load.await
            };
            (root_index, snapshot)
        })
        .buffer_unordered(MAX_CONCURRENT_ROOT_SCANS)
        .collect::<Vec<_>>()
        .await;
    indexed_snapshots.sort_unstable_by_key(|(root_index, _)| *root_index);

    for (_, snapshot) in &mut indexed_snapshots {
        snapshot
            .skills
            .retain(|skill| skill.matches_product_restriction_for_product(restriction_product));
    }

    merge_host_skill_root_snapshots(
        indexed_snapshots
            .into_iter()
            .map(|(_, snapshot)| snapshot)
            .collect(),
    )
}

fn merge_host_skill_root_snapshots(snapshots: Vec<HostSkillRootSnapshot>) -> SkillLoadOutcome {
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    let mut skill_roots = Vec::new();
    let mut skill_root_by_path = HashMap::new();
    let mut skill_discovery_path_by_path = HashMap::new();
    let mut file_systems_by_skill_path = HashMap::<PathUri, Arc<dyn ExecutorFileSystem>>::new();

    for snapshot in snapshots {
        if !snapshot.skills.is_empty() && !skill_roots.contains(&snapshot.root) {
            skill_roots.push(snapshot.root.clone());
        }
        for skill in &snapshot.skills {
            let path = skill.path_to_skills_md.clone();
            if !skill_root_by_path.contains_key(&path) {
                skill_root_by_path.insert(path.clone(), snapshot.root.clone());
                if let Some(discovery_path) = snapshot.skill_discovery_path_by_path.get(&path) {
                    skill_discovery_path_by_path.insert(path.clone(), discovery_path.clone());
                }
                file_systems_by_skill_path.insert(path, Arc::clone(&snapshot.file_system));
            }
        }
        skills.extend(snapshot.skills);
        errors.extend(snapshot.errors);
    }

    let mut seen_paths = HashSet::new();
    skills.retain(|skill| seen_paths.insert(skill.path_to_skills_md.clone()));
    let retained_paths = skills
        .iter()
        .map(|skill| skill.path_to_skills_md.clone())
        .collect::<HashSet<_>>();
    skill_root_by_path.retain(|path, _| retained_paths.contains(path));
    skill_discovery_path_by_path.retain(|path, _| retained_paths.contains(path));
    let retained_roots = skill_root_by_path.values().collect::<HashSet<_>>();
    skill_roots.retain(|root| retained_roots.contains(root));
    file_systems_by_skill_path.retain(|path, _| retained_paths.contains(path));
    // Keep native path ordering: URI escaping can reorder same-name skills.
    skills.sort_by_cached_key(|skill| {
        (
            scope_rank(skill.scope),
            skill.name.clone(),
            skill.path_to_skills_md.to_abs_path().ok(),
        )
    });

    SkillLoadOutcome::from_parts(
        skills,
        errors,
        skill_roots,
        skill_root_by_path,
        skill_discovery_path_by_path,
        file_systems_by_skill_path,
    )
}

fn scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::Repo => 0,
        SkillScope::User => 1,
        SkillScope::System => 2,
        SkillScope::Admin => 3,
    }
}
