using System.Net;
using System.Net.Http.Headers;
using System.Net.Sockets;
using Grpc.Core;
using Grpc.Net.Client;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Options;
using TVOSNetPlayer.Cache.V1;
using TVOSNetPlayer.CacheServer;
using TVOSNetPlayer.CacheServer.Services;

AppContext.SetSwitch("System.Net.Http.SocketsHttpHandler.Http2UnencryptedSupport", true);

var tempRoot = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-tests-{Guid.NewGuid():N}");
var outsidePath = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-outside-{Guid.NewGuid():N}.mp4");
var outsideDirectory = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-outside-dir-{Guid.NewGuid():N}");
var symlinkRootTarget = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-root-target-{Guid.NewGuid():N}");
var symlinkRoot = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-linked-root-{Guid.NewGuid():N}");
Directory.CreateDirectory(tempRoot);

try
{
    var moviePath = Path.Combine(tempRoot, "Movies", "Sample Clip.mp4");
    Directory.CreateDirectory(Path.GetDirectoryName(moviePath)!);
    await File.WriteAllTextAsync(moviePath, "0123456789abcdef");
    await File.WriteAllTextAsync(outsidePath, "outside-cache-root");
    File.CreateSymbolicLink(Path.Combine(tempRoot, "Movies", "Linked Outside.mp4"), outsidePath);
    if (!OperatingSystem.IsWindows())
    {
        File.CreateSymbolicLink(Path.Combine(tempRoot, "Movies", "Linked\\Outside.mp4"), outsidePath);
    }

    Directory.CreateDirectory(outsideDirectory);
    await File.WriteAllTextAsync(Path.Combine(outsideDirectory, "Directory Escape.mp4"), "outside-cache-root-directory");
    Directory.CreateSymbolicLink(Path.Combine(tempRoot, "Linked Directory"), outsideDirectory);

    var grpcAddress = $"http://127.0.0.1:{GetFreePort()}";
    var mediaAddress = $"http://127.0.0.1:{GetFreePort()}";

    await using var app = CacheServerHost.Create(
    [
        "--Cache:GrpcListenUrl",
        grpcAddress,
        "--Cache:MediaListenUrl",
        mediaAddress,
        "--Cache:RootPath",
        tempRoot,
        "--Cache:ServerName",
        "Test Cache",
        "--Logging:LogLevel:Default",
        "Warning"
    ]);

    await app.StartAsync();

    using var channel = GrpcChannel.ForAddress(grpcAddress, new GrpcChannelOptions
    {
        HttpHandler = new SocketsHttpHandler
        {
            EnableMultipleHttp2Connections = true
        }
    });

    await AssertServerInfoAsync(channel);
    var item = await AssertLibraryAsync(channel);
    await AssertLargePageTokenDoesNotOverflowAsync(channel);
    await AssertPaginatedLibraryAsync(channel, tempRoot);
    await AssertListCancellationAsync(tempRoot);
    await AssertListSkipsDeletedFileDuringMaterializationAsync();
    await AssertGetMediaFileReturnsNullWhenFileDisappearsAsync();
    await AssertGetLibraryItemReturnsNullWhenFileDisappearsDuringMaterializationAsync();
    var playback = await AssertPlaybackSourceAsync(channel, item);
    await AssertHttpRangePlaybackAsync(playback.Uri);
    await AssertHttpHeadPlaybackAsync(playback.Uri);
    await AssertMediaEndpointIsNotServedFromGrpcPortAsync(grpcAddress, item);
    await AssertMediaControlPlaneUnavailableWhenSecureOpenUnsupportedAsync(app.Services, channel, item);
    await AssertWatchTasksIsUnimplementedAsync(channel);
    await AssertTraversalIsRejectedAsync(mediaAddress);
    await AssertInvalidPathItemIdIsRejectedAsync(channel, mediaAddress);
    await AssertSymlinkIsRejectedAsync(channel, mediaAddress);
    await AssertSpecialMediaFileIsRejectedAsync(tempRoot);
    await AssertSymlinkSwapBeforeOpenIsRejectedAsync(tempRoot, outsidePath);
    await AssertParentSymlinkSwapBeforeOpenIsRejectedAsync(tempRoot);
    await AssertSecureMediaOpenUnavailableFailsClosedAsync(tempRoot);
    await AssertReadOnlyRootReportsNotWritableAsync(channel, tempRoot);
    AssertPlaybackBaseUriFormatting();
    AssertListenHostNormalization();
    AssertDefaultListenUrlsAreLoopback();
    await AssertListenUrlsMustBeCleartextAsync(tempRoot);
    await AssertSymlinkRootIsRejectedAsync(symlinkRootTarget, symlinkRoot);

    Console.WriteLine("Cache server integration tests passed.");
}
finally
{
    Directory.Delete(tempRoot, recursive: true);
    if (Directory.Exists(symlinkRoot))
    {
        Directory.Delete(symlinkRoot);
    }

    if (Directory.Exists(symlinkRootTarget))
    {
        Directory.Delete(symlinkRootTarget, recursive: true);
    }

    if (Directory.Exists(outsideDirectory))
    {
        Directory.Delete(outsideDirectory, recursive: true);
    }

    if (File.Exists(outsidePath))
    {
        File.Delete(outsidePath);
    }
}

