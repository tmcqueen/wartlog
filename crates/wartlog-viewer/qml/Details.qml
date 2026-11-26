import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// Bottom pane: the selected record (every field with Only this / Exclude), its context, its trace.
Pane {
    id: pane
    padding: 0
    signal filterRequested(string column, var value, bool exclude)
    signal contextRequested(real id, string scope)
    signal traceRequested(string traceId)

    property var rec: backend.details === "" ? null : JSON.parse(backend.details)
    property var ctx: backend.context === "" ? null : JSON.parse(backend.context)

    Connections {
        target: backend
        function onContextChanged() { tabs.currentIndex = 1 }
        function onTraceChanged() { tabs.currentIndex = 2 }
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0
        TabBar {
            id: tabs
            Layout.fillWidth: true
            TabButton { text: "Details"; width: implicitWidth }
            TabButton { text: "Context"; width: implicitWidth; enabled: pane.ctx !== null }
            TabButton { text: "Trace"; width: implicitWidth; enabled: backend.trace !== "" }
        }
        StackLayout {
            currentIndex: tabs.currentIndex
            Layout.fillWidth: true
            Layout.fillHeight: true

            // details
            ScrollView {
                clip: true
                contentWidth: availableWidth
                ColumnLayout {
                    width: parent.width
                    spacing: 6
                    Label { visible: pane.rec === null; text: "Select a record."; opacity: 0.6; Layout.margins: 10 }
                    RowLayout {
                        visible: pane.rec !== null
                        Layout.fillWidth: true
                        Layout.margins: 6
                        Button { text: "Show context ±50"; onClicked: pane.contextRequested(pane.rec.id, "source") }
                        Button { text: "Same thread"; visible: pane.rec !== null && pane.rec.hasThread; onClicked: pane.contextRequested(pane.rec.id, "thread") }
                        Button { text: "Open trace"; visible: pane.rec !== null && pane.rec.trace !== ""; onClicked: pane.traceRequested(pane.rec.trace) }
                        Item { Layout.fillWidth: true }
                        Label { text: pane.rec ? "#" + pane.rec.id : ""; opacity: 0.6 }
                    }
                    Label { visible: pane.rec !== null; text: pane.rec ? pane.rec.time : ""; Layout.leftMargin: 8; font.family: "monospace" }
                    TextArea {
                        visible: pane.rec !== null
                        Layout.fillWidth: true
                        Layout.leftMargin: 6
                        Layout.rightMargin: 6
                        readOnly: true
                        selectByMouse: true
                        wrapMode: TextEdit.Wrap
                        font.family: "monospace"
                        text: pane.rec ? pane.rec.body : ""
                    }
                    Label {
                        visible: pane.rec !== null && pane.rec.catalog !== ""
                        Layout.fillWidth: true
                        Layout.margins: 8
                        wrapMode: Text.Wrap
                        opacity: 0.8
                        text: pane.rec ? pane.rec.catalog : ""
                    }
                    Repeater {
                        model: pane.rec ? pane.rec.fields : []
                        delegate: RowLayout {
                            required property var modelData
                            Layout.fillWidth: true
                            Layout.leftMargin: 8
                            Layout.rightMargin: 8
                            spacing: 8
                            Label { text: modelData.key; Layout.preferredWidth: 200; elide: Text.ElideMiddle; opacity: modelData.resource ? 0.55 : 0.8 }
                            TextEdit {
                                text: modelData.value
                                readOnly: true
                                selectByMouse: true
                                Layout.fillWidth: true
                                wrapMode: TextEdit.Wrap
                                font.family: "monospace"
                                color: palette.text
                            }
                            ToolButton {
                                text: "＝"
                                visible: modelData.filter !== ""
                                onClicked: pane.filterRequested(modelData.filter, modelData.typed !== undefined ? modelData.typed : modelData.value, false)
                                ToolTip.text: "Only this"; ToolTip.visible: hovered; ToolTip.delay: 400
                            }
                            ToolButton {
                                text: "≠"
                                visible: modelData.filter !== ""
                                onClicked: pane.filterRequested(modelData.filter, modelData.typed !== undefined ? modelData.typed : modelData.value, true)
                                ToolTip.text: "Exclude"; ToolTip.visible: hovered; ToolTip.delay: 400
                            }
                        }
                    }
                }
            }

            // context
            ListView {
                clip: true
                model: pane.ctx ? pane.ctx.rows : []
                ScrollBar.vertical: ScrollBar { }
                onModelChanged: if (pane.ctx && pane.ctx.center >= 0) positionViewAtIndex(pane.ctx.center, ListView.Center)
                delegate: Rectangle {
                    required property var modelData
                    required property int index
                    width: ListView.view.width
                    height: 22
                    color: pane.ctx && index === pane.ctx.center ? palette.highlight : "transparent"
                    readonly property color fg: pane.ctx && index === pane.ctx.center ? palette.highlightedText : palette.text
                    RowLayout {
                        anchors.fill: parent
                        anchors.leftMargin: 6
                        spacing: 10
                        Label { text: modelData.ts; font.family: "monospace"; color: parent.parent.fg; opacity: 0.75 }
                        Label { text: modelData.level.toUpperCase(); Layout.preferredWidth: 46; font.family: "monospace"; color: parent.parent.fg }
                        Text { text: modelData.html; textFormat: Text.StyledText; Layout.fillWidth: true; elide: Text.ElideRight; font.family: "monospace"; color: parent.parent.fg }
                    }
                    MouseArea { anchors.fill: parent; onDoubleClicked: backend.showDetails(modelData.rid) }
                }
            }

            TracePanel { }
        }
    }
}
