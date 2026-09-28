//! Running someone else's command, with a deadline.
//!
//! Every command runs as `sh -c` in a process group of its own, and the group
//! is what gets killed. Killing `sh` alone is not enough: in `sleep 30 | cat`
//! the `sleep` survives it, handed to init, and a widget polling a tool that
//! hangs would leave one more of those behind on every refresh.
//!
//! Output is read while the command runs rather than once it has exited, so a
//! command that prints more than a pipe holds still finishes instead of
//! blocking on a full pipe until the deadline kills it.

use std::fs::File;
use std::io::{ErrorKind, Read, Seek};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a widget's command may take before it is killed.
///
/// Long enough for anything reasonable, short enough that a wedged script does
/// not hold up every other command widget behind it in the queue.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How often to check whether a command has finished.
const POLL: Duration = Duration::from_millis(20);
/// How often to check on a command left running past its window.
///
/// Nobody is waiting for its answer any more. This only reaps it soon after
/// it ends, so a slow look is enough.
const LINGER_POLL: Duration = Duration::from_millis(250);
/// The most of each of stdout and stderr kept. The rest is read and dropped.
pub const MAX_OUTPUT: usize = 64 << 10;
/// The longest first line kept, in characters.
pub const MAX_LINE: usize = 256;
/// Size of one read from a command's output.
const READ_SIZE: usize = 64 << 10;
/// Most reads per look at a command's output, so one that never stops
/// talking cannot keep the clock from being checked.
const READS_PER_LOOK: usize = 16;
/// Room for what a command left running writes to stderr: far more than a
/// state change has to say, and little enough that a program it started,
/// complaining for days, cannot run memory up.
const ERRORS_ROOM: u64 = 1 << 20;

/// How a command went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It exited by itself.
    ///
    /// `code` is its exit status, or 128 plus the signal that ended it, the
    /// way a shell reports one. Each line is the first of its stream with
    /// anything on it, trimmed and cut to [`MAX_LINE`] characters.
    Exited {
        code: i32,
        stdout_first_line: Option<String>,
        stderr_first_line: Option<String>,
    },
    /// It was still running at the deadline, and was killed along with
    /// everything it had started.
    TimedOut,
    /// It was still running when its window closed, and was left running.
    /// Only [`run_unkilled`] reports this.
    Launched,
    /// It could not be run, or could not be waited on.
    Failed(String),
}

/// Run a widget's command and take the first line of its output.
///
/// Killed if it overruns, so one wedged script cannot hold up every other
/// command widget behind it.
pub fn run(command: &str) -> Option<String> {
    match run_bounded(command, TIMEOUT, &[]) {
        Outcome::Exited {
            stdout_first_line, ..
        } => stdout_first_line,
        Outcome::TimedOut => {
            log::warn!("widget command exceeded {TIMEOUT:?}, killed it: {command}");
            None
        }
        Outcome::Failed(e) => {
            log::warn!("widget command {command:?} failed: {e}");
            None
        }
        Outcome::Launched => None,
    }
}

/// Run `command` with `env` added to the daemon's environment, until it
/// exits or `deadline` has passed, and say how it went.
///
/// At the deadline its whole process group is killed. The group is killed
/// once `sh` has exited as well, so nothing it started outlives it: not the
/// rest of a pipeline, and not something it put in the background. Nothing is
/// read from stdin. At most [`MAX_OUTPUT`] bytes of each of stdout and stderr
/// are kept; past that, output is read and dropped. A deadline further away
/// than the clock goes is no deadline.
pub fn run_bounded(command: &str, deadline: Duration, env: &[(&str, &str)]) -> Outcome {
    match collect(command, deadline, env) {
        Ok(collected) => Outcome::Exited {
            code: exit_code(collected.status),
            stdout_first_line: first_line(&collected.stdout),
            stderr_first_line: first_line(&collected.stderr),
        },
        Err(outcome) => outcome,
    }
}

