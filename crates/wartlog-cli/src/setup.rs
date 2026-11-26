//! `wl setup`: systemd user unit, default config, desktop entry.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(fallback))
}

fn find_daemon(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return std::fs::canonicalize(&p).with_context(|| format!("{}", p.display()));
    }
    if let Ok(me) = std::env::current_exe() {
        let sib = me.with_file_name("wartlogd");
        if sib.exists() {
            return Ok(sib);
        }
    }
    for d in std::env::var("PATH").unwrap_or_default().split(':') {
        let p = Path::new(d).join("wartlogd");
        if p.exists() {
            return Ok(p);
        }
    }
    bail!("cannot find wartlogd next to wl or on PATH (cargo install --path crates/wartlogd, or pass --daemon)")
}

pub fn unit_text(daemon: &Path) -> String {
    format!(
        "[Unit]
Description=WartLog log collector
Documentation=https://github.com/tmcqueen/wartlog

[Service]
Type=notify
ExecStart={}
Restart=on-failure
RestartSec=2
# wartlogd seals and indexes on SIGTERM; give it time on large stores
TimeoutStopSec=60

[Install]
WantedBy=default.target
",
        daemon.display()
    )
}

pub fn setup(daemon: Option<PathBuf>, no_start: bool) -> Result<()> {
    let daemon = find_daemon(daemon)?;
    let unit_dir = xdg("XDG_CONFIG_HOME", ".config").join("systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit = unit_dir.join("wartlogd.service");
    std::fs::write(&unit, unit_text(&daemon))?;
    println!("wrote {}", unit.display());

    let cfg = xdg("XDG_CONFIG_HOME", ".config").join("wartlog/wartlog.toml");
    if cfg.exists() {
        println!("kept   {}", cfg.display());
    } else {
        let out = std::process::Command::new(&daemon).arg("--print-default-config").output().context("running wartlogd --print-default-config")?;
        if !out.status.success() {
            bail!("wartlogd --print-default-config failed");
        }
        std::fs::create_dir_all(cfg.parent().unwrap())?;
        std::fs::write(&cfg, out.stdout)?;
        println!("wrote {}", cfg.display());
    }

    if let Ok(me) = std::env::current_exe() {
        let viewer = me.with_file_name("wartlog-viewer");
        if viewer.exists() {
            let apps = xdg("XDG_DATA_HOME", ".local/share").join("applications");
            std::fs::create_dir_all(&apps)?;
            let desk = apps.join("wartlog.desktop");
            std::fs::write(
                &desk,
                format!("[Desktop Entry]\nType=Application\nName=WartLog\nComment=Search and tail your logs\nExec={}\nIcon=utilities-log-viewer\nCategories=Development;System;Monitor;\nTerminal=false\n", viewer.display()),
            )?;
            println!("wrote {}", desk.display());
        }
    }

    if no_start {
        println!("next: systemctl --user daemon-reload && systemctl --user enable --now wartlogd");
        return Ok(());
    }
    let run = |args: &[&str]| -> Result<()> {
        let st = std::process::Command::new("systemctl").arg("--user").args(args).status().context("running systemctl")?;
        if !st.success() {
            bail!("systemctl --user {} failed", args.join(" "));
        }
        Ok(())
    };
    run(&["daemon-reload"])?;
    run(&["enable", "wartlogd.service"])?;
    // restart rather than start, so re-running setup after an upgrade picks up the new binary
    run(&["restart", "wartlogd.service"])?;
    println!("wartlogd is running. Try: wl status");
    Ok(())
}