static async System.Threading.Tasks.Task AssertServerInfoAsync(GrpcChannel channel)
{
    var client = new ServerService.ServerServiceClient(channel);
    var info = await client.GetServerInfoAsync(new GetServerInfoRequest());

    AssertEqual("Test Cache", info.Name, "server name");
    AssertTrue(info.Capabilities.Contains(ServerCapability.HttpRange), "server advertises HTTP range playback");
}

static async System.Threading.Tasks.Task<LibraryItem> AssertLibraryAsync(GrpcChannel channel)
{
    var client = new LibraryService.LibraryServiceClient(channel);
    var response = await client.ListLibraryItemsAsync(new ListLibraryItemsRequest());

    AssertEqual(1, response.Items.Count, "library item count");
    var item = response.Items[0];
    AssertEqual("Sample Clip", item.Title, "library title");
    AssertEqual("Movies/Sample Clip.mp4", item.Subtitle, "library subtitle");
    AssertEqual(LibrarySource.LocalCache, item.Source, "library source");
    AssertEqual(1, item.Variants.Count, "variant count");
    AssertEqual("original", item.Variants[0].Id, "variant id");
    AssertEqual(PlaybackProtocol.HttpFile, item.Variants[0].Protocol, "variant protocol");
    AssertEqual("mp4", item.Variants[0].Container, "variant container");

    return item;
}

static async System.Threading.Tasks.Task AssertLargePageTokenDoesNotOverflowAsync(GrpcChannel channel)
{
    var client = new LibraryService.LibraryServiceClient(channel);
    foreach (var pageToken in new[]
    {
        int.MaxValue.ToString(System.Globalization.CultureInfo.InvariantCulture),
        long.MaxValue.ToString(System.Globalization.CultureInfo.InvariantCulture)
    })
    {
        var response = await client.ListLibraryItemsAsync(new ListLibraryItemsRequest
        {
            PageSize = 200,
            PageToken = pageToken
        });

        AssertEqual(0, response.Items.Count, $"large page token {pageToken} item count");
        AssertEqual("", response.NextPageToken, $"large page token {pageToken} next page");
    }
}

static async System.Threading.Tasks.Task AssertPaginatedLibraryAsync(GrpcChannel channel, string rootPath)
{
    var moviesPath = Path.Combine(rootPath, "Movies");
    await File.WriteAllTextAsync(Path.Combine(moviesPath, "Aardvark Clip.mp4"), "aardvark");
    await File.WriteAllTextAsync(Path.Combine(moviesPath, "Zulu Clip.mp4"), "zulu");

    var client = new LibraryService.LibraryServiceClient(channel);
    var firstPage = await client.ListLibraryItemsAsync(new ListLibraryItemsRequest
    {
        PageSize = 1
    });

    AssertEqual(1, firstPage.Items.Count, "first page item count");
    AssertEqual("Aardvark Clip", firstPage.Items[0].Title, "first page title");
    AssertTrue(!string.IsNullOrEmpty(firstPage.NextPageToken), "first page next token");

    var secondPage = await client.ListLibraryItemsAsync(new ListLibraryItemsRequest
    {
        PageSize = 1,
        PageToken = firstPage.NextPageToken
    });

    AssertEqual(1, secondPage.Items.Count, "second page item count");
    AssertEqual("Sample Clip", secondPage.Items[0].Title, "second page title");
}

