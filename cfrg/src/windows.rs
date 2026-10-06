//! Native Windows backend. Release enablement requires native cancellation proof.
#![forbid(unsafe_code)]

use super::process::Runner;
use super::{failure, Result, INTERRUPTED};
use process_wrap::tokio::{ChildWrapper, CommandWrap, JobObject, KillOnDrop};
use serde_json::json;
use std::{
    path::Path,
    process::Stdio,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command, runtime::Builder, time};

struct OwnedJob(Box<dyn ChildWrapper>);
impl Drop for OwnedJob {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

pub(super) fn run(runner: &Runner, argv: &[String], capture: bool) -> Result<String> {
    Builder::new_current_thread().enable_all().build()?.block_on(async {
        let started = Instant::now();
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).current_dir(&runner.root).env_clear().envs(&runner.environment)
            .stdin(Stdio::null()).stderr(Stdio::inherit())
            .stdout(if capture { Stdio::piped() } else { Stdio::inherit() });
        let mut command = CommandWrap::from(command);
        // process-wrap creates the child suspended, assigns it to this job and
        // only then resumes it. KillOnDrop also enables KILL_ON_JOB_CLOSE.
        command.wrap(KillOnDrop).wrap(JobObject);
        let mut child = OwnedJob(command.spawn()?);
        let reader = if capture {
            let stdout = child.0.stdout().take().ok_or_else(|| failure("Missing captured stdout"))?;
            Some(tokio::spawn(async move {
                let mut bytes = Vec::new();
                stdout.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes).await?;
                Ok::<_, std::io::Error>(bytes)
            }))
        } else { None };
        let status = loop {
            if INTERRUPTED.load(Ordering::SeqCst) || Instant::now() >= runner.deadline {
                child.0.start_kill()?;
                // try_wait only proves the parent exited. The owned Job Object,
                // explicit termination and kill-on-close cover its descendants;
                // do not rely on completion-port messages as an all-exited proof.
                let grace = Instant::now() + Duration::from_secs(10);
                while Instant::now() < grace && child.0.try_wait()?.is_none() {
                    time::sleep(Duration::from_millis(20)).await;
                }
                return Err(failure("Command interrupted or timed out; its owned Windows job was terminated"));
            }
            if let Some(status) = child.0.try_wait()? { break status; }
            time::sleep(Duration::from_millis(20)).await;
        };
        child.0.start_kill()?;
        // Closing the job terminates remaining descendants even when the parent
        // exited first. A hard termination of cfrg also closes this owned handle.
        drop(child);
        runner.command_event(json!({"event":"command","executable":Path::new(&argv[0]).file_name().map(|s|s.to_string_lossy()),"seconds":started.elapsed().as_secs_f64(),"exit_code":status.code()}));
        if !status.success() { return Err(failure(format!("{} failed: {status}",Path::new(&argv[0]).file_name().unwrap_or_default().to_string_lossy()))); }
        if let Some(reader) = reader {
            let bytes = time::timeout(Duration::from_secs(10), reader).await
                .map_err(|_| failure("Captured output did not close after command termination"))???;
            if bytes.len() > 16 * 1024 * 1024 { return Err(failure("Captured command output exceeded 16 MiB")); }
            return Ok(String::from_utf8(bytes)?.trim().to_owned());
        }
        Ok(String::new())
    })
}
