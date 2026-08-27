use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use prost_types::Timestamp;
use serde::{Deserialize, Serialize};

use crate::generated::tvos_net_player::v1::{
    BilibiliDownloadOptions, BilibiliPlaybackOptions, BilibiliPlaybackSession,
    BilibiliPlaybackVariant, BilibiliTaskResultItem, BilibiliTaskSelection, CacheResourceRef,
    LanTranscodingPlan, PlaybackSource, Task, TaskArtifact, TaskProblem, TaskResult,
    TaskResultProgress,
};
use crate::playback_policy::PlaybackPolicy;
use crate::task_output::{TaskOutputRecord, TaskResourceRecord};

const LEGACY_TASK_STATE_SCHEMA_VERSION: u32 = 1;
const TASK_STATE_SCHEMA_VERSION: u32 = 2;

#[derive(Clone)]
pub(crate) struct TaskStateStore {
    path: Arc<PathBuf>,
}

impl TaskStateStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(path.into()),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> io::Result<Vec<PersistedTaskRecord>> {
        let bytes = match fs::read(self.path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let snapshot: PersistedTaskSnapshot =
            serde_json::from_slice(&bytes).map_err(invalid_data)?;
        if !matches!(
            snapshot.schema_version,
            LEGACY_TASK_STATE_SCHEMA_VERSION | TASK_STATE_SCHEMA_VERSION
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported task state schema version: {}",
                    snapshot.schema_version
                ),
            ));
        }

        let schema_version = snapshot.schema_version;
        snapshot
            .tasks
            .into_iter()
            .map(|file| file.into_record(schema_version))
            .collect()
    }

    pub(crate) fn save(&self, records: &[PersistedTaskRecord]) -> io::Result<()> {
        if let Some(parent) = self.path().parent() {
            fs::create_dir_all(parent)?;
        }

        let snapshot = PersistedTaskSnapshot {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            tasks: records
                .iter()
                .cloned()
                .map(PersistedTaskFile::from)
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(invalid_data)?;
        let temp_path = temp_path_for(self.path());
        let mut temp_file = File::create(&temp_path)?;
        temp_file.write_all(&bytes)?;
        temp_file.write_all(b"\n")?;
        temp_file.sync_all()?;
        drop(temp_file);
        fs::rename(temp_path, self.path())?;
        sync_parent_directory(self.path())
    }
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Clone)]
pub(crate) struct PersistedTaskRecord {
    pub(crate) task: Task,
    pub(crate) options: Option<BilibiliDownloadOptions>,
    pub(crate) playback_options: Option<BilibiliPlaybackOptions>,
    pub(crate) output: TaskOutputRecord,
}