static async System.Threading.Tasks.Task AssertListCancellationAsync(string rootPath)
{
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }));
    using var cancellation = new CancellationTokenSource();
    await cancellation.CancelAsync();

    try
    {
        await library.ListItemsPageAsync(null, 0, 1, cancellation.Token);
        throw new InvalidOperationException("ListItemsPageAsync unexpectedly ignored cancellation.");
    }
    catch (OperationCanceledException)
    {
    }
}

static async System.Threading.Tasks.Task AssertListSkipsDeletedFileDuringMaterializationAsync()
{
    var rootPath = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-list-race-{Guid.NewGuid():N}");
    Directory.CreateDirectory(rootPath);
    var mediaPath = Path.Combine(rootPath, "A Vanishing Clip.mp4");
    var survivorPath = Path.Combine(rootPath, "B Survivor Clip.mp4");
    await File.WriteAllTextAsync(mediaPath, "vanishing");
    await File.WriteAllTextAsync(survivorPath, "survivor");
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }))
    {
        BeforeCreateLibraryItemForTests = path =>
        {
            if (path == mediaPath)
            {
                File.Delete(path);
            }
        }
    };

    try
    {
        var page = await library.ListItemsPageAsync(null, 0, 1, CancellationToken.None);
        AssertEqual(1, page.Items.Count, "vanishing list refill item count");
        AssertEqual("B Survivor Clip", page.Items[0].Title, "vanishing list refill title");
        AssertEqual<long?>(null, page.NextPageOffset, "vanishing list refill next page");
    }
    finally
    {
        if (Directory.Exists(rootPath))
        {
            Directory.Delete(rootPath, recursive: true);
        }
    }
}

static async System.Threading.Tasks.Task AssertGetMediaFileReturnsNullWhenFileDisappearsAsync()
{
    var rootPath = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-stat-race-{Guid.NewGuid():N}");
    Directory.CreateDirectory(rootPath);
    var mediaPath = Path.Combine(rootPath, "Vanishing Playback.mp4");
    await File.WriteAllTextAsync(mediaPath, "vanishing");
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }))
    {
        BeforeCreateMediaFileForTests = path =>
        {
            AssertEqual(mediaPath, path, "media file materialization hook media path");
            File.Delete(path);
        }
    };

    try
    {
        var mediaFile = await library.GetMediaFileAsync(CreateLocalItemId("Vanishing Playback.mp4"), "original", CancellationToken.None);
        AssertEqual<MediaFile?>(null, mediaFile, "vanishing media file");
    }
    finally
    {
        if (Directory.Exists(rootPath))
        {
            Directory.Delete(rootPath, recursive: true);
        }
    }
}

static async System.Threading.Tasks.Task AssertGetLibraryItemReturnsNullWhenFileDisappearsDuringMaterializationAsync()
{
    var rootPath = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-get-item-race-{Guid.NewGuid():N}");
    Directory.CreateDirectory(rootPath);
    var mediaPath = Path.Combine(rootPath, "Vanishing Detail.mp4");
    await File.WriteAllTextAsync(mediaPath, "vanishing");
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }))
    {
        BeforeCreateLibraryItemForTests = path =>
        {
            AssertEqual(mediaPath, path, "get item materialization hook media path");
            File.Delete(path);
        }
    };

    try
    {
        var item = await library.GetItemAsync(CreateLocalItemId("Vanishing Detail.mp4"), CancellationToken.None);
        AssertEqual<LibraryItem?>(null, item, "vanishing library item");
    }
    finally
    {
        if (Directory.Exists(rootPath))
        {
            Directory.Delete(rootPath, recursive: true);
        }
    }
}

