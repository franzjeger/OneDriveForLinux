#include <KOverlayIconPlugin>
#include <KPluginFactory>

#include <QHash>
#include <QUrl>
#include <QString>
#include <QStringList>
#include <QFile>
#include <QTextStream>
#include <QDir>
#include <QStandardPaths>
#include <QTimer>
#include <QDateTime>
#include <QThread>

#include <sys/xattr.h>
#include <cerrno>
#include <cstring>
#include <csignal>

/**
 * OneDriveOverlayPlugin
 *
 * A KOverlayIconPlugin that reads the "user.onedrive.syncstate" extended
 * attribute served by the OneDrive FUSE filesystem and maps it to a KDE
 * VCS overlay icon name.
 *
 * getOverlays() NEVER blocks — it returns the cached state immediately and
 * schedules a background refresh for stale entries. All getxattr() calls
 * happen on a dedicated QThread (m_workerThread) to keep Dolphin's main
 * thread responsive even when the FUSE filesystem is under heavy load.
 *
 * Install namespace: kf6/overlayicon
 */

// ── Worker ────────────────────────────────────────────────────────────────────

/**
 * XattrWorker lives on a background QThread and performs the blocking
 * getxattr() calls requested by the plugin.  Results are posted back to the
 * main thread via a queued signal.
 */
class XattrWorker : public QObject
{
    Q_OBJECT
public:
    explicit XattrWorker(QObject *parent = nullptr) : QObject(parent) {}

public Q_SLOTS:
    /**
     * Read "user.onedrive.syncstate" for each path in @p paths.
     * Emits resultsReady() with a map of { path → state } for every path
     * that returned a valid xattr (missing/error entries are omitted).
     */
    void refresh(const QStringList &paths)
    {
        QHash<QString, QString> results;
        results.reserve(paths.size());

        for (const QString &localFile : paths) {
            const QByteArray utf8 = localFile.toUtf8();
            char buf[64];
            ssize_t len = ::getxattr(utf8.constData(),
                                     "user.onedrive.syncstate",
                                     buf, sizeof(buf) - 1);
            if (len > 0) {
                buf[len] = '\0';
                results.insert(localFile, QString::fromUtf8(buf, static_cast<int>(len)));
            }
        }

        emit resultsReady(results);
    }

Q_SIGNALS:
    void resultsReady(const QHash<QString, QString> &results);
};

// ── Plugin ────────────────────────────────────────────────────────────────────

class OneDriveOverlayPlugin : public KOverlayIconPlugin
{
    Q_PLUGIN_METADATA(IID "org.kde.overlayicon.onedrive" FILE "onedrive-overlay.json")
    Q_OBJECT

public:
    explicit OneDriveOverlayPlugin(QObject *parent = nullptr)
        : KOverlayIconPlugin(parent)
    {
        m_syncDir = readSyncDir();

        // Worker thread setup — worker object lives on m_workerThread.
        m_worker = new XattrWorker;
        m_workerThread = new QThread(this);
        m_worker->moveToThread(m_workerThread);
        // Clean up worker when thread finishes.
        connect(m_workerThread, &QThread::finished, m_worker, &QObject::deleteLater);
        // Results posted back to main thread via QueuedConnection (automatic
        // because the signal crosses thread boundaries).
        connect(m_worker, &XattrWorker::resultsReady,
                this,     &OneDriveOverlayPlugin::onXattrResults);
        m_workerThread->start();

        // Timer fires in the main thread but only collects stale paths and
        // dispatches them to the worker — it never calls getxattr() itself.
        m_refreshTimer = new QTimer(this);
        m_refreshTimer->setInterval(4000);
        connect(m_refreshTimer, &QTimer::timeout,
                this, &OneDriveOverlayPlugin::onRefreshTimer);
        m_refreshTimer->start();
    }

    ~OneDriveOverlayPlugin() override
    {
        m_workerThread->quit();
        m_workerThread->wait();
    }

    /**
     * Called by Dolphin (main thread) to get the overlay icons for a URL.
     * NEVER blocks — returns cached state immediately.  If the cache entry is
     * stale (or absent), schedules a background fetch; Dolphin will be notified
     * via overlaysChanged() once the result is ready.
     */
    QStringList getOverlays(const QUrl &url) override
    {
        if (!url.isLocalFile())
            return {};

        const QString localFile = url.toLocalFile();

        if (m_syncDir.isEmpty() || !localFile.startsWith(m_syncDir))
            return {};

        if (!isDaemonAlive()) {
            m_cache.clear();
            return {};
        }

        const qint64 now = QDateTime::currentMSecsSinceEpoch();
        auto it = m_cache.constFind(localFile);
        if (it != m_cache.constEnd()) {
            if (now - it->timestamp < kCacheTtlMs)
                return overlaysForState(it->state); // fresh — return immediately

            // Stale — return last-known state immediately and schedule refresh.
            scheduleRefresh(localFile);
            return overlaysForState(it->state);
        }

        // Not in cache — schedule fetch and return nothing for now.
        // overlaysChanged() will fire when the result arrives.
        scheduleRefresh(localFile);
        return {};
    }

private Q_SLOTS:
    /**
     * Timer tick (main thread): collect all stale cached paths and dispatch a
     * single batch refresh to the worker thread.  No getxattr() here.
     */
    void onRefreshTimer()
    {
        if (!isDaemonAlive()) {
            m_cache.clear();
            return;
        }

        const qint64 now = QDateTime::currentMSecsSinceEpoch();
        QStringList stale;
        for (auto it = m_cache.constBegin(); it != m_cache.constEnd(); ++it) {
            if (now - it->timestamp >= kCacheTtlMs)
                stale.append(it.key());
        }

        if (!stale.isEmpty())
            dispatchRefresh(stale);
    }

