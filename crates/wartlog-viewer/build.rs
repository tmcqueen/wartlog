use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("org.wartlog.viewer").qml_files([
        "qml/Main.qml",
        "qml/QueryBar.qml",
        "qml/Sidebar.qml",
        "qml/LogList.qml",
        "qml/Details.qml",
        "qml/TracePanel.qml",
        "qml/TemplatesView.qml",
        "qml/AddSourceDialog.qml",
    ]))
    .qt_module("Quick")
    .qt_module("QuickControls2")
    .files(["src/bridge.rs"])
    .build();
}
