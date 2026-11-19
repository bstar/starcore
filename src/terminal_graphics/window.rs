//! Local-only, fixed Kitty window controls. Never runs a command from the host.
use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::{
    cell::Cell,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub struct Window {
    sender: Sender<Action>,
    cursor_hidden: Cell<bool>,
    pointer_hidden: Cell<bool>,
    pub notices: Receiver<String>,
    worker: Option<std::thread::JoinHandle<()>>,
}
enum Action {
    Fullscreen(bool),
    Cursor(bool),
    Pointer(bool),
}
/// A new private directory for this process image's Kitty helper. A view that
/// re-executes itself keeps its PID, and the previous image's directory may
/// still exist, so the name also carries a timestamp and the directory is
/// always created exclusively, never reused.
fn private_root() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    for attempt in 0..16_u32 {
        let root = std::env::temp_dir().join(format!(
            "star-core-window-{}-{stamp:x}-{attempt}",
            std::process::id()
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("Create the Kitty window helper directory"),
        }
    }
    anyhow::bail!("Could not create a private Kitty window helper directory")
}
impl Window {
    pub fn new() -> Result<Self> {
        let root = private_root()?;
        let path = root.join("fullscreen.py");
        std::fs::write(
            &path,
            include_bytes!("../../integrations/kitty/fullscreen.py"),
        )?;
        let (sender, rx) = bounded(8);
        let (tx, notices) = bounded(4);
        let worker = std::thread::Builder::new().name("star-kitty-window".into()).spawn(move || {
            for action in rx {
                let (name, cursor) = match action {
                    Action::Fullscreen(true) => ("enter", false),
                    Action::Fullscreen(false) => ("leave", false),
                    Action::Cursor(true) => ("cursor-hide", true),
                    Action::Cursor(false) => ("cursor-show", true),
                    Action::Pointer(true) => ("pointer-hide", true),
                    Action::Pointer(false) => ("pointer-show", true),
                };
                let action = if name == "pointer-hide" {format!("pointer-hide:{}",std::process::id())} else {name.into()};
                match operation_action(&path,&action) {
                    Ok(()) => {},
                    Err(error) => { tracing::warn!(%error, "Kitty window operation failed"); if !cursor { let _ = tx.try_send(format!("Desktop fullscreen unavailable: {error:#}. Use Kitty’s fullscreen shortcut.")); } }
                }
            }
            // Kitty removes its helper after processing the final restore.
            // One-way terminal delivery can return before Kitty reads the file.
            if operation_action(&path, "leave-cleanup").is_err() {
                let _ = std::fs::remove_dir_all(root);
            }
        })?;
        Ok(Self {
            sender,
            cursor_hidden: Cell::new(false),
            pointer_hidden: Cell::new(false),
            notices,
            worker: Some(worker),
        })
    }
    pub fn set(&self, enabled: bool) {
        let _ = self.sender.try_send(Action::Fullscreen(enabled));
    }
    pub fn pointer_hidden(&self, hidden: bool) {
        if cfg!(target_os = "macos")
            && hidden != self.pointer_hidden.get()
            && self.sender.try_send(Action::Pointer(hidden)).is_ok()
        {
            self.pointer_hidden.set(hidden);
        }
    }
    pub fn cursor(&self, hidden: bool) {
        if cfg!(target_os = "macos")
            && hidden != self.cursor_hidden.get()
            && self.sender.try_send(Action::Cursor(hidden)).is_ok()
        {
            self.cursor_hidden.set(hidden);
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        let (tx, _) = bounded(1);
        drop(std::mem::replace(&mut self.sender, tx));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn operation_action(path: &PathBuf, action: &str) -> Result<()> {
    let id = std::env::var("KITTY_WINDOW_ID").context("Kitty window identity missing")?;
    anyhow::ensure!(id.parse::<u64>().is_ok(), "Invalid Kitty window identity");
    // A second reader on /dev/tty races crossterm: remote-control replies can
    // become Escape and text key events, exiting fullscreen and stopping video.
    // Socket controls have a private reply channel; terminal controls must be
    // one-way and use the application's serialized stdout writer.
    if std::env::var_os("KITTY_LISTEN_ON").is_none() {
        let wire = terminal_command(path, &id, action)?;
        let mut out = std::io::stdout().lock();
        out.write_all(&wire)?;
        out.flush()?;
        return Ok(());
    }
    let mut command = Command::new("kitten");
    command.arg("@");
    if let Ok(socket) = std::env::var("KITTY_LISTEN_ON") {
        command.args(["--to", &socket]);
    }
    let mut child = command
        .args(["kitten", "--match", &format!("id:{id}")])
        .arg(path)
        .arg(action)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Kitty control timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = child.wait_with_output()?;
    anyhow::ensure!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(())
}

fn terminal_command(path: &std::path::Path, id: &str, action: &str) -> Result<Vec<u8>> {
    let id: u64 = id.parse().context("Invalid Kitty window identity")?;
    let command = serde_json::json!({
        "cmd": "kitten", "version": [0, 35, 0], "no_response": true,
        "kitty_window_id": id,
        "payload": {"kitten": path, "match": format!("id:{id}"),
            "args": [action]}
    });
    let mut wire = b"\x1bP@kitty-cmd".to_vec();
    serde_json::to_writer(&mut wire, &command)?;
    wire.extend_from_slice(b"\x1b\\");
    Ok(wire)
}

#[cfg(test)]
mod private_root_tests {
    #[test]
    fn each_image_gets_a_new_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let first = super::private_root().unwrap();
        let second = super::private_root().unwrap();
        assert_ne!(
            first, second,
            "a re-executed view must not reuse the old directory"
        );
        for root in [first, second] {
            let mode = std::fs::metadata(&root).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            std::fs::remove_dir(root).unwrap();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_fullscreen_never_requests_a_reply_on_the_input_stream() {
        for enabled in [true, false] {
            let wire = terminal_command(
                std::path::Path::new("/tmp/fullscreen.py"),
                "7",
                if enabled { "enter" } else { "leave" },
            )
            .unwrap();
            assert!(wire.starts_with(b"\x1bP@kitty-cmd"));
            assert!(wire.ends_with(b"\x1b\\"));
            let command: serde_json::Value =
                serde_json::from_slice(&wire[12..wire.len() - 2]).unwrap();
            assert_eq!(command["no_response"], true);
            assert_eq!(command["kitty_window_id"], 7);
            assert_eq!(command["payload"]["match"], "id:7");
            assert_eq!(
                command["payload"]["args"][0],
                if enabled { "enter" } else { "leave" }
            );
        }
        assert!(terminal_command(
            std::path::Path::new("/tmp/fullscreen.py"),
            "7;other",
            "enter"
        )
        .is_err());
    }
}
