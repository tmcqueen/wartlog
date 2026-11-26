import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// The KQL query bar: completion popup (tables, operators, columns, attribute keys, recent values)
// and a red underline under the part of the query an error points at.
TextField {
    id: field
    signal submitted(string query)
    placeholderText: "logs | where Level == \"error\"     (Ctrl+K)"
    font.family: "monospace"
    selectByMouse: true

    property var items: []
    property int compStart: 0
    property int compEnd: 0

    function refreshCompletion() {
        if (!activeFocus) return
        const r = JSON.parse(backend.complete(text, cursorPosition))
        items = r.items
        compStart = r.start
        compEnd = r.end
        list.currentIndex = 0
        if (items.length > 0 && !(items.length === 1 && text.substring(compStart, compEnd) === items[0].text)) popup.open()
        else popup.close()
    }

    function accept(i) {
        const t = items[i].text
        const before = text.substring(0, compStart)
        const after = text.substring(compEnd)
        const sep = (t.endsWith("(") || after.startsWith(" ")) ? "" : " "
        text = before + t + sep + after
        cursorPosition = before.length + t.length + sep.length
        popup.close()
    }

    onTextEdited: refreshCompletion()
    onActiveFocusChanged: if (!activeFocus) popup.close()

    Keys.onPressed: (e) => {
        if (popup.opened) {
            if (e.key === Qt.Key_Down) { list.incrementCurrentIndex(); e.accepted = true }
            else if (e.key === Qt.Key_Up) { list.decrementCurrentIndex(); e.accepted = true }
            else if (e.key === Qt.Key_Tab) { accept(list.currentIndex); e.accepted = true }
            else if (e.key === Qt.Key_Escape) { popup.close(); e.accepted = true }
        } else if (e.key === Qt.Key_Space && (e.modifiers & Qt.ControlModifier)) {
            refreshCompletion(); e.accepted = true
        }
    }
    onAccepted: {
        if (popup.opened && list.currentIndex >= 0 && items.length > 0 && text.substring(compStart, compEnd) !== "") { accept(list.currentIndex); return }
        popup.close()
        submitted(text)
    }

    // error underline
    Rectangle {
        visible: backend.errorStart >= 0 && backend.errorEnd > backend.errorStart
        color: "#da4453"
        height: 2
        y: parent.height - 6
        x: visible ? field.positionToRectangle(backend.errorStart).x : 0
        width: visible ? Math.max(6, field.positionToRectangle(backend.errorEnd).x - x) : 0
    }

    Popup {
        id: popup
        y: field.height
        x: Math.min(field.positionToRectangle(field.compStart).x, field.width - width)
        width: 360
        height: Math.min(list.contentHeight + 8, 300)
        padding: 4
        focus: false
        closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutsideParent
        ListView {
            id: list
            anchors.fill: parent
            clip: true
            model: field.items
            delegate: ItemDelegate {
                width: ListView.view.width
                height: 26
                highlighted: ListView.isCurrentItem
                onClicked: field.accept(index)
                contentItem: RowLayout {
                    Label { text: modelData.text; font.family: "monospace"; Layout.fillWidth: true; elide: Text.ElideRight }
                    Label { text: modelData.kind; opacity: 0.5; font.pointSize: 8 }
                }
            }
        }
    }
}
