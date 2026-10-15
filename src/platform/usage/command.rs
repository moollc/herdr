//! Bounded helper execution shared by browser discovery and credential storage.
use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub(super) struct Output {
    pub(super) code: Option<i32>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}
/// Every helper shares an overall deadline. Drain pipes concurrently to avoid blocking
/// on a large database response; a timeout kills and reaps the helper before returning.
pub(super) fn execute(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    deadline: Instant,
) -> Result<Output, ()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    let mut child = Command::new(program)
        .args(args)
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ())?;
    let process_group = child.id() as i32;
    let stdout = child.stdout.take().ok_or(())?;
    let stderr = child.stderr.take().ok_or(())?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.by_ref().take(8 * 1024 * 1024).read_to_end(&mut buf);
            buf
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let writer = input.map(|input| {
        let input = input.to_vec();
        let stdin = child.stdin.take();
        std::thread::spawn(move || {
            stdin
                .ok_or(())
                .and_then(|mut stdin| stdin.write_all(&input).map_err(|_| ()))
        })
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                // SAFETY: the child has its own process group; terminate inherited pipe holders too.
                unsafe {
                    libc::kill(-process_group, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // SAFETY: stop descendants that retained output pipes after their helper exited.
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    let stdout = out.join().map_err(|_| ())?;
    let stderr = err.join().map_err(|_| ())?;
    if let Some(writer) = writer {
        writer.join().map_err(|_| ())??;
    }
    let status = status.ok_or(())?;
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_fake_helper_stdin_and_deadline_include_descendants() {
        let output = execute(
            "/bin/sh",
            &["-c", "cat"],
            Some(b"synthetic"),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(output.stdout, b"synthetic");
        let start = Instant::now();
        assert!(execute(
            "/bin/sh",
            &["-c", "sleep 10 & wait"],
            None,
            Instant::now() + Duration::from_millis(30)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        let start = Instant::now();
        assert!(execute(
            "/bin/sh",
            &["-c", "sleep 10 & exit 0"],
            None,
            Instant::now() + Duration::from_secs(1)
        )
        .is_ok());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
