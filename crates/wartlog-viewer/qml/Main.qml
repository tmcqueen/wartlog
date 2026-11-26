import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import org.wartlog.viewer

ApplicationWindow {
    id: win
    width: 1360
    height: 860
    visible: true
    title: "WartLog"

    property string query: "logs"
    property string range: "1h"
    property bool showTemplates: false
    property var selected: null          // row object of the selected log row
    property string lastNotice: ""
    // what the user asked for; live tail may drop out (bad query, daemon restart) and comes back
    property bool wantLive: true

    Backend {
        id: backend
        onRowsReady: (json, replace) => logList.receive(json, replace)
        onNotice: (text) => { win.lastNotice = text; noticeTimer.restart() }
        onDaemonChanged: {
            if (!daemon && live) setTail(false, win.query)
            else if (daemon && win.wantLive && !live) setTail(true, win.query)
        }
    }

    function run(q) {
        if (q !== undefined) win.query = q
        queryBar.text = win.query
        backend.runQuery(win.query, win.range)
        if (showTemplates) backend.loadTemplates(win.query, win.range)
        if (wantLive && backend.daemon) backend.setTail(true, win.query)
    }

    function toggleLive() {
        wantLive = !backend.live
        backend.setTail(wantLive, win.query)
    }

    function filter(column, value, exclude) {
        run(backend.addFilter(win.query, column, JSON.stringify(value), exclude))
    }

    function openSource(display, kind) {
        run(backend.onlySource(win.query, display, kind))
        wantLive = true
        if (backend.daemon && !backend.live) backend.setTail(true, win.query)
    }

    Component.onCompleted: {
        console.log("WartLog viewer started; store " + backend.storePath)
        backend.refresh()
        run("logs")
    }

    Timer { interval: 5000; running: true; repeat: true; onTriggered: backend.refresh() }
    // first refresh is async; turn live tail on once the daemon answers
    Connections {
        target: backend
        property bool firstAnswer: true
        function onSourcesChanged() {
            if (firstAnswer && backend.daemon && win.wantLive && !backend.live) backend.setTail(true, win.query)
            firstAnswer = false
        }
    }
    // WARTLOG_SCREENSHOT=path: save the window after the first results, then quit.
    // WARTLOG_SCREENSHOT_STEP drives the UI first (details, context, trace, templates, error).
    Timer {
        interval: 1500
        running: backend.screenshot !== "" && backend.screenshotStep !== ""
        onTriggered: {
            const s = backend.screenshotStep
            if (s === "details" || s === "context") logList.selectIndex(5)
            if (s === "context") detailsPane.contextRequested(win.selected.rid, "source")
            if (s === "trace") { logList.selectIndex(1); backend.loadTrace(win.selected.trace) }
            if (s === "templates") { win.showTemplates = true; backend.loadTemplates(win.query, win.range) }
            if (s === "error") win.run("logs | where Level = \"error\"")
            if (s === "filter") { logList.selectIndex(1); win.openSource(win.selected.source, win.selected.kind) }
        }
    }
    Timer {
        interval: 3000
        running: backend.screenshot !== ""
        onTriggered: {
            const ok = win.contentItem.parent.grabToImage(function (r) {
                console.log("screenshot saved: " + r.saveToFile(backend.screenshot) + " " + backend.screenshot)
                Qt.quit()
            })
            console.log("screenshot requested: " + ok)
        }
    }
    Timer { id: noticeTimer; interval: 6000; onTriggered: win.lastNotice = "" }

    Shortcut { sequence: "Ctrl+K"; onActivated: { queryBar.forceActiveFocus(); queryBar.selectAll() } }
    Shortcut { sequence: "Ctrl+L"; enabled: backend.daemon; onActivated: win.toggleLive() }
    Shortcut { sequence: "/"; enabled: !queryBar.activeFocus; onActivated: logList.openFind() }
    Shortcut { sequence: "Esc"; onActivated: { logList.clearSelection(); win.selected = null } }
    Shortcut { sequence: "F5"; onActivated: win.run() }

    header: ToolBar {
        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 6
            anchors.rightMargin: 6
            spacing: 6
            ComboBox {
                id: rangeBox
                model: [
                    { text: "Last 5 min", value: "5m" }, { text: "Last 15 min", value: "15m" }, { text: "Last hour", value: "1h" },
                    { text: "Last 6 hours", value: "6h" }, { text: "Last 24 hours", value: "24h" }, { text: "Last 7 days", value: "7d" },
                    { text: "Last 30 days", value: "30d" }, { text: "All time", value: "all" }
                ]
                textRole: "text"
                valueRole: "value"
                currentIndex: 2
                implicitContentWidthPolicy: ComboBox.WidestText
                onActivated: { win.range = currentValue; win.run() }
                function select(v) { for (let i = 0; i < model.length; i++) if (model[i].value === v) currentIndex = i; win.range = v }
            }
            QueryBar {
                id: queryBar
                Layout.fillWidth: true
                onSubmitted: (q) => win.run(q)
            }
            Button { text: "Run"; onClicked: win.run(queryBar.text); ToolTip.text: "Run the query (Enter)"; ToolTip.visible: hovered }
            ToolButton {
                text: backend.live ? "● Live" : "○ Live"
                checkable: true
                checked: backend.live
                enabled: backend.daemon
                onClicked: win.toggleLive()
                ToolTip.text: backend.daemon ? "Live tail (Ctrl+L)" : "Live tail needs wartlogd"
                ToolTip.visible: hovered
            }
            ToolButton {
                text: "Templates"
                checkable: true
                checked: win.showTemplates
                onClicked: { win.showTemplates = !win.showTemplates; if (win.showTemplates) backend.loadTemplates(win.query, win.range) }
                ToolTip.text: "Group the results by message template"
                ToolTip.visible: hovered
            }
            ToolButton { text: "Save view"; onClicked: saveDialog.open() }
        }
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        // no daemon: say what is off and offer to start it
        Rectangle {
            Layout.fillWidth: true
            visible: !backend.daemon
            color: Qt.rgba(0.96, 0.45, 0.0, 0.18)
            implicitHeight: bannerRow.implicitHeight + 12
            RowLayout {
                id: bannerRow
                anchors.fill: parent
                anchors.margins: 6
                Label { Layout.fillWidth: true; wrapMode: Text.Wrap; text: "wartlogd is not running. Search, context, templates and traces work from the files; live tail and source changes are off." }
                Button { text: "Start wartlogd"; onClicked: backend.startDaemon() }
            }
        }
        // query error with the bad part marked in the query bar
        Label {
            Layout.fillWidth: true
            Layout.margins: 4
            visible: backend.error !== ""
            text: "⚠ " + backend.error
            color: "#da4453"
            wrapMode: Text.Wrap
        }

        SplitView {
            Layout.fillWidth: true
            Layout.fillHeight: true
            orientation: Qt.Horizontal

            Sidebar {
                SplitView.preferredWidth: 260
                SplitView.minimumWidth: 160
                onSourceClicked: (display, kind) => win.openSource(display, kind)
                onViewClicked: (q, r) => { rangeBox.select(r); win.run(q) }
            }

            SplitView {
                orientation: Qt.Vertical
                SplitView.fillWidth: true

                StackLayout {
                    SplitView.fillHeight: true
                    SplitView.minimumHeight: 120
                    currentIndex: win.showTemplates ? 1 : 0
                    LogList {
                        id: logList
                        live: backend.live
                        onRowSelected: (row) => { win.selected = row; if (row && row.rid >= 0) backend.showDetails(row.rid) }
                        onFilterRequested: (column, value, exclude) => win.filter(column, value, exclude)
                        onSourceRequested: (display, kind) => win.openSource(display, kind)
                    }
                    TemplatesView {
                        onTemplateClicked: (t, exclude) => win.filter("Template", t, exclude)
                    }
                }

                Details {
                    id: detailsPane
                    SplitView.preferredHeight: 300
                    SplitView.minimumHeight: 80
                    onFilterRequested: (column, value, exclude) => win.filter(column, value, exclude)
                    onContextRequested: (id, scope) => backend.loadContext(id, scope)
                    onTraceRequested: (t) => backend.loadTrace(t)
                }
            }
        }
    }

    footer: ToolBar {
        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 8
            anchors.rightMargin: 8
            BusyIndicator { running: backend.busy; visible: backend.busy; implicitWidth: 20; implicitHeight: 20 }
            Label { text: backend.status; elide: Text.ElideRight; Layout.fillWidth: true }
            Label { text: win.lastNotice; elide: Text.ElideLeft; Layout.maximumWidth: win.width / 2; color: palette.highlight }
            Label { text: backend.storePath; opacity: 0.6; elide: Text.ElideMiddle; Layout.maximumWidth: 280 }
        }
    }

    Dialog {
        id: saveDialog
        title: "Save view"
        anchors.centerIn: parent
        modal: true
        standardButtons: Dialog.Save | Dialog.Cancel
        ColumnLayout {
            Label { text: "Name for this query and time range" }
            TextField { id: viewName; Layout.preferredWidth: 320; placeholderText: "foo errors"; onAccepted: saveDialog.accept() }
            Label { text: win.query; opacity: 0.7; elide: Text.ElideRight; Layout.preferredWidth: 320 }
        }
        onOpened: { viewName.text = ""; viewName.forceActiveFocus() }
        onAccepted: if (viewName.text.trim() !== "") backend.saveView(viewName.text.trim(), win.query, win.range)
    }
}
