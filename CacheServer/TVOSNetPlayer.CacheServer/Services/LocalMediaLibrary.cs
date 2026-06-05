using System.Security.Cryptography;
using System.Text;
using Google.Protobuf.WellKnownTypes;
using Microsoft.Win32.SafeHandles;
using Microsoft.Extensions.Options;
using TVOSNetPlayer.Cache.V1;

namespace TVOSNetPlayer.CacheServer.Services;

public sealed class LocalMediaLibrary
{
    private const string RootId = "default";
    private const string VariantId = "original";
    private const int OpenReadOnly = 0;
    private const int DarwinOpenNoFollow = 0x00000100;
    private const int DarwinOpenDirectory = 0x00100000;
    private const int LinuxOpenNoFollow = 0x00020000;
    private const int LinuxOpenDirectory = 0x00010000;
    private readonly IOptionsMonitor<CacheServerOptions> options;

    public LocalMediaLibrary(IOptionsMonitor<CacheServerOptions> options)
    {
        this.options = options;
    }

    internal Action<string>? BeforeOpenMediaFileForTests { get; set; }
    internal Action<string>? BeforeCreateLibraryItemForTests { get; set; }
    internal Action<string>? BeforeCreateMediaFileForTests { get; set; }

    public string RootPath => Path.GetFullPath(options.CurrentValue.RootPath);

    public System.Threading.Tasks.Task<LibraryItemPage> ListItemsPageAsync(LibraryFilter? filter, long pageOffset, int pageSize, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();

        if (!IsRootAvailable())
        {
            return System.Threading.Tasks.Task.FromResult(LibraryItemPage.Empty);
        }

        var retainedLimit = GetRetainedCandidateLimit(pageOffset, pageSize);
        if (retainedLimit is null)
        {
            return System.Threading.Tasks.Task.FromResult(LibraryItemPage.Empty);
        }

        var rootPath = RootPath;
        var retainedCandidates = new SortedSet<MediaCandidate>(MediaCandidateComparer.Instance);
        foreach (var candidate in EnumerateMediaCandidates(rootPath, filter, cancellationToken))
        {
            retainedCandidates.Add(candidate);
            if (retainedCandidates.Count > retainedLimit.Value)
            {
                retainedCandidates.Remove(retainedCandidates.Max!);
            }
        }

        var pageStart = (int)Math.Min(pageOffset, retainedCandidates.Count);
        var items = retainedCandidates
            .Skip(pageStart)
            .Take(pageSize)
            .Select(candidate => TryCreateLibraryItem(rootPath, candidate.Path))
            .OfType<LibraryItem>()
            .ToArray();
        var nextPageOffset = pageOffset + pageSize;
        var page = new LibraryItemPage(
            items,
            retainedCandidates.Count > nextPageOffset ? nextPageOffset : null);
        return System.Threading.Tasks.Task.FromResult(page);
    }

    public async System.Threading.Tasks.Task<LibraryItem?> GetItemAsync(string id, CancellationToken cancellationToken)
    {
        var mediaFile = await GetMediaFileAsync(id, VariantId, cancellationToken);
        return mediaFile is null ? null : CreateLibraryItem(RootPath, mediaFile.Path);
    }

    public System.Threading.Tasks.Task<MediaFile?> GetMediaFileAsync(string itemId, string variantId, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return System.Threading.Tasks.Task.FromResult(ResolveMediaFile(itemId, variantId));
    }

