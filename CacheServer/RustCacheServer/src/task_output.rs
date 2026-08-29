use std::collections::{HashMap, HashSet};

use crate::generated::tvos_net_player::v1::{
    BilibiliContentIdentity, BilibiliContentKind, BilibiliPlaybackSession, BilibiliPlaybackVariant,
    BilibiliTaskResultDetails, CacheResourceRef, Task, TaskArtifactKind, TaskArtifactState,
    TaskOutputSummary, TaskProblem, TaskProblemCategory, TaskResult, TaskResultProgress,
    TaskResultProviderDetails, TaskResultSubject, TaskState, task_result_provider_details,
};
use http::HeaderValue;
use prost::Message;
use prost_types::Timestamp;
use uuid::Uuid;

const MAX_RESOURCE_ID_BYTES: usize = 200;
pub(crate) const MAX_TASK_RESULTS: usize = 10_000;
pub(crate) const MAX_TASK_RESOURCES: usize = 50_000;
pub(crate) const MAX_REGISTERED_TASK_RESOURCES: usize = 50_000;
pub(crate) const MAX_TASK_ARTIFACTS: usize = 50_000;
pub(crate) const MAX_TASK_RESULT_ENCODED_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TASK_RESOURCE_BASE_URI_BYTES: usize = 2 * 1024;
const MAX_TASK_RESOURCE_URI_PROJECTION_OVERHEAD_BYTES: usize = 16;
const MAX_TASK_CLIENT_REDACTION_MESSAGE_BYTES: usize = 256;
const MAX_TASK_CLIENT_REDACTION_OVERHEAD_BYTES: usize = 16;
const MAX_TASK_RESOURCE_ENCODED_BYTES: usize = 64 * 1024;
const MAX_TASK_OUTPUT_STRING_BYTES: usize = 8 * 1024 * 1024;
const MAX_TASK_OUTPUT_ENCODED_BYTES: usize = 32 * 1024 * 1024;
const MAX_TASK_RESULT_PROVIDER_VARIANTS: usize = 10_000;
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
        results: Vec<TaskResult>,
        resources: Vec<TaskResourceRecord>,
    ) -> Result<Self, TaskOutputValidationError> {
        Self::replace_with_primary_result(previous, results, resources, None)
    }

    pub(crate) fn replace_with_primary_result(
        previous: Option<&Self>,
        mut results: Vec<TaskResult>,
        resources: Vec<TaskResourceRecord>,
        preferred_primary_result_id: Option<&str>,
    ) -> Result<Self, TaskOutputValidationError> {
        validate_collection_sizes(&results, &resources)?;
        validate_and_bind_resources(&mut results, &resources)?;
        validate_collection_sizes(&results, &resources)?;
        validate_result_ids(&results)?;
        validate_resource_representations(previous, &resources)?;

        let primary_result_id = match preferred_primary_result_id {
            Some(primary_result_id)
                if results.iter().any(|result| result.id == primary_result_id) =>
            {
                primary_result_id.to_owned()
            }
            Some(_) => {
                return Err(TaskOutputValidationError::new(
                    "preferred task output primary result is missing",
                ));
            }
            None => {
                let has_successful_result = results.iter().any(|result| {
                    matches!(result.state(), TaskState::Succeeded | TaskState::Completed)
                });
                previous
                    .map(|output| output.primary_result_id.as_str())
                    .filter(|primary_result_id| {
                        results.iter().any(|result| {
                            result.id.as_str() == *primary_result_id
                                && (!has_successful_result
                                    || matches!(
                                        result.state(),
                                        TaskState::Succeeded | TaskState::Completed
                                    ))
                        })
                    })
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| inferred_primary_result_id(&results))
            }
        };
        let unchanged = previous.is_some_and(|previous| {
            previous.results == results
                && previous.resources == resources
                && previous.primary_result_id == primary_result_id
                && !previous.legacy_managed
        });
        let revision = match previous {
            Some(previous) if unchanged => previous.revision.max(1),
            Some(previous) => previous.revision.saturating_add(1).max(1),
            None => 1,
        };
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
        validate_collection_sizes(&results, &resources)?;
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

    pub(crate) fn reconcile_legacy_task(
        &mut self,
        task: &Task,
    ) -> Result<bool, TaskOutputValidationError> {
        if !self.legacy_managed {
            return Ok(false);
        }
        let mut results = legacy_task_results(task);
        validate_collection_sizes(&results, &[])?;
        validate_and_bind_resources(&mut results, &[])?;
        validate_collection_sizes(&results, &[])?;
        validate_result_ids(&results)?;
        let primary_result_id = legacy_primary_result_id(task, &results);
        if self.results == results && self.primary_result_id == primary_result_id {
            return Ok(false);
        }
        self.results = results;
        self.primary_result_id = primary_result_id;
        self.revision = self.revision.saturating_add(1).max(1);
        self.snapshot_id = new_snapshot_id();
        Ok(true)
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

    pub(crate) fn encoded_bytes(&self) -> usize {
        task_output_encoded_bytes(&self.results, &self.resources)
            .saturating_add(self.snapshot_id.len())
            .saturating_add(self.primary_result_id.len())
    }

    pub(crate) fn has_expired_resources_except(
        &self,
        now: &Timestamp,
        excluded_resource_ids: &HashSet<String>,
    ) -> bool {
        self.resources.iter().any(|resource| {
            !excluded_resource_ids.contains(&resource.resource.id)
                && resource
                    .resource
                    .expires_at
                    .as_ref()
                    .is_some_and(|expires_at| timestamp_at_or_before(expires_at, now))
        })
    }

    pub(crate) fn retire_expired_resources_except(
        &mut self,
        now: &Timestamp,
        excluded_resource_ids: &HashSet<String>,
    ) -> Vec<String> {
        let expired_ids = self
            .resources
            .iter()
            .filter(|resource| {
                !excluded_resource_ids.contains(&resource.resource.id)
                    && resource
                        .resource
                        .expires_at
                        .as_ref()
                        .is_some_and(|expires_at| timestamp_at_or_before(expires_at, now))
            })
            .map(|resource| resource.resource.id.clone())
            .collect::<HashSet<_>>();
        if expired_ids.is_empty() {
            return Vec::new();
        }

        let mut retired_ids = Vec::new();
        self.resources.retain(|resource| {
            if expired_ids.contains(&resource.resource.id) {
                retired_ids.push(resource.resource.id.clone());
                false
            } else {
                true
            }
        });
        for result in &mut self.results {
            for artifact in &mut result.artifacts {
                let is_expired = artifact
                    .resource
                    .as_ref()
                    .is_some_and(|resource| expired_ids.contains(&resource.id));
                if !is_expired {
                    continue;
                }
                artifact.resource = None;
                if artifact.state() == TaskArtifactState::Available {
                    artifact.state = TaskArtifactState::Unavailable.into();
                    artifact.problem = Some(TaskProblem {
                        category: TaskProblemCategory::NotFound.into(),
                        code: "cache.resource_expired".to_owned(),
                        message: "Task resource expired.".to_owned(),
                        retryable: false,
                    });
                }
            }
        }
        self.revision = self.revision.saturating_add(1).max(1);
        self.snapshot_id = new_snapshot_id();
        retired_ids
    }

    pub(crate) fn mark_playback_cache_deleted(
        &mut self,
        session_id: &str,
        library_item_id: &str,
        message: &str,
    ) -> Result<Vec<String>, TaskOutputValidationError> {
        if self.legacy_managed {
            return Ok(Vec::new());
        }
        let mut updated = self.clone();
        let mut changed = false;
        for result in &mut updated.results {
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
                artifact.library_item_id.clear();
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
            return Ok(Vec::new());
        }

        let referenced_ids = updated
            .results
            .iter()
            .flat_map(|result| &result.artifacts)
            .filter_map(|artifact| artifact.resource.as_ref())
            .map(|resource| resource.id.as_str())
            .collect::<HashSet<_>>();
        let mut retired_ids = Vec::new();
        updated.resources.retain(|resource| {
            let retained = referenced_ids.contains(resource.resource.id.as_str());
            if !retained {
                retired_ids.push(resource.resource.id.clone());
            }
            retained
        });
        if !updated
            .results
            .iter()
            .any(|result| result.id == updated.primary_result_id)
        {
            updated.primary_result_id = inferred_primary_result_id(&updated.results);
        }
        validate_collection_sizes(&updated.results, &updated.resources)?;
        validate_and_bind_resources(&mut updated.results, &updated.resources)?;
        validate_collection_sizes(&updated.results, &updated.resources)?;
        validate_result_ids(&updated.results)?;
        updated.revision = updated.revision.saturating_add(1).max(1);
        updated.snapshot_id = new_snapshot_id();
        *self = updated;
        Ok(retired_ids)
    }

    pub(crate) fn mark_library_item_deleted(
        &mut self,
        library_item_id: &str,
        message: &str,
    ) -> Result<Option<Vec<String>>, TaskOutputValidationError> {
        if self.legacy_managed {
            return Ok(None);
        }
        let mut updated = self.clone();
        let mut changed = false;
        for result in &mut updated.results {
            let result_matches = result.library_item_id == library_item_id
                || result
                    .playback_source
                    .as_ref()
                    .is_some_and(|source| source.item_id == library_item_id);
            let mut artifact_matches = false;
            for artifact in &mut result.artifacts {
                if artifact.library_item_id != library_item_id {
                    continue;
                }
                artifact_matches = true;
                artifact.state = TaskArtifactState::Deleted.into();
                artifact.resource = None;
                artifact.library_item_id.clear();
                artifact.problem = Some(TaskProblem {
                    category: TaskProblemCategory::NotFound.into(),
                    code: "cache.library_item_deleted".to_owned(),
                    message: message.to_owned(),
                    retryable: false,
                });
            }
            if !result_matches && !artifact_matches {
                continue;
            }
            result.state = TaskState::Failed.into();
            if result.library_item_id == library_item_id {
                result.library_item_id.clear();
            }
            if result
                .playback_source
                .as_ref()
                .is_some_and(|source| source.item_id == library_item_id)
            {
                result.playback_source = None;
            }
            result.problem = Some(TaskProblem {
                category: TaskProblemCategory::NotFound.into(),
                code: "cache.library_item_deleted".to_owned(),
                message: message.to_owned(),
                retryable: false,
            });
            if let Some(progress) = result.progress.as_mut() {
                progress.phase = "deleted".to_owned();
                progress.message = message.to_owned();
            }
            changed = true;
        }
        if !changed {
            return Ok(None);
        }

        let referenced_ids = updated
            .results
            .iter()
            .flat_map(|result| &result.artifacts)
            .filter_map(|artifact| artifact.resource.as_ref())
            .map(|resource| resource.id.as_str())
            .collect::<HashSet<_>>();
        let mut retired_ids = Vec::new();
        updated.resources.retain(|resource| {
            let retained = referenced_ids.contains(resource.resource.id.as_str());
            if !retained {
                retired_ids.push(resource.resource.id.clone());
            }
            retained
        });
        updated.primary_result_id = inferred_primary_result_id(&updated.results);
        validate_collection_sizes(&updated.results, &updated.resources)?;
        validate_and_bind_resources(&mut updated.results, &updated.resources)?;
        validate_collection_sizes(&updated.results, &updated.resources)?;
        validate_result_ids(&updated.results)?;
        updated.revision = updated.revision.saturating_add(1).max(1);
        updated.snapshot_id = new_snapshot_id();
        *self = updated;
        Ok(Some(retired_ids))
    }
}