    /**
     * Called in main thread (via QueuedConnection) when the worker has results.
     * Updates the cache and emits overlaysChanged() for any path whose state
     * changed.
     */
    void onXattrResults(const QHash<QString, QString> &results)
    {
        const qint64 now = QDateTime::currentMSecsSinceEpoch();

        // Paths that the worker was asked about but returned no xattr: bump
        // their timestamp so we don't hammer them every cycle.
        // We track pending paths in m_pendingRefresh.
        for (const QString &path : m_pendingRefresh) {
            if (!results.contains(path)) {
                auto it = m_cache.find(path);
                if (it != m_cache.end())
                    it->timestamp = now;
                // If not in cache at all, leave it — next getOverlays() call
                // will schedule another fetch.
            }
        }
        m_pendingRefresh.clear();

        for (auto rit = results.constBegin(); rit != results.constEnd(); ++rit) {
            const QString &path  = rit.key();
            const QString &state = rit.value();

            auto it = m_cache.find(path);
            if (it == m_cache.end()) {
                // New entry — insert and notify Dolphin.
                m_cache.insert(path, {state, now});
                emit overlaysChanged(QUrl::fromLocalFile(path), overlaysForState(state));
            } else {
                if (state != it->state) {
                    it->state = state;
                    emit overlaysChanged(QUrl::fromLocalFile(path), overlaysForState(state));
                }
                it->timestamp = now;
            }
        }
    }

private:
    // Queue a single path for background refresh (deduplicates via m_pendingRefresh).
    void scheduleRefresh(const QString &path)
    {
        if (m_pendingRefresh.contains(path))
            return;
        m_pendingRefresh.append(path);
        dispatchRefresh({path});
    }

    // Dispatch a list of paths to the worker thread.
    void dispatchRefresh(const QStringList &paths)
    {
        // Add to pending set so onXattrResults() can bump timestamps for misses.
        for (const QString &p : paths) {
            if (!m_pendingRefresh.contains(p))
                m_pendingRefresh.append(p);
        }
        QMetaObject::invokeMethod(m_worker, "refresh",
                                  Qt::QueuedConnection,
                                  Q_ARG(QStringList, paths));
    }

    // Map sync state string → KDE overlay icon name(s).
    static QStringList overlaysForState(const QString &state)
    {
        if (state == QLatin1String("synced")) {
            return {QStringLiteral("vcs-normal")};
        } else if (state == QLatin1String("pinned")) {
            // Always on device — green checkmark, same as synced.
            return {QStringLiteral("vcs-normal")};
        } else if (state == QLatin1String("syncing")) {
            return {QStringLiteral("vcs-update-required")};
        } else if (state == QLatin1String("cloud")) {
            return {QStringLiteral("onedrive-cloud")};
        } else if (state == QLatin1String("partial")) {
            return {QStringLiteral("onedrive-partial")};
        } else if (state == QLatin1String("error")) {
            return {QStringLiteral("vcs-conflicting")};
        } else if (state == QLatin1String("conflict")) {
            return {QStringLiteral("vcs-conflicting")};
        }
        return {};
    }

    // Parse sync_dir from ~/.config/onedrive-linux/config.toml.
    static QString readSyncDir()
    {
        const QString configPath =
            QStandardPaths::writableLocation(QStandardPaths::ConfigLocation)
            + QStringLiteral("/onedrive-linux/config.toml");

        QFile file(configPath);
        if (!file.open(QIODevice::ReadOnly | QIODevice::Text))
            return {};

        QTextStream in(&file);
        while (!in.atEnd()) {
            const QString line = in.readLine().trimmed();
            if (!line.startsWith(QStringLiteral("sync_dir")))
                continue;
            const int eq = line.indexOf(QLatin1Char('='));
            if (eq < 0)
                continue;
            QString value = line.mid(eq + 1).trimmed();
            if (value.length() >= 2
                && ((value.startsWith(QLatin1Char('"')) && value.endsWith(QLatin1Char('"')))
                    || (value.startsWith(QLatin1Char('\'')) && value.endsWith(QLatin1Char('\''))))) {
                value = value.mid(1, value.length() - 2);
            }
            if (value.startsWith(QLatin1Char('~')))
                value = QDir::homePath() + value.mid(1);
            if (!value.endsWith(QLatin1Char('/')))
                value += QLatin1Char('/');
            return value;
        }
        return {};
    }

    // Returns true if the daemon process recorded in the PID file is alive.
    static bool isDaemonAlive()
    {
        const QString runtimeDir = QString::fromLocal8Bit(qgetenv("XDG_RUNTIME_DIR"));
        const QString pidPath = (runtimeDir.isEmpty()
            ? QStandardPaths::writableLocation(QStandardPaths::CacheLocation)
            : runtimeDir) + QStringLiteral("/onedrive-linux.pid");

        QFile f(pidPath);
        if (!f.open(QIODevice::ReadOnly | QIODevice::Text))
            return false;
        bool ok = false;
        const int pid = f.readAll().trimmed().toInt(&ok);
        if (!ok || pid <= 0)
            return false;
        return ::kill(static_cast<pid_t>(pid), 0) == 0;
    }

    struct CacheEntry { QString state; qint64 timestamp; };
    static constexpr qint64 kCacheTtlMs = 5000; // 5 seconds

    QHash<QString, CacheEntry> m_cache;
    QStringList                m_pendingRefresh; // paths dispatched, awaiting results
    QTimer*                    m_refreshTimer  = nullptr;
    XattrWorker*               m_worker        = nullptr;
    QThread*                   m_workerThread  = nullptr;
    QString                    m_syncDir;
};

#include "onedrive-overlay.moc"
