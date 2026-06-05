using Microsoft.AspNetCore.Server.Kestrel.Core;
using Microsoft.Extensions.Options;
using System.Net;
using TVOSNetPlayer.CacheServer.Services;

namespace TVOSNetPlayer.CacheServer;

public static class CacheServerHost
{
    public static WebApplication Create(string[] args)
    {
        var builder = WebApplication.CreateBuilder(args);
        builder.WebHost.ConfigureKestrel((context, options) =>
        {
            var cacheOptions = new CacheServerOptions();
            context.Configuration.GetSection("Cache").Bind(cacheOptions);
            ValidateDistinctListenerPorts(cacheOptions);

            Listen(options, cacheOptions.GrpcListenUrl, HttpProtocols.Http2);
            Listen(options, cacheOptions.MediaListenUrl, HttpProtocols.Http1);
        });

        builder.Services.AddGrpc();
        builder.Services.Configure<CacheServerOptions>(builder.Configuration.GetSection("Cache"));
        builder.Services.AddSingleton<LocalMediaLibrary>();
        builder.Services.AddSingleton<PlaybackUriFactory>();

        var app = builder.Build();

        app.MapGrpcService<ServerGrpcService>();
        app.MapGrpcService<LibraryGrpcService>();
        app.MapGrpcService<TaskGrpcService>();
        app.MapGrpcService<CacheGrpcService>();

        app.MapGet("/", () => Results.Json(new
        {
            service = "TVOSNetPlayer.CacheServer",
            controlPlane = "gRPC",
            mediaPlane = "HTTP"
        }));

        app.MapMethods("/media/{itemId}/{variantId}", ["GET", "HEAD"], async (
            string itemId,
            string variantId,
            HttpContext httpContext,
            LocalMediaLibrary library,
            IOptionsMonitor<CacheServerOptions> options,
            CancellationToken cancellationToken) =>
        {
            if (!IsMediaRequestPort(httpContext, options.CurrentValue.MediaListenUrl))
            {
                return Results.NotFound();
            }

            var mediaFile = await library.OpenMediaFileAsync(itemId, variantId, cancellationToken);
            if (mediaFile is null)
            {
                return Results.NotFound();
            }

            return Results.File(
                mediaFile.Stream,
                mediaFile.ContentType,
                enableRangeProcessing: true,
                lastModified: mediaFile.LastModified);
        });

        return app;
    }

    internal static bool IsMediaRequestPort(HttpContext context, string mediaListenUrl)
    {
        return context.Connection.LocalPort == new Uri(mediaListenUrl).Port;
    }

    private static void ValidateDistinctListenerPorts(CacheServerOptions options)
    {
        var grpcListenUri = new Uri(options.GrpcListenUrl);
        var mediaListenUri = new Uri(options.MediaListenUrl);
        if (grpcListenUri.Port == mediaListenUri.Port)
        {
            throw new InvalidOperationException("gRPC and media listen URLs must use distinct ports in this slice.");
        }
    }

    private static void Listen(KestrelServerOptions options, string listenUrl, HttpProtocols protocols)
    {
        var uri = new Uri(listenUrl);
        if (!uri.Scheme.Equals(Uri.UriSchemeHttp, StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidOperationException("Only cleartext http listen URLs are supported in this slice.");
        }

        var host = NormalizeListenHost(uri);
        void Configure(ListenOptions listenOptions)
        {
            listenOptions.Protocols = protocols;
        }

        if (host is "0.0.0.0" or "::" or "*" or "+")
        {
            options.ListenAnyIP(uri.Port, Configure);
            return;
        }

        if (host.Equals("localhost", StringComparison.OrdinalIgnoreCase))
        {
            options.ListenLocalhost(uri.Port, Configure);
            return;
        }

        options.Listen(IPAddress.Parse(host), uri.Port, Configure);
    }

    internal static string NormalizeListenHost(Uri uri)
    {
        var host = uri.Host;
        return host.Length >= 2 && host[0] == '[' && host[^1] == ']'
            ? host[1..^1]
            : host;
    }
}
