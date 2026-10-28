//! Running shell commands and collecting their output.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// The shell profile used by `M-!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellProgram {
    Cmd,
    Powershell,
    Unix,
    GitBash,
}

#[allow(clippy::derivable_impls)]
impl Default for ShellProgram {
    fn default() -> Self {
        #[cfg(windows)]
        {
            Self::Powershell
        }
        #[cfg(not(windows))]
        {
            Self::Unix
        }
    }
}

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
    /// Start `command` in `dir` using the platform-default shell profile.
    #[cfg(test)]
    pub fn spawn(command: &str, dir: &Path) -> io::Result<Self> {
        Self::spawn_with_profile(command, dir, ShellProgram::default())
    }

    /// Start `command` in `dir` using `profile`, merging stderr into stdout.
    pub fn spawn_with_profile(
        command: &str,
        dir: &Path,
        profile: ShellProgram,
    ) -> io::Result<Self> {
        let mut command = profile.command(command)?;
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

impl ShellProgram {
    /// Build the hidden program and argument prefix for this profile.
    fn command(self, command: &str) -> io::Result<Command> {
        let (program, prefix): (OsString, &[&str]) = match self {
            Self::Cmd => (
                std::env::var_os("COMSPEC")
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "cmd.exe".into()),
                &["/C"],
            ),
            Self::Powershell => {
                #[cfg(windows)]
                let program = OsString::from("powershell.exe");
                #[cfg(not(windows))]
                let program = OsString::from("pwsh");
                (
                    program,
                    &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"],
                )
            }
            Self::Unix => {
                let fallback = if cfg!(windows) { "sh" } else { "/bin/sh" };
                (
                    std::env::var_os("SHELL")
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| fallback.into()),
                    &["-c"],
                )
            }
            Self::GitBash => {
                #[cfg(not(windows))]
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the gitbash profile is only available on Windows",
                ));
                #[cfg(windows)]
                {
                    let program = git_bash_program().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "Git Bash not found (expected <Git>\\bin\\bash.exe)",
                        )
                    })?;
                    (program.into_os_string(), &["--noprofile", "--norc", "-c"])
                }
            }
        };
        let mut process = Command::new(program);
        process.args(prefix).arg(command);
        Ok(process)
    }
}

/// The Git for Windows root, if Git Bash is installed.
pub fn git_bash_root() -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        None
    }
    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        if let Some(shell) = std::env::var_os("SHELL") {
            if let Some(root) = git_root_from_bash(Path::new(&shell)) {
                roots.push(root);
            }
        }
        if let Some(git) = find_on_path("git.exe") {
            if let Some(root) = git.parent().and_then(Path::parent).map(Path::to_path_buf) {
                roots.push(root);
            }
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(variable) {
                roots.push(PathBuf::from(base).join("Git"));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Programs").join("Git"));
        }
        roots.into_iter().find(|root| git_bash_in(root).is_some())
    }
}

/// Directories containing Git Bash commands.
pub fn git_bash_command_dirs() -> Vec<PathBuf> {
    let Some(root) = git_bash_root() else {
        return Vec::new();
    };
    ["usr/bin", "mingw64/bin", "bin"]
        .into_iter()
        .map(|path| root.join(path))
        .filter(|path| path.is_dir())
        .collect()
}

#[cfg(windows)]
fn git_bash_program() -> Option<PathBuf> {
    git_bash_root().and_then(|root| git_bash_in(&root))
}

#[cfg(windows)]
fn git_bash_in(root: &Path) -> Option<PathBuf> {
    [root.join("bin/bash.exe"), root.join("usr/bin/bash.exe")]
        .into_iter()
        .find(|path| path.is_file())
}

#[cfg(windows)]
fn git_root_from_bash(bash: &Path) -> Option<PathBuf> {
    let bin = bash.parent()?;
    if !bin.file_name()?.to_str()?.eq_ignore_ascii_case("bin") {
        return None;
    }
    let parent = bin.parent()?;
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("usr"))
    {
        return parent.parent().map(Path::to_path_buf);
    }
    Some(parent.to_path_buf())
}

#[cfg(windows)]
fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Names of commands available to the PowerShell profile.
pub fn powershell_command_names() -> io::Result<Vec<String>> {
    let script = "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); \
                  Get-Command -CommandType Cmdlet,Function,Filter,Alias \
                  -ErrorAction SilentlyContinue | ForEach-Object Name";
    let output = ShellProgram::Powershell
        .command(script)?
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for name in String::from_utf8_lossy(&output.stdout).lines() {
        let name = name.trim();
        if !name.is_empty() && seen.insert(name.to_lowercase()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
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
    fn profiles_use_the_expected_command_prefixes() {
        let args = |profile: ShellProgram| {
            profile
                .command("run me")
                .unwrap()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(args(ShellProgram::Cmd), ["/C", "run me"]);
        assert_eq!(
            args(ShellProgram::Powershell),
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "run me"
            ]
        );
        assert_eq!(args(ShellProgram::Unix), ["-c", "run me"]);
        #[cfg(windows)]
        if git_bash_root().is_some() {
            assert_eq!(
                args(ShellProgram::GitBash),
                ["--noprofile", "--norc", "-c", "run me"]
            );
        }
    }

    #[test]
    fn captures_output_and_status() {
        let dir = temp_dir("echo");
        #[cfg(windows)]
        let command = "Write-Output hello";
        #[cfg(not(windows))]
        let command = "echo hello";
        let mut process = ShellProcess::spawn(command, &dir).unwrap();
        let status = wait(&mut process);
        assert!(status.success());
        assert!(String::from_utf8_lossy(process.output()).contains("hello"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn merges_stderr_into_output() {
        let dir = temp_dir("stderr");
        #[cfg(windows)]
        let command = "Write-Output out; [Console]::Error.WriteLine('err')";
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
        let command = "(Get-Location).Path";
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

    #[cfg(windows)]
    #[test]
    fn git_bash_profile_runs_when_available() {
        if git_bash_root().is_none() {
            return;
        }
        let dir = temp_dir("gitbash");
        let mut process =
            ShellProcess::spawn_with_profile("printf 'gitbash-ok\\n'", &dir, ShellProgram::GitBash)
                .unwrap();
        let status = wait(&mut process);
        assert!(status.success());
        assert!(String::from_utf8_lossy(process.output()).contains("gitbash-ok"));
        fs::remove_dir_all(dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn powershell_command_names_include_cmdlets() {
        let names = powershell_command_names().unwrap();
        assert!(names
            .iter()
            .any(|name| name.eq_ignore_ascii_case("Compress-Archive")));
    }

    #[cfg(windows)]
    #[test]
    fn explicit_profiles_run_with_their_own_syntax() {
        let dir = temp_dir("profiles");
        let mut powershell =
            ShellProcess::spawn_with_profile("Write-Output ps", &dir, ShellProgram::Powershell)
                .unwrap();
        let status = wait(&mut powershell);
        assert!(status.success());
        assert!(String::from_utf8_lossy(powershell.output()).contains("ps"));

        let mut cmd =
            ShellProcess::spawn_with_profile("echo one& echo two", &dir, ShellProgram::Cmd)
                .unwrap();
        let status = wait(&mut cmd);
        assert!(status.success());
        let output = String::from_utf8_lossy(cmd.output());
        assert!(output.contains("one"), "{output:?}");
        assert!(output.contains("two"), "{output:?}");

        fs::remove_dir_all(dir).ok();
    }
}