fn timestamp_at_or_before(left: &Timestamp, right: &Timestamp) -> bool {
    (left.seconds, left.nanos) <= (right.seconds, right.nanos)
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
            subject: None,
            provider_details: None,
        }];
    }

    task.result_items
        .iter()
        .map(|item| {
            let subject = (!item.source_kind.trim().is_empty()
                && item.source_kind == item.source_kind.trim()
                && !item.content_id.trim().is_empty()
                && item.content_id == item.content_id.trim())
            .then(|| TaskResultSubject {
                provider: "bilibili".to_owned(),
                kind: item.source_kind.clone(),
                id: item.content_id.clone(),
                index: item.index,
            });
            let provider_details = (subject.is_some()
                && (item.identity.is_some() || item.playback_session.is_some()))
            .then(|| TaskResultProviderDetails {
                details: Some(task_result_provider_details::Details::Bilibili(
                    BilibiliTaskResultDetails {
                        identity: item.identity.clone(),
                        playback_session: item.playback_session.clone(),
                    },
                )),
            });
            TaskResult {
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
                subject,
                provider_details,
            }
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
        validate_result_subject_and_provider_details(result)?;
    }
    Ok(())
}

fn validate_result_subject_and_provider_details(
    result: &TaskResult,
) -> Result<(), TaskOutputValidationError> {
    if let Some(subject) = result.subject.as_ref()
        && [
            subject.provider.as_str(),
            subject.kind.as_str(),
            subject.id.as_str(),
        ]
        .into_iter()
        .any(|value| value.trim().is_empty() || value != value.trim())
    {
        return Err(TaskOutputValidationError::new(format!(
            "task result has an invalid subject: {}",
            result.id
        )));
    }

    let Some(provider_details) = result.provider_details.as_ref() else {
        return Ok(());
    };
    let Some(details) = provider_details.details.as_ref() else {
        return Err(TaskOutputValidationError::new(format!(
            "task result has empty provider details: {}",
            result.id
        )));
    };
    let Some(subject) = result.subject.as_ref() else {
        return Err(TaskOutputValidationError::new(format!(
            "task result provider details require a subject: {}",
            result.id
        )));
    };

    match details {
        task_result_provider_details::Details::Bilibili(details) => {
            if subject.provider != "bilibili" {
                return Err(TaskOutputValidationError::new(format!(
                    "Bilibili task result details require a Bilibili subject: {}",
                    result.id
                )));
            }
            if details.identity.is_none() && details.playback_session.is_none() {
                return Err(TaskOutputValidationError::new(format!(
                    "Bilibili task result details must not be empty: {}",
                    result.id
                )));
            }
            if let Some(identity) = details.identity.as_ref() {
                validate_bilibili_result_identity(identity, &subject.id, &result.id)?;
            }
            if let Some(session) = details.playback_session.as_ref()
                && session.variants.len() > MAX_TASK_RESULT_PROVIDER_VARIANTS
            {
                return Err(TaskOutputValidationError::new(format!(
                    "Bilibili task result playback session cannot exceed {MAX_TASK_RESULT_PROVIDER_VARIANTS} variants: {}",
                    result.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_bilibili_result_identity(
    identity: &BilibiliContentIdentity,
    subject_id: &str,
    result_id: &str,
) -> Result<(), TaskOutputValidationError> {
    let bvid = identity.bvid.as_str();
    let aid_or_bvid = identity.aid > 0 || !bvid.is_empty();
    let complete = bvid == bvid.trim()
        && match identity.kind() {
            BilibiliContentKind::VideoPage | BilibiliContentKind::CollectionItem => {
                identity.cid > 0 && aid_or_bvid && identity.epid == 0
            }
            BilibiliContentKind::SeasonEpisode => identity.epid > 0,
            BilibiliContentKind::Unspecified => false,
        };
    let matches_subject = match identity.kind() {
        BilibiliContentKind::VideoPage => subject_id == identity.cid.to_string(),
        BilibiliContentKind::SeasonEpisode => subject_id == identity.epid.to_string(),
        BilibiliContentKind::CollectionItem => {
            (!bvid.is_empty() && subject_id == bvid)
                || (identity.aid > 0 && subject_id == format!("av{}", identity.aid))
        }
        BilibiliContentKind::Unspecified => false,
    };
    if !complete || !matches_subject {
        return Err(TaskOutputValidationError::new(format!(
            "Bilibili task result has an invalid identity: {result_id}"
        )));
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
    let artifact_count = results
        .iter()
        .try_fold(0_usize, |total, result| {
            total.checked_add(result.artifacts.len())
        })
        .unwrap_or(usize::MAX);
    if artifact_count > MAX_TASK_ARTIFACTS {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_ARTIFACTS} artifacts"
        )));
    }
    if let Some(result) = results
        .iter()
        .find(|result| result.encoded_len() > MAX_TASK_RESULT_ENCODED_BYTES)
    {
        return Err(TaskOutputValidationError::new(format!(
            "task result cannot exceed {MAX_TASK_RESULT_ENCODED_BYTES} encoded bytes: {}",
            result.id
        )));
    }
    if let Some(result) = results
        .iter()
        .find(|result| projected_task_result_encoded_bytes(result) > MAX_TASK_RESULT_ENCODED_BYTES)
    {
        return Err(TaskOutputValidationError::new(format!(
            "task result cannot exceed {MAX_TASK_RESULT_ENCODED_BYTES} encoded bytes after client projection: {}",
            result.id
        )));
    }
    if let Some(resource) = resources
        .iter()
        .find(|resource| resource.resource.encoded_len() > MAX_TASK_RESOURCE_ENCODED_BYTES)
    {
        return Err(TaskOutputValidationError::new(format!(
            "task resource cannot exceed {MAX_TASK_RESOURCE_ENCODED_BYTES} encoded bytes: {}",
            resource.resource.id
        )));
    }
    let string_bytes = task_output_string_bytes(results, resources);
    if string_bytes > MAX_TASK_OUTPUT_STRING_BYTES {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_OUTPUT_STRING_BYTES} string bytes"
        )));
    }
    let encoded_bytes = task_output_encoded_bytes(results, resources);
    if encoded_bytes > MAX_TASK_OUTPUT_ENCODED_BYTES {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_OUTPUT_ENCODED_BYTES} encoded bytes"
        )));
    }
    let projected_encoded_bytes = task_output_projected_encoded_bytes(results, resources);
    if projected_encoded_bytes > MAX_TASK_OUTPUT_ENCODED_BYTES {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_OUTPUT_ENCODED_BYTES} encoded bytes after client projection"
        )));
    }
    Ok(())
}

