using Grpc.Core;
using TVOSNetPlayer.Cache.V1;
using CacheTask = TVOSNetPlayer.Cache.V1.Task;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class TaskGrpcService : TaskService.TaskServiceBase
{
    public override System.Threading.Tasks.Task<CacheTask> CreateBilibiliTask(CreateBilibiliTaskRequest request, ServerCallContext context)
    {
        throw new RpcException(new Status(StatusCode.Unimplemented, "Bilibili task adapter is not implemented in this slice."));
    }

    public override System.Threading.Tasks.Task<CacheTask> GetTask(GetTaskRequest request, ServerCallContext context)
    {
        throw new RpcException(new Status(StatusCode.NotFound, "Task not found."));
    }

    public override async System.Threading.Tasks.Task WatchTasks(WatchTasksRequest request, IServerStreamWriter<TaskEvent> responseStream, ServerCallContext context)
    {
        await System.Threading.Tasks.Task.CompletedTask;
    }

    public override System.Threading.Tasks.Task<CacheTask> CancelTask(CancelTaskRequest request, ServerCallContext context)
    {
        throw new RpcException(new Status(StatusCode.Unimplemented, "Task cancellation is not implemented in this slice."));
    }
}
