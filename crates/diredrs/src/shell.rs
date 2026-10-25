//! Running shell commands and collecting their output.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// The buffer shown for `shell-command` output.
pub const OUTPUT_BUFFER: &str = "*Shell Command Output*";

/// How long to wait for the reader thread to deliver the last bytes after the
/// child process exits.
const FINAL_READ: Duration = Duration::from_millis(100);

/// A child shell process whose stdout is collected on a worker thread.
pub struct ShellProcess {
    child: Child,
    output_rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    status: Option<ExitStatus>,
}

impl ShellProcess {
    /// Start `command` in `dir`, merging stderr into stdout.
    pub fn spawn(command: &str, dir: &Path) -> io::Result<Self> {
        let mut command = shell_command(command);
        let mut child = command
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("shell has no stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("shell has no stderr pipe"))?;
        let (tx, output_rx) = mpsc::channel();
        read_stream(stdout, tx.clone());
        read_stream(stderr, tx);
        Ok(ShellProcess {
            child,
            output_rx,
            output: Vec::new(),
            status: None,
        })
    }

    /// Take any output that has arrived so far.
    pub fn pump(&mut self) {
        while let Ok(bytes) = self.output_rx.try_recv() {
            self.output.extend(bytes);
        }
    }

    /// The exit status, once the shell has exited and its output is drained.
    pub fn try_finish(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        self.pump();
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };

        // The process can exit before its reader thread has delivered the last
        // bytes. Drain until the pipe closes, but do not hang forever if a
        // background grandchild inherited it.
        let deadline = Instant::now() + FINAL_READ;
        loop {
            match self.output_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(bytes) => self.output.extend(bytes),
                Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        self.status = Some(status);
        Ok(self.status)
    }

    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Terminate the shell process.
    pub fn kill(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            self.status = self.child.wait().ok();
        }
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_stream(mut stream: impl Read + Send + 'static, tx: Sender<Vec<u8>>) {
    thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
}

/// Build the platform shell invocation.
fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let shell = std::env::var_os("COMSPEC")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "cmd.exe".into());
        let mut process = Command::new(shell);
        process.arg("/C").arg(command);
        process
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let mut process = Command::new(shell);
        process.arg("-c").arg(command);
        process
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "diredrs-shell-{tag}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wait(process: &mut ShellProcess) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = process.try_finish().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "shell command timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn captures_output_and_status() {
        let dir = temp_dir("echo");
        let mut process = ShellProcess::spawn("echo hello", &dir).unwrap();
        let status = wait(&mut process);
        assert!(status.success());
        assert!(String::from_utf8_lossy(process.output()).contains("hello"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn merges_stderr_into_output() {
        let dir = temp_dir("stderr");
        #[cfg(windows)]
        let command = "echo out & echo err 1>&2";
        #[cfg(not(windows))]
        let command = "echo out; echo err 1>&2";
        let mut process = ShellProcess::spawn(command, &dir).unwrap();
        let status = wait(&mut process);
        assert!(status.success());
        let output = String::from_utf8_lossy(process.output());
        assert!(output.contains("out"), "{output:?}");
        assert!(output.contains("err"), "{output:?}");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reports_a_failing_exit_status() {
        let dir = temp_dir("exit");
        let mut process = ShellProcess::spawn("exit 7", &dir).unwrap();
        let status = wait(&mut process);
        assert_eq!(status.code(), Some(7));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn runs_in_the_requested_directory() {
        let dir = temp_dir("cwd");
        #[cfg(windows)]
        let command = "cd";
        #[cfg(not(windows))]
        let command = "pwd";
        let mut process = ShellProcess::spawn(command, &dir).unwrap();
        let status = wait(&mut process);
        assert!(status.success());
        let output = String::from_utf8_lossy(process.output());
        let actual = Path::new(output.trim()).canonicalize().unwrap();
        assert_eq!(actual, dir.canonicalize().unwrap());
        fs::remove_dir_all(dir).ok();
    }
}
