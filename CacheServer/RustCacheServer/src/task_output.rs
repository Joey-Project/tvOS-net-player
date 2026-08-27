use std::collections::{HashMap, HashSet};

use crate::generated::tvos_net_player::v1::{
    CacheResourceRef, Task, TaskArtifactKind, TaskArtifactState, TaskOutputSummary, TaskProblem,
    TaskProblemCategory, TaskResult, TaskResultProgress, TaskState,
};
use http::HeaderValue;
use uuid::Uuid;

const MAX_RESOURCE_ID_BYTES: usize = 200;
const MAX_TASK_RESULTS: usize = 10_000;
const MAX_TASK_RESOURCES: usize = 50_000;
const INTERNAL_RESOURCE_DIR: &str = ".tvos-net-player/resources";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TaskResourceRecord {
    pub(crate) resource: CacheResourceRef,
}

impl TaskResourceRecord {
    pub(crate) fn new(mut resource: CacheResourceRef) -> Result<Self, TaskOutputValidationError> {
        validate_resource_id(&resource.id)?;
        resource.id.make_ascii_lowercase();
        resource.uri = resource_uri(&resource.id);
        if resource.content_type.trim().is_empty()
            || HeaderValue::from_str(&resource.content_type).is_err()
        {
            resource.content_type = "application/octet-stream".to_owned();
        }
        if resource.size_bytes < 0 {
            resource.size_bytes = 0;
            resource.size_known = false;
        }
        resource.supports_byte_ranges = true;
        if resource.etag.len() > 512 || HeaderValue::from_str(&resource.etag).is_err() {
            resource.etag.clear();
        }
        Ok(Self { resource })
    }

    pub(crate) fn relative_path(&self) -> String {
        Self::relative_path_for_id(&self.resource.id)
    }

    pub(crate) fn relative_path_for_id(id: &str) -> String {
        format!("{INTERNAL_RESOURCE_DIR}/{id}/body")
    }

