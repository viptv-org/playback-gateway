//! Ownership at the OS boundary. Cancellation still uses kill/wait guards;
//! Linux parent death additionally prevents an abrupt service exit orphaning
//! a local replay reader that would otherwise poll its final playlist forever.
pub fn contain_media_process(command: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: getpid has no mutable process state and cannot fail.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the child hook uses only syscall wrappers and raw OS errors,
        // without locks, allocation, logging or Tokio after fork and before exec.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                // Parent death before registration does not deliver a signal.
                // Refuse exec if that race already reparented this child.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn media_child_dies_when_its_creating_thread_exits() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut command = tokio::process::Command::new("/bin/sleep");
            command.arg("30");
            contain_media_process(&mut command);
            // Exercise the real configured pre-exec hook without requiring a
            // Tokio runtime in the intentionally short-lived creator thread.
            sender.send(command.as_std_mut().spawn().unwrap()).unwrap();
        })
        .join()
        .unwrap();
        let mut child = receiver.recv().unwrap();
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.signal(), Some(libc::SIGKILL));
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(2) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("configured child outlived its creating thread");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
