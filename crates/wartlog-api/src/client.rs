//! Async client for the daemon socket.

use crate::proto::{read_frame, write_frame, Request, Response};
use crate::PROTOCOL_VERSION;
use anyhow::{bail, Context, Result};
use std::path::Path;
use tokio::net::UnixStream;

pub struct Client {
    s: UnixStream,
    pub server: String,
}

impl Client {
    /// Connects and does the version handshake.
    pub async fn connect(path: &Path, client: &str) -> Result<Client> {
        let mut s = UnixStream::connect(path).await.with_context(|| format!("cannot reach wartlogd at {} (is it running? try `systemctl --user start wartlogd`)", path.display()))?;
        write_frame(&mut s, &Request::Hello { version: PROTOCOL_VERSION, client: client.to_string() }).await?;
        match read_frame::<_, Response>(&mut s).await? {
            Some(Response::Hello { version, server }) if version == PROTOCOL_VERSION => Ok(Client { s, server }),
            Some(Response::Hello { version, server }) => bail!("{server} speaks API version {version}, this client speaks {PROTOCOL_VERSION}"),
            Some(Response::Error { message, .. }) => bail!("{message}"),
            other => bail!("unexpected handshake reply: {other:?}"),
        }
    }

    /// Connects to the default socket.
    pub async fn connect_default(client: &str) -> Result<Client> {
        Self::connect(&crate::api_socket_path(), client).await
    }

    pub async fn send(&mut self, r: &Request) -> Result<()> {
        write_frame(&mut self.s, r).await
    }

    /// Next message of the current reply; `None` if the daemon closed the connection.
    pub async fn recv(&mut self) -> Result<Option<Response>> {
        read_frame(&mut self.s).await
    }

    /// Sends a request and collects the whole reply (row batches, then the final message).
    /// Not for `Tail`, which never ends.
    pub async fn call(&mut self, r: &Request) -> Result<Vec<Response>> {
        self.send(r).await?;
        let mut out = Vec::new();
        loop {
            match self.recv().await? {
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
    pub async fn call_one(&mut self, r: &Request) -> Result<Response> {
        let mut v = self.call(r).await?;
        match v.pop() {
            Some(Response::Error { message, .. }) => bail!("{message}"),
            Some(m) => Ok(m),
            None => bail!("empty reply"),
        }
    }
}
