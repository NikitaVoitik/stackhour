#include "backend.h"

#include <QCoreApplication>
#include <QDir>
#include <QDirIterator>
#include <QFile>
#include <QFileInfo>
#include <QFutureWatcher>
#include <QJsonArray>
#include <QJsonDocument>
#include <QMutexLocker>
#include <QProcess>
#include <QQuickWindow>
#include <QRegularExpression>
#include <QTextStream>
#include <QUrl>
#include <QtConcurrent>
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <numbers>
#include <stdexcept>
#include <utility>

namespace {

QStringList scanProject(const QString &root) {
    QStringList files;
    QDir base(root);
    QDirIterator iterator(root, {QStringLiteral("*.ts")}, QDir::Files, QDirIterator::Subdirectories);
    while (iterator.hasNext()) {
        files.push_back(QDir::fromNativeSeparators(base.relativeFilePath(iterator.next())));
    }
    std::sort(files.begin(), files.end());
    return files;
}

QString readSource(const QString &path) {
    QFile file(path);
    if (!file.open(QIODevice::ReadOnly)) {
        throw std::runtime_error(file.errorString().toStdString());
    }
    return QString::fromUtf8(file.readAll());
}

QJsonObject readLspMessage(QProcess &process, QByteArray &buffer) {
    qsizetype separator = -1;
    while ((separator = buffer.indexOf("\r\n\r\n")) < 0) {
        if (!process.waitForReadyRead(30000)) {
            throw std::runtime_error("language server response timed out");
        }
        buffer += process.readAllStandardOutput();
    }
    const QByteArray headers = buffer.left(separator);
    const QRegularExpression lengthPattern(QStringLiteral("Content-Length:\\s*(\\d+)"),
                                           QRegularExpression::CaseInsensitiveOption);
    const auto match = lengthPattern.match(QString::fromLatin1(headers));
    if (!match.hasMatch()) {
        throw std::runtime_error("language server response missing content length");
    }
    const qsizetype length = match.captured(1).toLongLong();
    buffer.remove(0, separator + 4);
    while (buffer.size() < length) {
        if (!process.waitForReadyRead(30000)) {
            throw std::runtime_error("language server response body timed out");
        }
        buffer += process.readAllStandardOutput();
    }
    const QByteArray body = buffer.left(length);
    buffer.remove(0, length);
    return QJsonDocument::fromJson(body).object();
}

void writeLsp(QProcess &process, const QJsonObject &message) {
    const QByteArray body = QJsonDocument(message).toJson(QJsonDocument::Compact);
    process.write("Content-Length: " + QByteArray::number(body.size()) + "\r\n\r\n" + body);
    if (!process.waitForBytesWritten(30000)) {
        throw std::runtime_error("language server write timed out");
    }
}

QJsonObject requestLsp(QProcess &process, QByteArray &buffer, int id, const QString &method,
                       const QJsonObject &params) {
    writeLsp(process, {
        {QStringLiteral("jsonrpc"), QStringLiteral("2.0")},
        {QStringLiteral("id"), id},
        {QStringLiteral("method"), method},
        {QStringLiteral("params"), params},
    });
    for (;;) {
        const QJsonObject message = readLspMessage(process, buffer);
        if (message.value(QStringLiteral("id")).toInt(-1) == id) {
            return message;
        }
    }
}

void notifyLsp(QProcess &process, const QString &method, const QJsonObject &params) {
    writeLsp(process, {
        {QStringLiteral("jsonrpc"), QStringLiteral("2.0")},
        {QStringLiteral("method"), method},
        {QStringLiteral("params"), params},
    });
}

QVariantList documentSymbols(const QString &command, const QString &root, const QString &relative) {
    QProcess process;
    process.setProgram(command);
    process.setArguments({QStringLiteral("--stdio")});
    process.setWorkingDirectory(root);
    process.setProcessChannelMode(QProcess::SeparateChannels);
    process.start();
    if (!process.waitForStarted(30000)) {
        throw std::runtime_error(process.errorString().toStdString());
    }
    QByteArray buffer;
    const QString rootUri = QUrl::fromLocalFile(QFileInfo(root).canonicalFilePath()).toString();
    requestLsp(process, buffer, 1, QStringLiteral("initialize"), {
        {QStringLiteral("processId"), static_cast<qint64>(QCoreApplication::applicationPid())},
        {QStringLiteral("rootUri"), rootUri},
        {QStringLiteral("capabilities"), QJsonObject{
            {QStringLiteral("textDocument"), QJsonObject{
                {QStringLiteral("documentSymbol"), QJsonObject{
                    {QStringLiteral("hierarchicalDocumentSymbolSupport"), true},
                }},
            }},
        }},
        {QStringLiteral("workspaceFolders"), QJsonArray{
            QJsonObject{{QStringLiteral("uri"), rootUri},
                        {QStringLiteral("name"), QStringLiteral("controlled-fixture")}},
        }},
    });
    notifyLsp(process, QStringLiteral("initialized"), {});
    const QString absolute = QDir(root).filePath(relative);
    const QString uri = QUrl::fromLocalFile(QFileInfo(absolute).canonicalFilePath()).toString();
    const QString source = readSource(absolute);
    notifyLsp(process, QStringLiteral("textDocument/didOpen"), {
        {QStringLiteral("textDocument"), QJsonObject{
            {QStringLiteral("uri"), uri},
            {QStringLiteral("languageId"), QStringLiteral("typescript")},
            {QStringLiteral("version"), 1},
            {QStringLiteral("text"), source},
        }},
    });
    const QJsonObject response = requestLsp(process, buffer, 2,
        QStringLiteral("textDocument/documentSymbol"),
        {{QStringLiteral("textDocument"), QJsonObject{{QStringLiteral("uri"), uri}}}});
    QVariantList symbols;
    for (const QJsonValue value : response.value(QStringLiteral("result")).toArray()) {
        const QJsonObject symbol = value.toObject();
        symbols.push_back(QVariantMap{
            {QStringLiteral("name"), symbol.value(QStringLiteral("name")).toString()},
            {QStringLiteral("kind"), symbol.value(QStringLiteral("kind")).toInt()},
        });
    }
    process.kill();
    process.waitForFinished(1000);
    return symbols;
}

QList<int> descendants(int root) {
    QList<int> pids{root};
#ifdef Q_OS_LINUX
    bool changed = true;
    while (changed) {
        changed = false;
        QDir proc(QStringLiteral("/proc"));
        for (const QString &entry : proc.entryList(QDir::Dirs | QDir::NoDotAndDotDot)) {
            bool ok = false;
            const int pid = entry.toInt(&ok);
            if (!ok || pids.contains(pid)) continue;
            QFile status(QStringLiteral("/proc/%1/status").arg(pid));
            if (!status.open(QIODevice::ReadOnly)) continue;
            const QRegularExpression pattern(QStringLiteral("^PPid:\\s+(\\d+)"),
                                             QRegularExpression::MultilineOption);
            const auto match = pattern.match(QString::fromUtf8(status.readAll()));
            if (match.hasMatch() && pids.contains(match.captured(1).toInt())) {
                pids.push_back(pid);
                changed = true;
            }
        }
    }
#endif
    return pids;
}

QJsonArray processMemory() {
    QJsonArray rows;
#ifdef Q_OS_LINUX
    for (const int pid : descendants(static_cast<int>(QCoreApplication::applicationPid()))) {
        QFile status(QStringLiteral("/proc/%1/status").arg(pid));
        QFile cmdline(QStringLiteral("/proc/%1/cmdline").arg(pid));
        if (!status.open(QIODevice::ReadOnly)) continue;
        cmdline.open(QIODevice::ReadOnly);
        const QString statusText = QString::fromUtf8(status.readAll());
        const QString command = QString::fromUtf8(cmdline.readAll()).replace(QChar('\0'), QChar(' '));
        const auto match = QRegularExpression(QStringLiteral("^VmRSS:\\s+(\\d+)"),
                                              QRegularExpression::MultilineOption).match(statusText);
        const QString group = command.contains(QStringLiteral("typescript-language-server"))
            ? QStringLiteral("typescript-language-server")
            : command.contains(QStringLiteral("tsserver")) ? QStringLiteral("tsserver")
                                                           : QStringLiteral("ui-runtime");
        rows.append(QJsonObject{{QStringLiteral("pid"), pid},
                                {QStringLiteral("rssKb"), match.captured(1).toLongLong()},
                                {QStringLiteral("group"), group}});
    }
#endif
    return rows;
}

QString htmlEscape(QString value) {
    return value.replace('&', QStringLiteral("&amp;"))
        .replace('<', QStringLiteral("&lt;"))
        .replace('>', QStringLiteral("&gt;"));
}

} // namespace