static async System.Threading.Tasks.Task<PlaybackSource> AssertPlaybackSourceAsync(GrpcChannel channel, LibraryItem item)
{
    var client = new LibraryService.LibraryServiceClient(channel);
    var playback = await client.GetPlaybackSourceAsync(new GetPlaybackSourceRequest
    {
        ItemId = item.Id,
        VariantId = "original"
    });

    AssertEqual(item.Id, playback.ItemId, "playback item id");
    AssertEqual("original", playback.VariantId, "playback variant id");
    AssertEqual(PlaybackProtocol.HttpFile, playback.Protocol, "playback protocol");
    AssertTrue(playback.Uri.StartsWith("http://127.0.0.1:", StringComparison.Ordinal), "playback URL uses local HTTP endpoint");
    AssertTrue(playback.Uri.Contains($"/media/{Uri.EscapeDataString(item.Id)}/original", StringComparison.Ordinal), "playback URL points at media endpoint");

    return playback;
}

static async System.Threading.Tasks.Task AssertHttpRangePlaybackAsync(string playbackUri)
{
    using var httpClient = new HttpClient();
    using var request = new HttpRequestMessage(HttpMethod.Get, playbackUri);
    request.Headers.Range = new RangeHeaderValue(2, 5);

    using var response = await httpClient.SendAsync(request);
    var body = await response.Content.ReadAsStringAsync();

    AssertEqual(HttpStatusCode.PartialContent, response.StatusCode, "range status");
    AssertEqual("2345", body, "range body");
    AssertEqual("bytes", response.Headers.AcceptRanges.Single(), "accept-ranges header");
    AssertEqual("video/mp4", response.Content.Headers.ContentType?.MediaType, "content type");
}

static async System.Threading.Tasks.Task AssertHttpHeadPlaybackAsync(string playbackUri)
{
    using var httpClient = new HttpClient();
    using var request = new HttpRequestMessage(HttpMethod.Head, playbackUri);
    using var response = await httpClient.SendAsync(request);
    var body = await response.Content.ReadAsByteArrayAsync();

    AssertEqual(HttpStatusCode.OK, response.StatusCode, "HEAD status");
    AssertEqual("bytes", response.Headers.AcceptRanges.Single(), "HEAD accept-ranges header");
    AssertEqual("video/mp4", response.Content.Headers.ContentType?.MediaType, "HEAD content type");
    AssertEqual(0, body.Length, "HEAD body length");
}

static async System.Threading.Tasks.Task AssertMediaEndpointIsNotServedFromGrpcPortAsync(string grpcAddress, LibraryItem item)
{
    using var httpClient = new HttpClient(new SocketsHttpHandler
    {
        EnableMultipleHttp2Connections = true
    });
    using var request = new HttpRequestMessage(HttpMethod.Get, $"{grpcAddress}/media/{Uri.EscapeDataString(item.Id)}/original")
    {
        Version = HttpVersion.Version20,
        VersionPolicy = HttpVersionPolicy.RequestVersionExact
    };

    using var response = await httpClient.SendAsync(request);
    AssertEqual(HttpStatusCode.NotFound, response.StatusCode, "media lookup on gRPC listener");
}

static async System.Threading.Tasks.Task AssertMediaControlPlaneUnavailableWhenSecureOpenUnsupportedAsync(IServiceProvider services, GrpcChannel channel, LibraryItem item)
{
    var library = services.GetRequiredService<LocalMediaLibrary>();
    var serverClient = new ServerService.ServerServiceClient(channel);
    var libraryClient = new LibraryService.LibraryServiceClient(channel);
    library.IsSecureNoFollowOpenSupportedForTests = () => false;

    try
    {
        var serverInfo = await serverClient.GetServerInfoAsync(new GetServerInfoRequest());
        AssertTrue(!serverInfo.Capabilities.Contains(ServerCapability.HttpRange), "unsupported secure open omits HTTP range capability");

        var listResponse = await libraryClient.ListLibraryItemsAsync(new ListLibraryItemsRequest());
        var listedItem = listResponse.Items.Single(candidate => candidate.Id == item.Id);
        AssertEqual(0, listedItem.Variants.Count, "unsupported secure open variants");

        try
        {
            await libraryClient.GetPlaybackSourceAsync(new GetPlaybackSourceRequest
            {
                ItemId = item.Id,
                VariantId = "original"
            });
            throw new InvalidOperationException("GetPlaybackSource unexpectedly returned a URL when secure no-follow open is unavailable.");
        }
        catch (RpcException exception) when (exception.StatusCode == StatusCode.FailedPrecondition)
        {
        }
    }
    finally
    {
        library.IsSecureNoFollowOpenSupportedForTests = null;
    }

    var restoredServerInfo = await serverClient.GetServerInfoAsync(new GetServerInfoRequest());
    AssertTrue(restoredServerInfo.Capabilities.Contains(ServerCapability.HttpRange), "restored HTTP range capability");
}

