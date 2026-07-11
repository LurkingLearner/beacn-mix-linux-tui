//! Local control socket used by the configuration front-ends to request live
//! daemon changes. Persistence alone is not enough for controls such as mute:
//! the daemon owns the in-memory state that drives the hardware and audio.

use crate::state;
#[cfg(feature = "gui")]
use anyhow::bail;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(1);

/// A live adjustment the `run` daemon can apply atomically with its in-memory
/// state and persisted state.
#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    SetMute { channel: usize, muted: bool },
}

#[derive(Serialize, Deserialize)]
struct Response {
    ok: bool,
    error: Option<String>,
}

/// Submit a command to a running daemon and wait until it has either applied
/// it or rejected it. A missing socket means `run` is not active.
#[cfg(feature = "gui")]
pub fn request(command: Command) -> Result<()> {
    let path = state::control_socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connecting to running mixer at {}", path.display()))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let request = serde_json::to_vec(&command)?;
    stream.write_all(&request)?;
    // Tell the daemon the request is complete while keeping the read half open
    // for its acknowledgement.
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    let response: Response = serde_json::from_str(&raw).context("reading daemon response")?;
    if response.ok {
        Ok(())
    } else {
        bail!(response
            .error
            .unwrap_or_else(|| "daemon rejected command".into()))
    }
}

/// Server endpoint held for the lifetime of `run`.
pub struct Listener {
    listener: UnixListener,
    path: std::path::PathBuf,
}

impl Listener {
    /// Bind the per-user control socket, replacing a stale socket left behind
    /// by a previous daemon process.
    pub fn bind() -> Result<Self> {
        let path = state::control_socket_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding control socket {}", path.display()))?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, path })
    }

    /// Handle all queued requests. The handler's successful return is the
    /// acknowledgement the GUI receives, so it must apply *and persist* a
    /// command before returning `Ok(())`.
    pub fn service(&self, mut handle: impl FnMut(Command) -> Result<()>) -> Result<()> {
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e).context("accepting control connection"),
            };
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;

            let result = read_command(&mut stream).and_then(&mut handle);
            let response = match result {
                Ok(()) => Response {
                    ok: true,
                    error: None,
                },
                Err(e) => Response {
                    ok: false,
                    error: Some(format!("{e:#}")),
                },
            };
            let response = serde_json::to_vec(&response)?;
            stream.write_all(&response)?;
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read_command(stream: &mut UnixStream) -> Result<Command> {
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    if raw.is_empty() {
        return Err(anyhow!("empty control request"));
    }
    serde_json::from_str(&raw).context("parsing control request")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_command_roundtrips_over_the_control_protocol() {
        let command = Command::SetMute {
            channel: 2,
            muted: true,
        };
        let json = serde_json::to_string(&command).expect("serialize command");
        let Command::SetMute { channel, muted } =
            serde_json::from_str(&json).expect("deserialize command");
        assert_eq!(channel, 2);
        assert!(muted);
    }
}