    public System.Threading.Tasks.Task<OpenedMediaFile?> OpenMediaFileAsync(string itemId, string variantId, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        var mediaFile = ResolveMediaFile(itemId, variantId);
        if (mediaFile is null)
        {
            return System.Threading.Tasks.Task.FromResult<OpenedMediaFile?>(null);
        }

        BeforeOpenMediaFileForTests?.Invoke(mediaFile.Path);
        FileStream stream;
        try
        {
            stream = OpenReadNoFollow(RootPath, mediaFile.RelativePath, mediaFile.Path);
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException or PathTooLongException)
        {
            return System.Threading.Tasks.Task.FromResult<OpenedMediaFile?>(null);
        }

        try
        {
            var attributes = File.GetAttributes(stream.SafeFileHandle);
            if (attributes.HasFlag(FileAttributes.Directory) || attributes.HasFlag(FileAttributes.ReparsePoint))
            {
                stream.Dispose();
                return System.Threading.Tasks.Task.FromResult<OpenedMediaFile?>(null);
            }

            var lastWriteTimeUtc = DateTime.SpecifyKind(File.GetLastWriteTimeUtc(stream.SafeFileHandle), DateTimeKind.Utc);
            var openedFile = new OpenedMediaFile(
                stream,
                mediaFile.ContentType,
                new DateTimeOffset(lastWriteTimeUtc, TimeSpan.Zero),
                stream.Length);
            return System.Threading.Tasks.Task.FromResult<OpenedMediaFile?>(openedFile);
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or NotSupportedException)
        {
            stream.Dispose();
            return System.Threading.Tasks.Task.FromResult<OpenedMediaFile?>(null);
        }
    }

    private MediaFile? ResolveMediaFile(string itemId, string variantId)
    {
        if (variantId != VariantId || !TryDecodeItemId(itemId, out var relativePath))
        {
            return null;
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
            return null;
        }

        if (!candidatePath.StartsWith(fullRootPath, StringComparison.Ordinal) || !File.Exists(candidatePath))
        {
            return null;
        }

        if (PathContainsLink(rootPath, candidatePath))
        {
            return null;
        }

        if (!GetAllowedExtensions().Contains(Path.GetExtension(candidatePath)))
        {
            return null;
        }

        try
        {
            BeforeCreateMediaFileForTests?.Invoke(candidatePath);
            var fileInfo = new FileInfo(candidatePath);
            var candidateRelativePath = Path.GetRelativePath(rootPath, candidatePath);
            return new MediaFile(
                candidatePath,
                candidateRelativePath,
                GetContentType(candidatePath),
                new DateTimeOffset(fileInfo.LastWriteTimeUtc, TimeSpan.Zero),
                fileInfo.Length);
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException or PathTooLongException)
        {
            return null;
        }
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
        cancellationToken.ThrowIfCancellationRequested();

        if (!IsRootAvailable())
        {
            return 0;
        }

        var count = 0;
        foreach (var _ in EnumerateMediaCandidates(RootPath, null, cancellationToken))
        {
            if (count < int.MaxValue)
            {
                count++;
            }
        }

        return await System.Threading.Tasks.Task.FromResult(count);
    }