pub(crate) fn projected_task_result_encoded_bytes(result: &TaskResult) -> usize {
    let projected_resource_bytes = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.resource.is_some())
        .count()
        .saturating_mul(
            MAX_TASK_RESOURCE_BASE_URI_BYTES
                .saturating_add(MAX_TASK_RESOURCE_URI_PROJECTION_OVERHEAD_BYTES),
        );
    let projected_redaction_bytes = projected_task_result_redaction_bytes(result);
    result
        .encoded_len()
        .saturating_add(projected_resource_bytes)
        .saturating_add(projected_redaction_bytes)
}

fn projected_task_result_redaction_bytes(result: &TaskResult) -> usize {
    result
        .problem
        .iter()
        .map(|problem| projected_redaction_message_growth(&problem.message))
        .chain(
            result
                .progress
                .iter()
                .map(|progress| projected_redaction_message_growth(&progress.message)),
        )
        .chain(result.artifacts.iter().filter_map(|artifact| {
            artifact
                .problem
                .as_ref()
                .map(|problem| projected_redaction_message_growth(&problem.message))
        }))
        .fold(0_usize, usize::saturating_add)
}

fn projected_redaction_message_growth(message: &str) -> usize {
    MAX_TASK_CLIENT_REDACTION_MESSAGE_BYTES
        .saturating_sub(message.len())
        .saturating_add(MAX_TASK_CLIENT_REDACTION_OVERHEAD_BYTES)
}