    pub(crate) fn relative_directory_for_id(id: &str) -> String {
        format!("{INTERNAL_RESOURCE_DIR}/{id}")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TaskOutputRecord {
    pub(crate) revision: u64,
    pub(crate) snapshot_id: String,
    pub(crate) primary_result_id: String,
    pub(crate) results: Vec<TaskResult>,
    pub(crate) resources: Vec<TaskResourceRecord>,
    pub(crate) legacy_managed: bool,
}

impl TaskOutputRecord {
    pub(crate) fn from_legacy_task(task: &Task) -> Self {
        let results = legacy_task_results(task);
        Self {
            revision: 1,
            snapshot_id: new_snapshot_id(),
            primary_result_id: legacy_primary_result_id(task, &results),
            results,
            resources: Vec::new(),
            legacy_managed: true,
        }
    }

    pub(crate) fn removed_task_tombstone(task: &Task, previous: Option<&Self>) -> Self {
        let mut output = Self::from_legacy_task(task);
        if let Some(previous) = previous {
            output.revision = previous.revision.saturating_add(1).max(1);
        }
        output
    }

    pub(crate) fn replace(
        previous: Option<&Self>,
        mut results: Vec<TaskResult>,
        resources: Vec<TaskResourceRecord>,
    ) -> Result<Self, TaskOutputValidationError> {
        validate_collection_sizes(&results, &resources)?;
        validate_and_bind_resources(&mut results, &resources)?;
        validate_result_ids(&results)?;
        validate_resource_representations(previous, &resources)?;

        let unchanged = previous.is_some_and(|previous| {
            previous.results == results
                && previous.resources == resources
                && !previous.legacy_managed
        });
        let revision = match previous {
            Some(previous) if unchanged => previous.revision.max(1),
            Some(previous) => previous.revision.saturating_add(1).max(1),
            None => 1,
        };
        let primary_result_id = previous
            .map(|output| output.primary_result_id.as_str())
            .filter(|primary_result_id| {
                results
                    .iter()
                    .any(|result| result.id.as_str() == *primary_result_id)
            })
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| inferred_primary_result_id(&results));
        Ok(Self {
            revision,
            snapshot_id: if unchanged {
                previous
                    .map(|output| output.snapshot_id.clone())
                    .unwrap_or_else(new_snapshot_id)
            } else {
                new_snapshot_id()
            },
            primary_result_id,
            results,
            resources,
            legacy_managed: false,
        })
    }

    pub(crate) fn restored(
        revision: u64,
        snapshot_id: String,
        primary_result_id: String,
        mut results: Vec<TaskResult>,
        resources: Vec<TaskResourceRecord>,
        legacy_managed: bool,
    ) -> Result<Self, TaskOutputValidationError> {
        validate_collection_sizes(&results, &resources)?;
        validate_result_ids(&results)?;
        validate_and_bind_resources(&mut results, &resources)?;
        let snapshot_id = if snapshot_id.trim().is_empty() {
            new_snapshot_id()
        } else {
            validate_snapshot_id(&snapshot_id)?;
            snapshot_id
        };
        let primary_result_id = if primary_result_id.is_empty() {
            inferred_primary_result_id(&results)
        } else if results.iter().any(|result| result.id == primary_result_id) {
            primary_result_id
        } else {
            return Err(TaskOutputValidationError::new(
                "task output primary result id does not exist in results",
            ));
        };
        Ok(Self {
            revision: revision.max(1),
            snapshot_id,
            primary_result_id,
            results,
            resources,
            legacy_managed,
        })
    }

    pub(crate) fn reconcile_legacy_task(&mut self, task: &Task) -> bool {
        if !self.legacy_managed {
            return false;
        }
        let results = legacy_task_results(task);
        let primary_result_id = legacy_primary_result_id(task, &results);
        if self.results == results && self.primary_result_id == primary_result_id {
            return false;
        }
        self.results = results;
        self.primary_result_id = primary_result_id;
        self.revision = self.revision.saturating_add(1).max(1);
        self.snapshot_id = new_snapshot_id();
        true
    }

    pub(crate) fn summary(&self) -> TaskOutputSummary {
        let mut terminal_result_count = 0_u64;
        let mut successful_result_count = 0_u64;
        let mut failed_result_count = 0_u64;
        let mut cancelled_result_count = 0_u64;
        let mut available_artifact_count = 0_u64;

        for result in &self.results {
            let state = result.state();
            if matches!(
                state,
                TaskState::Succeeded
                    | TaskState::Completed
                    | TaskState::Failed
                    | TaskState::Cancelled
            ) {
                terminal_result_count = terminal_result_count.saturating_add(1);
            }
            if matches!(state, TaskState::Succeeded | TaskState::Completed) {
                successful_result_count = successful_result_count.saturating_add(1);
            } else if state == TaskState::Failed {
                failed_result_count = failed_result_count.saturating_add(1);
            } else if state == TaskState::Cancelled {
                cancelled_result_count = cancelled_result_count.saturating_add(1);
            }
            available_artifact_count = available_artifact_count.saturating_add(
                result
                    .artifacts
                    .iter()
                    .filter(|artifact| artifact.state() == TaskArtifactState::Available)
                    .count()
                    .try_into()
                    .unwrap_or(u64::MAX),
            );
        }

        TaskOutputSummary {
            revision: self.revision.max(1),
            result_count: self.results.len().try_into().unwrap_or(u64::MAX),
            terminal_result_count,
            successful_result_count,
            failed_result_count,
            cancelled_result_count,
            available_artifact_count,
            primary_result_id: self.primary_result_id.clone(),
        }
    }

    pub(crate) fn resource(&self, id: &str) -> Option<&TaskResourceRecord> {
        let available = self
            .results
            .iter()
            .flat_map(|result| &result.artifacts)
            .any(|artifact| {
                artifact.state() == TaskArtifactState::Available
                    && artifact
                        .resource
                        .as_ref()
                        .is_some_and(|resource| resource.id == id)
            });
        available
            .then(|| {
                self.resources
                    .iter()
                    .find(|record| record.resource.id == id)
            })
            .flatten()
    }

    pub(crate) fn mark_playback_cache_deleted(
        &mut self,
        session_id: &str,
        library_item_id: &str,
        message: &str,
    ) -> Vec<String> {
        if self.legacy_managed {
            return Vec::new();
        }
        let mut changed = false;
        for result in &mut self.results {
            let matches_cache = (!library_item_id.is_empty()
                && result.library_item_id == library_item_id)
                || result.id == session_id
                || result
                    .playback_source
                    .as_ref()
                    .is_some_and(|source| source.item_id == session_id);
            if !matches_cache {
                continue;
            }
            result.state = TaskState::Failed.into();
            result.library_item_id.clear();
            result.playback_source = None;
            result.problem = Some(TaskProblem {
                category: TaskProblemCategory::NotFound.into(),
                code: "cache.playback_deleted".to_owned(),
                message: message.to_owned(),
                retryable: false,
            });
            for artifact in &mut result.artifacts {
                if artifact.kind() != TaskArtifactKind::Media
                    || artifact.state() != TaskArtifactState::Available
                {
                    continue;
                }
                artifact.state = TaskArtifactState::Deleted.into();
                artifact.resource = None;
                artifact.problem = Some(TaskProblem {
                    category: TaskProblemCategory::NotFound.into(),
                    code: "cache.resource_deleted".to_owned(),
                    message: message.to_owned(),
                    retryable: false,
                });
            }
            changed = true;
        }
        if !changed {
            return Vec::new();
        }

        let referenced_ids = self
            .results
            .iter()
            .flat_map(|result| &result.artifacts)
            .filter_map(|artifact| artifact.resource.as_ref())
            .map(|resource| resource.id.as_str())
            .collect::<HashSet<_>>();
        let mut retired_ids = Vec::new();
        self.resources.retain(|resource| {
            let retained = referenced_ids.contains(resource.resource.id.as_str());
            if !retained {
                retired_ids.push(resource.resource.id.clone());
            }
            retained
        });
        self.primary_result_id = inferred_primary_result_id(&self.results);
        self.revision = self.revision.saturating_add(1).max(1);
        self.snapshot_id = new_snapshot_id();
        retired_ids
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TaskOutputValidationError {
    message: String,
}

impl TaskOutputValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TaskOutputValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TaskOutputValidationError {}

pub(crate) fn legacy_task_results(task: &Task) -> Vec<TaskResult> {
    if task.result_items.is_empty() {
        return vec![TaskResult {
            id: task.id.clone(),
            state: task.state,
            title: task.title.clone(),
            subtitle: String::new(),
            progress: Some(TaskResultProgress {
                fraction: normalized_progress(task.progress),
                completed_bytes: task.downloaded_bytes,
                total_bytes: task.total_bytes,
                total_bytes_known: task.total_bytes > 0,
                phase: String::new(),
                message: String::new(),
            }),
            problem: legacy_problem(task.state()),
            library_item_id: task.library_item_id.clone(),
            playback_source: task.playback_source.clone(),
            artifacts: Vec::new(),
            created_at: task.created_at,
            updated_at: task.updated_at,
        }];
    }

    task.result_items
        .iter()
        .map(|item| TaskResult {
            id: item.id.clone(),
            state: item.state,
            title: item.title.clone(),
            subtitle: item.subtitle.clone(),
            progress: Some(TaskResultProgress {
                fraction: terminal_progress(item.state()),
                completed_bytes: 0,
                total_bytes: 0,
                total_bytes_known: false,
                phase: String::new(),
                message: String::new(),
            }),
            problem: legacy_problem(item.state()),
            library_item_id: item.library_item_id.clone(),
            playback_source: item.playback_source.clone(),
            artifacts: Vec::new(),
            created_at: task.created_at,
            updated_at: task.updated_at,
        })
        .collect()
}

fn legacy_primary_result_id(task: &Task, results: &[TaskResult]) -> String {
    if let Some(primary_session_id) = task
        .playback_session
        .as_ref()
        .map(|session| session.id.as_str())
        .filter(|id| !id.is_empty())
        && let Some(result) = task.result_items.iter().find(|item| {
            item.id == primary_session_id
                || item
                    .playback_session
                    .as_ref()
                    .is_some_and(|session| session.id == primary_session_id)
        })
    {
        return result.id.clone();
    }
    if !task.library_item_id.is_empty()
        && let Some(result) = results
            .iter()
            .find(|result| result.library_item_id == task.library_item_id)
    {
        return result.id.clone();
    }
    inferred_primary_result_id(results)
}

fn inferred_primary_result_id(results: &[TaskResult]) -> String {
    results
        .iter()
        .find(|result| matches!(result.state(), TaskState::Succeeded | TaskState::Completed))
        .or_else(|| results.first())
        .map(|result| result.id.clone())
        .unwrap_or_default()
}

fn validate_result_ids(results: &[TaskResult]) -> Result<(), TaskOutputValidationError> {
    let mut ids = HashSet::new();
    for result in results {
        if result.id.trim().is_empty() {
            return Err(TaskOutputValidationError::new(
                "task result id must not be empty",
            ));
        }
        if !ids.insert(result.id.as_str()) {
            return Err(TaskOutputValidationError::new(format!(
                "duplicate task result id: {}",
                result.id
            )));
        }
        if result.state() == TaskState::Unspecified {
            return Err(TaskOutputValidationError::new(format!(
                "task result has an unspecified or unknown state: {}",
                result.id
            )));
        }
        if let Some(progress) = result.progress.as_ref()
            && (!progress.fraction.is_finite()
                || !(0.0..=1.0).contains(&progress.fraction)
                || progress.completed_bytes < 0
                || progress.total_bytes < 0)
        {
            return Err(TaskOutputValidationError::new(format!(
                "task result has invalid progress: {}",
                result.id
            )));
        }
        validate_problem(result.problem.as_ref(), &result.id)?;
    }
    Ok(())
}

fn validate_collection_sizes(
    results: &[TaskResult],
    resources: &[TaskResourceRecord],
) -> Result<(), TaskOutputValidationError> {
    if results.len() > MAX_TASK_RESULTS {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_RESULTS} results"
        )));
    }
    if resources.len() > MAX_TASK_RESOURCES {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_RESOURCES} resources"
        )));
    }
    Ok(())
}