Backend::Backend(QObject *parent) : QObject(parent) {
    m_clock.start();
    m_fixture = qEnvironmentVariable("BENCH_FIXTURE", "../.fixture");
    m_lspCommand = qEnvironmentVariable("BENCH_LSP", "typescript-language-server");
    m_phase = qEnvironmentVariable("BENCH_PHASE", "visual");
    m_runId = qEnvironmentVariable("BENCH_RUN_ID", "manual");
    m_logPath = qEnvironmentVariable("BENCH_LOG");
    m_autorun = qEnvironmentVariable("BENCH_AUTORUN") == QStringLiteral("1");
    emitEvent({{QStringLiteral("event"), QStringLiteral("process_start")}});
}

QString Backend::scannerText() const {
    return m_files.isEmpty() ? QStringLiteral("waiting…")
                             : QStringLiteral("%1 TypeScript files").arg(m_files.size());
}

QString Backend::languageText() const {
    return m_symbols.isEmpty() ? QStringLiteral("language server initializing…")
                               : QStringLiteral("%1 document symbols").arg(m_symbols.size());
}

void Backend::attachWindow(QQuickWindow *window) {
    m_window = window;
    connect(window, &QQuickWindow::frameSwapped, this, &Backend::onFrameSwapped,
            Qt::QueuedConnection);
    requestFrame();
}