fn task_output_projected_encoded_bytes(
    results: &[TaskResult],
    resources: &[TaskResourceRecord],
) -> usize {
    results
        .iter()
        .map(projected_task_result_encoded_bytes)
        .chain(
            resources
                .iter()
                .map(|resource| resource.resource.encoded_len()),
        )
        .fold(0_usize, usize::saturating_add)
}

fn task_output_encoded_bytes(results: &[TaskResult], resources: &[TaskResourceRecord]) -> usize {
    results
        .iter()
        .map(Message::encoded_len)
        .chain(
            resources
                .iter()
                .map(|resource| resource.resource.encoded_len()),
        )
        .fold(0_usize, usize::saturating_add)
}

fn task_output_string_bytes(results: &[TaskResult], resources: &[TaskResourceRecord]) -> usize {
    results
        .iter()
        .map(task_result_string_bytes)
        .chain(
            resources
                .iter()
                .map(|resource| resource_string_bytes(&resource.resource)),
        )
        .fold(0_usize, usize::saturating_add)
}

fn task_result_string_bytes(result: &TaskResult) -> usize {
    [
        result.id.len(),
        result.title.len(),
        result.subtitle.len(),
        result.library_item_id.len(),
    ]
    .into_iter()
    .chain(
        result
            .progress
            .iter()
            .flat_map(|progress| [progress.phase.len(), progress.message.len()]),
    )
    .chain(result.problem.iter().map(problem_string_bytes))
    .chain(
        result
            .playback_source
            .iter()
            .map(playback_source_string_bytes),
    )
    .chain(result.subject.iter().map(|subject| {
        subject
            .provider
            .len()
            .saturating_add(subject.kind.len())
            .saturating_add(subject.id.len())
    }))
    .chain(
        result
            .provider_details
            .iter()
            .map(provider_details_string_bytes),
    )
    .chain(result.artifacts.iter().map(artifact_string_bytes))
    .fold(0_usize, usize::saturating_add)
}

fn provider_details_string_bytes(details: &TaskResultProviderDetails) -> usize {
    match details.details.as_ref() {
        Some(task_result_provider_details::Details::Bilibili(details)) => details
            .identity
            .iter()
            .map(|identity| identity.bvid.len())
            .chain(
                details
                    .playback_session
                    .iter()
                    .map(bilibili_playback_session_string_bytes),
            )
            .fold(0_usize, usize::saturating_add),
        None => 0,
    }
}

fn bilibili_playback_session_string_bytes(session: &BilibiliPlaybackSession) -> usize {
    [
        session.id.len(),
        session.title.len(),
        session.content_id.len(),
        session.selected_variant_id.len(),
    ]
    .into_iter()
    .chain(
        session
            .selected_variant
            .iter()
            .map(bilibili_playback_variant_string_bytes),
    )
    .chain(
        session
            .variants
            .iter()
            .map(bilibili_playback_variant_string_bytes),
    )
    .chain(session.transcoding_plan.iter().map(|plan| {
        [
            plan.profile_id.len(),
            plan.reason.len(),
            plan.source_variant_id.len(),
            plan.target_container.len(),
            plan.target_video_codec.len(),
            plan.target_audio_codec.len(),
        ]
        .into_iter()
        .fold(0_usize, usize::saturating_add)
    }))
    .fold(0_usize, usize::saturating_add)
}

fn bilibili_playback_variant_string_bytes(variant: &BilibiliPlaybackVariant) -> usize {
    [
        variant.id.len(),
        variant.label.len(),
        variant.source_kind.len(),
        variant.container.len(),
        variant.video_codec.len(),
        variant.audio_codec.len(),
    ]
    .into_iter()
    .fold(0_usize, usize::saturating_add)
}

