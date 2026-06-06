using System.Threading.Channels;
using Google.Protobuf.WellKnownTypes;
using TVOSNetPlayer.Cache.V1;
using CacheTask = TVOSNetPlayer.Cache.V1.Task;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class BilibiliTaskRegistry
{
    private const string QueuedMessage = "Queued for the BBDown adapter.";
    private const string CancelledMessage = "Cancelled before the download adapter started.";
    private const int WatcherEventBufferCapacity = 128;

    private readonly Lock syncRoot = new();
    private readonly Dictionary<string, TaskRecord> tasksById = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> activeTaskIdsBySource = new(StringComparer.Ordinal);
    private readonly List<TaskWatcher> watchers = [];

    public CacheTask CreateBilibiliTask(string source, BilibiliDownloadOptions? options)
    {
        var normalizedSource = NormalizeSource(source);
        if (normalizedSource.Length == 0)
        {
            throw new ArgumentException("Bilibili URL or id is required.", nameof(source));
        }

        lock (syncRoot)
        {
            if (activeTaskIdsBySource.TryGetValue(normalizedSource, out var activeTaskId)
                && tasksById.TryGetValue(activeTaskId, out var activeTask)
                && activeTask.IsActive)
            {
                return activeTask.ToProto();
            }

            var now = CurrentTimestamp();
            var record = new TaskRecord
            {
                Id = $"bilibili-{Guid.NewGuid():N}",
                Source = normalizedSource,
                Options = options?.Clone() ?? new BilibiliDownloadOptions(),
                State = TaskState.Queued,
                Message = QueuedMessage,
                CreatedAt = now,
                UpdatedAt = now
            };

            tasksById.Add(record.Id, record);
            activeTaskIdsBySource[record.Source] = record.Id;

            var task = record.ToProto();
            NotifyLocked(task);
            return task;
        }
    }

    public bool TryGetTask(string id, out CacheTask task)
    {
        lock (syncRoot)
        {
            if (tasksById.TryGetValue(id, out var record))
            {
                task = record.ToProto();
                return true;
            }
        }

        task = new CacheTask();
        return false;
    }

    public bool TryCancelTask(string id, out CacheTask task)
    {
        lock (syncRoot)
        {
            if (!tasksById.TryGetValue(id, out var record))
            {
                task = new CacheTask();
                return false;
            }

            if (record.IsTerminal)
            {
                task = record.ToProto();
                return true;
            }

            record.State = TaskState.Cancelled;
            record.Message = CancelledMessage;
            record.UpdatedAt = CurrentTimestamp();
            record.FinishedAt = record.UpdatedAt;

            if (activeTaskIdsBySource.TryGetValue(record.Source, out var activeTaskId)
                && string.Equals(activeTaskId, record.Id, StringComparison.Ordinal))
            {
                activeTaskIdsBySource.Remove(record.Source);
            }

            task = record.ToProto();
            NotifyLocked(task);
            return true;
        }
    }

    public TaskSubscription Subscribe(IEnumerable<string> ids)
    {
        var watchedIds = new HashSet<string>(StringComparer.Ordinal);
        foreach (var id in ids.Select(NormalizeId))
        {
            if (id.Length == 0)
            {
                throw new ArgumentException("Task id filter cannot be empty.", nameof(ids));
            }

            watchedIds.Add(id);
        }

        var watcher = new TaskWatcher(watchedIds);
        List<CacheTask> snapshots;

        lock (syncRoot)
        {
            watchers.Add(watcher);
            snapshots = tasksById.Values
                .Where(record => watcher.Matches(record.Id))
                .Select(record => record.ToProto())
                .ToList();
        }

        return new TaskSubscription(this, watcher, snapshots);
    }

    private void RemoveWatcher(TaskWatcher watcher)
    {
        lock (syncRoot)
        {
            watchers.Remove(watcher);
        }

        watcher.Complete();
    }

    private void NotifyLocked(CacheTask task)
    {
        List<TaskWatcher>? overflowedWatchers = null;
        foreach (var watcher in watchers)
        {
            if (watcher.Matches(task.Id) && !watcher.TryWrite(task.Clone()))
            {
                overflowedWatchers ??= [];
                overflowedWatchers.Add(watcher);
            }
        }

        if (overflowedWatchers is null)
        {
            return;
        }

        foreach (var watcher in overflowedWatchers)
        {
            watchers.Remove(watcher);
            watcher.Complete(new TaskWatcherOverflowException());
        }
    }

    private static string NormalizeSource(string source)
    {
        return source.Trim();
    }

    private static string NormalizeId(string id)
    {
        return id.Trim();
    }

    private static Timestamp CurrentTimestamp()
    {
        return Timestamp.FromDateTime(DateTime.UtcNow);
    }

    public sealed class TaskSubscription : IAsyncDisposable
    {
        private readonly BilibiliTaskRegistry owner;
        private readonly TaskWatcher watcher;

        internal TaskSubscription(BilibiliTaskRegistry owner, TaskWatcher watcher, IReadOnlyList<CacheTask> snapshots)
        {
            this.owner = owner;
            this.watcher = watcher;
            Snapshots = snapshots;
        }

        public IReadOnlyList<CacheTask> Snapshots { get; }

        public ChannelReader<CacheTask> Reader => watcher.Reader;

        public ValueTask DisposeAsync()
        {
            owner.RemoveWatcher(watcher);
            return default;
        }
    }

    internal sealed class TaskWatcher
    {
        private readonly HashSet<string> ids;
        private readonly Channel<CacheTask> channel = Channel.CreateBounded<CacheTask>(
            new BoundedChannelOptions(WatcherEventBufferCapacity)
            {
                SingleReader = true,
                SingleWriter = false,
                FullMode = BoundedChannelFullMode.Wait
            });

        public TaskWatcher(HashSet<string> ids)
        {
            this.ids = ids;
        }

        public ChannelReader<CacheTask> Reader => channel.Reader;

        public bool Matches(string id)
        {
            return ids.Count == 0 || ids.Contains(id);
        }

        public bool TryWrite(CacheTask task)
        {
            return channel.Writer.TryWrite(task);
        }

        public void Complete(Exception? exception = null)
        {
            channel.Writer.TryComplete(exception);
        }
    }

    internal sealed class TaskWatcherOverflowException : Exception
    {
        public TaskWatcherOverflowException()
            : base("Task watcher event buffer is full.")
        {
        }
    }

    private sealed class TaskRecord
    {
        public required string Id { get; init; }
        public required string Source { get; init; }
        public required BilibiliDownloadOptions Options { get; init; }
        public TaskState State { get; set; }
        public string Message { get; set; } = string.Empty;
        public required Timestamp CreatedAt { get; init; }
        public required Timestamp UpdatedAt { get; set; }
        public Timestamp? FinishedAt { get; set; }

        public bool IsActive => State is TaskState.Queued or TaskState.Running or TaskState.CancelRequested;

        public bool IsTerminal => State is TaskState.Succeeded or TaskState.Failed or TaskState.Cancelled;

        public CacheTask ToProto()
        {
            var task = new CacheTask
            {
                Id = Id,
                Kind = TaskKind.BilibiliDownload,
                State = State,
                Source = Source,
                Progress = 0,
                Message = Message,
                CreatedAt = CreatedAt.Clone(),
                UpdatedAt = UpdatedAt.Clone()
            };

            if (FinishedAt is not null)
            {
                task.FinishedAt = FinishedAt.Clone();
            }

            return task;
        }
    }
}
