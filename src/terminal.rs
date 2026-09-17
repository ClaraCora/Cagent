//! Local terminal sessions exposed through the agent WebSocket.
//!
//! The agent opens a PTY on the monitored host. The hub only relays terminal
//! frames, so no SSH password or private key ever needs to leave this host.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{read, write, Pid};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const MAX_TERMINAL_ID: usize = 128;
const MAX_INPUT: usize = 8 * 1024;
const MAX_COLS: u32 = 500;
const MAX_ROWS: u32 = 200;
const OUTPUT_BUFFER: usize = 64;

#[derive(Default)]
pub struct Manager {
    sessions: HashMap<String, Entry>,
}

struct Entry {
    tx: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}

enum Command {
    Input(String),
    Resize { cols: u32, rows: u32 },
    Close,
}

#[derive(Debug, Deserialize)]
struct TerminalId {
    terminal_id: String,
}

#[derive(Debug, Deserialize)]
struct OpenParams {
    terminal_id: String,
    cols: u32,
    rows: u32,
}

#[derive(Debug, Deserialize)]
struct InputParams {
    terminal_id: String,
    data: String,
}

#[derive(Debug, Deserialize)]
struct ResizeParams {
    terminal_id: String,
    cols: u32,
    rows: u32,
}

impl Manager {
    /// Handles one terminal notification received from the hub.
    pub async fn handle(&mut self, method: &str, params: Value, output: &mpsc::Sender<Message>) {
        match method {
            "terminal.open" => {
                let Ok(params) = serde_json::from_value::<OpenParams>(params) else {
                    return self.send_error("", "终端打开参数无效", output).await;
                };
                if !valid_id(&params.terminal_id) || !valid_size(params.cols, params.rows) {
                    return self.send_error(&params.terminal_id, "终端窗口参数无效", output).await;
                }
                if unsafe { libc::geteuid() } != 0 {
                    return self
                        .send_error(
                            &params.terminal_id,
                            "Cagent 未以 root 运行，请重新执行节点安装命令",
                            output,
                        )
                        .await;
                }
                self.close(&params.terminal_id);
                let (tx, rx) = mpsc::channel(OUTPUT_BUFFER);
                let id = params.terminal_id.clone();
                let result_tx = output.clone();
                let task = tokio::spawn(async move {
                    run_session(id, params.cols, params.rows, rx, result_tx).await;
                });
                self.sessions.insert(params.terminal_id, Entry { tx, task });
            }
            "terminal.input" => {
                let Ok(params) = serde_json::from_value::<InputParams>(params) else { return };
                if !valid_id(&params.terminal_id) || params.data.len() > MAX_INPUT {
                    return;
                }
                self.send(params.terminal_id, Command::Input(params.data));
            }
            "terminal.resize" => {
                let Ok(params) = serde_json::from_value::<ResizeParams>(params) else { return };
                if !valid_id(&params.terminal_id) || !valid_size(params.cols, params.rows) {
                    return;
                }
                self.send(params.terminal_id, Command::Resize { cols: params.cols, rows: params.rows });
            }
            "terminal.close" => {
                let Ok(params) = serde_json::from_value::<TerminalId>(params) else { return };
                if valid_id(&params.terminal_id) {
                    self.close(&params.terminal_id);
                }
            }
            _ => {}
        }
    }

    fn send(&mut self, id: String, command: Command) {
        let Some(tx) = self.sessions.get(&id).map(|entry| entry.tx.clone()) else { return };
        if tx.try_send(command).is_err() {
            if let Some(entry) = self.sessions.remove(&id) {
                entry.task.abort();
            }
        }
    }

    fn close(&mut self, id: &str) {
        if let Some(entry) = self.sessions.remove(id) {
            let _ = entry.tx.try_send(Command::Close);
            entry.task.abort();
        }
    }

    async fn send_error(&self, id: &str, message: &str, output: &mpsc::Sender<Message>) {
        let _ = output.send(notify("terminal.error", json!({ "terminal_id": id, "message": message }))).await;
    }