fn validate_and_bind_resources(
    results: &mut [TaskResult],
    resources: &[TaskResourceRecord],
) -> Result<(), TaskOutputValidationError> {
    let mut resources_by_id = HashMap::new();
    for resource in resources {
        validate_resource_id(&resource.resource.id)?;
        if resources_by_id
            .insert(resource.resource.id.as_str(), &resource.resource)
            .is_some()
        {
            return Err(TaskOutputValidationError::new(format!(
                "duplicate task resource id: {}",
                resource.resource.id
            )));
        }
    }

    let mut referenced_ids = HashSet::new();
    for result in results {
        let mut artifact_ids = HashSet::new();
        for artifact in &mut result.artifacts {
            if artifact.id.trim().is_empty() {
                return Err(TaskOutputValidationError::new(
                    "task artifact id must not be empty",
                ));
            }
            if !artifact_ids.insert(artifact.id.as_str()) {
                return Err(TaskOutputValidationError::new(format!(
                    "duplicate task artifact id in result {}: {}",
                    result.id, artifact.id
                )));
            }
            if artifact.kind() == TaskArtifactKind::Unspecified
                || artifact.state() == TaskArtifactState::Unspecified
            {
                return Err(TaskOutputValidationError::new(format!(
                    "task artifact has an unspecified or unknown kind/state: {}",
                    artifact.id
                )));
            }
            validate_problem(artifact.problem.as_ref(), &artifact.id)?;
            let Some(reference) = artifact.resource.as_ref() else {
                if artifact.state() == TaskArtifactState::Available {
                    return Err(TaskOutputValidationError::new(format!(
                        "available task artifact has no resource: {}",
                        artifact.id
                    )));
                }
                continue;
            };
            let canonical_id = reference.id.to_ascii_lowercase();
            let Some(canonical) = resources_by_id.get(canonical_id.as_str()) else {
                return Err(TaskOutputValidationError::new(format!(
                    "task artifact references unknown resource: {}",
                    reference.id
                )));
            };
            referenced_ids.insert(canonical_id);
            artifact.resource = Some((*canonical).clone());
        }
    }

    if let Some(unreferenced) = resources
        .iter()
        .find(|resource| !referenced_ids.contains(&resource.resource.id))
    {
        return Err(TaskOutputValidationError::new(format!(
            "task resource is not referenced by an artifact: {}",
            unreferenced.resource.id
        )));
    }
    Ok(())
}

