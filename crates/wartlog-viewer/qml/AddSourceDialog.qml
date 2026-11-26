import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// One dialog for every source kind: fills a config table, shows the TOML it will add and how a
// program sends to it, then asks wartlogd to add it (the daemon writes wartlog.toml).
Dialog {
    id: dlg
    title: "Add source"
    modal: true
    anchors.centerIn: Overlay.overlay
    width: 560
    standardButtons: Dialog.Cancel

    property string kind: kinds.currentValue || "otlp-grpc"
    property string role: kind === "journald" || kind === "kubernetes" ? "collector" : "receiver"

    function q(s) { return JSON.stringify(s) }
    function list(s) { return "[" + s.split(",").map(x => x.trim()).filter(x => x !== "").map(q).join(", ") + "]" }

    property string toml: {
        const n = nameField.text.trim() !== "" ? "name = " + q(nameField.text.trim()) + "\n" : ""
        switch (kind) {
        case "otlp-grpc": return "type = \"otlp-grpc\"\n" + n + "listen = " + q(listen.text) + (fwd.text.trim() !== "" ? "\nforward = [{ endpoint = " + q(fwd.text.trim()) + " }]" : "")
        case "http": return "type = \"http\"\n" + n + "listen = " + q(listen.text)
        case "ndjson-tcp": return "type = \"ndjson-tcp\"\n" + n + "listen = " + q(listen.text) + "\nsource = " + q(srcName.text)
        case "journald": return "type = \"journald\"\n" + n + "journals = [" + q(journal.currentText) + "]\nbackfill = " + q(backfill.text)
        case "kubernetes": return "type = \"kubernetes\"\n" + n + (ctx.text.trim() !== "" ? "context = " + q(ctx.text.trim()) + "\n" : "") + "namespaces = " + list(ns.text) + (sel.text.trim() !== "" ? "\nlabel_selector = " + q(sel.text.trim()) : "") + "\nbackfill = " + q(backfill.text)
        }
        return ""
    }
    property string snippet: {
        switch (kind) {
        case "otlp-grpc": return "OTEL_EXPORTER_OTLP_ENDPOINT=http://" + listen.text
        case "http": return "OTEL_EXPORTER_OTLP_ENDPOINT=http://" + listen.text + " OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf\ncurl --data-binary @app.ndjson 'http://" + listen.text + "/ingest?source=my-app'"
        case "ndjson-tcp": return "my-app 2>&1 | nc " + listen.text.replace(":", " ")
        case "journald": return journal.currentText === "system" ? "Reading the system journal needs membership in the systemd-journal group." : "Reads your user journal."
        case "kubernetes": return "Reads pod logs through the API server with your kubeconfig; nothing is installed in the cluster."
        }
        return ""
    }

    onKindChanged: {
        listen.text = kind === "otlp-grpc" ? "127.0.0.1:4317" : kind === "http" ? "127.0.0.1:4318" : "127.0.0.1:5170"
        backfill.text = kind === "kubernetes" ? "1h" : "24h"
    }

    ColumnLayout {
        anchors.fill: parent
        GridLayout {
            columns: 2
            Layout.fillWidth: true
            Label { text: "Kind" }
            ComboBox {
                id: kinds
                Layout.fillWidth: true
                textRole: "text"
                valueRole: "value"
                model: [
                    { text: "OpenTelemetry (OTLP/gRPC)", value: "otlp-grpc" },
                    { text: "HTTP (OTLP/HTTP and NDJSON)", value: "http" },
                    { text: "NDJSON over TCP", value: "ndjson-tcp" },
                    { text: "journald", value: "journald" },
                    { text: "Kubernetes", value: "kubernetes" }
                ]
            }
            Label { text: "Name" }
            TextField { id: nameField; Layout.fillWidth: true; placeholderText: "optional" }
            Label { text: "Listen"; visible: dlg.role === "receiver" }
            TextField { id: listen; Layout.fillWidth: true; visible: dlg.role === "receiver"; text: "127.0.0.1:4317" }
            Label { text: "Forward to"; visible: dlg.kind === "otlp-grpc" }
            TextField { id: fwd; Layout.fillWidth: true; visible: dlg.kind === "otlp-grpc"; placeholderText: "http://localhost:18889 (Aspire dashboard, optional)" }
            Label { text: "Source name"; visible: dlg.kind === "ndjson-tcp" }
            TextField { id: srcName; Layout.fillWidth: true; visible: dlg.kind === "ndjson-tcp"; text: "tcp" }
            Label { text: "Journal"; visible: dlg.kind === "journald" }
            ComboBox { id: journal; Layout.fillWidth: true; visible: dlg.kind === "journald"; model: ["user", "system"] }
            Label { text: "Context"; visible: dlg.kind === "kubernetes" }
            TextField { id: ctx; Layout.fillWidth: true; visible: dlg.kind === "kubernetes"; placeholderText: "current context" }
            Label { text: "Namespaces"; visible: dlg.kind === "kubernetes" }
            TextField { id: ns; Layout.fillWidth: true; visible: dlg.kind === "kubernetes"; placeholderText: "all, or a comma separated list" }
            Label { text: "Label selector"; visible: dlg.kind === "kubernetes" }
            TextField { id: sel; Layout.fillWidth: true; visible: dlg.kind === "kubernetes"; placeholderText: "app.kubernetes.io/part-of=shop" }
            Label { text: "Backfill"; visible: dlg.role === "collector" }
            TextField { id: backfill; Layout.fillWidth: true; visible: dlg.role === "collector"; text: "24h" }
        }
        Label { text: "Adds to wartlog.toml:"; opacity: 0.7 }
        TextArea { Layout.fillWidth: true; readOnly: true; text: "[[" + dlg.role + "]]\n" + dlg.toml; font.family: "monospace"; selectByMouse: true }
        Label { text: "To send:"; opacity: 0.7 }
        TextArea { Layout.fillWidth: true; readOnly: true; text: dlg.snippet; font.family: "monospace"; wrapMode: TextEdit.Wrap; selectByMouse: true }
        Button {
            Layout.alignment: Qt.AlignRight
            text: "Add"
            highlighted: true
            onClicked: { backend.addSource(dlg.role, dlg.toml); dlg.close() }
        }
    }
}
