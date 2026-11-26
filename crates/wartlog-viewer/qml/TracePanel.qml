import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// A trace: spans as a waterfall (name indented by depth, bar by start and duration), then its logs.
Item {
    id: tp
    property var t: backend.trace === "" ? null : JSON.parse(backend.trace)

    ColumnLayout {
        anchors.fill: parent
        anchors.margins: 6
        spacing: 4
        Label {
            text: tp.t ? "Trace " + tp.t.id + "   ·   " + tp.t.spans.items.length + " spans, " + tp.t.logs.rows.length + " logs, " + tp.t.spans.total : ""
            font.bold: true
        }
        ListView {
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(contentHeight, parent.height * 0.6)
            clip: true
            model: tp.t ? tp.t.spans.items : []
            ScrollBar.vertical: ScrollBar { }
            delegate: Item {
                required property var modelData
                width: ListView.view.width
                height: 22
                RowLayout {
                    anchors.fill: parent
                    spacing: 8
                    Label {
                        Layout.preferredWidth: 300
                        leftPadding: modelData.depth * 14
                        text: modelData.name
                        elide: Text.ElideRight
                        color: modelData.status === "error" ? "#da4453" : palette.text
                    }
                    Label { Layout.preferredWidth: 140; text: modelData.source; elide: Text.ElideRight; opacity: 0.8 }
                    Item {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        Rectangle {
                            x: parent.width * modelData.offset
                            width: Math.max(2, parent.width * modelData.width)
                            height: 10
                            anchors.verticalCenter: parent.verticalCenter
                            radius: 2
                            color: modelData.status === "error" ? "#da4453" : "#3daee9"
                        }
                    }
                    Label { Layout.preferredWidth: 90; text: modelData.duration; horizontalAlignment: Text.AlignRight; font.family: "monospace" }
                }
            }
        }
        MenuSeparator { Layout.fillWidth: true }
        ListView {
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            model: tp.t ? tp.t.logs.rows : []
            ScrollBar.vertical: ScrollBar { }
            delegate: RowLayout {
                required property var modelData
                width: ListView.view.width
                height: 22
                spacing: 10
                Label { text: modelData.ts; font.family: "monospace"; opacity: 0.75 }
                Label { text: modelData.level.toUpperCase(); Layout.preferredWidth: 46; font.family: "monospace" }
                Label { text: modelData.source; Layout.preferredWidth: 140; elide: Text.ElideRight }
                Text { text: modelData.html; textFormat: Text.StyledText; Layout.fillWidth: true; elide: Text.ElideRight; font.family: "monospace"; color: palette.text }
            }
        }
    }
}
