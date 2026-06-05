using Grpc.Core;
using TVOSNetPlayer.Cache.V1;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class CacheGrpcService : CacheService.CacheServiceBase
{
    private readonly LocalMediaLibrary library;

    public CacheGrpcService(LocalMediaLibrary library)
    {
        this.library = library;
    }

    public override async System.Threading.Tasks.Task<ListCacheRootsResponse> ListCacheRoots(ListCacheRootsRequest request, ServerCallContext context)
    {
        var response = new ListCacheRootsResponse();
        response.Roots.Add(await library.GetCacheRootAsync(context.CancellationToken));
        return response;
    }

    public override System.Threading.Tasks.Task<DeleteLibraryItemResponse> DeleteLibraryItem(DeleteLibraryItemRequest request, ServerCallContext context)
    {
        throw new RpcException(new Status(StatusCode.Unimplemented, "Cache deletion is not implemented in this slice."));
    }
}
