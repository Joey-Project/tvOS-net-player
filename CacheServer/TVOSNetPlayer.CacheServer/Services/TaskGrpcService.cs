using System.Threading.Channels;
using Grpc.Core;
using TVOSNetPlayer.Cache.V1;
using CacheTask = TVOSNetPlayer.Cache.V1.Task;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class TaskGrpcService : TaskService.TaskServiceBase
{
    private readonly BilibiliTaskRegistry tasks;

    public TaskGrpcService(BilibiliTaskRegistry tasks)
    {
        this.tasks = tasks;
    }

    public override System.Threading.Tasks.Task<CacheTask> CreateBilibiliTask(CreateBilibiliTaskRequest request, ServerCallContext context)
    {
        var source = request.UrlOrId.Trim();
        if (source.Length == 0)
        {
            throw InvalidArgument("Bilibili URL or id is required.");
        }

        return System.Threading.Tasks.Task.FromResult(tasks.CreateBilibiliTask(source, request.Options));
    }

    public override System.Threading.Tasks.Task<CacheTask> GetTask(GetTaskRequest request, ServerCallContext context)
    {
        var id = NormalizeRequiredId(request.Id);
        if (!tasks.TryGetTask(id, out var task))
        {
            throw TaskNotFound();
        }

        return System.Threading.Tasks.Task.FromResult(task);
    }

    public override async System.Threading.Tasks.Task WatchTasks(WatchTasksRequest request, IServerStreamWriter<TaskEvent> responseStream, ServerCallContext context)
    {
        await using var subscription = tasks.Subscribe(NormalizeWatchIds(request.Ids));
        foreach (var task in subscription.Snapshots)
        {
            await responseStream.WriteAsync(new TaskEvent
            {
                Task = task
            });
        }

        try
        {
            await foreach (var task in subscription.Reader.ReadAllAsync(context.CancellationToken))
            {
                await responseStream.WriteAsync(new TaskEvent
                {
                    Task = task
                });
            }
        }
        catch (OperationCanceledException) when (context.CancellationToken.IsCancellationRequested)
        {
        }
        catch (ChannelClosedException exception) when (exception.InnerException is BilibiliTaskRegistry.TaskWatcherOverflowException)
        {
            throw new RpcException(new Status(StatusCode.ResourceExhausted, "Task watcher fell behind."));
        }
        catch (BilibiliTaskRegistry.TaskWatcherOverflowException)
        {
            throw new RpcException(new Status(StatusCode.ResourceExhausted, "Task watcher fell behind."));
        }
    }

    public override System.Threading.Tasks.Task<CacheTask> CancelTask(CancelTaskRequest request, ServerCallContext context)
    {
        var id = NormalizeRequiredId(request.Id);
        if (!tasks.TryCancelTask(id, out var task))
        {
            throw TaskNotFound();
        }

        return System.Threading.Tasks.Task.FromResult(task);
    }

    private static string NormalizeRequiredId(string id)
    {
        var normalizedId = id.Trim();
        if (normalizedId.Length == 0)
        {
            throw InvalidArgument("Task id is required.");
        }

        return normalizedId;
    }

    private static IReadOnlyCollection<string> NormalizeWatchIds(IEnumerable<string> ids)
    {
        var normalizedIds = new List<string>();
        foreach (var id in ids)
        {
            var normalizedId = id.Trim();
            if (normalizedId.Length == 0)
            {
                throw InvalidArgument("Task id filter cannot be empty.");
            }

            normalizedIds.Add(normalizedId);
        }

        return normalizedIds;
    }

    private static RpcException InvalidArgument(string message)
    {
        return new RpcException(new Status(StatusCode.InvalidArgument, message));
    }

    private static RpcException TaskNotFound()
    {
        return new RpcException(new Status(StatusCode.NotFound, "Task not found."));
    }
}
