import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// Sources seen in the data (grouped by kind), configured sources with their state, saved views.
Pane {
    id: side
    padding: 0
    signal sourceClicked(string display, string kind)
    signal viewClicked(string query, string range)

    property var src: JSON.parse(backend.sources)
    property var views: JSON.parse(backend.views)
    property string search: ""

    ColumnLayout {
        anchors.fill: parent
        spacing: 0
        TextField {
            Layout.fillWidth: true
            Layout.margins: 6
            placeholderText: "Filter sources"
            onTextChanged: side.search = text.toLowerCase()
        }
        ScrollView {
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            ColumnLayout {
                width: side.width
                spacing: 0
                Repeater {
                    model: side.src.groups
                    delegate: ColumnLayout {
                        id: grp
                        required property var modelData
                        property bool open: true
                        Layout.fillWidth: true
                        spacing: 0
                        ItemDelegate {
                            Layout.fillWidth: true
                            text: (grp.open ? "▾ " : "▸ ") + grp.modelData.label + "  (" + grp.modelData.items.length + ")"
                            font.bold: true
                            onClicked: grp.open = !grp.open
                        }
                        Repeater {
                            model: grp.open ? grp.modelData.items : []
                            delegate: ItemDelegate {
                                required property var modelData
                                Layout.fillWidth: true
                                visible: side.search === "" || modelData.display.toLowerCase().indexOf(side.search) >= 0
                                leftPadding: 24
                                text: modelData.display
                                onClicked: side.sourceClicked(modelData.display, modelData.kind)
                                ToolTip.text: "Show only " + modelData.display + " (live)"
                                ToolTip.visible: hovered
                                ToolTip.delay: 600
                            }
                        }
                    }
                }
                Label {
                    visible: side.src.groups.length === 0
                    Layout.margins: 10
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    opacity: 0.7
                    text: "No data yet. Sources appear here once they send something."
                }

                MenuSeparator { Layout.fillWidth: true }
                RowLayout {
                    Layout.fillWidth: true
                    Label { text: "  Saved views"; font.bold: true; Layout.fillWidth: true }
                }
                Repeater {
                    model: side.views
                    delegate: ItemDelegate {
                        required property var modelData
                        Layout.fillWidth: true
                        leftPadding: 24
                        text: modelData.name
                        onClicked: side.viewClicked(modelData.query, modelData.range)
                        ToolButton {
                            anchors.right: parent.right
                            anchors.verticalCenter: parent.verticalCenter
                            text: "✕"
                            onClicked: backend.deleteView(modelData.name)
                        }
                    }
                }
                Label { visible: side.views.length === 0; text: "    none yet"; opacity: 0.6 }

                MenuSeparator { Layout.fillWidth: true }
                RowLayout {
                    Layout.fillWidth: true
                    Label { text: "  Configured"; font.bold: true; Layout.fillWidth: true }
                    ToolButton { text: "+ Add"; enabled: backend.daemon; onClicked: addDialog.open() }
                }
                Repeater {
                    model: side.src.configured
                    delegate: ItemDelegate {
                        required property var modelData
                        Layout.fillWidth: true
                        leftPadding: 12
                        contentItem: RowLayout {
                            Rectangle {
                                width: 8; height: 8; radius: 4
                                color: modelData.state === "running" ? "#27ae60" : (modelData.state === "disabled" ? "gray" : "#da4453")
                            }
                            Label { text: modelData.name; Layout.fillWidth: true; elide: Text.ElideRight }
                            Label { text: modelData.received; opacity: 0.6 }
                        }
                        ToolTip.text: modelData.kind + ": " + modelData.state + (modelData.error ? "\n" + modelData.error : "") + "\n" + modelData.hint
                        ToolTip.visible: hovered
                        ToolTip.delay: 400
                        onClicked: cfgMenu.popup()
                        Menu {
                            id: cfgMenu
                            MenuItem { text: "Copy how to send"; onTriggered: { clip.text = modelData.hint; clip.selectAll(); clip.copy() } }
                            MenuItem { text: "Remove"; onTriggered: backend.removeSource(modelData.name) }
                        }
                    }
                }
                Label { visible: !backend.daemon; Layout.margins: 10; Layout.fillWidth: true; wrapMode: Text.Wrap; opacity: 0.6; text: "Start wartlogd to see and change sources." }
            }
        }
    }
    TextEdit { id: clip; visible: false }
    AddSourceDialog { id: addDialog }
}