static async System.Threading.Tasks.Task AssertWatchTasksIsUnimplementedAsync(GrpcChannel channel)
{
    var client = new TaskService.TaskServiceClient(channel);
    using var call = client.WatchTasks(new WatchTasksRequest());

    try
    {
        await call.ResponseStream.MoveNext(CancellationToken.None);
        throw new InvalidOperationException("WatchTasks unexpectedly returned a successful stream.");
    }
    catch (RpcException exception) when (exception.StatusCode == StatusCode.Unimplemented)
    {
    }
}

static async System.Threading.Tasks.Task AssertTraversalIsRejectedAsync(string address)
{
    var uri = $"{address}/media/{CreateLocalItemId("../secret.mp4")}/original";

    using var httpClient = new HttpClient();
    using var response = await httpClient.GetAsync(uri);

    AssertEqual(HttpStatusCode.NotFound, response.StatusCode, "path traversal media lookup");
}

static async System.Threading.Tasks.Task AssertInvalidPathItemIdIsRejectedAsync(GrpcChannel channel, string address)
{
    var invalidItemId = CreateLocalItemId("bad\0path.mp4");
    var client = new LibraryService.LibraryServiceClient(channel);

    try
    {
        await client.GetPlaybackSourceAsync(new GetPlaybackSourceRequest
        {
            ItemId = invalidItemId,
            VariantId = "original"
        });
        throw new InvalidOperationException("invalid path item id unexpectedly returned a playback source");
    }
    catch (RpcException exception) when (exception.StatusCode == StatusCode.NotFound)
    {
    }

    using var httpClient = new HttpClient();
    using var response = await httpClient.GetAsync($"{address}/media/{invalidItemId}/original");

    AssertEqual(HttpStatusCode.NotFound, response.StatusCode, "invalid path media lookup");
}

static async System.Threading.Tasks.Task AssertSymlinkIsRejectedAsync(GrpcChannel channel, string address)
{
    var client = new LibraryService.LibraryServiceClient(channel);
    var response = await client.ListLibraryItemsAsync(new ListLibraryItemsRequest());
    AssertTrue(response.Items.All(item => item.Title != "Linked Outside"), "symlink is hidden from library");

    var uri = $"{address}/media/{CreateLocalItemId("Movies/Linked Outside.mp4")}/original";

    using var httpClient = new HttpClient();
    using var mediaResponse = await httpClient.GetAsync(uri);

    AssertEqual(HttpStatusCode.NotFound, mediaResponse.StatusCode, "symlink media lookup");

    if (!OperatingSystem.IsWindows())
    {
        AssertTrue(response.Items.All(item => item.SourceId != "Movies/Linked\\Outside.mp4"), "backslash symlink is hidden from library");

        var backslashSymlinkUri = $"{address}/media/{CreateLocalItemId("Movies/Linked\\Outside.mp4")}/original";
        using var backslashSymlinkResponse = await httpClient.GetAsync(backslashSymlinkUri);

        AssertEqual(HttpStatusCode.NotFound, backslashSymlinkResponse.StatusCode, "backslash symlink media lookup");
    }

    var directorySymlinkUri = $"{address}/media/{CreateLocalItemId("Linked Directory/Directory Escape.mp4")}/original";
    using var directorySymlinkResponse = await httpClient.GetAsync(directorySymlinkUri);

    AssertEqual(HttpStatusCode.NotFound, directorySymlinkResponse.StatusCode, "symlink directory media lookup");
}

