//! Stdio discovery probes do not choose the connection's lifecycle. A client
//! may pipeline a legacy `initialize` after discovery; letting the SDK see the
//! probe first makes its per-request metadata requirement irreversible.
//!
//! Complete the first valid modern probe before starting the SDK lifecycle.
//! An immediately following `initialize` becomes the SDK's first request. Any
//! other message replays the probe and the following message in their original
//! order, preserving modern validation and notification handling. At most two
//! typed messages are buffered; decoding uses the SDK's existing byte-stream
//! transport.

use std::collections::VecDeque;

use rmcp::model::{
    ClientJsonRpcMessage, ClientRequest, DiscoverResult, GetMeta, RequestId, ServerJsonRpcMessage,
    ServerResult,
};
use rmcp::transport::Transport;
use rmcp::{RoleServer, ServerHandler};
use tokio::sync::oneshot;

use crate::{KnowellServer, KnowellTools};

/// A transport with a bounded startup prefix and one already-sent discovery
/// response. The guard matches both request ID and the complete result so ID
/// reuse cannot suppress an unrelated response.
pub(crate) struct PreparedStdio<T> {
    inner: T,
    prefix: VecDeque<ClientJsonRpcMessage>,
    sent_discovery: Option<(RequestId, DiscoverResult)>,
    replay_ready: Option<oneshot::Receiver<()>>,
    replay_signal: Option<oneshot::Sender<()>>,
}

/// Resolves only a valid, first modern discovery probe before SDK startup.
///
/// `None` means the client disconnected. Malformed, unsupported and legacy
/// probes are passed unchanged to the SDK; no metadata is filled or replaced.
pub(crate) async fn prepare_stdio<S, T>(
    server: &KnowellServer<S>,
    mut transport: T,
) -> Result<Option<PreparedStdio<T>>, T::Error>
where
    S: KnowellTools,
    T: Transport<RoleServer>,
{
    let Some(first) = transport.receive().await else {
        return Ok(None);
    };
    let Some((id, result)) = discovery_reply(server, &first) else {
        return Ok(Some(PreparedStdio {
            inner: transport,
            prefix: VecDeque::from([first]),
            sent_discovery: None,
            replay_ready: None,
            replay_signal: None,
        }));
    };
    transport
        .send(ServerJsonRpcMessage::response(
            ServerResult::DiscoverResult(result.clone()),
            id.clone(),
        ))
        .await?;
    let Some(next) = transport.receive().await else {
        return Ok(None);
    };
    if matches!(
        &next,
        ClientJsonRpcMessage::Request(request)
            if matches!(&request.request, ClientRequest::InitializeRequest(_))
    ) {
        return Ok(Some(PreparedStdio {
            inner: transport,
            prefix: VecDeque::from([next]),
            sent_discovery: None,
            replay_ready: None,
            replay_signal: None,
        }));
    }
    let (replay_signal, replay_ready) = oneshot::channel();
    Ok(Some(PreparedStdio {
        inner: transport,
        prefix: VecDeque::from([first, next]),
        sent_discovery: Some((id, result)),
        replay_ready: Some(replay_ready),
        replay_signal: Some(replay_signal),
    }))
}

fn discovery_reply<S: KnowellTools>(
    server: &KnowellServer<S>,
    message: &ClientJsonRpcMessage,
) -> Option<(RequestId, DiscoverResult)> {
    let ClientJsonRpcMessage::Request(request) = message else {
        return None;
    };
    if !matches!(&request.request, ClientRequest::DiscoverRequest(_)) {
        return None;
    }
    let meta = request.request.get_meta();
    let version = meta.protocol_version()?;
    let supported = server.supported_protocol_versions();
    if version.has_initialize()
        || !supported.contains(&version)
        || !meta.missing_required_keys(&version).is_empty()
    {
        return None;
    }
    Some((
        request.id.clone(),
        DiscoverResult::from_server_info(supported.into_owned(), server.get_info()),
    ))
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for PreparedStdio<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        message: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let duplicate = self.sent_discovery.as_ref().is_some_and(|(id, result)| {
            matches!(
                &message,
                ServerJsonRpcMessage::Response(response)
                    if response.id == *id
                        && matches!(&response.result, ServerResult::DiscoverResult(actual)
                            if actual == result)
            )
        });
        let sending = if duplicate {
            self.sent_discovery = None;
            // The SDK retires the replayed request before calling send. Release
            // the following message only now: its client may reuse the probe ID
            // after receiving our initial reply.
            if let Some(signal) = self.replay_signal.take() {
                let _ = signal.send(());
            }
            None
        } else {
            Some(self.inner.send(message))
        };
        async move {
            match sending {
                Some(sending) => sending.await,
                None => Ok(()),
            }
        }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        if self.prefix.len() == 1 {
            // The service loop can cancel receive while sending responses.
            // Borrow the receiver across await so a cancelled poll does not
            // discard the gate or forward the buffered message too soon.
            if let Some(ready) = self.replay_ready.as_mut()
                && ready.await.is_err()
            {
                self.replay_ready = None;
                return None;
            }
            self.replay_ready = None;
        }
        match self.prefix.pop_front() {
            Some(message) => Some(message),
            None => self.inner.receive().await,
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}