/// Run `command` without ever killing it, and tell `report` how it went, once:
/// how it exited if it did within `window`, else [`Outcome::Launched`] as the
/// window closes.
///
/// This is for a command that changes something, which may well start a
/// program meant to keep running. Killing it at a deadline would undo what
/// was asked for, so it is left alone, and this returns only once it has
/// exited and been reaped. It runs in the daemon's own environment, and its
/// stdout goes where the daemon's does, as a key action's would. Only its
/// stderr is read, so `stdout_first_line` is always `None`.
///
/// Its stderr is a file in memory rather than a pipe, because something it
/// started in the background keeps it: a launcher is written `cmd &`, and
/// that program may outlive not only the command but the daemon, whose
/// service is set to spare what it started when it restarts. A write to a
/// pipe nobody reads any more kills the writer; one to this file never
/// blocks and never kills, and past [`ERRORS_ROOM`] it only fails. So this
/// returns once `sh` is reaped, whatever it left running.
pub fn run_unkilled(command: &str, window: Duration, report: impl FnOnce(Outcome)) {
    let until = Instant::now().checked_add(window);
    let mut report = Some(report);
    let (stderr, mut errors) = match error_file() {
        Ok((theirs, ours)) => (Stdio::from(theirs), Some(ours)),
        Err(e) => {
            // Its errors go to the log instead, and the report goes without
            // them.
            log::debug!("no file for a command's errors: {e}");
            (Stdio::inherit(), None)
        }
    };
    let mut child = match shell(command, &[]).stderr(stderr).spawn() {
        Ok(child) => child,
        Err(e) => {
            return tell(
                &mut report,
                Outcome::Failed(format!("sh did not start: {e}")),
            )
        }
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let outcome = Outcome::Exited {
                    code: exit_code(status),
                    stdout_first_line: None,
                    stderr_first_line: errors.as_ref().and_then(first_line_written),
                };
                return tell(&mut report, outcome);
            }
            // Launched already: this is only here to reap it.
            Ok(None) if report.is_none() => std::thread::sleep(LINGER_POLL),
            Ok(None) => {
                if until.is_some_and(|until| Instant::now() >= until) {
                    tell(&mut report, Outcome::Launched);
                    // Nothing more will be read from it.
                    errors = None;
                }
                std::thread::sleep(POLL);
            }
            // Only a child something else has reaped gets here, and then
            // there is nothing left to wait for.
            Err(e) => {
                return tell(&mut report, Outcome::Failed(format!("waiting on it: {e}")));
            }
        }
    }
}

