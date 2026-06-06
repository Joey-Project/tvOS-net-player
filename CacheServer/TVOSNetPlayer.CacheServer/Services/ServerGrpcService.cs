using Google.Protobuf.WellKnownTypes;
using Grpc.Core;
using Microsoft.Extensions.Options;
using TVOSNetPlayer.Cache.V1;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class ServerGrpcService : ServerService.ServerServiceBase
{
    private readonly IOptionsMonitor<CacheServerOptions> options;
    private readonly LocalMediaLibrary library;

    public ServerGrpcService(IOptionsMonitor<CacheServerOptions> options, LocalMediaLibrary library)
    {
        this.options = options;
        this.library = library;
    }

    public override System.Threading.Tasks.Task<ServerInfo> GetServerInfo(GetServerInfoRequest request, ServerCallContext context)
    {
        var serverInfo = new ServerInfo
        {
            Id = options.CurrentValue.ServerId,
            Name = options.CurrentValue.ServerName,
            Version = "0.1.0"
        };

        if (library.SupportsHttpRangePlayback && !string.IsNullOrWhiteSpace(options.CurrentValue.PublicMediaBaseUri))
        {
            serverInfo.MediaBaseUris.Add(options.CurrentValue.PublicMediaBaseUri);
        }

        serverInfo.Capabilities.Add(ServerCapability.BilibiliTasks);

        if (library.SupportsHttpRangePlayback)
        {
            serverInfo.Capabilities.Add(ServerCapability.HttpRange);
        }

        return System.Threading.Tasks.Task.FromResult(serverInfo);
    }

    public override System.Threading.Tasks.Task<HealthStatus> CheckHealth(CheckHealthRequest request, ServerCallContext context)
    {
        var rootAvailable = library.IsRootAvailable();
        return System.Threading.Tasks.Task.FromResult(new HealthStatus
        {
            State = rootAvailable ? HealthState.Serving : HealthState.Degraded,
            Message = rootAvailable ? "Cache root is available." : "Cache root is unavailable.",
            CheckedAt = Timestamp.FromDateTime(DateTime.UtcNow)
        });
    }
}
