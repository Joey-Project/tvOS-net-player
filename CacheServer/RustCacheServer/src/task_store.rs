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
    BilibiliPlaybackVariant, BilibiliTaskResultItem, BilibiliTaskSelection, LanTranscodingPlan,
    PlaybackSource, Task,
};

const TASK_STATE_SCHEMA_VERSION: u32 = 1;

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
        if snapshot.schema_version != TASK_STATE_SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported task state schema version: {}",
                    snapshot.schema_version
                ),
            ));
        }

        Ok(snapshot
            .tasks
            .into_iter()
            .map(PersistedTaskRecord::from)
            .collect())
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
        fs::rename(temp_path, self.path())
    }
}

#[derive(Clone)]
pub(crate) struct PersistedTaskRecord {
    pub(crate) task: Task,
    pub(crate) options: Option<BilibiliDownloadOptions>,
    pub(crate) playback_options: Option<BilibiliPlaybackOptions>,
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
        }
    }
}

impl From<PersistedTaskFile> for PersistedTaskRecord {
    fn from(file: PersistedTaskFile) -> Self {
        Self {
            task: Task {
                id: file.id,
                kind: file.kind,
                state: file.state,
                source: file.source,
                title: file.title,
                progress: file.progress,
                downloaded_bytes: file.downloaded_bytes,
                total_bytes: file.total_bytes,
                message: file.message,
                library_item_id: file.library_item_id,
                created_at: file.created_at.map(Timestamp::from),
                updated_at: file.updated_at.map(Timestamp::from),
                finished_at: file.finished_at.map(Timestamp::from),
                playback_source: file.playback_source.map(PlaybackSource::from),
                playback_session: file.playback_session.map(BilibiliPlaybackSession::from),
                bilibili_selection: file.bilibili_selection.map(BilibiliTaskSelection::from),
                result_items: file
                    .result_items
                    .into_iter()
                    .map(BilibiliTaskResultItem::from)
                    .collect(),
            },
            options: file.bilibili_options.map(BilibiliDownloadOptions::from),
            playback_options: file
                .bilibili_playback_options
                .map(BilibiliPlaybackOptions::from),
        }
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
}

impl From<BilibiliPlaybackOptions> for PersistedBilibiliPlaybackOptions {
    fn from(options: BilibiliPlaybackOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            audio_language: options.audio_language,
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
        BilibiliDanmakuFormat, BilibiliPlaybackVariant, BilibiliSubtitleAiPolicy,
        BilibiliTaskResultItem, BilibiliTaskSelection, LanTranscodingPlan, LanTranscodingPlanState,
        PlaybackProtocol, TaskKind, TaskState,
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
        TaskStateStore::new(path.clone())
            .save(&[PersistedTaskRecord {
                task: Task {
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
                },
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
                }),
            }])
            .expect("task state should persist");

        let records = TaskStateStore::new(path)
            .load()
            .expect("task state should reload");

        assert_eq!(1, records.len());
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
    }
}
