//! The PATH an app opened from Finder does not get.
//!
//! macOS starts an app opened from Finder, the Dock or Spotlight through
//! launchd, and launchd gives it `PATH=/usr/bin:/bin:/usr/sbin:/sbin` and
//! nothing more. `node` is in none of those — Homebrew puts it in
//! `/opt/homebrew/bin`, nvm, fnm and volta under the home folder — and the
//! Claude Code and Codex adapters run under `node`, so every agent failed to
//! start with "failed to launch node: No such file or directory". `npm run
//! app` and `open` from a terminal pass the shell's PATH through, which is
//! why a development build never showed it.
//!
//! So when the PATH is launchd's and nothing more, the app asks the user's
//! login shell for the PATH it sets up, and puts that in front of its own.
//! It is the macOS side of what `registry_path_dirs` does on Windows
//! (crates/agents): the PATH as the user set it, which a process started
//! some other way never sees. A PATH with anything more in it was set by
//! someone on purpose — a terminal, or `launchctl config user path` — and
//! is left alone.
//!
//! It changes this process's environment, so [`adopt`] runs at the top of
//! `run`, before any thread exists. Logging has not started yet, so what it
//! did waits in [`OUTCOME`] until [`log`] can say.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The directories launchd's PATH is made of.
const LAUNCHD_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Printed on the line before the PATH. A startup file may print anything
/// first — a greeting, a terminal title, a shell integration's escape codes —
/// so the PATH is the line after this one, not the first line or the last.
const MARKER: &str = "__DIVIXI_LOGIN_PATH__";

/// How long the login shell may take. A heavy `.zshrc` takes a second; a
/// shell that is still going after this is stuck, and the window is waiting.
const TIMEOUT: Duration = Duration::from_secs(5);

/// When `SHELL` is not set: the default login shell since macOS 10.15.
const FALLBACK_SHELL: &str = "/bin/zsh";

/// What [`adopt`] did, for the log.
#[derive(Debug)]
enum Outcome {
    /// The PATH already had more than launchd's directories.
    Kept,
    /// The login shell's PATH is in front; `added` directories are new.
    Adopted { shell: String, added: usize },
    /// The login shell did not say, so the PATH is still launchd's.
    Failed { shell: String, error: String },
}

static OUTCOME: OnceLock<Outcome> = OnceLock::new();

/// Take the login shell's PATH when this process has only launchd's.
///
/// Call once, before any thread exists: it sets `PATH`.
pub fn adopt() {
    let current = std::env::var("PATH").unwrap_or_default();
    let outcome = if !is_launchd_default(&current) {
        Outcome::Kept
    } else {
        let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(FALLBACK_SHELL));
        match read(&shell, &[], TIMEOUT) {
            Ok(found) => {
                let (path, added) = merge(&found, &current);
                std::env::set_var("PATH", path);
                Outcome::Adopted { shell: shell.display().to_string(), added }
            }
            Err(error) => Outcome::Failed { shell: shell.display().to_string(), error },
        }
    };
    let _ = OUTCOME.set(outcome);
}

/// Say what [`adopt`] did. Call once logging has started.
pub fn log() {
    match OUTCOME.get() {
        Some(Outcome::Adopted { shell, added }) => tracing::info!(%shell, added, "PATH taken from the login shell"),
        Some(Outcome::Failed { shell, error }) => {
            tracing::warn!(%shell, %error, "the PATH is launchd's and the login shell's could not be read: agents that run under node will not start")
        }
        Some(Outcome::Kept) | None => {}
    }
}

/// Whether `path` is launchd's: nothing in it but [`LAUNCHD_DIRS`]. An empty
/// PATH counts, since it has even less.
fn is_launchd_default(path: &str) -> bool {
    std::env::split_paths(path).all(|dir| dir.as_os_str().is_empty() || LAUNCHD_DIRS.iter().any(|d| dir == Path::new(d)))
}

