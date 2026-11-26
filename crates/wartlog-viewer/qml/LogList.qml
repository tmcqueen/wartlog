import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// The result list. Log rows are drawn as log lines (newest at the top, live rows prepended);
// other results (summarize, project) as a plain table. Scrolling away from the top pauses the
// live view and counts what arrived; returning to the top shows it.
Item {
    id: root
    signal rowSelected(var row)
    signal filterRequested(string column, var value, bool exclude)
    signal sourceRequested(string display, string kind)

    property bool live: false
    property bool logShape: true
    property var columns: []
    property var pending: []
    property int maxRows: 20000
    readonly property bool atTop: list.contentY <= list.originY + 4
    property string findText: ""

    ListModel { id: model }

    function receive(json, replace) {
        const r = JSON.parse(json)
        if (replace) {
            model.clear()
            pending = []
            logShape = r.log
            columns = r.columns
            for (const row of r.rows) model.append(row)
            list.positionViewAtBeginning()
            return
        }
        if (!r.log || !logShape) return
        if (!atTop) {
            pending = pending.concat(r.rows)
            return
        }
        insertTop(r.rows)
    }

    function insertTop(rows) {
        // rows arrive oldest first: insert each at the top
        for (const row of rows) model.insert(0, row)
        while (model.count > maxRows) model.remove(model.count - 1)
    }

    onAtTopChanged: if (atTop && pending.length > 0) { insertTop(pending); pending = [] }

    function clearSelection() { list.currentIndex = -1 }
    function selectIndex(i) { list.currentIndex = Math.min(i, model.count - 1) }
    function selectWhere(pred) { for (let i = 0; i < model.count; i++) if (pred(model.get(i))) { list.currentIndex = i; return } }
    function openFind() { findBar.visible = true; findField.forceActiveFocus() }
    function findNext() {
        if (findText === "") return
        const n = model.count
        for (let k = 1; k <= n; k++) {
            const i = (list.currentIndex + k) % n
            const m = model.get(i)
            const hay = (m.html || m.text || "").toLowerCase()
            if (hay.indexOf(findText.toLowerCase()) >= 0) { list.currentIndex = i; list.positionViewAtIndex(i, ListView.Center); root.rowSelected(m); return }
        }
    }

    function levelColor(l) {
        return l === "error" || l === "fatal" ? "#da4453" : l === "warn" ? "#f67400" : (l === "debug" || l === "trace") ? palette.mid : palette.text
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        // table header for non-log results
        Row {
            visible: !root.logShape
            Layout.fillWidth: true
            spacing: 0
            Repeater {
                model: root.logShape ? [] : root.columns
                delegate: Label {
                    required property var modelData
                    width: Math.max(90, Math.min(360, list.width / Math.max(1, root.columns.length)))
                    text: modelData
                    font.bold: true
                    padding: 4
                    elide: Text.ElideRight
                }
            }
        }

        ListView {
            id: list
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            model: model
            currentIndex: -1
            reuseItems: true
            boundsBehavior: Flickable.StopAtBounds
            ScrollBar.vertical: ScrollBar { }
            highlightMoveDuration: 0
            keyNavigationEnabled: true
            focus: true
            onCurrentIndexChanged: if (currentIndex >= 0) root.rowSelected(model.get(currentIndex))

            delegate: Rectangle {
                id: d
                required property int index
                required property var model
                width: ListView.view.width
                height: 22
                color: ListView.isCurrentItem ? palette.highlight : (index % 2 ? palette.alternateBase : palette.base)
                readonly property color fg: ListView.isCurrentItem ? palette.highlightedText : palette.text
                readonly property bool current: ListView.isCurrentItem

                MouseArea {
                    anchors.fill: parent
                    acceptedButtons: Qt.LeftButton | Qt.RightButton
                    onClicked: (e) => {
                        list.currentIndex = d.index
                        list.forceActiveFocus()
                        if (e.button === Qt.RightButton && root.logShape) rowMenu.popup()
                    }
                }
                Menu {
                    id: rowMenu
                    MenuItem { text: "Only " + (d.model.source || ""); onTriggered: root.sourceRequested(d.model.source, d.model.kind) }
                    MenuItem { text: "Exclude " + (d.model.source || ""); onTriggered: root.filterRequested("Source", d.model.source, true) }
                    MenuItem { text: "Only level " + (d.model.level || ""); onTriggered: root.filterRequested("Level", d.model.level, false) }
                }

                // log line
                RowLayout {
                    visible: root.logShape
                    anchors.fill: parent
                    anchors.leftMargin: 6
                    anchors.rightMargin: 6
                    spacing: 10
                    Label { text: d.model.ts || ""; font.family: "monospace"; color: d.fg; opacity: 0.75 }
                    Label {
                        text: (d.model.level || "").toUpperCase()
                        Layout.preferredWidth: 46
                        font.family: "monospace"
                        font.bold: d.model.level === "error" || d.model.level === "fatal"
                        color: d.ListView.isCurrentItem ? d.fg : root.levelColor(d.model.level)
                    }
                    Label {
                        text: d.model.source || ""
                        Layout.preferredWidth: 170
                        Layout.maximumWidth: 170
                        elide: Text.ElideMiddle
                        color: d.fg
                        opacity: 0.85
                    }
                    Text {
                        text: d.current ? (d.model.html || "").replace(/<font color="[^"]*">/g, "<b>").replace(/<\/font>/g, "</b>") : (d.model.html || "")
                        textFormat: Text.StyledText
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                        maximumLineCount: 1
                        font.family: "monospace"
                        color: d.fg
                    }
                    Label { visible: (d.model.trace || "") !== ""; text: "⛓"; color: d.fg; opacity: 0.6; ToolTip.text: "Has a trace"; ToolTip.visible: false }
                }
                // table row
                Row {
                    visible: !root.logShape
                    anchors.verticalCenter: parent.verticalCenter
                    Repeater {
                        model: root.logShape ? [] : JSON.parse(d.model.cellsJson || "[]")
                        delegate: Label {
                            required property var modelData
                            width: Math.max(90, Math.min(360, list.width / Math.max(1, root.columns.length)))
                            text: modelData
                            padding: 4
                            elide: Text.ElideRight
                            font.family: "monospace"
                            color: d.fg
                        }
                    }
                }
            }
        }

        // find within results ("/")
        RowLayout {
            id: findBar
            visible: false
            Layout.fillWidth: true
            Layout.margins: 4
            Label { text: "Find:" }
            TextField {
                id: findField
                Layout.fillWidth: true
                onTextChanged: root.findText = text
                onAccepted: root.findNext()
                Keys.onEscapePressed: { findBar.visible = false; list.forceActiveFocus() }
            }
            Button { text: "Next"; onClicked: root.findNext() }
            ToolButton { text: "✕"; onClicked: findBar.visible = false }
        }
    }

    // "new rows" chip while paused
    Button {
        visible: root.live && root.pending.length > 0
        anchors.top: parent.top
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.topMargin: 8
        text: "● Live paused: " + root.pending.length + " new ▲"
        highlighted: true
        onClicked: list.positionViewAtBeginning()
    }
    Label {
        anchors.centerIn: parent
        visible: model.count === 0
        opacity: 0.6
        text: root.live ? "Waiting for matching records…" : "No matching records in this time range."
    }
}
