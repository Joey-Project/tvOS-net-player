using Grpc.Core;
using TVOSNetPlayer.Cache.V1;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class LibraryGrpcService : LibraryService.LibraryServiceBase
{
    private readonly LocalMediaLibrary library;
    private readonly PlaybackUriFactory playbackUriFactory;

    public LibraryGrpcService(LocalMediaLibrary library, PlaybackUriFactory playbackUriFactory)
    {
        this.library = library;
        this.playbackUriFactory = playbackUriFactory;
    }

    public override async System.Threading.Tasks.Task<ListLibraryItemsResponse> ListLibraryItems(ListLibraryItemsRequest request, ServerCallContext context)
    {
        var pageSize = request.PageSize <= 0 ? 50 : Math.Min(request.PageSize, 200);
        var requestedPageOffset = DecodePageToken(request.PageToken);
        var page = await library.ListItemsPageAsync(request.Filter, requestedPageOffset, pageSize, context.CancellationToken);

        var response = new ListLibraryItemsResponse();
        response.Items.AddRange(page.Items);

        if (page.NextPageOffset is { } nextPageOffset)
        {
            response.NextPageToken = nextPageOffset.ToString(System.Globalization.CultureInfo.InvariantCulture);
        }

        return response;
    }

    public override async System.Threading.Tasks.Task<LibraryItem> GetLibraryItem(GetLibraryItemRequest request, ServerCallContext context)
    {
        var item = await library.GetItemAsync(request.Id, context.CancellationToken);
        if (item is null)
        {
            throw new RpcException(new Status(StatusCode.NotFound, "Library item not found."));
        }

        return item;
    }

    public override async System.Threading.Tasks.Task<PlaybackSource> GetPlaybackSource(GetPlaybackSourceRequest request, ServerCallContext context)
    {
        if (!library.SupportsHttpRangePlayback)
        {
            throw new RpcException(new Status(StatusCode.FailedPrecondition, "HTTP range playback is unavailable on this platform."));
        }

        var mediaFile = await library.GetMediaFileAsync(request.ItemId, request.VariantId, context.CancellationToken);
        if (mediaFile is null)
        {
            throw new RpcException(new Status(StatusCode.NotFound, "Playback source not found."));
        }

        return new PlaybackSource
        {
            ItemId = request.ItemId,
            VariantId = request.VariantId,
            Protocol = PlaybackProtocol.HttpFile,
            Uri = playbackUriFactory.Create(context, request.ItemId, request.VariantId)
        };
    }

    public override async System.Threading.Tasks.Task<RescanLibraryResponse> RescanLibrary(RescanLibraryRequest request, ServerCallContext context)
    {
        return new RescanLibraryResponse
        {
            DiscoveredItemCount = await library.CountItemsAsync(context.CancellationToken)
        };
    }

    private static long DecodePageToken(string pageToken)
    {
        if (string.IsNullOrWhiteSpace(pageToken))
        {
            return 0;
        }

        return long.TryParse(pageToken, out var offset) && offset > 0 ? offset : 0;
    }
}
