#pragma once

#include <QElapsedTimer>
#include <QJsonObject>
#include <QMutex>
#include <QObject>
#include <QStringList>
#include <QVariantList>

class QQuickWindow;

class Backend final : public QObject {
    Q_OBJECT
    Q_PROPERTY(QStringList files READ files NOTIFY filesChanged)
    Q_PROPERTY(QStringList lines READ lines NOTIFY linesChanged)
    Q_PROPERTY(QVariantList symbols READ symbols NOTIFY symbolsChanged)
    Q_PROPERTY(QString selected READ selected NOTIFY selectedChanged)
    Q_PROPERTY(double treeTop READ treeTop NOTIFY positionsChanged)
    Q_PROPERTY(double editorTop READ editorTop NOTIFY positionsChanged)
    Q_PROPERTY(QString scannerText READ scannerText NOTIFY filesChanged)
    Q_PROPERTY(QString languageText READ languageText NOTIFY symbolsChanged)

public:
    explicit Backend(QObject *parent = nullptr);

    QStringList files() const { return m_files; }
    QStringList lines() const { return m_lines; }
    QVariantList symbols() const { return m_symbols; }
    QString selected() const { return m_selected; }
    double treeTop() const { return m_treeTop; }
    double editorTop() const { return m_editorTop; }
    QString scannerText() const;
    QString languageText() const;

    void attachWindow(QQuickWindow *window);
    Q_INVOKABLE QString highlight(const QString &source) const;

signals:
    void filesChanged();
    void linesChanged();
    void symbolsChanged();
    void selectedChanged();
    void positionsChanged();

private slots:
    void onFrameSwapped();

private:
    enum class Stage {
        Initial,
        WaitingScan,
        TreeNeedsFrame,
        WaitingRead,
        TextNeedsFrame,
        StableNeedsFrame,
        WaitingSymbols,
        OutlineNeedsFrame,
        Scrolling,
        Complete,
    };

    void emitEvent(QJsonObject event);
    void memorySample(const QString &label);
    void startScan();
    void startRead(const QString &path, bool isSwitch);
    void startSymbols();
    void stableFrame();
    void advanceScroll();
    void requestFrame();

    QQuickWindow *m_window = nullptr;
    Stage m_stage = Stage::Initial;
    QStringList m_files;
    QStringList m_lines;
    QVariantList m_symbols;
    QString m_selected = QStringLiteral("src/selected.ts");
    double m_treeTop = 0;
    double m_editorTop = 0;
    bool m_autorun = false;
    bool m_currentReadIsSwitch = false;
    int m_switchIndex = 0;
    int m_scrollStep = 0;
    qint64 m_lastFrameNs = 0;
    QList<double> m_frameTimes;
    QString m_fixture;
    QString m_lspCommand;
    QString m_phase;
    QString m_runId;
    QString m_logPath;
    QElapsedTimer m_clock;
    QMutex m_logMutex;
};
