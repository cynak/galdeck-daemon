//! Running someone else's command, with a deadline.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a command may take before it is killed.
///
/// Long enough for anything reasonable, short enough that a wedged script does
/// not hold up every other command widget behind it in the queue.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How often to check whether a command has finished.
const POLL: Duration = Duration::from_millis(20);

/// Run a command and take the first line of its output.
///
/// Killed if it overruns, so one wedged script cannot hold up every other
/// command widget behind it.
pub fn run(command: &str) -> Option<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| log::warn!("widget command failed to start: {e}"))
        .ok()?;

    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                log::warn!("widget command exceeded {TIMEOUT:?}, killing it: {command}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                log::warn!("waiting on a widget command: {e}");
                return None;
            }
        }
    }

    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        // Bounded: a key shows a handful of characters, and a command that
        // prints a megabyte should not cost a megabyte per refresh.
        let mut buffer = vec![0u8; galdeck_model::widget::MAX_OUTPUT_BYTES];
        if let Ok(n) = stdout.read(&mut buffer) {
            output = String::from_utf8_lossy(&buffer[..n]).into_owned();
        }
    }
    let first = output.lines().next().unwrap_or_default().trim().to_string();
    (!first.is_empty()).then_some(first)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