    pub fn close_all(&mut self) {
        for (_, entry) in self.sessions.drain() {
            let _ = entry.tx.try_send(Command::Close);
            entry.task.abort();
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_TERMINAL_ID
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn valid_size(cols: u32, rows: u32) -> bool {
    (1..=MAX_COLS).contains(&cols) && (1..=MAX_ROWS).contains(&rows)
}

fn notify(method: &str, params: Value) -> Message {
    Message::Text(json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string().into())
}

struct Pty {
    master: AsyncFd<OwnedFd>,
    child: tokio::process::Child,
    pid: Pid,
}

impl Pty {
    fn spawn(cols: u32, rows: u32) -> Result<Self> {
        let size = winsize(cols, rows);
        let pair = openpty(Some(&size), None).context("create PTY")?;
        set_nonblocking(pair.master.as_raw_fd()).context("set PTY nonblocking")?;
        let stdin = pair.slave.try_clone().context("clone PTY slave")?;
        let stdout = pair.slave.try_clone().context("clone PTY slave")?;
        let shell = if Path::new("/bin/bash").is_file() { "/bin/bash" } else { "/bin/sh" };
        let mut command = tokio::process::Command::new(shell);
        command
            .arg("-l")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(pair.slave))
            .env_remove("MONITOR_TOKEN")
            .env_remove("MONITOR_SERVER")
            .kill_on_drop(true);
        // The stdio descriptors are installed before pre_exec runs. setsid plus
        // TIOCSCTTY makes the PTY the login shell's controlling terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().context("start shell")?;
        let pid = Pid::from_raw(child.id().ok_or_else(|| anyhow::anyhow!("shell has no process id"))? as i32);
        Ok(Self { master: AsyncFd::new(pair.master)?, child, pid })
    }

    fn resize(&self, cols: u32, rows: u32) -> Result<()> {
        let size = winsize(cols, rows);
        let result = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        if result == -1 {
            return Err(io::Error::last_os_error()).context("resize PTY");
        }
        kill(self.pid, Signal::SIGWINCH).context("notify PTY resize")?;
        Ok(())
    }

    fn terminate(&self) {
        let group = Pid::from_raw(-self.pid.as_raw());
        let _ = kill(group, Signal::SIGHUP);
        let _ = kill(group, Signal::SIGKILL);
        let _ = kill(self.pid, Signal::SIGHUP);
        let _ = kill(self.pid, Signal::SIGKILL);
    }
}

struct ProcessGuard {
    pid: Pid,
    active: bool,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.active {
            let group = Pid::from_raw(-self.pid.as_raw());
            let _ = kill(group, Signal::SIGKILL);
            let _ = kill(self.pid, Signal::SIGKILL);
        }
    }
}

fn winsize(cols: u32, rows: u32) -> Winsize {
    Winsize { ws_row: rows as libc::c_ushort, ws_col: cols as libc::c_ushort, ws_xpixel: 0, ws_ypixel: 0 }
}

async fn run_session(
    terminal_id: String,
    cols: u32,
    rows: u32,
    mut commands: mpsc::Receiver<Command>,
    output: mpsc::Sender<Message>,
) {
    let mut pty = match Pty::spawn(cols, rows) {
        Ok(pty) => pty,
        Err(error) => {
            let _ = output
                .send(notify(
                    "terminal.error",
                    json!({ "terminal_id": terminal_id, "message": format!("无法启动终端: {error:#}") }),
                ))
                .await;
            return;
        }
    };
    let mut guard = ProcessGuard { pid: pty.pid, active: true };
    if output.send(notify("terminal.ready", json!({ "terminal_id": terminal_id }))).await.is_err() {
        return;
    }
    let mut buffer = [0u8; 8192];
    let (reason, terminate, status) = loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Input(data)) => {
                    if let Err(error) = write_all(&pty.master, data.as_bytes()).await {
                        break (format!("write:{error}"), true, None);
                    }
                }
                Some(Command::Resize { cols, rows }) => {
                    if let Err(error) = pty.resize(cols, rows) {
                        let _ = output.send(notify("terminal.error", json!({
                            "terminal_id": terminal_id,
                            "message": format!("终端调整失败: {error:#}"),
                        }))).await;
                    }
                }
                Some(Command::Close) | None => break ("closed".to_owned(), true, None),
            },
            read = read_once(&pty.master, &mut buffer) => match read {
                Ok(0) => break ("eof".to_owned(), false, None),
                Ok(size) => {
                    let data = String::from_utf8_lossy(&buffer[..size]);
                    if output.send(notify("terminal.output", json!({ "terminal_id": terminal_id, "data": data }))).await.is_err() {
                        break ("output-closed".to_owned(), true, None);
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break ("eof".to_owned(), false, None),
                Err(error) => break (format!("read:{error}"), true, None),
            },
            status = pty.child.wait() => break ("exit".to_owned(), false, status.ok()),
        }
    };

    if terminate {
        pty.terminate();
    }
    let status = match status {
        Some(status) => status.to_string(),
        None => match pty.child.wait().await {
            Ok(status) => status.to_string(),
            Err(error) => format!("wait:{error}"),
        },
    };
    guard.active = false;
    if reason != "closed" {
        let _ = output
            .send(notify(
                "terminal.exit",
                json!({ "terminal_id": terminal_id, "reason": reason, "status": status }),
            ))
            .await;
    }
}

fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

async fn read_once(master: &AsyncFd<OwnedFd>, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        let mut guard = master.readable().await?;
        match guard.try_io(|inner| read(inner.get_ref().as_raw_fd(), buffer).map_err(errno)) {
            Ok(result) => return result,
            Err(_would_block) => continue,
        }
    }
}

async fn write_all(master: &AsyncFd<OwnedFd>, data: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let mut guard = master.writable().await?;
        match guard.try_io(|inner| write(inner.get_ref(), &data[offset..]).map_err(errno)) {
            Ok(Ok(size)) if size > 0 => offset += size,
            Ok(Ok(_)) => return Err(io::Error::new(io::ErrorKind::WriteZero, "PTY write returned zero")),
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => {}
        }
    }
    Ok(())
}

fn errno(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_inputs_are_bounded_and_ids_are_local_tokens() {
        assert!(valid_id("abc-123_X"));
        assert!(!valid_id(""));
        assert!(!valid_id("../shell"));
        assert!(valid_size(120, 32));
        assert!(!valid_size(0, 32));
        assert!(!valid_size(501, 32));
        assert!(!valid_size(120, 201));
    }

    #[tokio::test]
    async fn pty_runs_a_real_shell_and_returns_its_output() {
        let mut pty = Pty::spawn(80, 24).expect("PTY starts");
        write_all(&pty.master, b"printf '__cagent_%s__\\n' pty_ready; exit\n").await.expect("write command");
        let mut collected = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut buffer = [0u8; 1024];
            while !collected.contains("__cagent_pty_ready__") {
                let read = read_once(&pty.master, &mut buffer).await.expect("read shell output");
                assert!(read > 0, "shell ended before producing output");
                collected.push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        })
        .await
        .expect("shell output deadline");
        pty.terminate();
        let _ = pty.child.wait().await;
        assert!(collected.contains("__cagent_pty_ready__"));
    }
}