    private LibraryItem CreateLibraryItem(string rootPath, string path)
    {
        BeforeCreateLibraryItemForTests?.Invoke(path);
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

    private LibraryItem? TryCreateLibraryItem(string rootPath, string path)
    {
        try
        {
            return CreateLibraryItem(rootPath, path);
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException or PathTooLongException)
        {
            return null;
        }
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
                && !relativePath.Split(GetPathSegmentSeparators()).Any(segment => segment is ".." or ".");
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
        return fullPath.EndsWith(Path.DirectorySeparatorChar) || fullPath.EndsWith(Path.AltDirectorySeparatorChar)
            ? fullPath
            : $"{fullPath}{Path.DirectorySeparatorChar}";
    }

    private static int? GetRetainedCandidateLimit(long pageOffset, int pageSize)
    {
        if (pageOffset < 0 || pageSize <= 0)
        {
            return 0;
        }

        var retainedCount = pageOffset + pageSize + 1L;
        return retainedCount <= int.MaxValue ? (int)retainedCount : null;
    }

    private IEnumerable<MediaCandidate> EnumerateMediaCandidates(string rootPath, LibraryFilter? filter, CancellationToken cancellationToken)
    {
        var requestedSources = filter?.Sources.Count > 0 ? filter.Sources.ToHashSet() : null;
        if (requestedSources is not null && !requestedSources.Contains(LibrarySource.LocalCache))
        {
            yield break;
        }

        var allowedExtensions = GetAllowedExtensions();
        var searchText = filter?.SearchText?.Trim();
        foreach (var path in Directory.EnumerateFiles(rootPath, "*", CreateEnumerationOptions()))
        {
            cancellationToken.ThrowIfCancellationRequested();
            if (!TryCreateMediaCandidate(rootPath, path, allowedExtensions, out var candidate))
            {
                continue;
            }

            if (!string.IsNullOrEmpty(searchText)
                && !candidate.Title.Contains(searchText, StringComparison.OrdinalIgnoreCase)
                && !candidate.Subtitle.Contains(searchText, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            yield return candidate;
        }
    }

    private static bool TryCreateMediaCandidate(string rootPath, string path, HashSet<string> allowedExtensions, out MediaCandidate candidate)
    {
        candidate = default!;
        try
        {
            if (IsLink(new FileInfo(path)) || !allowedExtensions.Contains(Path.GetExtension(path)))
            {
                return false;
            }

            var relativePath = Path.GetRelativePath(rootPath, path).Replace(Path.DirectorySeparatorChar, '/');
            candidate = new MediaCandidate(path, Path.GetFileNameWithoutExtension(path), relativePath);
            return true;
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException or PathTooLongException)
        {
            return false;
        }
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
            var fullCandidatePath = Path.GetFullPath(candidatePath);
            if (IsLink(new DirectoryInfo(fullRootPath)))
            {
                return true;
            }

            if (!IsWithinRoot(fullRootPath, fullCandidatePath))
            {
                return true;
            }

            var currentPath = fullCandidatePath;
            while (!PathsEqual(currentPath, fullRootPath))
            {
                if (IsLink(CreateFileSystemInfo(currentPath)))
                {
                    return true;
                }

                var parent = Directory.GetParent(currentPath);
                if (parent is null)
                {
                    return true;
                }

                currentPath = parent.FullName;
            }

            return false;
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            return true;
        }
    }

    private static FileSystemInfo CreateFileSystemInfo(string path)
    {
        return Directory.Exists(path) ? new DirectoryInfo(path) : new FileInfo(path);
    }

    private static FileStream OpenReadNoFollow(string rootPath, string relativePath, string path)
    {
        if (OperatingSystem.IsMacOS())
        {
            return OpenUnixReadNoFollow(rootPath, relativePath, DarwinOpenNoFollow, DarwinOpenDirectory);
        }

        if (OperatingSystem.IsLinux())
        {
            return OpenUnixReadNoFollow(rootPath, relativePath, LinuxOpenNoFollow, LinuxOpenDirectory);
        }

        return new FileStream(path, new FileStreamOptions
        {
            Mode = FileMode.Open,
            Access = FileAccess.Read,
            Share = FileShare.Read,
            Options = FileOptions.SequentialScan
        });
    }

    private static FileStream OpenUnixReadNoFollow(string rootPath, string relativePath, int openNoFollow, int openDirectory)
    {
        var segments = relativePath.Split(GetPathSegmentSeparators(), StringSplitOptions.RemoveEmptyEntries);
        if (segments.Length == 0)
        {
            throw new IOException("Could not open media file with an empty relative path.");
        }

        using var rootHandle = OpenUnixHandle(rootPath, OpenReadOnly | openNoFollow | openDirectory);
        SafeFileHandle? directoryHandle = null;
        try
        {
            directoryHandle = rootHandle;
            for (var index = 0; index < segments.Length - 1; index++)
            {
                var nextDirectoryHandle = OpenUnixHandleAt(directoryHandle, segments[index], OpenReadOnly | openNoFollow | openDirectory);
                if (!ReferenceEquals(directoryHandle, rootHandle))
                {
                    directoryHandle.Dispose();
                }

                directoryHandle = nextDirectoryHandle;
            }

            var fileHandle = OpenUnixHandleAt(directoryHandle, segments[^1], OpenReadOnly | openNoFollow);
            try
            {
                return new FileStream(fileHandle, FileAccess.Read, bufferSize: 1);
            }
            catch
            {
                fileHandle.Dispose();
                throw;
            }
        }
        finally
        {
            if (directoryHandle is not null && !ReferenceEquals(directoryHandle, rootHandle))
            {
                directoryHandle.Dispose();
            }
        }
    }

    private static SafeFileHandle OpenUnixHandle(string path, int flags)
    {
        var fileDescriptor = Open(path, flags);
        if (fileDescriptor < 0)
        {
            throw CreateOpenException(path);
        }

        return new SafeFileHandle(new IntPtr(fileDescriptor), ownsHandle: true);
    }

    private static SafeFileHandle OpenUnixHandleAt(SafeFileHandle directoryHandle, string path, int flags)
    {
        var fileDescriptor = OpenAt(ToFileDescriptor(directoryHandle), path, flags);
        if (fileDescriptor < 0)
        {
            throw CreateOpenException(path);
        }

        return new SafeFileHandle(new IntPtr(fileDescriptor), ownsHandle: true);
    }

    private static int ToFileDescriptor(SafeFileHandle handle)
    {
        return handle.DangerousGetHandle().ToInt32();
    }

    private static IOException CreateOpenException(string path)
    {
        return new IOException($"Could not open media file without following symlinks: {path}. errno={System.Runtime.InteropServices.Marshal.GetLastWin32Error()}");
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

    private static char[] GetPathSegmentSeparators()
    {
        return Path.DirectorySeparatorChar == Path.AltDirectorySeparatorChar
            ? [Path.DirectorySeparatorChar]
            : [Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar];
    }

    private static bool IsWithinRoot(string fullRootPath, string fullCandidatePath)
    {
        var comparison = PathStringComparison();
        return fullCandidatePath.Equals(fullRootPath, comparison)
            || fullCandidatePath.StartsWith(EnsureTrailingSeparator(fullRootPath), comparison);
    }

    private static bool PathsEqual(string left, string right)
    {
        return Path.GetFullPath(left).Equals(Path.GetFullPath(right), PathStringComparison());
    }

    private static StringComparison PathStringComparison()
    {
        return OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal;
    }

    [System.Runtime.InteropServices.DllImport("libc", EntryPoint = "open", SetLastError = true)]
    private static extern int Open(string path, int flags);

    [System.Runtime.InteropServices.DllImport("libc", EntryPoint = "openat", SetLastError = true)]
    private static extern int OpenAt(int directoryFileDescriptor, string path, int flags);
}

public sealed record MediaFile(string Path, string RelativePath, string ContentType, DateTimeOffset LastModified, long SizeBytes);

public sealed record OpenedMediaFile(FileStream Stream, string ContentType, DateTimeOffset LastModified, long SizeBytes);

public sealed record LibraryItemPage(IReadOnlyList<LibraryItem> Items, long? NextPageOffset)
{
    public static LibraryItemPage Empty { get; } = new([], null);
}

internal sealed record MediaCandidate(string Path, string Title, string Subtitle);

internal sealed class MediaCandidateComparer : IComparer<MediaCandidate>
{
    public static MediaCandidateComparer Instance { get; } = new();

    public int Compare(MediaCandidate? x, MediaCandidate? y)
    {
        if (ReferenceEquals(x, y))
        {
            return 0;
        }

        if (x is null)
        {
            return -1;
        }

        if (y is null)
        {
            return 1;
        }

        var titleComparison = StringComparer.OrdinalIgnoreCase.Compare(x.Title, y.Title);
        if (titleComparison != 0)
        {
            return titleComparison;
        }

        var subtitleComparison = StringComparer.OrdinalIgnoreCase.Compare(x.Subtitle, y.Subtitle);
        return subtitleComparison != 0
            ? subtitleComparison
            : StringComparer.Ordinal.Compare(x.Path, y.Path);
    }
}
