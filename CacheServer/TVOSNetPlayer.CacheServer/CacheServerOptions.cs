namespace TVOSNetPlayer.CacheServer;

public sealed class CacheServerOptions
{
    public string ServerId { get; set; } = "default";
    public string ServerName { get; set; } = "TVOS Net Player Cache";
    public string RootPath { get; set; } = Path.Combine(AppContext.BaseDirectory, "cache");
    public string GrpcListenUrl { get; set; } = "http://0.0.0.0:50051";
    public string MediaListenUrl { get; set; } = "http://0.0.0.0:8080";
    public string? PublicMediaBaseUri { get; set; }
    public string[] AllowedExtensions { get; set; } = [".mp4", ".m4v", ".mov"];
}
