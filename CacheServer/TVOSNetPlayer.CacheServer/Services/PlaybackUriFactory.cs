using Grpc.Core;
using Microsoft.Extensions.Options;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class PlaybackUriFactory
{
    private readonly IOptionsMonitor<CacheServerOptions> options;

    public PlaybackUriFactory(IOptionsMonitor<CacheServerOptions> options)
    {
        this.options = options;
    }

    public string Create(ServerCallContext context, string itemId, string variantId)
    {
        var baseUri = options.CurrentValue.PublicMediaBaseUri;
        if (string.IsNullOrWhiteSpace(baseUri))
        {
            baseUri = CreateMediaBaseUri(context);
        }

        return $"{baseUri.TrimEnd('/')}/media/{Uri.EscapeDataString(itemId)}/{Uri.EscapeDataString(variantId)}";
    }

    private string CreateMediaBaseUri(ServerCallContext context)
    {
        return CreateMediaBaseUri(context.Host, options.CurrentValue.MediaListenUrl);
    }

    internal static string CreateMediaBaseUri(string requestAuthority, string mediaListenUrl)
    {
        var mediaUri = new Uri(mediaListenUrl);
        var listenHost = CacheServerHost.NormalizeListenHost(mediaUri);
        var host = listenHost is "0.0.0.0" or "::" or "*" or "+"
            ? ExtractUriHost(requestAuthority)
            : FormatUriHost(listenHost);

        return $"{mediaUri.Scheme}://{host}:{mediaUri.Port}";
    }

    private static string ExtractUriHost(string authority)
    {
        if (string.IsNullOrWhiteSpace(authority))
        {
            return "localhost";
        }

        if (authority.StartsWith("[", StringComparison.Ordinal))
        {
            var bracketEnd = authority.IndexOf(']');
            return bracketEnd > 0 ? authority[..(bracketEnd + 1)] : "localhost";
        }

        var colonCount = authority.Count(character => character == ':');
        var host = colonCount == 1
            ? authority[..authority.LastIndexOf(':')]
            : authority;

        return FormatUriHost(host);
    }

    private static string FormatUriHost(string host)
    {
        return host.Contains(":", StringComparison.Ordinal) && !host.StartsWith("[", StringComparison.Ordinal)
            ? $"[{host}]"
            : host;
    }
}