fn artifact_string_bytes(artifact: &crate::generated::tvos_net_player::v1::TaskArtifact) -> usize {
    [
        artifact.id.len(),
        artifact.title.len(),
        artifact.format.len(),
        artifact.language_tag.len(),
        artifact.library_item_id.len(),
    ]
    .into_iter()
    .chain(artifact.resource.iter().map(resource_string_bytes))
    .chain(artifact.problem.iter().map(problem_string_bytes))
    .fold(0_usize, usize::saturating_add)
}

fn problem_string_bytes(problem: &TaskProblem) -> usize {
    problem.code.len().saturating_add(problem.message.len())
}

fn playback_source_string_bytes(
    source: &crate::generated::tvos_net_player::v1::PlaybackSource,
) -> usize {
    source
        .item_id
        .len()
        .saturating_add(source.variant_id.len())
        .saturating_add(source.uri.len())
}

fn resource_string_bytes(resource: &CacheResourceRef) -> usize {
    resource
        .id
        .len()
        .saturating_add(resource.uri.len())
        .saturating_add(resource.content_type.len())
        .saturating_add(resource.etag.len())
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
    validate_bound_resource_expansion(results, resources, &resources_by_id)?;

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
            let has_resource = artifact.resource.is_some();
            let has_library_item = !artifact.library_item_id.is_empty();
            if has_library_item
                && (artifact.library_item_id.trim().is_empty()
                    || artifact.library_item_id != artifact.library_item_id.trim())
            {
                return Err(TaskOutputValidationError::new(format!(
                    "task artifact has an invalid library item id: {}",
                    artifact.id
                )));
            }
            if has_resource && has_library_item {
                return Err(TaskOutputValidationError::new(format!(
                    "task artifact cannot have both resource and library item backings: {}",
                    artifact.id
                )));
            }
            if has_library_item && artifact.kind() != TaskArtifactKind::Media {
                return Err(TaskOutputValidationError::new(format!(
                    "only media task artifacts can use a library item backing: {}",
                    artifact.id
                )));
            }
            if artifact.state() == TaskArtifactState::Available
                && !has_resource
                && !has_library_item
            {
                return Err(TaskOutputValidationError::new(format!(
                    "available task artifact must have exactly one backing: {}",
                    artifact.id
                )));
            }
            let Some(reference) = artifact.resource.as_ref() else {
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

fn validate_bound_resource_expansion(
    results: &[TaskResult],
    resources: &[TaskResourceRecord],
    resources_by_id: &HashMap<&str, &CacheResourceRef>,
) -> Result<(), TaskOutputValidationError> {
    let mut total_encoded_bytes = resources
        .iter()
        .map(|resource| resource.resource.encoded_len())
        .fold(0_usize, usize::saturating_add);
    let mut total_string_bytes = resources
        .iter()
        .map(|resource| resource_string_bytes(&resource.resource))
        .fold(0_usize, usize::saturating_add);

    for result in results {
        let mut result_encoded_bytes = result.encoded_len();
        let mut result_string_bytes = task_result_string_bytes(result);
        for artifact in &result.artifacts {
            let Some(reference) = artifact.resource.as_ref() else {
                continue;
            };
            let canonical_id = reference.id.to_ascii_lowercase();
            let Some(canonical) = resources_by_id.get(canonical_id.as_str()) else {
                return Err(TaskOutputValidationError::new(format!(
                    "task artifact references unknown resource: {}",
                    reference.id
                )));
            };
            let original_artifact_bytes = length_delimited_field_bytes(artifact.encoded_len());
            let mut bound_artifact = artifact.clone();
            bound_artifact.resource = Some((*canonical).clone());
            let bound_artifact_bytes = length_delimited_field_bytes(bound_artifact.encoded_len());
            result_encoded_bytes = result_encoded_bytes
                .saturating_sub(original_artifact_bytes)
                .saturating_add(bound_artifact_bytes);
            result_string_bytes = result_string_bytes
                .saturating_sub(resource_string_bytes(reference))
                .saturating_add(resource_string_bytes(canonical));
        }
        if result_encoded_bytes > MAX_TASK_RESULT_ENCODED_BYTES {
            return Err(TaskOutputValidationError::new(format!(
                "task result cannot exceed {MAX_TASK_RESULT_ENCODED_BYTES} encoded bytes after resource binding: {}",
                result.id
            )));
        }
        total_encoded_bytes = total_encoded_bytes.saturating_add(result_encoded_bytes);
        total_string_bytes = total_string_bytes.saturating_add(result_string_bytes);
    }

    if total_string_bytes > MAX_TASK_OUTPUT_STRING_BYTES {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_OUTPUT_STRING_BYTES} string bytes after resource binding"
        )));
    }
    if total_encoded_bytes > MAX_TASK_OUTPUT_ENCODED_BYTES {
        return Err(TaskOutputValidationError::new(format!(
            "task output cannot exceed {MAX_TASK_OUTPUT_ENCODED_BYTES} encoded bytes after resource binding"
        )));
    }
    Ok(())
}

fn length_delimited_field_bytes(payload_bytes: usize) -> usize {
    1_usize
        .saturating_add(varint_bytes(payload_bytes))
        .saturating_add(payload_bytes)
}

