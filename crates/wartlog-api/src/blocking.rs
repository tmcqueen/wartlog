//! Client for the daemon socket on plain blocking sockets: no async runtime, so it can live inside
//! someone else's process (the C ABI, a .NET or Python host).

use crate::proto::{read_frame_sync, write_frame_sync, Request, Response};
use crate::PROTOCOL_VERSION;
use anyhow::{bail, Context, Result};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub struct BlockingClient {
    s: UnixStream,
    pub server: String,
}

impl BlockingClient {
    /// Connects and does the version handshake.
    pub fn connect(path: &Path, client: &str) -> Result<BlockingClient> {
        let mut s = UnixStream::connect(path).with_context(|| format!("cannot reach wartlogd at {} (is it running? try `systemctl --user start wartlogd`)", path.display()))?;
        write_frame_sync(&mut s, &Request::Hello { version: PROTOCOL_VERSION, client: client.to_string() })?;
        match read_frame_sync::<_, Response>(&mut s)? {
            Some(Response::Hello { version, server }) if version == PROTOCOL_VERSION => Ok(BlockingClient { s, server }),
            Some(Response::Hello { version, server }) => bail!("{server} speaks API version {version}, this client speaks {PROTOCOL_VERSION}"),
            Some(Response::Error { message, .. }) => bail!("{message}"),
            other => bail!("unexpected handshake reply: {other:?}"),
        }
    }

    /// Bounds how long a reply may take (None: wait forever).
    pub fn set_timeout(&self, t: Option<Duration>) -> Result<()> {
        self.s.set_read_timeout(t)?;
        self.s.set_write_timeout(t)?;
        Ok(())
    }

    pub fn send(&mut self, r: &Request) -> Result<()> {
        write_frame_sync(&mut self.s, r)
    }

    /// Next message of the current reply; `None` if the daemon closed the connection.
    pub fn recv(&mut self) -> Result<Option<Response>> {
        read_frame_sync(&mut self.s)
    }

    /// Sends a request and collects the whole reply. Not for `Tail`, which never ends.
    pub fn call(&mut self, r: &Request) -> Result<Vec<Response>> {
        self.send(r)?;
        let mut out = Vec::new();
        loop {
            match self.recv()? {
                Some(m) => {
                    let fin = m.is_final();
                    out.push(m);
                    if fin {
                        return Ok(out);
                    }
                }
                None => bail!("wartlogd closed the connection"),
            }
        }
    }

    /// Like `call`, for requests with one reply message; turns `Error` into an error.
    pub fn call_one(&mut self, r: &Request) -> Result<Response> {
        let mut v = self.call(r)?;
        match v.pop() {
            Some(Response::Error { message, .. }) => bail!("{message}"),
            Some(m) => Ok(m),
            None => bail!("empty reply"),
        }
    }
}
