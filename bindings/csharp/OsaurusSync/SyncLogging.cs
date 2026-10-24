using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using Microsoft.Extensions.Logging;
using MsLogLevel = Microsoft.Extensions.Logging.LogLevel;

namespace Osaurus.Sync;

/// <summary>
/// Routes the core's log records into the app. Logging is process-wide: one listener serves
/// every open <see cref="SyncCore"/>.
/// </summary>
public static class SyncLogging
{
    /// <summary>
    /// Sends records at <paramref name="level"/> and more severe to <paramref name="factory"/>,
    /// one logger category per core module. The record's fields are attached as a logging scope.
    /// </summary>
    public static void UseLogger(ILoggerFactory factory, LogLevel level = LogLevel.Info) =>
        OsaurusSyncFfiMethods.SetLogListener(level, new LoggerLogListener(factory));

    /// <summary>Calls <paramref name="onLog"/> for every record, on the core's log thread.</summary>
    public static void SetHandler(Action<LogRecord> onLog, LogLevel level = LogLevel.Info) =>
        OsaurusSyncFfiMethods.SetLogListener(level, new LogCallback(onLog));

    public static void Disable() => OsaurusSyncFfiMethods.SetLogListener(LogLevel.Info, null);

    /// <summary>The fields as <c>key=value</c> pairs, sorted by key.</summary>
    public static string FormattedFields(this LogRecord record) =>
        string.Join(" ", record.Fields.OrderBy(kv => kv.Key, StringComparer.Ordinal).Select(kv => $"{kv.Key}={kv.Value}"));

    private sealed class LogCallback(Action<LogRecord> f) : LogListener
    {
        public void OnLog(LogRecord record) => f(record);
    }
}

/// <summary>Forwards core log records to <see cref="ILogger"/>.</summary>
public sealed class LoggerLogListener(ILoggerFactory factory) : LogListener
{
    private readonly ConcurrentDictionary<string, ILogger> _loggers = new();

    public void OnLog(LogRecord record)
    {
        var logger = _loggers.GetOrAdd(record.Target, factory.CreateLogger);
        var level = record.Level switch
        {
            LogLevel.Error => MsLogLevel.Error,
            LogLevel.Warn => MsLogLevel.Warning,
            LogLevel.Info => MsLogLevel.Information,
            LogLevel.Debug => MsLogLevel.Debug,
            _ => MsLogLevel.Trace,
        };
        if (!logger.IsEnabled(level))
            return;
        using (logger.BeginScope(new Dictionary<string, object>(record.Fields.Select(kv => new KeyValuePair<string, object>(kv.Key, kv.Value)))))
        {
            logger.Log(level, "{Message} {Fields}", record.Message, record.FormattedFields());
        }
    }
}