/// Ask `shell`, as an interactive login shell, for the PATH it sets up.
///
/// Interactive as well as login: Homebrew's installer writes to `.zprofile`,
/// which a login shell reads, but nvm's and most people's own lines are in
/// `.zshrc`, which only an interactive one does. stdin is empty, so a startup
/// file that asks a question reads end-of-file instead of waiting for an
/// answer.
///
/// The output is read line by line on a thread of its own, and only until
/// the PATH has come: something a startup file starts in the background can
/// keep the pipe open long after the shell is gone, and waiting for the end
/// of it would wait for that. The shell is killed if it has not finished by
/// then, and when `timeout` runs out.
fn read(shell: &Path, env: &[(&str, &str)], timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(shell);
    cmd.args(["-l", "-i", "-c", &format!("echo {MARKER}; /usr/bin/printenv PATH")])
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", shell.display()))?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let mut lines = Vec::new();
    let found = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                lines.push(line);
                if let Some(path) = parse(&lines) {
                    break Ok(path);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break Err(format!("{} gave no PATH within {}s", shell.display(), timeout.as_secs())),
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err(format!("{} ended without printing a PATH", shell.display())),
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    found
}

/// The PATH out of a login shell's output: the line after [`MARKER`], if it
/// has come and looks like one. The marker's own line may carry escape codes
/// in front of it, printed without a newline of their own.
fn parse(lines: &[String]) -> Option<String> {
    let marker = lines.iter().position(|l| l.trim_end().ends_with(MARKER))?;
    let path = lines.get(marker + 1)?.trim();
    let looks_like_one = !path.chars().any(char::is_control) && std::env::split_paths(path).any(|dir| dir.is_absolute());
    looks_like_one.then(|| path.to_string())
}

/// The login shell's directories first, then this process's that it did not
/// have, each once. Also how many are new to this process.
fn merge(shell: &str, current: &str) -> (String, usize) {
    let before: Vec<PathBuf> = std::env::split_paths(current).collect();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(shell).chain(before.iter().cloned()) {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    let added = dirs.iter().filter(|d| !before.contains(d)).count();
    // Every entry came out of a split on ':', so none of them holds one and
    // joining cannot fail.
    let path = std::env::join_paths(&dirs).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| shell.to_string());
    (path, added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    /// A folder to stand in for the home one, holding a `.zshrc`: zsh reads
    /// its startup files from `ZDOTDIR` when it is set.
    fn zdotdir(zshrc: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("divixi-shell-path-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".zshrc"), zshrc).unwrap();
        dir
    }

    #[test]
    fn only_launchds_own_path_is_replaced() {
        assert!(is_launchd_default("/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(is_launchd_default("/bin:/usr/bin"));
        assert!(is_launchd_default(""));
        assert!(!is_launchd_default("/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(!is_launchd_default("/usr/local/bin:/usr/bin:/bin"));
    }

    #[test]
    fn the_path_is_the_line_after_the_marker_whatever_came_first() {
        let output = format!("Welcome back!\n\x1b]7;file://mac/Users/me\x07{MARKER}\n/Users/me/.nvm/versions/node/v24/bin:/opt/homebrew/bin:/usr/bin\n");
        assert_eq!(parse(&lines(&output)).as_deref(), Some("/Users/me/.nvm/versions/node/v24/bin:/opt/homebrew/bin:/usr/bin"));
    }

    #[test]
    fn nothing_is_taken_without_the_marker_or_before_the_path_has_come() {
        assert_eq!(parse(&lines("/opt/homebrew/bin:/usr/bin\n")), None);
        assert_eq!(parse(&lines(&format!("{MARKER}\n"))), None);
        assert_eq!(parse(&lines(&format!("{MARKER}\n\x1b[?2004h\n"))), None);
    }

    #[test]
    fn the_shells_directories_go_first_and_none_twice() {
        let (path, added) = merge("/opt/homebrew/bin:/usr/bin:/bin", "/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(path, "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(added, 1);
    }

    /// The whole of it against a real zsh, with a `.zshrc` that greets and
    /// sets a terminal title before it touches the PATH, as real ones do.
    #[test]
    fn a_zshrcs_path_comes_through_its_noise() {
        let home = zdotdir("echo 'Welcome back!'\nprintf '\\033]0;title\\007'\nexport PATH=/opt/divixi-test/bin:$PATH\n");
        let path = read(Path::new("/bin/zsh"), &[("ZDOTDIR", home.to_str().unwrap())], Duration::from_secs(20)).unwrap();
        assert!(path.starts_with("/opt/divixi-test/bin:"), "{path}");
    }

    #[test]
    fn a_shell_that_never_finishes_starting_is_given_up_on() {
        let home = zdotdir("sleep 10\n");
        let started = Instant::now();
        let err = read(Path::new("/bin/zsh"), &[("ZDOTDIR", home.to_str().unwrap())], Duration::from_millis(500)).unwrap_err();
        assert!(err.contains("gave no PATH"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    }
}