void Backend::emitEvent(QJsonObject event) {
    QMutexLocker lock(&m_logMutex);
    QJsonObject row{
        {QStringLiteral("schemaVersion"), 1},
        {QStringLiteral("candidate"), QStringLiteral("qt-qml")},
        {QStringLiteral("phase"), m_phase},
        {QStringLiteral("runId"), m_runId},
        {QStringLiteral("timestampNs"), m_clock.nsecsElapsed()},
    };
    for (auto iterator = event.begin(); iterator != event.end(); ++iterator) {
        row.insert(iterator.key(), iterator.value());
    }
    const QByteArray line = QJsonDocument(row).toJson(QJsonDocument::Compact) + '\n';
    if (m_logPath.isEmpty()) {
        fwrite(line.constData(), 1, static_cast<size_t>(line.size()), stdout);
        fflush(stdout);
    } else {
        QFile file(m_logPath);
        if (file.open(QIODevice::WriteOnly | QIODevice::Append)) file.write(line);
    }
}

void Backend::memorySample(const QString &label) {
    emitEvent({{QStringLiteral("event"), QStringLiteral("memory_sample")},
               {QStringLiteral("label"), label},
               {QStringLiteral("processes"), processMemory()}});
}

void Backend::onFrameSwapped() {
    switch (m_stage) {
    case Stage::Initial:
        emitEvent({{QStringLiteral("event"), QStringLiteral("first_frame")}});
        memorySample(QStringLiteral("idle"));
        emitEvent({{QStringLiteral("event"), QStringLiteral("project_open_requested")}});
        startScan();
        break;
    case Stage::TreeNeedsFrame:
        emitEvent({{QStringLiteral("event"), QStringLiteral("project_tree_visible")}});
        emitEvent({{QStringLiteral("event"), QStringLiteral("tree_presented")}});
        startRead(QStringLiteral("src/selected.ts"), false);
        break;
    case Stage::TextNeedsFrame:
        emitEvent({{QStringLiteral("event"), QStringLiteral("text_presented")},
                   {QStringLiteral("path"), m_selected}});
        m_stage = Stage::StableNeedsFrame;
        requestFrame();
        break;
    case Stage::StableNeedsFrame:
        stableFrame();
        break;
    case Stage::OutlineNeedsFrame:
        emitEvent({{QStringLiteral("event"), QStringLiteral("outline_presented")}});
        memorySample(QStringLiteral("loaded"));
        if (m_autorun) {
            m_stage = Stage::Scrolling;
            m_scrollStep = 0;
            m_lastFrameNs = m_clock.nsecsElapsed();
            advanceScroll();
        } else {
            m_stage = Stage::Complete;
        }
        break;
    case Stage::Scrolling:
        advanceScroll();
        break;
    default:
        break;
    }
}

