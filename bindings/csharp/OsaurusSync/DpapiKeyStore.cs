using System;
using System.IO;
using System.Collections.Concurrent;
using System.Runtime.Versioning;
using System.Security.Cryptography;
using System.Text;

namespace Osaurus.Sync;

/// <summary>
/// Stores the device key and user secret encrypted with DPAPI for the current Windows user.
/// Files live under <c>%LOCALAPPDATA%\Osaurus\Sync\keys</c> by default. DPAPI blobs are bound
/// to this user on this machine, so a restored or copied profile gets a fresh device key.
/// </summary>
[SupportedOSPlatform("windows")]
public sealed class DpapiKeyStore : KeyStore
{
    private static readonly byte[] Entropy = Encoding.UTF8.GetBytes("ai.osaurus.sync.keystore.v1");
    private readonly string _dir;

    public DpapiKeyStore(string? directory = null)
    {
        _dir = directory ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Osaurus", "Sync", "keys");
        Directory.CreateDirectory(_dir);
    }

    private string PathFor(string name)
    {
        foreach (var c in name)
        {
            if (!char.IsAsciiLetterOrDigit(c) && c != '-' && c != '_')
                throw new SyncException.InvalidInput(SyncErrorCode.InvalidInput, $"bad key name {name}");
        }
        return Path.Combine(_dir, name + ".dpapi");
    }

    /// <summary>
    /// Returns null only when the item doesn't exist. A file that can't be read or decrypted,
    /// for example one copied from another Windows account, throws, so the core never replaces
    /// the identity it holds.
    /// </summary>
    public byte[]? Load(string name)
    {
        var path = PathFor(name);
        if (!File.Exists(path)) return null;
        try
        {
            return ProtectedData.Unprotect(File.ReadAllBytes(path), Entropy, DataProtectionScope.CurrentUser);
        }
        catch (Exception e) when (e is IOException or CryptographicException or UnauthorizedAccessException)
        {
            throw new SyncException.KeyStore(SyncErrorCode.KeyStore, $"load {name}: {e.Message}");
        }
    }

    public void Store(string name, byte[] value)
    {
        try
        {
            var path = PathFor(name);
            var tmp = path + ".tmp";
            File.WriteAllBytes(tmp, ProtectedData.Protect(value, Entropy, DataProtectionScope.CurrentUser));
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception e) when (e is IOException or CryptographicException or UnauthorizedAccessException)
        {
            throw new SyncException.KeyStore(SyncErrorCode.KeyStore, $"store: {e.Message}");
        }
    }

    public void Delete(string name)
    {
        try
        {
            File.Delete(PathFor(name));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            throw new SyncException.KeyStore(SyncErrorCode.KeyStore, $"delete: {e.Message}");
        }
    }
}

/// <summary>In-memory key store for tests and previews. Secrets are lost on exit.</summary>
public sealed class MemoryKeyStore : KeyStore
{
    private readonly ConcurrentDictionary<string, byte[]> _items = new();

    public byte[]? Load(string name) => _items.TryGetValue(name, out var v) ? v : null;

    public void Store(string name, byte[] value) => _items[name] = value;

    public void Delete(string name) => _items.TryRemove(name, out _);
}