static async System.Threading.Tasks.Task AssertSpecialMediaFileIsRejectedAsync(string rootPath)
{
    if (!OperatingSystem.IsMacOS() && !OperatingSystem.IsLinux())
    {
        return;
    }

    var fifoPath = Path.Combine(rootPath, "Movies", "Named Pipe.mp4");
    if (MkFifo(fifoPath, 0x1A4) != 0)
    {
        throw new InvalidOperationException($"mkfifo failed: errno={System.Runtime.InteropServices.Marshal.GetLastWin32Error()}");
    }

    var beforeOpenCalled = false;
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }))
    {
        BeforeOpenMediaFileForTests = path =>
        {
            beforeOpenCalled = true;
            AssertEqual(fifoPath, path, "special file hook media path");
        }
    };

    try
    {
        var openedFile = await library
            .OpenMediaFileAsync(CreateLocalItemId("Movies/Named Pipe.mp4"), "original", CancellationToken.None)
            .WaitAsync(TimeSpan.FromSeconds(2));
        if (openedFile is not null)
        {
            openedFile.Stream.Dispose();
            throw new InvalidOperationException("OpenMediaFileAsync unexpectedly opened a FIFO media file.");
        }

        AssertTrue(beforeOpenCalled, "special file reached secure open path");
    }
    finally
    {
        if (File.Exists(fifoPath))
        {
            File.Delete(fifoPath);
        }
    }
}

static async System.Threading.Tasks.Task AssertSymlinkSwapBeforeOpenIsRejectedAsync(string rootPath, string outsidePath)
{
    if (!OperatingSystem.IsMacOS() && !OperatingSystem.IsLinux())
    {
        return;
    }

    var mediaPath = Path.Combine(rootPath, "Movies", "Swap Clip.mp4");
    await File.WriteAllTextAsync(mediaPath, "inside-cache-root");
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }))
    {
        BeforeOpenMediaFileForTests = path =>
        {
            AssertEqual(mediaPath, path, "swap hook media path");
            File.Delete(mediaPath);
            File.CreateSymbolicLink(mediaPath, outsidePath);
        }
    };

    try
    {
        var openedFile = await library.OpenMediaFileAsync(CreateLocalItemId("Movies/Swap Clip.mp4"), "original", CancellationToken.None);
        if (openedFile is not null)
        {
            openedFile.Stream.Dispose();
            throw new InvalidOperationException("OpenMediaFileAsync unexpectedly opened a symlink swapped in after validation.");
        }
    }
    finally
    {
        if (File.Exists(mediaPath))
        {
            File.Delete(mediaPath);
        }
    }
}

static async System.Threading.Tasks.Task AssertParentSymlinkSwapBeforeOpenIsRejectedAsync(string rootPath)
{
    if (!OperatingSystem.IsMacOS() && !OperatingSystem.IsLinux())
    {
        return;
    }

    var parentPath = Path.Combine(rootPath, "Race Parent");
    var mediaPath = Path.Combine(parentPath, "Parent Swap Clip.mp4");
    var outsideParentPath = Path.Combine(Path.GetTempPath(), $"tvnp-cache-server-parent-race-{Guid.NewGuid():N}");
    Directory.CreateDirectory(parentPath);
    Directory.CreateDirectory(outsideParentPath);
    await File.WriteAllTextAsync(mediaPath, "inside-cache-root");
    await File.WriteAllTextAsync(Path.Combine(outsideParentPath, "Parent Swap Clip.mp4"), "outside-cache-root");
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }))
    {
        BeforeOpenMediaFileForTests = path =>
        {
            AssertEqual(mediaPath, path, "parent swap hook media path");
            Directory.Delete(parentPath, recursive: true);
            Directory.CreateSymbolicLink(parentPath, outsideParentPath);
        }
    };

    try
    {
        var openedFile = await library.OpenMediaFileAsync(CreateLocalItemId("Race Parent/Parent Swap Clip.mp4"), "original", CancellationToken.None);
        if (openedFile is not null)
        {
            openedFile.Stream.Dispose();
            throw new InvalidOperationException("OpenMediaFileAsync unexpectedly followed a parent symlink swapped in after validation.");
        }
    }
    finally
    {
        if (Directory.Exists(parentPath))
        {
            Directory.Delete(parentPath);
        }

        if (Directory.Exists(outsideParentPath))
        {
            Directory.Delete(outsideParentPath, recursive: true);
        }
    }
}