#[derive(Serialize, Deserialize)]
struct PersistedTaskSnapshot {
    schema_version: u32,
    #[serde(default)]
    tasks: Vec<PersistedTaskFile>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskFile {
    id: String,
    kind: i32,
    state: i32,
    source: String,
    title: String,
    progress: f64,
    downloaded_bytes: i64,
    total_bytes: i64,
    message: String,
    library_item_id: String,
    created_at: Option<PersistedTimestamp>,
    updated_at: Option<PersistedTimestamp>,
    finished_at: Option<PersistedTimestamp>,
    #[serde(default)]
    playback_source: Option<PersistedPlaybackSource>,
    #[serde(default)]
    playback_session: Option<PersistedBilibiliPlaybackSession>,
    #[serde(default)]
    bilibili_options: Option<PersistedBilibiliDownloadOptions>,
    #[serde(default)]
    bilibili_playback_options: Option<PersistedBilibiliPlaybackOptions>,
    #[serde(default)]
    bilibili_selection: Option<PersistedBilibiliTaskSelection>,
    #[serde(default)]
    result_items: Vec<PersistedBilibiliTaskResultItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<PersistedTaskOutput>,
}

impl From<PersistedTaskRecord> for PersistedTaskFile {
    fn from(record: PersistedTaskRecord) -> Self {
        let task = record.task;
        Self {
            id: task.id,
            kind: task.kind,
            state: task.state,
            source: task.source,
            title: task.title,
            progress: task.progress,
            downloaded_bytes: task.downloaded_bytes,
            total_bytes: task.total_bytes,
            message: task.message,
            library_item_id: task.library_item_id,
            created_at: task.created_at.map(PersistedTimestamp::from),
            updated_at: task.updated_at.map(PersistedTimestamp::from),
            finished_at: task.finished_at.map(PersistedTimestamp::from),
            playback_source: task.playback_source.map(PersistedPlaybackSource::from),
            playback_session: task
                .playback_session
                .map(PersistedBilibiliPlaybackSession::from),
            bilibili_selection: task
                .bilibili_selection
                .map(PersistedBilibiliTaskSelection::from),
            result_items: task
                .result_items
                .into_iter()
                .map(PersistedBilibiliTaskResultItem::from)
                .collect(),
            bilibili_options: record.options.map(PersistedBilibiliDownloadOptions::from),
            bilibili_playback_options: record
                .playback_options
                .map(PersistedBilibiliPlaybackOptions::from),
            output: Some(PersistedTaskOutput::from(record.output)),
        }
    }
}

impl PersistedTaskFile {
    fn into_record(self, schema_version: u32) -> io::Result<PersistedTaskRecord> {
        let task = Task {
            id: self.id,
            kind: self.kind,
            state: self.state,
            source: self.source,
            title: self.title,
            progress: self.progress,
            downloaded_bytes: self.downloaded_bytes,
            total_bytes: self.total_bytes,
            message: self.message,
            library_item_id: self.library_item_id,
            created_at: self.created_at.map(Timestamp::from),
            updated_at: self.updated_at.map(Timestamp::from),
            finished_at: self.finished_at.map(Timestamp::from),
            playback_source: self.playback_source.map(PlaybackSource::from),
            playback_session: self.playback_session.map(BilibiliPlaybackSession::from),
            bilibili_selection: self.bilibili_selection.map(BilibiliTaskSelection::from),
            result_items: self
                .result_items
                .into_iter()
                .map(BilibiliTaskResultItem::from)
                .collect(),
            output_summary: None,
        };
        let output = match schema_version {
            LEGACY_TASK_STATE_SCHEMA_VERSION => {
                let legacy = TaskOutputRecord::from_legacy_task(&task);
                TaskOutputRecord::restored(
                    legacy.revision,
                    legacy.snapshot_id,
                    String::new(),
                    legacy.results,
                    legacy.resources,
                    legacy.legacy_managed,
                )
                .map_err(invalid_data)?
            }
            TASK_STATE_SCHEMA_VERSION => self
                .output
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "task state schema v2 task is missing output",
                    )
                })?
                .into_output()?,
            _ => unreachable!("task state schema version was validated before conversion"),
        };

        Ok(PersistedTaskRecord {
            task,
            options: self.bilibili_options.map(BilibiliDownloadOptions::from),
            playback_options: self
                .bilibili_playback_options
                .map(BilibiliPlaybackOptions::from),
            output,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskOutput {
    revision: u64,
    snapshot_id: String,
    primary_result_id: String,
    results: Vec<PersistedTaskResult>,
    resources: Vec<PersistedCacheResourceRef>,
    legacy_managed: bool,
}

impl From<TaskOutputRecord> for PersistedTaskOutput {
    fn from(output: TaskOutputRecord) -> Self {
        Self {
            revision: output.revision,
            snapshot_id: output.snapshot_id,
            primary_result_id: output.primary_result_id,
            results: output
                .results
                .into_iter()
                .map(PersistedTaskResult::from)
                .collect(),
            resources: output
                .resources
                .into_iter()
                .map(PersistedCacheResourceRef::from)
                .collect(),
            legacy_managed: output.legacy_managed,
        }
    }
}

impl PersistedTaskOutput {
    fn into_output(self) -> io::Result<TaskOutputRecord> {
        let results = self
            .results
            .into_iter()
            .map(PersistedTaskResult::into_result)
            .collect::<io::Result<Vec<_>>>()?;
        let resources = self
            .resources
            .into_iter()
            .map(PersistedCacheResourceRef::into_record)
            .collect::<io::Result<Vec<_>>>()?;
        TaskOutputRecord::restored(
            self.revision,
            self.snapshot_id,
            self.primary_result_id,
            results,
            resources,
            self.legacy_managed,
        )
        .map_err(invalid_data)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskResult {
    id: String,
    state: i32,
    title: String,
    subtitle: String,
    progress: Option<PersistedTaskResultProgress>,
    problem: Option<PersistedTaskProblem>,
    library_item_id: String,
    playback_source: Option<PersistedPlaybackSource>,
    artifacts: Vec<PersistedTaskArtifact>,
    created_at: Option<PersistedTimestamp>,
    updated_at: Option<PersistedTimestamp>,
}

impl From<TaskResult> for PersistedTaskResult {
    fn from(result: TaskResult) -> Self {
        Self {
            id: result.id,
            state: result.state,
            title: result.title,
            subtitle: result.subtitle,
            progress: result.progress.map(PersistedTaskResultProgress::from),
            problem: result.problem.map(PersistedTaskProblem::from),
            library_item_id: result.library_item_id,
            playback_source: result.playback_source.map(PersistedPlaybackSource::from),
            artifacts: result
                .artifacts
                .into_iter()
                .map(PersistedTaskArtifact::from)
                .collect(),
            created_at: result.created_at.map(PersistedTimestamp::from),
            updated_at: result.updated_at.map(PersistedTimestamp::from),
        }
    }
}

impl PersistedTaskResult {
    fn into_result(self) -> io::Result<TaskResult> {
        Ok(TaskResult {
            id: self.id,
            state: self.state,
            title: self.title,
            subtitle: self.subtitle,
            progress: self.progress.map(TaskResultProgress::from),
            problem: self.problem.map(TaskProblem::from),
            library_item_id: self.library_item_id,
            playback_source: self.playback_source.map(PlaybackSource::from),
            artifacts: self
                .artifacts
                .into_iter()
                .map(PersistedTaskArtifact::into_artifact)
                .collect::<io::Result<Vec<_>>>()?,
            created_at: self.created_at.map(Timestamp::from),
            updated_at: self.updated_at.map(Timestamp::from),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskResultProgress {
    fraction: f64,
    completed_bytes: i64,
    total_bytes: i64,
    total_bytes_known: bool,
    phase: String,
    message: String,
}

impl From<TaskResultProgress> for PersistedTaskResultProgress {
    fn from(progress: TaskResultProgress) -> Self {
        Self {
            fraction: progress.fraction,
            completed_bytes: progress.completed_bytes,
            total_bytes: progress.total_bytes,
            total_bytes_known: progress.total_bytes_known,
            phase: progress.phase,
            message: progress.message,
        }
    }
}

impl From<PersistedTaskResultProgress> for TaskResultProgress {
    fn from(progress: PersistedTaskResultProgress) -> Self {
        Self {
            fraction: progress.fraction,
            completed_bytes: progress.completed_bytes,
            total_bytes: progress.total_bytes,
            total_bytes_known: progress.total_bytes_known,
            phase: progress.phase,
            message: progress.message,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskProblem {
    category: i32,
    code: String,
    message: String,
    retryable: bool,
}

impl From<TaskProblem> for PersistedTaskProblem {
    fn from(problem: TaskProblem) -> Self {
        Self {
            category: problem.category,
            code: problem.code,
            message: problem.message,
            retryable: problem.retryable,
        }
    }
}

impl From<PersistedTaskProblem> for TaskProblem {
    fn from(problem: PersistedTaskProblem) -> Self {
        Self {
            category: problem.category,
            code: problem.code,
            message: problem.message,
            retryable: problem.retryable,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskArtifact {
    id: String,
    kind: i32,
    state: i32,
    title: String,
    format: String,
    language_tag: String,
    is_ai_generated: bool,
    resource: Option<PersistedCacheResourceRef>,
    problem: Option<PersistedTaskProblem>,
}

impl From<TaskArtifact> for PersistedTaskArtifact {
    fn from(artifact: TaskArtifact) -> Self {
        Self {
            id: artifact.id,
            kind: artifact.kind,
            state: artifact.state,
            title: artifact.title,
            format: artifact.format,
            language_tag: artifact.language_tag,
            is_ai_generated: artifact.is_ai_generated,
            resource: artifact.resource.map(PersistedCacheResourceRef::from),
            problem: artifact.problem.map(PersistedTaskProblem::from),
        }
    }
}

impl PersistedTaskArtifact {
    fn into_artifact(self) -> io::Result<TaskArtifact> {
        Ok(TaskArtifact {
            id: self.id,
            kind: self.kind,
            state: self.state,
            title: self.title,
            format: self.format,
            language_tag: self.language_tag,
            is_ai_generated: self.is_ai_generated,
            resource: self
                .resource
                .map(PersistedCacheResourceRef::into_record)
                .transpose()?
                .map(|record| record.resource),
            problem: self.problem.map(TaskProblem::from),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedCacheResourceRef {
    id: String,
    content_type: String,
    size_bytes: i64,
    size_known: bool,
    supports_byte_ranges: bool,
    etag: String,
    expires_at: Option<PersistedTimestamp>,
}

impl From<TaskResourceRecord> for PersistedCacheResourceRef {
    fn from(record: TaskResourceRecord) -> Self {
        Self::from(record.resource)
    }
}

impl From<CacheResourceRef> for PersistedCacheResourceRef {
    fn from(resource: CacheResourceRef) -> Self {
        Self {
            id: resource.id,
            content_type: resource.content_type,
            size_bytes: resource.size_bytes,
            size_known: resource.size_known,
            supports_byte_ranges: resource.supports_byte_ranges,
            etag: resource.etag,
            expires_at: resource.expires_at.map(PersistedTimestamp::from),
        }
    }
}

impl PersistedCacheResourceRef {
    fn into_record(self) -> io::Result<TaskResourceRecord> {
        TaskResourceRecord::new(CacheResourceRef {
            id: self.id,
            uri: String::new(),
            content_type: self.content_type,
            size_bytes: self.size_bytes,
            size_known: self.size_known,
            supports_byte_ranges: self.supports_byte_ranges,
            etag: self.etag,
            expires_at: self.expires_at.map(Timestamp::from),
        })
        .map_err(invalid_data)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliTaskSelection {
    mode: i32,
    #[serde(default)]
    selection_ids: Vec<String>,
    range_start_index: u32,
    range_end_index: u32,
}

impl From<BilibiliTaskSelection> for PersistedBilibiliTaskSelection {
    fn from(selection: BilibiliTaskSelection) -> Self {
        Self {
            mode: selection.mode,
            selection_ids: selection.selection_ids,
            range_start_index: selection.range_start_index,
            range_end_index: selection.range_end_index,
        }
    }
}

impl From<PersistedBilibiliTaskSelection> for BilibiliTaskSelection {
    fn from(selection: PersistedBilibiliTaskSelection) -> Self {
        Self {
            mode: selection.mode,
            selection_ids: selection.selection_ids,
            range_start_index: selection.range_start_index,
            range_end_index: selection.range_end_index,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliTaskResultItem {
    id: String,
    selection_id: String,
    title: String,
    subtitle: String,
    source_kind: String,
    content_id: String,
    index: u32,
    state: i32,
    message: String,
    library_item_id: String,
    playback_source: Option<PersistedPlaybackSource>,
    playback_session: Option<PersistedBilibiliPlaybackSession>,
}

impl From<BilibiliTaskResultItem> for PersistedBilibiliTaskResultItem {
    fn from(item: BilibiliTaskResultItem) -> Self {
        Self {
            id: item.id,
            selection_id: item.selection_id,
            title: item.title,
            subtitle: item.subtitle,
            source_kind: item.source_kind,
            content_id: item.content_id,
            index: item.index,
            state: item.state,
            message: item.message,
            library_item_id: item.library_item_id,
            playback_source: item.playback_source.map(PersistedPlaybackSource::from),
            playback_session: item
                .playback_session
                .map(PersistedBilibiliPlaybackSession::from),
        }
    }
}

impl From<PersistedBilibiliTaskResultItem> for BilibiliTaskResultItem {
    fn from(item: PersistedBilibiliTaskResultItem) -> Self {
        Self {
            id: item.id,
            selection_id: item.selection_id,
            title: item.title,
            subtitle: item.subtitle,
            source_kind: item.source_kind,
            content_id: item.content_id,
            index: item.index,
            state: item.state,
            message: item.message,
            library_item_id: item.library_item_id,
            playback_source: item.playback_source.map(PlaybackSource::from),
            playback_session: item.playback_session.map(BilibiliPlaybackSession::from),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTimestamp {
    seconds: i64,
    nanos: i32,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedPlaybackSource {
    item_id: String,
    variant_id: String,
    protocol: i32,
    uri: String,
    expires_at: Option<PersistedTimestamp>,
}

impl From<PlaybackSource> for PersistedPlaybackSource {
    fn from(source: PlaybackSource) -> Self {
        Self {
            item_id: source.item_id,
            variant_id: source.variant_id,
            protocol: source.protocol,
            uri: source.uri,
            expires_at: source.expires_at.map(PersistedTimestamp::from),
        }
    }
}

impl From<PersistedPlaybackSource> for PlaybackSource {
    fn from(source: PersistedPlaybackSource) -> Self {
        Self {
            item_id: source.item_id,
            variant_id: source.variant_id,
            protocol: source.protocol,
            uri: source.uri,
            expires_at: source.expires_at.map(Timestamp::from),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliPlaybackSession {
    id: String,
    title: String,
    content_id: String,
    selected_variant_id: String,
    selected_variant: Option<PersistedBilibiliPlaybackVariant>,
    #[serde(default)]
    variants: Vec<PersistedBilibiliPlaybackVariant>,
    #[serde(default)]
    transcoding_plan: Option<PersistedLanTranscodingPlan>,
    #[serde(default)]
    effective_policy: Option<PlaybackPolicy>,
}

impl From<BilibiliPlaybackSession> for PersistedBilibiliPlaybackSession {
    fn from(session: BilibiliPlaybackSession) -> Self {
        Self {
            id: session.id,
            title: session.title,
            content_id: session.content_id,
            selected_variant_id: session.selected_variant_id,
            selected_variant: session
                .selected_variant
                .map(PersistedBilibiliPlaybackVariant::from),
            variants: session
                .variants
                .into_iter()
                .map(PersistedBilibiliPlaybackVariant::from)
                .collect(),
            transcoding_plan: session
                .transcoding_plan
                .map(PersistedLanTranscodingPlan::from),
            effective_policy: session.effective_policy.map(|policy| {
                PlaybackPolicy::from_proto(Some(&policy))
                    .expect("server-owned playback session policy should use known enum values")
            }),
        }
    }
}

impl From<PersistedBilibiliPlaybackSession> for BilibiliPlaybackSession {
    fn from(session: PersistedBilibiliPlaybackSession) -> Self {
        Self {
            id: session.id,
            title: session.title,
            content_id: session.content_id,
            selected_variant_id: session.selected_variant_id,
            selected_variant: session.selected_variant.map(BilibiliPlaybackVariant::from),
            variants: session
                .variants
                .into_iter()
                .map(BilibiliPlaybackVariant::from)
                .collect(),
            transcoding_plan: session.transcoding_plan.map(LanTranscodingPlan::from),
            effective_policy: Some(session.effective_policy.unwrap_or_default().to_proto()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedLanTranscodingPlan {
    state: i32,
    profile_id: String,
    reason: String,
    source_variant_id: String,
    target_container: String,
    target_video_codec: String,
    target_audio_codec: String,
    output_protocol: i32,
}

impl From<LanTranscodingPlan> for PersistedLanTranscodingPlan {
    fn from(plan: LanTranscodingPlan) -> Self {
        Self {
            state: plan.state,
            profile_id: plan.profile_id,
            reason: plan.reason,
            source_variant_id: plan.source_variant_id,
            target_container: plan.target_container,
            target_video_codec: plan.target_video_codec,
            target_audio_codec: plan.target_audio_codec,
            output_protocol: plan.output_protocol,
        }
    }
}

impl From<PersistedLanTranscodingPlan> for LanTranscodingPlan {
    fn from(plan: PersistedLanTranscodingPlan) -> Self {
        Self {
            state: plan.state,
            profile_id: plan.profile_id,
            reason: plan.reason,
            source_variant_id: plan.source_variant_id,
            target_container: plan.target_container,
            target_video_codec: plan.target_video_codec,
            target_audio_codec: plan.target_audio_codec,
            output_protocol: plan.output_protocol,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliPlaybackVariant {
    id: String,
    label: String,
    source_kind: String,
    container: String,
    video_codec: String,
    audio_codec: String,
    width: i32,
    height: i32,
    bitrate: i64,
    size_bytes: i64,
}

impl From<BilibiliPlaybackVariant> for PersistedBilibiliPlaybackVariant {
    fn from(variant: BilibiliPlaybackVariant) -> Self {
        Self {
            id: variant.id,
            label: variant.label,
            source_kind: variant.source_kind,
            container: variant.container,
            video_codec: variant.video_codec,
            audio_codec: variant.audio_codec,
            width: variant.width,
            height: variant.height,
            bitrate: variant.bitrate,
            size_bytes: variant.size_bytes,
        }
    }
}

impl From<PersistedBilibiliPlaybackVariant> for BilibiliPlaybackVariant {
    fn from(variant: PersistedBilibiliPlaybackVariant) -> Self {
        Self {
            id: variant.id,
            label: variant.label,
            source_kind: variant.source_kind,
            container: variant.container,
            video_codec: variant.video_codec,
            audio_codec: variant.audio_codec,
            width: variant.width,
            height: variant.height,
            bitrate: variant.bitrate,
            size_bytes: variant.size_bytes,
        }
    }
}

impl From<Timestamp> for PersistedTimestamp {
    fn from(timestamp: Timestamp) -> Self {
        Self {
            seconds: timestamp.seconds,
            nanos: timestamp.nanos,
        }
    }
}

impl From<PersistedTimestamp> for Timestamp {
    fn from(timestamp: PersistedTimestamp) -> Self {
        Self {
            seconds: timestamp.seconds,
            nanos: timestamp.nanos,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliDownloadOptions {
    quality_preference: String,
    encoding_preference: String,
    prefer_tv_api: bool,
    download_subtitles: bool,
    download_danmaku: bool,
    #[serde(default)]
    audio_language: String,
    #[serde(default)]
    subtitle_ai_policy: i32,
    #[serde(default)]
    download_cover: bool,
    #[serde(default)]
    danmaku_formats: Vec<i32>,
}

impl From<BilibiliDownloadOptions> for PersistedBilibiliDownloadOptions {
    fn from(options: BilibiliDownloadOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            download_subtitles: options.download_subtitles,
            download_danmaku: options.download_danmaku,
            audio_language: options.audio_language,
            subtitle_ai_policy: options.subtitle_ai_policy,
            download_cover: options.download_cover,
            danmaku_formats: options.danmaku_formats,
        }
    }
}

impl From<PersistedBilibiliDownloadOptions> for BilibiliDownloadOptions {
    fn from(options: PersistedBilibiliDownloadOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            download_subtitles: options.download_subtitles,
            download_danmaku: options.download_danmaku,
            audio_language: options.audio_language,
            subtitle_ai_policy: options.subtitle_ai_policy,
            download_cover: options.download_cover,
            danmaku_formats: options.danmaku_formats,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliPlaybackOptions {
    quality_preference: String,
    encoding_preference: String,
    prefer_tv_api: bool,
    #[serde(default)]
    audio_language: String,
    #[serde(default)]
    playback_policy: Option<PlaybackPolicy>,
}

impl From<BilibiliPlaybackOptions> for PersistedBilibiliPlaybackOptions {
    fn from(options: BilibiliPlaybackOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            audio_language: options.audio_language,
            playback_policy: options.playback_policy.map(|policy| {
                PlaybackPolicy::from_proto(Some(&policy))
                    .expect("validated playback options should use known policy enum values")
            }),
        }
    }
}

impl From<PersistedBilibiliPlaybackOptions> for BilibiliPlaybackOptions {
    fn from(options: PersistedBilibiliPlaybackOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            audio_language: options.audio_language,
            playback_policy: options.playback_policy.map(PlaybackPolicy::to_proto),
        }
    }
}

fn temp_path_for(path: &Path) -> PathBuf {
    let temp_name = path
        .file_name()
        .map(|name| format!("{}.tmp", name.to_string_lossy()))
        .unwrap_or_else(|| "tasks.json.tmp".to_owned());
    path.with_file_name(temp_name)
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tvos_net_player::v1::{
        BilibiliCompatibleVariantPreference, BilibiliDanmakuFormat, BilibiliPlaybackPolicy,
        BilibiliPlaybackVariant, BilibiliSubtitleAiPolicy, BilibiliTaskResultItem,
        BilibiliTaskSelection, BilibiliTranscodingPreference, BilibiliWeakNetworkPreference,
        LanTranscodingPlan, LanTranscodingPlanState, PlaybackProtocol, TaskArtifactKind,
        TaskArtifactState, TaskKind, TaskProblemCategory, TaskState,
    };

    #[test]
    fn load_legacy_snapshot_defaults_bilibili_schema_fields() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-playback-legacy",
      "kind": 3,
      "state": 10,
      "source": "BV1legacy",
      "title": "Legacy video",
      "progress": 1.0,
      "downloaded_bytes": 0,
      "total_bytes": 0,
      "message": "Cached.",
      "library_item_id": "bilibili.hls.legacy",
      "created_at": null,
      "updated_at": null,
      "finished_at": null
    }
  ]
}"#,
        )
        .expect("legacy snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("legacy snapshot should load");

        assert_eq!(1, records.len());
        assert!(records[0].task.bilibili_selection.is_none());
        assert!(records[0].task.result_items.is_empty());
    }

    #[test]
    fn migrates_v1_snapshot_to_legacy_managed_output() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-v1-output",
      "kind": 1,
      "state": 10,
      "source": "BV1legacyOutput",
      "title": "Legacy collection",
      "progress": 1.0,
      "downloaded_bytes": 200,
      "total_bytes": 200,
      "message": "Finished.",
      "library_item_id": "",
      "created_at": { "seconds": 100, "nanos": 1 },
      "updated_at": { "seconds": 200, "nanos": 2 },
      "finished_at": { "seconds": 200, "nanos": 2 },
      "result_items": [
        {
          "id": "legacy-result-second",
          "selection_id": "page:2",
          "title": "Second",
          "subtitle": "Page 2",
          "source_kind": "video_page",
          "content_id": "cid-2",
          "index": 2,
          "state": 4,
          "message": "Provider failed.",
          "library_item_id": ""
        },
        {
          "id": "legacy-result-first",
          "selection_id": "page:1",
          "title": "First",
          "subtitle": "Page 1",
          "source_kind": "video_page",
          "content_id": "cid-1",
          "index": 1,
          "state": 10,
          "message": "Cached.",
          "library_item_id": "bilibili.hls.first"
        }
      ]
    }
  ]
}"#,
        )
        .expect("v1 snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("v1 snapshot should migrate");
        let output = &records[0].output;

        assert_eq!(1, output.revision);
        assert!(output.snapshot_id.starts_with("task-output-"));
        assert_eq!("legacy-result-first", output.primary_result_id);
        assert!(output.legacy_managed);
        assert!(output.resources.is_empty());
        assert_eq!(
            vec!["legacy-result-second", "legacy-result-first"],
            output
                .results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(TaskState::Failed, output.results[0].state());
        assert_eq!("Page 2", output.results[0].subtitle);
        assert_eq!(
            Some(TaskProblemCategory::Upstream),
            output.results[0]
                .problem
                .as_ref()
                .map(|problem| problem.category())
        );
        assert_eq!(
            1.0,
            output.results[1]
                .progress
                .as_ref()
                .expect("legacy result progress should exist")
                .fraction
        );
        assert_eq!(
            Some(Timestamp {
                seconds: 100,
                nanos: 1,
            }),
            output.results[0].created_at
        );
    }

    #[test]
    fn load_legacy_playback_sessions_default_effective_policy() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-playback-legacy-policy",
      "kind": 3,
      "state": 6,
      "source": "BV1legacy",
      "title": "Legacy playback",
      "progress": 0.5,
      "downloaded_bytes": 0,
      "total_bytes": 0,
      "message": "Playable.",
      "library_item_id": "",
      "created_at": null,
      "updated_at": null,
      "finished_at": null,
      "playback_source": null,
      "playback_session": {
        "id": "bilibili-playback-legacy-policy",
        "title": "Legacy playback",
        "content_id": "cid-primary",
        "selected_variant_id": "h264",
        "selected_variant": null
      },
      "result_items": [
        {
          "id": "bilibili-playback-legacy-policy-result-1",
          "selection_id": "page:1",
          "title": "Part 1",
          "subtitle": "Page 1",
          "source_kind": "video_page",
          "content_id": "cid-result",
          "index": 1,
          "state": 6,
          "message": "Playable.",
          "library_item_id": "",
          "playback_source": null,
          "playback_session": {
            "id": "bilibili-playback-legacy-policy-result-1",
            "title": "Part 1",
            "content_id": "cid-result",
            "selected_variant_id": "h264",
            "selected_variant": null
          }
        }
      ]
    }
  ]
}"#,
        )
        .expect("legacy playback snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("legacy playback snapshot should load");
        let expected = PlaybackPolicy::default().to_proto();

        assert_eq!(
            Some(&expected),
            records[0]
                .task
                .playback_session
                .as_ref()
                .and_then(|session| session.effective_policy.as_ref())
        );
        assert_eq!(
            Some(&expected),
            records[0].task.result_items[0]
                .playback_session
                .as_ref()
                .and_then(|session| session.effective_policy.as_ref())
        );
    }

    #[test]
    fn load_legacy_bilibili_options_defaults_new_schema_fields() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-options-legacy",
      "kind": 1,
      "state": 1,
      "source": "BV1legacy",
      "title": "Legacy options",
      "progress": 0.0,
      "downloaded_bytes": 0,
      "total_bytes": 0,
      "message": "Queued.",
      "library_item_id": "",
      "created_at": null,
      "updated_at": null,
      "finished_at": null,
      "bilibili_options": {
        "quality_preference": "1080p",
        "encoding_preference": "",
        "prefer_tv_api": true,
        "download_subtitles": true,
        "download_danmaku": false
      },
      "bilibili_playback_options": {
        "quality_preference": "720p",
        "encoding_preference": "h264",
        "prefer_tv_api": false
      }
    }
  ]
}"#,
        )
        .expect("legacy options snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("legacy options snapshot should load");

        let options = records[0]
            .options
            .as_ref()
            .expect("download options should restore");
        assert_eq!("1080p", options.quality_preference);
        assert!(options.prefer_tv_api);
        assert!(options.download_subtitles);
        assert!(options.audio_language.is_empty());
        assert_eq!(
            BilibiliSubtitleAiPolicy::Unspecified,
            options.subtitle_ai_policy()
        );
        assert!(!options.download_cover);
        assert!(options.danmaku_formats.is_empty());

        let playback_options = records[0]
            .playback_options
            .as_ref()
            .expect("playback options should restore");
        assert_eq!("720p", playback_options.quality_preference);
        assert!(playback_options.audio_language.is_empty());
        assert!(playback_options.playback_policy.is_none());
    }

    #[test]
    fn round_trips_nested_v2_task_output_without_persisting_resource_locations() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let subtitle_resource = TaskResourceRecord::new(CacheResourceRef {
            id: "subtitle-z".to_owned(),
            uri: "file:///private/tmp/must-not-persist".to_owned(),
            content_type: "text/vtt; charset=utf-8".to_owned(),
            size_bytes: 321,
            size_known: true,
            supports_byte_ranges: true,
            etag: "subtitle-etag".to_owned(),
            expires_at: Some(Timestamp {
                seconds: 10_000,
                nanos: 11,
            }),
        })
        .expect("subtitle resource should be valid");
        let cover_resource = TaskResourceRecord::new(CacheResourceRef {
            id: "cover-a".to_owned(),
            uri: "https://upstream.example.test/private-cover".to_owned(),
            content_type: "image/jpeg".to_owned(),
            size_bytes: 654,
            size_known: false,
            supports_byte_ranges: false,
            etag: "cover-etag".to_owned(),
            expires_at: Some(Timestamp {
                seconds: 20_000,
                nanos: 22,
            }),
        })
        .expect("cover resource should be valid");
        let results = vec![
            TaskResult {
                id: "result-z".to_owned(),
                state: TaskState::Failed.into(),
                title: "Failed result".to_owned(),
                subtitle: "Nested fields".to_owned(),
                progress: Some(TaskResultProgress {
                    fraction: 0.75,
                    completed_bytes: 750,
                    total_bytes: 1_000,
                    total_bytes_known: true,
                    phase: "packaging".to_owned(),
                    message: "Packaging artifacts.".to_owned(),
                }),
                problem: Some(TaskProblem {
                    category: TaskProblemCategory::Upstream.into(),
                    code: "bilibili.test_failure".to_owned(),
                    message: "Provider failed.".to_owned(),
                    retryable: true,
                }),
                library_item_id: "library.result-z".to_owned(),
                playback_source: Some(PlaybackSource {
                    item_id: "library.result-z".to_owned(),
                    variant_id: "h264".to_owned(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: "http://media.example.test/result-z/master.m3u8".to_owned(),
                    expires_at: Some(Timestamp {
                        seconds: 30_000,
                        nanos: 33,
                    }),
                }),
                artifacts: vec![
                    TaskArtifact {
                        id: "artifact-cover".to_owned(),
                        kind: TaskArtifactKind::CoverImage.into(),
                        state: TaskArtifactState::Unavailable.into(),
                        title: "Cover".to_owned(),
                        format: "jpeg".to_owned(),
                        language_tag: String::new(),
                        is_ai_generated: false,
                        resource: Some(cover_resource.resource.clone()),
                        problem: Some(TaskProblem {
                            category: TaskProblemCategory::Permission.into(),
                            code: "artifact.cover_denied".to_owned(),
                            message: "Cover access denied.".to_owned(),
                            retryable: false,
                        }),
                    },
                    TaskArtifact {
                        id: "artifact-subtitle".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Available.into(),
                        title: "Japanese subtitles".to_owned(),
                        format: "vtt".to_owned(),
                        language_tag: "ja-JP".to_owned(),
                        is_ai_generated: true,
                        resource: Some(subtitle_resource.resource.clone()),
                        problem: None,
                    },
                ],
                created_at: Some(Timestamp {
                    seconds: 100,
                    nanos: 1,
                }),
                updated_at: Some(Timestamp {
                    seconds: 200,
                    nanos: 2,
                }),
            },
            TaskResult {
                id: "result-a".to_owned(),
                state: TaskState::Completed.into(),
                title: "Completed result".to_owned(),
                subtitle: "Second result".to_owned(),
                progress: None,
                problem: None,
                library_item_id: "library.result-a".to_owned(),
                playback_source: None,
                artifacts: Vec::new(),
                created_at: None,
                updated_at: None,
            },
        ];
        let output = TaskOutputRecord::restored(
            42,
            "snapshot-v2-nested".to_owned(),
            "result-a".to_owned(),
            results,
            vec![subtitle_resource, cover_resource],
            false,
        )
        .expect("nested output should be valid");
        let task = Task {
            id: "task-v2-output".to_owned(),
            kind: TaskKind::BilibiliDownload.into(),
            state: TaskState::Failed.into(),
            source: "BV1nestedOutput".to_owned(),
            ..Default::default()
        };
        TaskStateStore::new(path.clone())
            .save(&[PersistedTaskRecord {
                task,
                options: None,
                playback_options: None,
                output: output.clone(),
            }])
            .expect("v2 task output should persist");

        let mut snapshot: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).expect("persisted snapshot should be readable"),
        )
        .expect("persisted snapshot should be valid JSON");
        assert_eq!(Some(2), snapshot["schema_version"].as_u64());
        let resource_json = snapshot["tasks"][0]["output"]["resources"][0]
            .as_object()
            .expect("persisted output resource should be an object");
        assert!(!resource_json.contains_key("uri"));
        assert!(!resource_json.contains_key("relative_path"));
        assert!(!resource_json.contains_key("local_path"));
        let artifact_resource_json =
            snapshot["tasks"][0]["output"]["results"][0]["artifacts"][0]["resource"]
                .as_object()
                .expect("persisted artifact resource should be an object");
        assert!(!artifact_resource_json.contains_key("uri"));
        assert!(!artifact_resource_json.contains_key("relative_path"));
        assert!(!artifact_resource_json.contains_key("local_path"));

        snapshot["tasks"][0]["output"]["resources"][0]
            .as_object_mut()
            .expect("persisted output resource should be mutable")
            .insert(
                "uri".to_owned(),
                serde_json::Value::String("file:///private/tmp/tampered".to_owned()),
            );
        snapshot["tasks"][0]["output"]["results"][0]["artifacts"][0]["resource"]
            .as_object_mut()
            .expect("persisted artifact resource should be mutable")
            .insert(
                "uri".to_owned(),
                serde_json::Value::String("https://upstream.example.test/tampered".to_owned()),
            );
        fs::write(
            &path,
            serde_json::to_vec_pretty(&snapshot).expect("tampered snapshot should serialize"),
        )
        .expect("tampered snapshot should be written");

        let records = TaskStateStore::new(path.clone())
            .load()
            .expect("v2 task output should reload");
        assert_eq!(output, records[0].output);
        assert_eq!("result-a", records[0].output.primary_result_id);
        assert_eq!(
            vec!["result-z", "result-a"],
            records[0]
                .output
                .results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            vec!["artifact-cover", "artifact-subtitle"],
            records[0].output.results[0]
                .artifacts
                .iter()
                .map(|artifact| artifact.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            vec!["subtitle-z", "cover-a"],
            records[0]
                .output
                .resources
                .iter()
                .map(|resource| resource.resource.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            "/resources/subtitle-z",
            records[0].output.resources[0].resource.uri
        );
        assert_eq!(
            "/resources/cover-a",
            records[0].output.results[0].artifacts[0]
                .resource
                .as_ref()
                .expect("cover artifact resource should reload")
                .uri
        );

        snapshot["tasks"][0]["output"]["snapshot_id"] = serde_json::Value::String(String::new());
        fs::write(
            &path,
            serde_json::to_vec_pretty(&snapshot).expect("legacy snapshot should serialize"),
        )
        .expect("legacy snapshot should be written");
        let regenerated = TaskStateStore::new(path)
            .load()
            .expect("blank snapshot id should be repaired");
        assert_eq!(42, regenerated[0].output.revision);
        assert!(
            regenerated[0]
                .output
                .snapshot_id
                .starts_with("task-output-")
        );
    }

    #[test]
    fn round_trips_bilibili_selection_and_result_items() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let timestamp = Timestamp {
            seconds: 100,
            nanos: 0,
        };
        let selection = BilibiliTaskSelection {
            mode: 4,
            selection_ids: vec!["page:1".to_owned(), "page:2".to_owned()],
            range_start_index: 0,
            range_end_index: 0,
        };
        let playback_source = PlaybackSource {
            item_id: "bilibili-playback-result-1".to_owned(),
            variant_id: "h264".to_owned(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: "http://media.example.test:8080/hls/bilibili-playback-result-1/master.m3u8"
                .to_owned(),
            expires_at: None,
        };
        let playback_policy = BilibiliPlaybackPolicy {
            transcoding_preference: BilibiliTranscodingPreference::Force.into(),
            compatible_variant_preference: BilibiliCompatibleVariantPreference::PreferRequested
                .into(),
            weak_network_preference: BilibiliWeakNetworkPreference::HoldDowngrade.into(),
        };
        let playback_session = BilibiliPlaybackSession {
            id: "bilibili-playback-result-1".to_owned(),
            title: "Part 1".to_owned(),
            content_id: "cid-1".to_owned(),
            selected_variant_id: "h264".to_owned(),
            selected_variant: Some(BilibiliPlaybackVariant {
                id: "h264".to_owned(),
                label: "1080p".to_owned(),
                source_kind: "dash".to_owned(),
                container: "mp4".to_owned(),
                video_codec: "avc1".to_owned(),
                audio_codec: "mp4a".to_owned(),
                width: 1920,
                height: 1080,
                bitrate: 1_000_000,
                size_bytes: 10_000_000,
            }),
            variants: Vec::new(),
            transcoding_plan: Some(LanTranscodingPlan {
                state: LanTranscodingPlanState::NotRequired.into(),
                profile_id: "avplayer-h264-aac-hls-v1".to_owned(),
                reason: "Already compatible.".to_owned(),
                source_variant_id: "h264".to_owned(),
                target_container: "hls/fmp4".to_owned(),
                target_video_codec: "h264".to_owned(),
                target_audio_codec: "aac".to_owned(),
                output_protocol: PlaybackProtocol::Hls.into(),
            }),
            effective_policy: Some(playback_policy),
        };
        let result_item = BilibiliTaskResultItem {
            id: "bilibili-playback-result-1".to_owned(),
            selection_id: "page:1".to_owned(),
            title: "Part 1".to_owned(),
            subtitle: "Page 1".to_owned(),
            source_kind: "video_page".to_owned(),
            content_id: "cid-1".to_owned(),
            index: 1,
            state: TaskState::Playable.into(),
            message: "Playable online.".to_owned(),
            library_item_id: String::new(),
            playback_source: Some(playback_source.clone()),
            playback_session: Some(playback_session.clone()),
        };
        let task = Task {
            id: "bilibili-playback-task".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Playable.into(),
            source: "BV1persist".to_owned(),
            title: "Persisted task".to_owned(),
            progress: 0.5,
            downloaded_bytes: 0,
            total_bytes: 0,
            message: "Playable.".to_owned(),
            library_item_id: String::new(),
            created_at: Some(Timestamp {
                seconds: timestamp.seconds,
                nanos: timestamp.nanos,
            }),
            updated_at: Some(timestamp),
            finished_at: None,
            playback_source: Some(playback_source.clone()),
            playback_session: Some(playback_session.clone()),
            bilibili_selection: Some(selection.clone()),
            result_items: vec![result_item.clone()],
            output_summary: None,
        };
        let output = TaskOutputRecord::from_legacy_task(&task);
        TaskStateStore::new(path.clone())
            .save(&[PersistedTaskRecord {
                task,
                options: Some(BilibiliDownloadOptions {
                    quality_preference: "1080p".to_owned(),
                    encoding_preference: String::new(),
                    prefer_tv_api: false,
                    download_subtitles: true,
                    download_danmaku: true,
                    audio_language: "ja-jp".to_owned(),
                    subtitle_ai_policy: BilibiliSubtitleAiPolicy::PreferNonAi.into(),
                    download_cover: true,
                    danmaku_formats: vec![BilibiliDanmakuFormat::Ass.into()],
                }),
                playback_options: Some(BilibiliPlaybackOptions {
                    quality_preference: "720p".to_owned(),
                    encoding_preference: "h264".to_owned(),
                    prefer_tv_api: false,
                    audio_language: "ja-jp".to_owned(),
                    playback_policy: Some(playback_policy),
                }),
                output: output.clone(),
            }])
            .expect("task state should persist");

        let records = TaskStateStore::new(path)
            .load()
            .expect("task state should reload");

        assert_eq!(1, records.len());
        assert_eq!(output, records[0].output);
        assert_eq!(Some(selection), records[0].task.bilibili_selection);
        assert_eq!(vec![result_item], records[0].task.result_items);
        assert_eq!(
            Some(LanTranscodingPlanState::NotRequired),
            records[0]
                .task
                .playback_session
                .as_ref()
                .and_then(|session| session.transcoding_plan.as_ref())
                .map(|plan| plan.state())
        );
        let options = records[0]
            .options
            .as_ref()
            .expect("download options should round-trip");
        assert_eq!("ja-jp", options.audio_language);
        assert_eq!(
            BilibiliSubtitleAiPolicy::PreferNonAi,
            options.subtitle_ai_policy()
        );
        assert!(options.download_cover);
        assert_eq!(
            vec![i32::from(BilibiliDanmakuFormat::Ass)],
            options.danmaku_formats
        );

        let playback_options = records[0]
            .playback_options
            .as_ref()
            .expect("playback options should round-trip");
        assert_eq!("ja-jp", playback_options.audio_language);
        assert_eq!(
            Some(BilibiliTranscodingPreference::Force),
            playback_options
                .playback_policy
                .as_ref()
                .map(|policy| policy.transcoding_preference())
        );
        assert_eq!(
            Some(BilibiliWeakNetworkPreference::HoldDowngrade),
            records[0]
                .task
                .playback_session
                .as_ref()
                .and_then(|session| session.effective_policy.as_ref())
                .map(|policy| policy.weak_network_preference())
        );
    }
}