fn validate_resource_representations(
    previous: Option<&TaskOutputRecord>,
    resources: &[TaskResourceRecord],
) -> Result<(), TaskOutputValidationError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let previous_by_id = previous
        .resources
        .iter()
        .map(|resource| (resource.resource.id.as_str(), resource))
        .collect::<HashMap<_, _>>();
    for resource in resources {
        if let Some(existing) = previous_by_id.get(resource.resource.id.as_str())
            && *existing != resource
        {
            return Err(TaskOutputValidationError::new(format!(
                "task resource id cannot be reused for a different representation: {}",
                resource.resource.id
            )));
        }
    }
    Ok(())
}

fn validate_problem(
    problem: Option<&TaskProblem>,
    owner_id: &str,
) -> Result<(), TaskOutputValidationError> {
    if problem.is_some_and(|problem| problem.category() == TaskProblemCategory::Unspecified) {
        return Err(TaskOutputValidationError::new(format!(
            "task problem has an unspecified or unknown category: {owner_id}"
        )));
    }
    Ok(())
}

fn validate_resource_id(id: &str) -> Result<(), TaskOutputValidationError> {
    if !resource_id_is_valid(id) {
        return Err(TaskOutputValidationError::new(
            "task resource id must use 1-200 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

pub(crate) fn resource_id_is_canonical(id: &str) -> bool {
    resource_id_is_valid(id) && !id.bytes().any(|byte| byte.is_ascii_uppercase())
}

fn resource_id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_RESOURCE_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_snapshot_id(id: &str) -> Result<(), TaskOutputValidationError> {
    if id.is_empty()
        || id.len() > MAX_RESOURCE_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(TaskOutputValidationError::new(
            "task output snapshot id must use 1-200 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

fn resource_uri(id: &str) -> String {
    format!("/resources/{id}")
}

fn new_snapshot_id() -> String {
    format!("task-output-{}", Uuid::new_v4().simple())
}

fn normalized_progress(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn terminal_progress(state: TaskState) -> f64 {
    if matches!(state, TaskState::Succeeded | TaskState::Completed) {
        1.0
    } else {
        0.0
    }
}

fn legacy_problem(state: TaskState) -> Option<TaskProblem> {
    match state {
        TaskState::Failed => Some(TaskProblem {
            category: TaskProblemCategory::Upstream.into(),
            code: "bilibili.legacy_failure".to_owned(),
            message: "Bilibili operation failed.".to_owned(),
            retryable: true,
        }),
        TaskState::Cancelled => Some(TaskProblem {
            category: TaskProblemCategory::Cancelled.into(),
            code: "task.cancelled".to_owned(),
            message: "Task was cancelled.".to_owned(),
            retryable: false,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tvos_net_player::v1::{TaskArtifact, TaskArtifactKind, TaskKind};

    #[test]
    fn summary_counts_results_and_available_artifacts() {
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "subtitle-one".to_owned(),
            content_type: "text/vtt".to_owned(),
            size_bytes: 12,
            size_known: true,
            supports_byte_ranges: true,
            ..Default::default()
        })
        .unwrap();
        let output = TaskOutputRecord::replace(
            None,
            vec![TaskResult {
                id: "result-one".to_owned(),
                state: TaskState::Completed.into(),
                artifacts: vec![TaskArtifact {
                    id: "artifact-one".to_owned(),
                    kind: TaskArtifactKind::Subtitle.into(),
                    state: TaskArtifactState::Available.into(),
                    resource: Some(resource.resource.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            vec![resource],
        )
        .unwrap();

        assert_eq!("/resources/subtitle-one", output.resources[0].resource.uri);
        assert_eq!(1, output.summary().result_count);
        assert_eq!(1, output.summary().successful_result_count);
        assert_eq!(1, output.summary().available_artifact_count);
        assert_eq!("result-one", output.summary().primary_result_id);
    }

    #[test]
    fn resource_paths_are_derived_from_opaque_ids() {
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "subtitle-one".to_owned(),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(
            ".tvos-net-player/resources/subtitle-one/body",
            resource.relative_path()
        );
    }

    #[test]
    fn resource_ids_are_canonicalized_before_filesystem_mapping() {
        let upper = TaskResourceRecord::new(CacheResourceRef {
            id: "Cover-One".to_owned(),
            ..Default::default()
        })
        .unwrap();
        let lower = TaskResourceRecord::new(CacheResourceRef {
            id: "cover-one".to_owned(),
            ..Default::default()
        })
        .unwrap();

        assert_eq!("cover-one", upper.resource.id);
        assert_eq!(lower.resource.id, upper.resource.id);
        let error = TaskOutputRecord::replace(None, Vec::new(), vec![upper, lower])
            .expect_err("case-distinct resource ids must collide");
        assert!(error.to_string().contains("duplicate task resource id"));
    }

    #[test]
    fn resource_ids_cannot_change_representation_within_a_task() {
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "subtitle-one".to_owned(),
            content_type: "text/vtt".to_owned(),
            etag: "v1".to_owned(),
            ..Default::default()
        })
        .unwrap();
        let result = TaskResult {
            id: "result-one".to_owned(),
            state: TaskState::Completed.into(),
            artifacts: vec![TaskArtifact {
                id: "artifact-one".to_owned(),
                kind: TaskArtifactKind::Subtitle.into(),
                state: TaskArtifactState::Available.into(),
                resource: Some(resource.resource.clone()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let previous =
            TaskOutputRecord::replace(None, vec![result.clone()], vec![resource.clone()]).unwrap();
        let changed = TaskResourceRecord::new(CacheResourceRef {
            id: "subtitle-one".to_owned(),
            content_type: "text/vtt".to_owned(),
            etag: "v2".to_owned(),
            ..Default::default()
        })
        .unwrap();

        let error = TaskOutputRecord::replace(Some(&previous), vec![result], vec![changed])
            .expect_err("a resource id must identify one immutable representation");
        assert!(error.to_string().contains("different representation"));
    }

    #[test]
    fn legacy_task_always_exposes_one_primary_result() {
        let task = Task {
            id: "task-one".to_owned(),
            kind: TaskKind::BilibiliDownload.into(),
            state: TaskState::Running.into(),
            progress: 0.25,
            downloaded_bytes: 25,
            total_bytes: 100,
            ..Default::default()
        };

        let output = TaskOutputRecord::from_legacy_task(&task);

        assert_eq!(1, output.results.len());
        assert_eq!("task-one", output.results[0].id);
        assert_eq!(0.25, output.results[0].progress.as_ref().unwrap().fraction);
        assert_eq!(1, output.summary().revision);
    }

    #[test]
    fn removed_task_tombstone_advances_the_previous_revision() {
        let task = Task {
            id: "task-one".to_owned(),
            kind: TaskKind::BilibiliDownload.into(),
            state: TaskState::Completed.into(),
            ..Default::default()
        };
        let mut previous = TaskOutputRecord::from_legacy_task(&task);
        previous.revision = 17;
        let failed = Task {
            state: TaskState::Failed.into(),
            ..task
        };

        let tombstone = TaskOutputRecord::removed_task_tombstone(&failed, Some(&previous));

        assert_eq!(18, tombstone.revision);
        assert_ne!(previous.snapshot_id, tombstone.snapshot_id);
        assert_eq!(TaskState::Failed, tombstone.results[0].state());
    }
}