static async System.Threading.Tasks.Task AssertSecureMediaOpenUnavailableFailsClosedAsync(string rootPath)
{
    var mediaPath = Path.Combine(rootPath, "Movies", "Unsupported Secure Open.mp4");
    await File.WriteAllTextAsync(mediaPath, "inside-cache-root");
    var library = new LocalMediaLibrary(new StaticOptionsMonitor<CacheServerOptions>(new CacheServerOptions
    {
        RootPath = rootPath
    }))
    {
        IsSecureNoFollowOpenSupportedForTests = () => false
    };

    try
    {
        var openedFile = await library.OpenMediaFileAsync(CreateLocalItemId("Movies/Unsupported Secure Open.mp4"), "original", CancellationToken.None);
        if (openedFile is not null)
        {
            openedFile.Stream.Dispose();
            throw new InvalidOperationException("OpenMediaFileAsync unexpectedly opened media when secure no-follow open is unavailable.");
        }
    }
    finally
    {
        if (File.Exists(mediaPath))
        {
            File.Delete(mediaPath);
        }
    }
}

static async System.Threading.Tasks.Task AssertReadOnlyRootReportsNotWritableAsync(GrpcChannel channel, string rootPath)
{
    if (OperatingSystem.IsWindows())
    {
        return;
    }

    File.SetUnixFileMode(
        rootPath,
        UnixFileMode.UserRead | UnixFileMode.UserExecute |
        UnixFileMode.GroupRead | UnixFileMode.GroupExecute |
        UnixFileMode.OtherRead | UnixFileMode.OtherExecute);

    try
    {
        var client = new CacheService.CacheServiceClient(channel);
        var response = await client.ListCacheRootsAsync(new ListCacheRootsRequest());

        AssertEqual(1, response.Roots.Count, "cache root count");
        AssertEqual(false, response.Roots[0].Writable, "read-only cache root writable flag");
    }
    finally
    {
        File.SetUnixFileMode(
            rootPath,
            UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute |
            UnixFileMode.GroupRead | UnixFileMode.GroupWrite | UnixFileMode.GroupExecute |
            UnixFileMode.OtherRead | UnixFileMode.OtherExecute);
    }
}

static async System.Threading.Tasks.Task AssertSymlinkRootIsRejectedAsync(string symlinkRootTarget, string symlinkRoot)
{
    Directory.CreateDirectory(symlinkRootTarget);
    await File.WriteAllTextAsync(Path.Combine(symlinkRootTarget, "Root Link Movie.mp4"), "linked-root");
    Directory.CreateSymbolicLink(symlinkRoot, symlinkRootTarget);

    var grpcAddress = $"http://127.0.0.1:{GetFreePort()}";
    var mediaAddress = $"http://127.0.0.1:{GetFreePort()}";

    await using var app = CacheServerHost.Create(
    [
        "--Cache:GrpcListenUrl",
        grpcAddress,
        "--Cache:MediaListenUrl",
        mediaAddress,
        "--Cache:RootPath",
        symlinkRoot,
        "--Logging:LogLevel:Default",
        "Warning"
    ]);

    await app.StartAsync();

    using var channel = GrpcChannel.ForAddress(grpcAddress, new GrpcChannelOptions
    {
        HttpHandler = new SocketsHttpHandler
        {
            EnableMultipleHttp2Connections = true
        }
    });

    var serverClient = new ServerService.ServerServiceClient(channel);
    var health = await serverClient.CheckHealthAsync(new CheckHealthRequest());
    AssertEqual(HealthState.Degraded, health.State, "symlink root health");

    var libraryClient = new LibraryService.LibraryServiceClient(channel);
    var listResponse = await libraryClient.ListLibraryItemsAsync(new ListLibraryItemsRequest());
    AssertEqual(0, listResponse.Items.Count, "symlink root item count");

    using var httpClient = new HttpClient();
    using var mediaResponse = await httpClient.GetAsync($"{mediaAddress}/media/{CreateLocalItemId("Root Link Movie.mp4")}/original");
    AssertEqual(HttpStatusCode.NotFound, mediaResponse.StatusCode, "symlink root media lookup");
}

