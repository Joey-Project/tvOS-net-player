use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use prost_types::Timestamp;
use serde::{Deserialize, Serialize};

use crate::generated::tvos_net_player::v1::{BilibiliDownloadOptions, Task};

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
    bilibili_options: Option<PersistedBilibiliDownloadOptions>,
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
            bilibili_options: record.options.map(PersistedBilibiliDownloadOptions::from),
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
            },
            options: file.bilibili_options.map(BilibiliDownloadOptions::from),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTimestamp {
    seconds: i64,
    nanos: i32,
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
}

impl From<BilibiliDownloadOptions> for PersistedBilibiliDownloadOptions {
    fn from(options: BilibiliDownloadOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            download_subtitles: options.download_subtitles,
            download_danmaku: options.download_danmaku,
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