fn varint_bytes(value: usize) -> usize {
    let bits = usize::BITS as usize - value.leading_zeros() as usize;
    bits.max(1).div_ceil(7)
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
    use crate::generated::tvos_net_player::v1::{
        BilibiliTaskResultItem, TaskArtifact, TaskArtifactKind, TaskKind,
    };

    fn bilibili_subject() -> TaskResultSubject {
        TaskResultSubject {
            provider: "bilibili".to_owned(),
            kind: "video_page".to_owned(),
            id: "2001".to_owned(),
            index: 1,
        }
    }

    fn bilibili_provider_details() -> TaskResultProviderDetails {
        TaskResultProviderDetails {
            details: Some(task_result_provider_details::Details::Bilibili(
                BilibiliTaskResultDetails {
                    identity: Some(BilibiliContentIdentity {
                        kind: BilibiliContentKind::VideoPage.into(),
                        aid: 1_001,
                        bvid: "BV1stable".to_owned(),
                        cid: 2_001,
                        epid: 0,
                    }),
                    playback_session: Some(BilibiliPlaybackSession {
                        id: "session-one".to_owned(),
                        content_id: "2001".to_owned(),
                        ..Default::default()
                    }),
                },
            )),
        }
    }

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
    fn preferred_primary_result_replaces_an_existing_queued_primary() {
        let previous = TaskOutputRecord::replace(
            None,
            vec![
                TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Queued.into(),
                    ..Default::default()
                },
                TaskResult {
                    id: "result-two".to_owned(),
                    state: TaskState::Queued.into(),
                    ..Default::default()
                },
            ],
            Vec::new(),
        )
        .unwrap();

        let output = TaskOutputRecord::replace_with_primary_result(
            Some(&previous),
            vec![
                TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Failed.into(),
                    ..Default::default()
                },
                TaskResult {
                    id: "result-two".to_owned(),
                    state: TaskState::Succeeded.into(),
                    library_item_id: "local.default.result-two".to_owned(),
                    ..Default::default()
                },
            ],
            Vec::new(),
            Some("result-two"),
        )
        .unwrap();

        assert_eq!("result-two", output.primary_result_id);
        assert!(output.revision > previous.revision);
    }

    #[test]
    fn retiring_expired_resources_updates_artifacts_and_revision() {
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "expired-subtitle".to_owned(),
            content_type: "text/vtt".to_owned(),
            expires_at: Some(Timestamp {
                seconds: 10,
                nanos: 0,
            }),
            ..Default::default()
        })
        .unwrap();
        let mut output = TaskOutputRecord::replace(
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
        let original_revision = output.revision;
        let original_snapshot_id = output.snapshot_id.clone();

        let retired = output.retire_expired_resources_except(
            &Timestamp {
                seconds: 10,
                nanos: 0,
            },
            &HashSet::new(),
        );

        assert_eq!(vec!["expired-subtitle"], retired);
        assert!(output.resources.is_empty());
        assert_eq!(original_revision + 1, output.revision);
        assert_ne!(original_snapshot_id, output.snapshot_id);
        assert_eq!(0, output.summary().available_artifact_count);
        let artifact = &output.results[0].artifacts[0];
        assert_eq!(TaskArtifactState::Unavailable, artifact.state());
        assert!(artifact.resource.is_none());
        assert_eq!(
            "cache.resource_expired",
            artifact.problem.as_ref().unwrap().code
        );
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
    fn artifact_backing_validation_accepts_library_media_and_rejects_ambiguous_backings() {
        let library_media = TaskResult {
            id: "result-one".to_owned(),
            state: TaskState::Completed.into(),
            artifacts: vec![TaskArtifact {
                id: "media-one".to_owned(),
                kind: TaskArtifactKind::Media.into(),
                state: TaskArtifactState::Available.into(),
                library_item_id: "library-one".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        TaskOutputRecord::replace(None, vec![library_media.clone()], Vec::new())
            .expect("available media may use a library item backing");
        TaskOutputRecord::replace(
            None,
            vec![TaskResult {
                id: "result-unavailable".to_owned(),
                state: TaskState::Completed.into(),
                artifacts: vec![TaskArtifact {
                    id: "metadata-unavailable".to_owned(),
                    kind: TaskArtifactKind::Metadata.into(),
                    state: TaskArtifactState::Unavailable.into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("non-available artifacts may omit their backing");

        let mut missing = library_media.clone();
        missing.artifacts[0].library_item_id.clear();
        let error = TaskOutputRecord::replace(None, vec![missing], Vec::new())
            .expect_err("available artifacts require exactly one backing");
        assert!(error.to_string().contains("exactly one backing"));

        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "media-one".to_owned(),
            ..Default::default()
        })
        .unwrap();
        let mut ambiguous = library_media.clone();
        ambiguous.artifacts[0].resource = Some(resource.resource.clone());
        let error = TaskOutputRecord::replace(None, vec![ambiguous], vec![resource])
            .expect_err("an artifact cannot expose two backings");
        assert!(error.to_string().contains("both resource and library item"));

        let mut non_media = library_media;
        non_media.artifacts[0].kind = TaskArtifactKind::Subtitle.into();
        non_media.artifacts[0].state = TaskArtifactState::Unavailable.into();
        let error = TaskOutputRecord::replace(None, vec![non_media], Vec::new())
            .expect_err("library items are media-only even for non-available artifacts");
        assert!(error.to_string().contains("only media"));

        let mut malformed = TaskResult {
            id: "result-malformed".to_owned(),
            state: TaskState::Completed.into(),
            artifacts: vec![TaskArtifact {
                id: "media-malformed".to_owned(),
                kind: TaskArtifactKind::Media.into(),
                state: TaskArtifactState::Unavailable.into(),
                library_item_id: " library-one ".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let error = TaskOutputRecord::replace(None, vec![malformed.clone()], Vec::new())
            .expect_err("non-available artifact backings remain structurally validated");
        assert!(error.to_string().contains("invalid library item id"));
        malformed.artifacts[0].library_item_id = "library-one".to_owned();
        TaskOutputRecord::replace(None, vec![malformed], Vec::new())
            .expect("non-available media may retain a valid library item backing");
    }

    #[test]
    fn provider_details_are_validated_and_budgeted() {
        let valid = TaskResult {
            id: "result-one".to_owned(),
            state: TaskState::Completed.into(),
            subject: Some(bilibili_subject()),
            provider_details: Some(bilibili_provider_details()),
            ..Default::default()
        };
        TaskOutputRecord::replace(None, vec![valid.clone()], Vec::new())
            .expect("matching Bilibili details should be accepted");

        let mut wrong_provider = valid.clone();
        wrong_provider.subject.as_mut().unwrap().provider = "other".to_owned();
        let error = TaskOutputRecord::replace(None, vec![wrong_provider], Vec::new())
            .expect_err("provider details must match the generic subject");
        assert!(error.to_string().contains("require a Bilibili subject"));

        let mut mismatched_identity = valid.clone();
        let Some(task_result_provider_details::Details::Bilibili(details)) = mismatched_identity
            .provider_details
            .as_mut()
            .and_then(|details| details.details.as_mut())
        else {
            panic!("test fixture should contain Bilibili details");
        };
        details.identity.as_mut().unwrap().cid = 2_002;
        let error = TaskOutputRecord::replace(None, vec![mismatched_identity], Vec::new())
            .expect_err("provider identity must match the generic subject");
        assert!(error.to_string().contains("invalid identity"));

        let mut too_many_variants = valid.clone();
        let Some(task_result_provider_details::Details::Bilibili(details)) = too_many_variants
            .provider_details
            .as_mut()
            .and_then(|details| details.details.as_mut())
        else {
            panic!("test fixture should contain Bilibili details");
        };
        details.playback_session.as_mut().unwrap().variants =
            vec![BilibiliPlaybackVariant::default(); MAX_TASK_RESULT_PROVIDER_VARIANTS + 1];
        let error = TaskOutputRecord::replace(None, vec![too_many_variants], Vec::new())
            .expect_err("provider variant collections must be bounded");
        assert!(error.to_string().contains("variants"));

        let mut aggregate = valid;
        let Some(task_result_provider_details::Details::Bilibili(details)) = aggregate
            .provider_details
            .as_mut()
            .and_then(|details| details.details.as_mut())
        else {
            panic!("test fixture should contain Bilibili details");
        };
        details.playback_session.as_mut().unwrap().title = "x".repeat(950_000);
        let results = (0..9)
            .map(|index| TaskResult {
                id: format!("provider-result-{index}"),
                ..aggregate.clone()
            })
            .collect();
        let error = TaskOutputRecord::replace(None, results, Vec::new())
            .expect_err("provider detail strings must count toward the aggregate budget");
        assert!(error.to_string().contains("string bytes"));
    }

    #[test]
    fn output_rejects_unbounded_nested_artifacts() {
        let artifacts = (0..=MAX_TASK_ARTIFACTS)
            .map(|index| TaskArtifact {
                id: format!("artifact-{index}"),
                kind: TaskArtifactKind::Metadata.into(),
                state: TaskArtifactState::Unavailable.into(),
                ..Default::default()
            })
            .collect();
        let error = TaskOutputRecord::replace(
            None,
            vec![TaskResult {
                id: "result-one".to_owned(),
                state: TaskState::Completed.into(),
                artifacts,
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect_err("nested artifacts must be bounded");

        assert!(error.to_string().contains("artifacts"));
    }

    #[test]
    fn output_rejects_oversized_results_and_aggregate_strings() {
        let oversized_result = TaskResult {
            id: "oversized-result".to_owned(),
            state: TaskState::Completed.into(),
            title: "x".repeat(MAX_TASK_RESULT_ENCODED_BYTES + 1),
            ..Default::default()
        };
        let error = TaskOutputRecord::replace(None, vec![oversized_result], Vec::new())
            .expect_err("one result must not dominate a page");
        assert!(error.to_string().contains("task result cannot exceed"));

        let results = (0..9)
            .map(|index| TaskResult {
                id: format!("result-{index}"),
                state: TaskState::Completed.into(),
                title: "x".repeat(950_000),
                ..Default::default()
            })
            .collect();
        let error = TaskOutputRecord::replace(None, results, Vec::new())
            .expect_err("aggregate strings must be bounded");
        assert!(error.to_string().contains("string bytes"));
    }

    #[test]
    fn output_preflights_resource_expansion_before_binding_artifacts() {
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "large-metadata".to_owned(),
            content_type: "x".repeat(60_000),
            ..Default::default()
        })
        .expect("resource metadata should be individually valid");
        let artifacts = (0..20)
            .map(|index| TaskArtifact {
                id: format!("artifact-{index}"),
                kind: TaskArtifactKind::Metadata.into(),
                state: TaskArtifactState::Available.into(),
                resource: Some(CacheResourceRef {
                    id: resource.resource.id.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();

        let error = TaskOutputRecord::replace(
            None,
            vec![TaskResult {
                id: "result-one".to_owned(),
                state: TaskState::Completed.into(),
                artifacts,
                ..Default::default()
            }],
            vec![resource],
        )
        .expect_err("resource binding must be bounded before repeated metadata is cloned");

        assert!(error.to_string().contains("after resource binding"));
    }

    #[test]
    fn output_preflights_public_resource_uri_projection() {
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "shared-cover".to_owned(),
            content_type: "image/jpeg".to_owned(),
            ..Default::default()
        })
        .expect("resource should be valid");
        let artifacts = (0..600)
            .map(|index| TaskArtifact {
                id: format!("artifact-{index}"),
                kind: TaskArtifactKind::CoverImage.into(),
                state: TaskArtifactState::Available.into(),
                resource: Some(resource.resource.clone()),
                ..Default::default()
            })
            .collect();

        let error = TaskOutputRecord::replace(
            None,
            vec![TaskResult {
                id: "result-one".to_owned(),
                state: TaskState::Completed.into(),
                artifacts,
                ..Default::default()
            }],
            vec![resource],
        )
        .expect_err("public resource URI projection must stay within the result limit");

        assert!(error.to_string().contains("client projection"));
    }

    #[test]
    fn cache_deletion_preserves_an_existing_primary_result() {
        let primary = TaskResult {
            id: "result-two".to_owned(),
            state: TaskState::Completed.into(),
            ..Default::default()
        };
        let previous = TaskOutputRecord::replace(None, vec![primary.clone()], Vec::new()).unwrap();
        let mut output = TaskOutputRecord::replace(
            Some(&previous),
            vec![
                TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Completed.into(),
                    ..Default::default()
                },
                primary,
                TaskResult {
                    id: "result-three".to_owned(),
                    state: TaskState::Completed.into(),
                    ..Default::default()
                },
            ],
            Vec::new(),
        )
        .unwrap();

        output
            .mark_playback_cache_deleted("result-three", "library-result-three", "Cache deleted.")
            .unwrap();

        assert_eq!("result-two", output.primary_result_id);
        assert_eq!(TaskState::Failed, output.results[2].state());
    }

    #[test]
    fn library_deletion_tombstones_media_and_promotes_a_surviving_primary() {
        let mut output = TaskOutputRecord::replace(
            None,
            vec![
                TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Succeeded.into(),
                    library_item_id: "library-one".to_owned(),
                    artifacts: vec![TaskArtifact {
                        id: "media-one".to_owned(),
                        kind: TaskArtifactKind::Media.into(),
                        state: TaskArtifactState::Available.into(),
                        library_item_id: "library-one".to_owned(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                TaskResult {
                    id: "result-two".to_owned(),
                    state: TaskState::Succeeded.into(),
                    library_item_id: "library-two".to_owned(),
                    artifacts: vec![TaskArtifact {
                        id: "media-two".to_owned(),
                        kind: TaskArtifactKind::Media.into(),
                        state: TaskArtifactState::Available.into(),
                        library_item_id: "library-two".to_owned(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            Vec::new(),
        )
        .expect("task output should be valid");

        let update = output
            .mark_library_item_deleted("library-one", "Cached media was deleted.")
            .expect("library deletion should preserve output validity");

        assert_eq!(Some(Vec::new()), update);
        assert_eq!(TaskState::Failed, output.results[0].state());
        assert!(output.results[0].library_item_id.is_empty());
        assert_eq!(
            TaskArtifactState::Deleted,
            output.results[0].artifacts[0].state()
        );
        assert!(output.results[0].artifacts[0].library_item_id.is_empty());
        assert_eq!(TaskState::Succeeded, output.results[1].state());
        assert_eq!("result-two", output.primary_result_id);
    }

    #[test]
    fn cache_deletion_rejects_an_oversized_mutation_without_changing_output() {
        let mut output = TaskOutputRecord::replace(
            None,
            vec![TaskResult {
                id: "result-one".to_owned(),
                state: TaskState::Completed.into(),
                ..Default::default()
            }],
            Vec::new(),
        )
        .unwrap();
        let original = output.clone();

        let error = output
            .mark_playback_cache_deleted(
                "result-one",
                "library-result-one",
                &"x".repeat(MAX_TASK_RESULT_ENCODED_BYTES + 1),
            )
            .expect_err("cache deletion must preserve persisted output limits");

        assert!(error.to_string().contains("task result cannot exceed"));
        assert_eq!(original, output);
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
    fn legacy_bilibili_results_project_subject_identity_and_playback_session() {
        let identity = BilibiliContentIdentity {
            kind: BilibiliContentKind::VideoPage.into(),
            aid: 1_001,
            bvid: "BV1stable".to_owned(),
            cid: 2_001,
            epid: 0,
        };
        let playback_session = BilibiliPlaybackSession {
            id: "session-one".to_owned(),
            content_id: "2001".to_owned(),
            ..Default::default()
        };
        let task = Task {
            id: "task-one".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Playable.into(),
            result_items: vec![BilibiliTaskResultItem {
                id: "result-one".to_owned(),
                source_kind: "video_page".to_owned(),
                content_id: "2001".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                identity: Some(identity.clone()),
                playback_session: Some(playback_session.clone()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let output = TaskOutputRecord::from_legacy_task(&task);
        let result = &output.results[0];
        assert_eq!(Some(bilibili_subject()), result.subject.clone());
        let Some(task_result_provider_details::Details::Bilibili(details)) = result
            .provider_details
            .as_ref()
            .and_then(|details| details.details.as_ref())
        else {
            panic!("legacy result should expose Bilibili provider details");
        };
        assert_eq!(Some(&identity), details.identity.as_ref());
        assert_eq!(Some(&playback_session), details.playback_session.as_ref());
    }

    #[test]
    fn legacy_bilibili_result_projection_allows_session_transport_content_ids() {
        let task = Task {
            id: "task-one".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Playable.into(),
            result_items: vec![BilibiliTaskResultItem {
                id: "result-two".to_owned(),
                source_kind: "video_page".to_owned(),
                content_id: "logical-page-two".to_owned(),
                state: TaskState::Playable.into(),
                playback_session: Some(BilibiliPlaybackSession {
                    id: "result-two".to_owned(),
                    content_id: "transport-resource-one".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let output = TaskOutputRecord::from_legacy_task(&task);
        TaskOutputRecord::replace(None, output.results, Vec::new())
            .expect("transport content ids are independent from logical result subjects");
    }

    #[test]
    fn incomplete_legacy_bilibili_results_omit_generic_provider_metadata() {
        let task = Task {
            id: "task-one".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Preparing.into(),
            result_items: vec![BilibiliTaskResultItem {
                id: "result-one".to_owned(),
                state: TaskState::Cancelled.into(),
                playback_session: Some(BilibiliPlaybackSession {
                    id: "result-one".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let output = TaskOutputRecord::from_legacy_task(&task);
        assert!(output.results[0].subject.is_none());
        assert!(output.results[0].provider_details.is_none());
        TaskOutputRecord::replace(None, output.results, Vec::new())
            .expect("incomplete legacy provider metadata should remain representable");
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