void Backend::startScan() {
    m_stage = Stage::WaitingScan;
    auto *watcher = new QFutureWatcher<QStringList>(this);
    connect(watcher, &QFutureWatcher<QStringList>::finished, this, [this, watcher] {
        try {
            m_files = watcher->result();
            emitEvent({{QStringLiteral("event"), QStringLiteral("project_scan_complete")},
                       {QStringLiteral("count"), m_files.size()}});
            emit filesChanged();
            m_stage = Stage::TreeNeedsFrame;
            requestFrame();
        } catch (const std::exception &error) {
            emitEvent({{QStringLiteral("event"), QStringLiteral("benchmark_error")},
                       {QStringLiteral("message"), QString::fromUtf8(error.what())}});
            m_stage = Stage::Complete;
        }
        watcher->deleteLater();
    });
    watcher->setFuture(QtConcurrent::run([root = m_fixture] { return scanProject(root); }));
}

void Backend::startRead(const QString &path, bool isSwitch) {
    m_selected = path;
    m_currentReadIsSwitch = isSwitch;
    emit selectedChanged();
    emitEvent({{QStringLiteral("event"), QStringLiteral("file_click")},
               {QStringLiteral("path"), path}});
    m_stage = Stage::WaitingRead;
    auto *watcher = new QFutureWatcher<QString>(this);
    connect(watcher, &QFutureWatcher<QString>::finished, this, [this, watcher, path] {
        try {
            const QString source = watcher->result();
            emitEvent({{QStringLiteral("event"), QStringLiteral("disk_read_complete")},
                       {QStringLiteral("path"), path},
                       {QStringLiteral("bytes"), source.toUtf8().size()}});
            m_lines = source.split('\n');
            m_editorTop = 0;
            emit linesChanged();
            emit positionsChanged();
            m_stage = Stage::TextNeedsFrame;
            requestFrame();
        } catch (const std::exception &error) {
            emitEvent({{QStringLiteral("event"), QStringLiteral("benchmark_error")},
                       {QStringLiteral("message"), QString::fromUtf8(error.what())}});
            m_stage = Stage::Complete;
        }
        watcher->deleteLater();
    });
    watcher->setFuture(QtConcurrent::run(
        [absolute = QDir(m_fixture).filePath(path)] { return readSource(absolute); }));
}

void Backend::startSymbols() {
    emitEvent({{QStringLiteral("event"), QStringLiteral("lsp_request")},
               {QStringLiteral("method"), QStringLiteral("textDocument/documentSymbol")}});
    m_stage = Stage::WaitingSymbols;
    auto *watcher = new QFutureWatcher<QVariantList>(this);
    connect(watcher, &QFutureWatcher<QVariantList>::finished, this, [this, watcher] {
        try {
            m_symbols = watcher->result();
            emitEvent({{QStringLiteral("event"), QStringLiteral("lsp_response")},
                       {QStringLiteral("count"), m_symbols.size()}});
            emit symbolsChanged();
            m_stage = Stage::OutlineNeedsFrame;
            requestFrame();
        } catch (const std::exception &error) {
            emitEvent({{QStringLiteral("event"), QStringLiteral("benchmark_error")},
                       {QStringLiteral("message"), QString::fromUtf8(error.what())}});
            m_stage = Stage::Complete;
        }
        watcher->deleteLater();
    });
    watcher->setFuture(QtConcurrent::run([command = m_lspCommand, root = m_fixture] {
        return documentSymbols(command, root, QStringLiteral("src/selected.ts"));
    }));
}