static void AssertPlaybackBaseUriFormatting()
{
    AssertEqual(
        "http://127.0.0.1:8080",
        PlaybackUriFactory.CreateMediaBaseUri("127.0.0.1:50051", "http://0.0.0.0:8080"),
        "IPv4 wildcard playback base URI");

    AssertEqual(
        "http://mac-mini.local:8080",
        PlaybackUriFactory.CreateMediaBaseUri("mac-mini.local:50051", "http://0.0.0.0:8080"),
        "DNS wildcard playback base URI");

    AssertEqual(
        "http://[::1]:8080",
        PlaybackUriFactory.CreateMediaBaseUri("[::1]:50051", "http://0.0.0.0:8080"),
        "bracketed IPv6 wildcard playback base URI");

    AssertEqual(
        "http://[::1]:8080",
        PlaybackUriFactory.CreateMediaBaseUri("127.0.0.1:50051", "http://[::1]:8080"),
        "explicit IPv6 playback base URI");

    AssertEqual(
        "http://[::1]:8080",
        PlaybackUriFactory.CreateMediaBaseUri("[::1]:50051", "http://[::]:8080"),
        "bracketed IPv6 wildcard playback base URI");
}

static void AssertListenHostNormalization()
{
    AssertEqual(
        "::1",
        CacheServerHost.NormalizeListenHost(new Uri("http://[::1]:8080")),
        "IPv6 loopback listen host normalization");

    AssertEqual(
        "::",
        CacheServerHost.NormalizeListenHost(new Uri("http://[::]:8080")),
        "IPv6 wildcard listen host normalization");

    AssertEqual(
        "127.0.0.1",
        CacheServerHost.NormalizeListenHost(new Uri("http://127.0.0.1:8080")),
        "IPv4 listen host normalization");
}

static void AssertDefaultListenUrlsAreLoopback()
{
    var options = new CacheServerOptions();
    AssertEqual("http://localhost:50051", options.GrpcListenUrl, "default gRPC listen URL");
    AssertEqual("http://localhost:8080", options.MediaListenUrl, "default media listen URL");
}

static async System.Threading.Tasks.Task AssertListenUrlsMustBeCleartextAsync(string rootPath)
{
    await AssertListenUrlRejectedAsync(
        "gRPC HTTPS listen URL",
        [
            "--Cache:GrpcListenUrl",
            $"https://127.0.0.1:{GetFreePort()}",
            "--Cache:MediaListenUrl",
            $"http://127.0.0.1:{GetFreePort()}",
            "--Cache:RootPath",
            rootPath
        ]);

    await AssertListenUrlRejectedAsync(
        "media HTTPS listen URL",
        [
            "--Cache:GrpcListenUrl",
            $"http://127.0.0.1:{GetFreePort()}",
            "--Cache:MediaListenUrl",
            $"https://127.0.0.1:{GetFreePort()}",
            "--Cache:RootPath",
            rootPath
        ]);
}

static async System.Threading.Tasks.Task AssertListenUrlRejectedAsync(string label, string[] args)
{
    try
    {
        await using var app = CacheServerHost.Create(args);
        await app.StartAsync();
        throw new InvalidOperationException($"{label} unexpectedly started.");
    }
    catch (InvalidOperationException exception) when (exception.Message.Contains("Only cleartext http listen URLs", StringComparison.Ordinal))
    {
    }
}

static void AssertEqual<T>(T expected, T actual, string label)
{
    if (!EqualityComparer<T>.Default.Equals(expected, actual))
    {
        throw new InvalidOperationException($"{label}: expected {expected}, got {actual}");
    }
}

static void AssertTrue(bool condition, string label)
{
    if (!condition)
    {
        throw new InvalidOperationException($"Assertion failed: {label}");
    }
}

static int GetFreePort()
{
    using var listener = new TcpListener(IPAddress.Loopback, 0);
    listener.Start();
    return ((IPEndPoint)listener.LocalEndpoint).Port;
}

static string CreateLocalItemId(string relativePath)
{
    return $"local.default.{Convert.ToBase64String(System.Text.Encoding.UTF8.GetBytes(relativePath))
        .TrimEnd('=')
        .Replace('+', '-')
        .Replace('/', '_')}";
}

[System.Runtime.InteropServices.DllImport("libc", EntryPoint = "mkfifo", SetLastError = true)]
static extern int MkFifo(string path, uint mode);

sealed class StaticOptionsMonitor<T> : IOptionsMonitor<T>
{
    public StaticOptionsMonitor(T currentValue)
    {
        CurrentValue = currentValue;
    }

    public T CurrentValue { get; }

    public T Get(string? name)
    {
        return CurrentValue;
    }

    public IDisposable? OnChange(Action<T, string?> listener)
    {
        return null;
    }
}
