//! Sync→async bridge for injected-adapter capabilities.
//!
//! [`super::SopCapability::execute`] is synchronous and runs while the caller
//! blocks a host thread (typically under the engine mutex, sometimes on a
//! current-thread runtime such as the channel dispatch context). Spawning the
//! async work back onto the HOST runtime is therefore unsound: on a
//! current-thread context the spawned task cannot be polled until the blocked
//! caller returns, which is a guaranteed timeout (observed in the field: the
//! model call only started executing the instant the capability gave up
//! waiting). Instead, each bridged call runs on a DEDICATED OS thread with its
//! own small current-thread runtime, fully independent of the host executor.
//! These calls are rare (one per side-effecting SOP step), so the per-call
//! thread + runtime cost is noise.

use std::future::Future;
use std::time::Duration;

/// Run `fut` to completion on a dedicated bridge thread, waiting at most
/// `timeout`. `what` names the operation for thread naming and error text.
pub(super) fn run_bridged<T, F>(fut: F, timeout: Duration, what: &str) -> Result<T, String>
where
    T: Send + 'static,
    F: Future<Output = Result<T, String>> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name(format!("sop-bridge-{what}"))
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(fut),
                Err(e) => Err(format!("bridge runtime build failed: {e}")),
            };
            // Receiver gone = caller timed out; nothing left to report to.
            let _ = tx.send(result);
        });
    if let Err(e) = spawned {
        return Err(format!("failed to spawn the {what} bridge thread: {e}"));
    }
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "timed out after {}s waiting for the {what}",
            timeout.as_secs()
        )),
        // Sender dropped without a result: the bridged task died (panic) — a
        // different failure than a slow operation.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(format!("{what} task died before reporting a result"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_future_from_a_plain_thread() {
        let out = run_bridged(
            async { Ok::<_, String>(7u32) },
            Duration::from_secs(5),
            "test",
        );
        assert_eq!(out, Ok(7));
    }

    #[test]
    fn runs_even_while_the_caller_blocks_inside_a_current_thread_runtime() {
        // The regression this bridge exists for: the caller blocks the ONLY
        // thread of a current-thread runtime while the bridged future must
        // still make progress (it cannot, if spawned onto that same runtime).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(async {
            run_bridged(
                async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok::<_, String>("done".to_string())
                },
                Duration::from_secs(5),
                "test",
            )
        });
        assert_eq!(out, Ok("done".to_string()));
    }

    #[test]
    fn timeout_and_error_paths_are_distinguished() {
        let slow = run_bridged(
            async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok::<_, String>(0u8)
            },
            Duration::from_millis(50),
            "slowop",
        );
        assert!(slow.unwrap_err().contains("timed out"));

        let failing = run_bridged(
            async { Err::<u8, _>("boom".to_string()) },
            Duration::from_secs(5),
            "failop",
        );
        assert_eq!(failing.unwrap_err(), "boom");
    }
}
