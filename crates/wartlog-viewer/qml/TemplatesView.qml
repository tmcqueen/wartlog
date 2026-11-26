import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// The current query's results grouped by template (or by body when a record has none), with a
// count and a sparkline. Click to keep only that template, right click to exclude it.
Item {
    id: tv
    signal templateClicked(string template, bool exclude)
    property var items: backend.templates === "" ? [] : JSON.parse(backend.templates)

    ListView {
        anchors.fill: parent
        clip: true
        model: tv.items
        ScrollBar.vertical: ScrollBar { }
        header: RowLayout {
            width: ListView.view ? ListView.view.width - 16 : 0
            x: 8
            spacing: 10
            Label { text: "Count"; Layout.preferredWidth: 80; horizontalAlignment: Text.AlignRight; font.bold: true }
            Label { text: "Over time"; Layout.preferredWidth: 150; font.bold: true }
            Label { text: "Template"; Layout.fillWidth: true; font.bold: true }
            Label { text: "Last seen"; Layout.preferredWidth: 140; font.bold: true }
        }
        delegate: ItemDelegate {
            required property var modelData
            width: ListView.view.width
            height: 26
            leftPadding: 8
            rightPadding: 16
            onClicked: tv.templateClicked(modelData.template, false)
            MouseArea { anchors.fill: parent; acceptedButtons: Qt.RightButton; onClicked: tv.templateClicked(modelData.template, true) }
            contentItem: RowLayout {
                spacing: 10
                Label { text: modelData.count; Layout.preferredWidth: 80; horizontalAlignment: Text.AlignRight; font.family: "monospace" }
                Rectangle {
                    Layout.preferredWidth: 150
                    Layout.preferredHeight: 18
                    color: Qt.rgba(0.24, 0.68, 0.91, 0.10)
                    radius: 2
                Row {
                    anchors.fill: parent
                    anchors.margins: 1
                    spacing: 1
                    property real peak: Math.max(1, Math.max.apply(null, modelData.spark))
                    Repeater {
                        model: modelData.spark
                        delegate: Item {
                            required property var modelData
                            width: 5
                            height: 16
                            Rectangle {
                                anchors.bottom: parent.bottom
                                width: parent.width
                                height: modelData === 0 ? 0 : Math.max(2, 16 * modelData / parent.parent.peak)
                                color: "#3daee9"
                            }
                        }
                    }
                }
                }
                Label { text: modelData.template; Layout.fillWidth: true; elide: Text.ElideRight; font.family: "monospace" }
                Label { text: modelData.last; Layout.minimumWidth: implicitWidth; Layout.preferredWidth: 140; font.family: "monospace"; opacity: 0.7 }
            }
        }
    }
    Label { anchors.centerIn: parent; visible: tv.items.length === 0; opacity: 0.6; text: "No templates for this query." }
}