void Backend::stableFrame() {
    emitEvent({{QStringLiteral("event"), QStringLiteral("stable_frame")},
               {QStringLiteral("path"), m_selected}});
    if (!m_currentReadIsSwitch) {
        startSymbols();
        return;
    }
    ++m_switchIndex;
    if (m_switchIndex < 30) {
        startRead(m_switchIndex % 2 == 0 ? QStringLiteral("src/alternate.ts")
                                        : QStringLiteral("src/selected.ts"),
                  true);
    } else {
        emitEvent({{QStringLiteral("event"), QStringLiteral("benchmark_complete")}});
        memorySample(QStringLiteral("complete"));
        m_stage = Stage::Complete;
        QCoreApplication::quit();
    }
}

void Backend::advanceScroll() {
    const qint64 now = m_clock.nsecsElapsed();
    if (m_scrollStep > 0) {
        m_frameTimes.push_back((now - m_lastFrameNs) / 1'000'000.0);
        m_lastFrameNs = now;
    }
    if (m_scrollStep < 640) {
        const int leg = m_scrollStep / 160;
        const int step = m_scrollStep % 160;
        const double t = step / 159.0;
        const double eased = (1.0 - std::cos(std::numbers::pi * t)) / 2.0;
        const double value = leg % 2 == 0 ? eased : 1.0 - eased;
        m_editorTop = value * 19'970.0;
        m_treeTop = value * 5'080.0;
        ++m_scrollStep;
        emit positionsChanged();
        requestFrame();
        return;
    }
    double worst = 0;
    for (const double duration : std::as_const(m_frameTimes)) {
        worst = std::max(worst, duration);
        emitEvent({{QStringLiteral("event"), QStringLiteral("frame")},
                   {QStringLiteral("durationMs"), duration},
                   {QStringLiteral("workload"), QStringLiteral("scroll")}});
    }
    emitEvent({{QStringLiteral("event"), QStringLiteral("main_thread_stall")},
               {QStringLiteral("durationMs"), std::max(0.0, worst - 16.7)},
               {QStringLiteral("workload"), QStringLiteral("scroll")}});
    m_switchIndex = 0;
    startRead(QStringLiteral("src/alternate.ts"), true);
}

void Backend::requestFrame() {
    if (m_window) m_window->update();
}

QString Backend::highlight(const QString &source) const {
    static const QRegularExpression tokens(QStringLiteral(
        "//.*$|\"[^\"\\n]*\"|\\b(?:export|interface|const|function|return|type|void|null|boolean|string|number)\\b|\\b\\d+\\b|\\b(?:Item|Result|Record)\\d+\\b|\\bformat\\d+\\b"));
    QString result;
    qsizetype cursor = 0;
    auto matches = tokens.globalMatch(source);
    while (matches.hasNext()) {
        const auto match = matches.next();
        result += htmlEscape(source.sliced(cursor, match.capturedStart() - cursor));
        const QString value = match.captured();
        QString color;
        QString extra;
        if (value.startsWith(QStringLiteral("//"))) {
            color = QStringLiteral("#636b7a");
            extra = QStringLiteral(";font-style:italic");
        } else if (value.startsWith('"')) {
            color = QStringLiteral("#c3e88d");
        } else if (QRegularExpression(QStringLiteral("^\\d+$")).match(value).hasMatch()) {
            color = QStringLiteral("#f78c6c");
        } else if (value.startsWith(QStringLiteral("Item"))
                   || value.startsWith(QStringLiteral("Result"))
                   || value.startsWith(QStringLiteral("Record"))) {
            color = QStringLiteral("#ffcb6b");
        } else if (value.startsWith(QStringLiteral("format"))) {
            color = QStringLiteral("#82aaff");
        } else {
            color = QStringLiteral("#c792ea");
        }
        result += QStringLiteral("<span style=\"color:%1%2\">%3</span>")
                      .arg(color, extra, htmlEscape(value));
        cursor = match.capturedEnd();
    }
    result += htmlEscape(source.sliced(cursor));
    return result;
}
