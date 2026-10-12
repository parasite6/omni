//! Running helper programs (grim, hyprctl) from the UI thread without risking a hang.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Run `cmd` and return its stdout if it succeeds within `timeout`. Otherwise kill it
/// (and reap it) and return None. stdout is read on a thread so large output (a
/// screenshot) can't fill the pipe and deadlock.
pub fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<Vec<u8>> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(timeout) {
        Ok(buf) => {
            // stdout is closed, but the process may still be running (it can close its
            // output and then hang). Don't block the caller on wait().
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return status.success().then_some(buf),
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => return None,
                }
            }
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn returns_output() {
        let out = output_with_timeout(Command::new("echo").arg("hi"), Duration::from_secs(2));
        assert_eq!(out.as_deref(), Some(&b"hi\n"[..]));
    }

    #[test]
    fn kills_a_hung_command() {
        let start = Instant::now();
        let out = output_with_timeout(Command::new("sleep").arg("10"), Duration::from_millis(200));
        assert!(out.is_none());
        assert!(start.elapsed() < Duration::from_secs(2), "took {:?}", start.elapsed());
    }

    #[test]
    fn large_output_does_not_deadlock() {
        // Bigger than a pipe buffer, like a screenshot.
        let out = output_with_timeout(Command::new("head").args(["-c", "3000000", "/dev/zero"]), Duration::from_secs(5));
        assert_eq!(out.map(|o| o.len()), Some(3_000_000));
    }

    #[test]
    fn failing_command_is_none() {
        assert!(output_with_timeout(&mut Command::new("false"), Duration::from_secs(2)).is_none());
    }

    #[test]
    fn closes_stdout_then_hangs() {
        let start = Instant::now();
        // Close stdout, then sleep: the reader finishes, and we must not wait out the sleep.
        let out = output_with_timeout(
            Command::new("sh").args(["-c", "printf hi; exec 1>&-; exec sleep 30"]),
            Duration::from_secs(5),
        );
        assert!(out.is_none());
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }
}