/// A file in memory for a command's stderr, as two descriptors of it: one to
/// hand the command, one to read back what it wrote.
///
/// It is [`ERRORS_ROOM`] bytes long, and sealed at that length, so a write
/// past the end fails rather than growing it, whoever holds it by then.
fn error_file() -> std::io::Result<(File, File)> {
    // SAFETY: the name is a NUL-terminated constant, which is only read.
    let fd = unsafe {
        libc::memfd_create(
            c"galdeck-errors".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, which nothing else owns.
    let file = unsafe { File::from_raw_fd(fd) };
    // Holes until written, so the room costs nothing unused.
    file.set_len(ERRORS_ROOM)?;
    let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: fcntl on a descriptor this owns; no memory is passed.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((file.try_clone()?, file))
}

/// The first line with anything on it of what a command wrote to `file`,
/// one of the descriptors [`error_file`] gave.
fn first_line_written(file: &File) -> Option<String> {
    // The two descriptors share one offset, which the command's writes have
    // moved to just past what they wrote. Reading at an offset leaves it be.
    let mut ours = file;
    let written = ours.stream_position().ok()?;
    let mut kept = vec![0; usize::try_from(written).ok()?.min(MAX_OUTPUT)];
    file.read_exact_at(&mut kept, 0).ok()?;
    first_line(&kept)
}

/// Hand `outcome` to `report` if it has not had one yet.
fn tell(report: &mut Option<impl FnOnce(Outcome)>, outcome: Outcome) {
    if let Some(report) = report.take() {
        report(outcome);
    }
}

/// What a command printed, and how it exited.
struct Collected {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// [`run_bounded`], keeping the output itself rather than its first lines.
fn collect(command: &str, deadline: Duration, env: &[(&str, &str)]) -> Result<Collected, Outcome> {
    let until = Instant::now().checked_add(deadline);
    let mut child = shell(command, env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Outcome::Failed(format!("sh did not start: {e}")))?;
    let mut stdout = child.stdout.take().expect("stdout was asked to be piped");
    let mut stderr = child.stderr.take().expect("stderr was asked to be piped");
    let stop = |child: &mut Child| {
        kill_group(child);
        child.wait()
    };
    if let Err(e) = set_nonblocking(&stdout).and(set_nonblocking(&stderr)) {
        let _ = stop(&mut child);
        return Err(Outcome::Failed(format!("could not read its output: {e}")));
    }

    let mut buffer = vec![0; READ_SIZE];
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        let looked =
            drain(&mut stdout, &mut buffer, &mut out) | drain(&mut stderr, &mut buffer, &mut err);
        match has_exited(&child) {
            Ok(true) => {
                let status =
                    stop(&mut child).map_err(|e| Outcome::Failed(format!("reaping it: {e}")))?;
                // Whatever it wrote between the last look and exiting.
                drain(&mut stdout, &mut buffer, &mut out);
                drain(&mut stderr, &mut buffer, &mut err);
                return Ok(Collected {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            Ok(false) if until.is_some_and(|until| Instant::now() >= until) => {
                let _ = stop(&mut child);
                return Err(Outcome::TimedOut);
            }
            // It is still talking, so look again straight away.
            Ok(false) if looked => {}
            Ok(false) => std::thread::sleep(POLL),
            // Only a child something else has reaped gets here. Its number
            // may be a stranger's by now, so its group is not killed.
            Err(e) => return Err(Outcome::Failed(format!("waiting on it: {e}"))),
        }
    }
}

/// `sh -c command` with `env` added, reading nothing, as the leader of a
/// process group of its own.
fn shell(command: &str, env: &[(&str, &str)]) -> Command {
    let mut process = crate::actions::action_command(command, None);
    process.envs(env.iter().copied()).process_group(0);
    process
}

/// Kill every process in the group `child` leads.
///
/// Only ever called while `child` is unreaped: until then its process id,
/// which names the group, cannot have been given to anything else.
fn kill_group(child: &Child) {
    let Ok(group) = libc::pid_t::try_from(child.id()) else {
        return;
    };
    // SAFETY: a plain system call; no memory is passed. Failing because
    // nothing is left in the group to kill changes nothing.
    unsafe { libc::kill(-group, libc::SIGKILL) };
}

/// Whether `child` has exited, leaving it unreaped.
///
/// Unreaped, it keeps its process id, so the group [`kill_group`] kills next
/// is still its own, and not a stranger's that was given the same number.
fn has_exited(child: &Child) -> std::io::Result<bool> {
    // SAFETY: siginfo_t is plain data, for which all zeroes is a value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `info` is a live local, which waitid only writes into.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != ErrorKind::Interrupted {
            return Err(e);
        }
    }
    // SAFETY: waitid filled `info` in, or left it zeroed because the child
    // has not exited yet, and then the pid in it is 0.
    Ok(unsafe { info.si_pid() } != 0)
}

/// An exit status as a shell would give it.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or_default())
}

/// The first line of `output` with anything on it, trimmed and cut to
/// [`MAX_LINE`] characters.
fn first_line(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let cut: String = line.chars().take(MAX_LINE).collect();
    Some(cut.trim_end().to_string())
}

/// Make reads from a pipe return at once when it has nothing.
///
/// So one thread can both collect output and watch the clock; a blocking read
/// would wait for as long as the command chose to stay quiet.
fn set_nonblocking(pipe: &impl AsRawFd) -> std::io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: fcntl on a descriptor this process owns and keeps open for the
    // duration; no memory is passed.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Take what has been written to `pipe` so far, keeping at most
/// [`MAX_OUTPUT`] bytes of it all in `kept`, and say whether there was any.
fn drain(pipe: &mut impl Read, buffer: &mut [u8], kept: &mut Vec<u8>) -> bool {
    let mut read = false;
    for _ in 0..READS_PER_LOOK {
        match pipe.read(buffer) {
            Ok(0) => break,
            Ok(n) => {
                read = true;
                let room = MAX_OUTPUT.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..n.min(room)]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // WouldBlock: nothing more for now. Anything else will show up
            // again, or the command's exit will.
            Err(_) => break,
        }
    }
    read
}

/// The number a line of output starts with, if it starts with one.
///
/// So `42`, `42%` and `42.5 °C` can all be graphed, and the text shown is
/// still whatever the script printed.
pub fn leading_number(line: &str) -> Option<f64> {
    let end = line
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && c == '-')))
        .map_or(line.len(), |(i, _)| i);
    line[..end].parse().ok()
}

