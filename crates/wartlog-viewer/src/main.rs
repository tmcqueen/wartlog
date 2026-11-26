//! WartLog viewer: QtQuick Controls UI over the store (read in-process) and the daemon (live tail,
//! sources). Works without the daemon, minus live tail and source changes.

mod bridge;
pub mod logic;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQuickStyle, QString, QUrl};

/// Uses the Plasma style when it is installed (the app then follows the desktop theme), else Fusion.
fn pick_style() {
    if std::env::var_os("QT_QUICK_CONTROLS_STYLE").is_some() {
        return;
    }
    let plasma = ["/usr/lib/x86_64-linux-gnu/qt6/qml/org/kde/desktop", "/usr/lib/qt6/qml/org/kde/desktop", "/usr/lib64/qt6/qml/org/kde/desktop", "/usr/lib/aarch64-linux-gnu/qt6/qml/org/kde/desktop"]
        .iter()
        .any(|p| std::path::Path::new(p).exists());
    QQuickStyle::set_style(&QString::from(if plasma { "org.kde.desktop" } else { "Fusion" }));
}

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("wartlog-viewer {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    pick_style();
    let mut app = QGuiApplication::new();
    if let Some(mut app) = app.as_mut() {
        app.as_mut().set_application_name(&QString::from("WartLog"));
        app.as_mut().set_organization_name(&QString::from("wartlog"));
        QGuiApplication::set_desktop_file_name(&QString::from("wartlog"));
    }
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/org/wartlog/viewer/qml/Main.qml"));
    }
    if let Some(app) = app.as_mut() {
        std::process::exit(app.exec());
    }
}
