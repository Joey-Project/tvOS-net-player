using System.Security.Cryptography;
using System.Text;
using Google.Protobuf.WellKnownTypes;
using Microsoft.Extensions.Options;
using TVOSNetPlayer.Cache.V1;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class LocalMediaLibrary
{
    private const string RootId = "default";
    private const string VariantId = "original";
    private readonly IOptionsMonitor<CacheServerOptions> options;

    public LocalMediaLibrary(IOptionsMonitor<CacheServerOptions> options)
    {
        this.options = options;
    }

    public string RootPath => Path.GetFullPath(options.CurrentValue.RootPath);

    public System.Threading.Tasks.Task<IReadOnlyList<LibraryItem>> ListItemsAsync(LibraryFilter? filter, CancellationToken cancellationToken)
    {
        if (!IsRootAvailable())
        {
            return System.Threading.Tasks.Task.FromResult<IReadOnlyList<LibraryItem>>([]);
        }

        var allowedExtensions = GetAllowedExtensions();
        var requestedSources = filter?.Sources.Count > 0 ? filter.Sources.ToHashSet() : null;
        var searchText = filter?.SearchText?.Trim();
        var rootPath = RootPath;

        var items = Directory
            .EnumerateFiles(rootPath, "*", CreateEnumerationOptions())
            .Where(path => !IsLink(new FileInfo(path)))
            .Where(path => allowedExtensions.Contains(Path.GetExtension(path)))
            .Select(path => CreateLibraryItem(rootPath, path))
            .Where(item => requestedSources is null || requestedSources.Contains(item.Source))
            .Where(item => string.IsNullOrEmpty(searchText) || item.Title.Contains(searchText, StringComparison.OrdinalIgnoreCase) || item.Subtitle.Contains(searchText, StringComparison.OrdinalIgnoreCase))
            .OrderBy(item => item.Title, StringComparer.OrdinalIgnoreCase)
            .ThenBy(item => item.Subtitle, StringComparer.OrdinalIgnoreCase)
            .ToArray();

        cancellationToken.ThrowIfCancellationRequested();
        return System.Threading.Tasks.Task.FromResult<IReadOnlyList<LibraryItem>>(items);
    }

    public async System.Threading.Tasks.Task<LibraryItem?> GetItemAsync(string id, CancellationToken cancellationToken)
    {
        var mediaFile = await GetMediaFileAsync(id, VariantId, cancellationToken);
        return mediaFile is null ? null : CreateLibraryItem(RootPath, mediaFile.Path);
    }

    public System.Threading.Tasks.Task<MediaFile?> GetMediaFileAsync(string itemId, string variantId, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();

        if (variantId != VariantId || !TryDecodeItemId(itemId, out var relativePath))
        {
            return System.Threading.Tasks.Task.FromResult<MediaFile?>(null);
        }

        var rootPath = RootPath;
        var fullRootPath = EnsureTrailingSeparator(rootPath);
        string candidatePath;
        try
        {
            candidatePath = Path.GetFullPath(Path.Combine(rootPath, relativePath));
        }
        catch (Exception exception) when (exception is ArgumentException or NotSupportedException or PathTooLongException)
        {
            return System.Threading.Tasks.Task.FromResult<MediaFile?>(null);
        }

        if (!candidatePath.StartsWith(fullRootPath, StringComparison.Ordinal) || !File.Exists(candidatePath))
        {
            return System.Threading.Tasks.Task.FromResult<MediaFile?>(null);
        }

        if (PathContainsLink(rootPath, candidatePath))
        {
            return System.Threading.Tasks.Task.FromResult<MediaFile?>(null);
        }

        if (!GetAllowedExtensions().Contains(Path.GetExtension(candidatePath)))
        {
            return System.Threading.Tasks.Task.FromResult<MediaFile?>(null);
        }

        var fileInfo = new FileInfo(candidatePath);
        return System.Threading.Tasks.Task.FromResult<MediaFile?>(new MediaFile(
            candidatePath,
            GetContentType(candidatePath),
            new DateTimeOffset(fileInfo.LastWriteTimeUtc, TimeSpan.Zero),
            fileInfo.Length));
    }

    public System.Threading.Tasks.Task<CacheRoot> GetCacheRootAsync(CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();

        var rootPath = RootPath;
        var root = new CacheRoot
        {
            Id = RootId,
            Label = "Local Cache",
            Kind = CacheRootKind.LocalDirectory,
            Path = rootPath,
            Writable = IsRootAvailable() && CanWriteToDirectory(rootPath)
        };

        if (root.Writable)
        {
            var drive = new DriveInfo(Path.GetPathRoot(rootPath)!);
            root.FreeBytes = drive.AvailableFreeSpace;
            root.TotalBytes = drive.TotalSize;
        }

        return System.Threading.Tasks.Task.FromResult(root);
    }

    public bool IsRootAvailable()
    {
        try
        {
            return Directory.Exists(RootPath) && !IsLink(new DirectoryInfo(RootPath));
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            return false;
        }
    }

    public async System.Threading.Tasks.Task<int> CountItemsAsync(CancellationToken cancellationToken)
    {
        var items = await ListItemsAsync(null, cancellationToken);
        return items.Count;
    }

    private LibraryItem CreateLibraryItem(string rootPath, string path)
    {
        var fileInfo = new FileInfo(path);
        var relativePath = Path.GetRelativePath(rootPath, path).Replace(Path.DirectorySeparatorChar, '/');
        var item = new LibraryItem
        {
            Id = CreateItemId(relativePath),
            Title = Path.GetFileNameWithoutExtension(path),
            Subtitle = relativePath,
            Source = LibrarySource.LocalCache,
            SourceId = relativePath,
            CreatedAt = Timestamp.FromDateTime(DateTime.SpecifyKind(fileInfo.CreationTimeUtc, DateTimeKind.Utc)),
            UpdatedAt = Timestamp.FromDateTime(DateTime.SpecifyKind(fileInfo.LastWriteTimeUtc, DateTimeKind.Utc))
        };

        item.Variants.Add(new MediaVariant
        {
            Id = VariantId,
            Label = "Original",
            Protocol = PlaybackProtocol.HttpFile,
            Container = Path.GetExtension(path).TrimStart('.').ToLowerInvariant(),
            SizeBytes = fileInfo.Length
        });

        return item;
    }

    private HashSet<string> GetAllowedExtensions()
    {
        return options.CurrentValue.AllowedExtensions
            .Select(extension => extension.StartsWith('.') ? extension : $".{extension}")
            .Select(extension => extension.ToLowerInvariant())
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
    }

    private static string CreateItemId(string relativePath)
    {
        var bytes = Encoding.UTF8.GetBytes(relativePath);
        return $"local.{RootId}.{Base64UrlEncode(bytes)}";
    }

    private static bool TryDecodeItemId(string itemId, out string relativePath)
    {
        relativePath = "";
        var prefix = $"local.{RootId}.";
        if (!itemId.StartsWith(prefix, StringComparison.Ordinal))
        {
            return false;
        }

        try
        {
            relativePath = Encoding.UTF8.GetString(Base64UrlDecode(itemId[prefix.Length..]));
            return relativePath.IndexOfAny(Path.GetInvalidPathChars()) < 0
                && !Path.IsPathRooted(relativePath)
                && !relativePath.Split(['/', '\\']).Any(segment => segment is ".." or ".");
        }
        catch (Exception exception) when (exception is FormatException or ArgumentException or NotSupportedException or PathTooLongException)
        {
            return false;
        }
    }

    private static string Base64UrlEncode(byte[] bytes)
    {
        return Convert.ToBase64String(bytes)
            .TrimEnd('=')
            .Replace('+', '-')
            .Replace('/', '_');
    }

    private static byte[] Base64UrlDecode(string value)
    {
        var padded = value.Replace('-', '+').Replace('_', '/');
        padded = padded.PadRight(padded.Length + (4 - padded.Length % 4) % 4, '=');
        return Convert.FromBase64String(padded);
    }

    private static string EnsureTrailingSeparator(string path)
    {
        var fullPath = Path.GetFullPath(path);
        return fullPath.EndsWith(Path.DirectorySeparatorChar)
            ? fullPath
            : $"{fullPath}{Path.DirectorySeparatorChar}";
    }

    private static string GetContentType(string path)
    {
        return Path.GetExtension(path).ToLowerInvariant() switch
        {
            ".m4v" => "video/x-m4v",
            ".mov" => "video/quicktime",
            _ => "video/mp4"
        };
    }

    private static bool IsLink(FileSystemInfo fileInfo)
    {
        return fileInfo.LinkTarget is not null || fileInfo.Attributes.HasFlag(FileAttributes.ReparsePoint);
    }

    private static EnumerationOptions CreateEnumerationOptions()
    {
        return new EnumerationOptions
        {
            RecurseSubdirectories = true,
            IgnoreInaccessible = true,
            AttributesToSkip = FileAttributes.ReparsePoint
        };
    }

    private static bool PathContainsLink(string rootPath, string candidatePath)
    {
        try
        {
            var fullRootPath = Path.GetFullPath(rootPath);
            if (IsLink(new DirectoryInfo(fullRootPath)))
            {
                return true;
            }

            var relativePath = Path.GetRelativePath(fullRootPath, candidatePath);
            var currentPath = fullRootPath;
            foreach (var segment in relativePath.Split(['/', '\\'], StringSplitOptions.RemoveEmptyEntries))
            {
                currentPath = Path.Combine(currentPath, segment);
                var fileSystemInfo = Directory.Exists(currentPath)
                    ? new DirectoryInfo(currentPath)
                    : new FileInfo(currentPath) as FileSystemInfo;
                if (IsLink(fileSystemInfo))
                {
                    return true;
                }
            }

            return false;
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            return true;
        }
    }

    private static bool CanWriteToDirectory(string path)
    {
        var probePath = Path.Combine(path, $".tvnp-write-probe-{Guid.NewGuid():N}");
        try
        {
            using (new FileStream(probePath, FileMode.CreateNew, FileAccess.Write, FileShare.None, 1, FileOptions.DeleteOnClose))
            {
            }

            return true;
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException)
        {
            return false;
        }
        finally
        {
            if (File.Exists(probePath))
            {
                File.Delete(probePath);
            }
        }
    }
}

public sealed record MediaFile(string Path, string ContentType, DateTimeOffset LastModified, long SizeBytes);