/// What tests of commands, here and in the states runner, need to watch the
/// processes they start.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// An empty directory for one test, whose path needs no quoting.
    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("galdeck-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Wait up to `limit` for `done` to hold, and say whether it did.
    pub fn eventually(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        done()
    }

    /// What a command wrote to `file` with `echo $$`: the process id of its
    /// shell, which is also the id of its group.
    pub fn pid_in(file: &Path) -> i32 {
        let mut pid = None;
        let written = eventually(Duration::from_secs(5), || {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            pid = text.strip_suffix('\n').and_then(|line| line.parse().ok());
            pid.is_some()
        });
        assert!(written, "{} was never written", file.display());
        pid.unwrap()
    }

    /// The processes in `group` that have not exited. A zombie has: it only
    /// waits for its parent to notice.
    pub fn living_in_group(group: i32) -> Vec<i32> {
        let mut living = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            if state_and_group(pid).is_some_and(|(state, g)| g == group && state != 'Z') {
                living.push(pid);
            }
        }
        living
    }

    /// Whether `pid` is running: there, and not a zombie.
    pub fn is_alive(pid: i32) -> bool {
        state_and_group(pid).is_some_and(|(state, _)| state != 'Z')
    }

    /// A process's state letter and its group, from /proc.
    fn state_and_group(pid: i32) -> Option<(char, i32)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The name in parentheses may hold spaces and parentheses of its own,
        // so the fields are counted from after the last one.
        let (_, rest) = stat.rsplit_once(')')?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let _parent = fields.next()?;
        let group = fields.next()?.parse().ok()?;
        Some((state, group))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    /// A kill takes a moment to land, and a killed pipeline member a moment
    /// longer to become a zombie for init to collect.
    const SETTLE: Duration = Duration::from_secs(3);

    #[test]
    fn the_first_line_is_kept() {
        assert_eq!(run("printf 'one\\ntwo\\n'").as_deref(), Some("one"));
        assert_eq!(run("true"), None);
    }

    #[test]
    fn a_number_is_found_at_the_start_of_a_line() {
        assert_eq!(leading_number("42"), Some(42.0));
        assert_eq!(leading_number("42.5 °C"), Some(42.5));
        assert_eq!(leading_number("-3%"), Some(-3.0));
        assert_eq!(leading_number("up 3 days"), None);
        assert_eq!(leading_number(""), None);
    }

    #[test]
    fn a_pipeline_that_overruns_is_killed_whole() {
        let dir = scratch("command-overrun");
        let pid = dir.join("pid");
        let started = Instant::now();
        let outcome = run_bounded(
            &format!("echo $$ > '{}'; sleep 30 | cat", pid.display()),
            Duration::from_millis(300),
            &[],
        );
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));

        let group = pid_in(&pid);
        assert!(
            eventually(SETTLE, || living_in_group(group).is_empty()),
            "still running: {:?}",
            living_in_group(group)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_a_command_started_outlives_it() {
        let dir = scratch("command-background");
        let pid = dir.join("pid");
        let outcome = run_bounded(
            &format!(
                "echo $$ > '{}'; sleep 30 > /dev/null 2>&1 & echo started",
                pid.display()
            ),
            Duration::from_secs(5),
            &[],
        );
        assert_eq!(
            outcome,
            Outcome::Exited {
                code: 0,
                stdout_first_line: Some("started".into()),
                stderr_first_line: None,
            }
        );

        let group = pid_in(&pid);
        assert!(
            eventually(SETTLE, || living_in_group(group).is_empty()),
            "still running: {:?}",
            living_in_group(group)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flood_of_output_is_bounded_and_finishes_quickly() {
        // Far more than a pipe holds: it only finishes if it is read as it
        // goes, and only the start of it is kept.
        let started = Instant::now();
        let collected = collect("yes | head -c 10000000", Duration::from_secs(5), &[])
            .unwrap_or_else(|outcome| panic!("{outcome:?}"));
        assert!(collected.status.success());
        assert_eq!(collected.stdout.len(), MAX_OUTPUT);
        assert!(collected.stderr.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );

        let outcome = run_bounded("yes | head -c 10000000", Duration::from_secs(5), &[]);
        assert!(
            matches!(&outcome, Outcome::Exited { code: 0, stdout_first_line: Some(y), .. } if y == "y"),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_command_that_never_stops_talking_still_times_out() {
        let started = Instant::now();
        let outcome = run_bounded("yes", Duration::from_millis(200), &[]);
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn each_stream_gives_its_first_line_with_anything_on_it() {
        let outcome = run_bounded(
            "printf '\\n   \\n  performance  \\nbalanced\\n'; echo 'went wrong' >&2; exit 3",
            Duration::from_secs(5),
            &[],
        );
        assert_eq!(
            outcome,
            Outcome::Exited {
                code: 3,
                stdout_first_line: Some("performance".into()),
                stderr_first_line: Some("went wrong".into()),
            }
        );
    }

    #[test]
    fn a_long_first_line_is_cut_short() {
        let outcome = run_bounded(
            "head -c 100000 /dev/zero | tr '\\0' x",
            Duration::from_secs(5),
            &[],
        );
        let Outcome::Exited {
            stdout_first_line: Some(line),
            ..
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(*line, "x".repeat(MAX_LINE));
    }

    #[test]
    fn death_by_a_signal_is_reported_as_a_shell_would() {
        let outcome = run_bounded("kill -9 $$", Duration::from_secs(5), &[]);
        assert!(
            matches!(outcome, Outcome::Exited { code: 137, .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_environment_given_is_added_to_the_daemons() {
        let outcome = run_bounded(
            "echo \"$LC_ALL:${HOME:+home}\"",
            Duration::from_secs(5),
            &[("LC_ALL", "C.UTF-8")],
        );
        assert!(
            matches!(&outcome, Outcome::Exited { stdout_first_line: Some(line), .. } if line == "C.UTF-8:home"),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_unkilled_command_that_exits_in_time_says_how() {
        let (tx, rx) = std::sync::mpsc::channel();
        run_unkilled("echo nope >&2; exit 1", Duration::from_secs(5), |outcome| {
            tx.send(outcome).unwrap()
        });
        assert_eq!(
            rx.try_recv(),
            Ok(Outcome::Exited {
                code: 1,
                stdout_first_line: None,
                stderr_first_line: Some("nope".into()),
            })
        );
        assert!(rx.try_recv().is_err(), "told only once");
    }

    #[test]
    fn a_program_an_unkilled_command_left_behind_outlives_whoever_ran_it() {
        // Its write to stderr comes after this has returned, when nothing of
        // the daemon's is left to read it, as after a restart. Had stderr
        // been a pipe, that write would kill it before it got to the touch.
        let dir = scratch("command-left-behind");
        let wrote = dir.join("wrote");
        let command = format!(
            "(sleep 1; echo late >&2; touch '{}') & exit 0",
            wrote.display()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        run_unkilled(&command, Duration::from_secs(5), |outcome| {
            tx.send(outcome).unwrap()
        });
        let returned = started.elapsed();
        assert!(returned < Duration::from_millis(800), "{returned:?}");
        assert_eq!(
            rx.try_recv(),
            Ok(Outcome::Exited {
                code: 0,
                stdout_first_line: None,
                stderr_first_line: None,
            })
        );
        assert!(eventually(Duration::from_secs(3), || wrote.exists()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unkilled_command_that_says_too_much_is_neither_held_up_nor_killed() {
        // Past the room, its writes fail, which head says with its exit
        // status, and the command goes on to its end.
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        run_unkilled(
            "echo first >&2; head -c 3000000 /dev/zero >&2; exit $((10 + $?))",
            Duration::from_secs(5),
            |outcome| tx.send(outcome).unwrap(),
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            rx.try_recv(),
            Ok(Outcome::Exited {
                code: 11,
                stdout_first_line: None,
                stderr_first_line: Some("first".into()),
            })
        );
    }

    #[test]
    fn an_unkilled_command_past_its_window_is_launched_then_reaped() {
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        run_unkilled(
            "echo slow >&2; sleep 0.5",
            Duration::from_millis(100),
            |outcome| tx.send((outcome, started.elapsed())).unwrap(),
        );
        let (outcome, told) = rx.try_recv().unwrap();
        assert_eq!(outcome, Outcome::Launched);
        assert!(told < Duration::from_millis(400), "{told:?}");
        // It returned only once the command had exited, having reaped it.
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert!(rx.try_recv().is_err(), "told only once");
    }

    #[test]
    fn a_command_that_cannot_be_run_fails_rather_than_panics() {
        let outcome = run_bounded("echo a\0b", Duration::from_secs(5), &[]);
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        let (tx, rx) = std::sync::mpsc::channel();
        run_unkilled("echo a\0b", Duration::from_secs(5), |outcome| {
            tx.send(outcome).unwrap()
        });
        assert!(
            matches!(rx.try_recv(), Ok(Outcome::Failed(_))),
            "told of the failure"
        );
    }
}
