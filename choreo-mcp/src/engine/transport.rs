//! A client-transport wrapper that forwards progress notifications inline.
//!
//! rmcp's client serve loop handles an incoming server-to-client
//! **notification** by spawning the [`ClientHandler`](rmcp::ClientHandler)
//! callback on a separate task, but resolves a **response** inline in the serve
//! loop. A server that emits `notifications/progress` immediately before its
//! `tools/call` result therefore lets the caller observe the response while the
//! progress callback is still pending: the call task's `select!` (in
//! [`engine/call.rs`](super::call)) can win the response race and drop the
//! chunk it was expecting.
//!
//! Forwarding each progress notification here — synchronously inside
//! [`receive`](Transport::receive), before the message is handed back to rmcp —
//! removes that race. The event is enqueued on the per-connection broadcast
//! before rmcp has even seen the notification, hence necessarily before it can
//! read the response, so the call task's biased `select!` (events arm before the
//! response arm) always delivers the chunk. This covers both transports, since
//! the wrapper is generic over the wrapped [`Transport`].

use super::handler::ServerEvent;
use rmcp::RoleClient;
use rmcp::model::{JsonRpcMessage, JsonRpcNotification, ServerNotification};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::broadcast;

/// Wraps any client transport, forwarding progress notifications inline.
///
/// Only `receive` is augmented: the wrapped transport's `send` and `close`
/// pass through unchanged, so the wrapper is transparent to the protocol.
pub(super) struct ProgressForwarding<T> {
    inner: T,
    /// The per-connection server-event broadcast the in-flight call tasks
    /// subscribe to; the wrapper is a second producer alongside rmcp's
    /// notification callbacks.
    events: broadcast::Sender<ServerEvent>,
}

impl<T> ProgressForwarding<T> {
    /// Wrap `inner`, forwarding its progress notifications onto `events`.
    pub(super) fn new(inner: T, events: broadcast::Sender<ServerEvent>) -> Self {
        Self { inner, events }
    }
}

impl<T> Transport<RoleClient> for ProgressForwarding<T>
where
    T: Transport<RoleClient>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        // The wrapped future is already `Send + 'static`; pass it through
        // without an extra layer, so a send is never serialized behind the
        // inline progress forwarding.
        self.inner.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        let events = self.events.clone();
        let inner = self.inner.receive();
        async move {
            let msg = inner.await;
            // Match by reference so the notification is forwarded and the
            // message is then returned unchanged for rmcp to dispatch. Sending
            // here, before `msg` is yielded to the serve loop, is what makes the
            // progress event precede the response deterministically.
            if let Some(JsonRpcMessage::Notification(JsonRpcNotification {
                notification: ServerNotification::ProgressNotification(n),
                ..
            })) = &msg
            {
                // Best-effort: `send` fails only when no receiver is attached
                // (no call is in flight), which the broadcast already tolerates.
                let _ = events.send(ServerEvent::Progress {
                    token: n.params.progress_token.clone(),
                    progress: n.params.progress,
                    total: n.params.total,
                    message: n.params.message.clone(),
                });
            }
            msg
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Notification, NumberOrString, ProgressNotificationParam, ProgressToken};
    use std::collections::VecDeque;

    /// A transport that yields a fixed script of messages, so the wrapper's
    /// forwarding can be driven deterministically.
    struct ScriptedTransport {
        incoming: VecDeque<RxJsonRpcMessage<RoleClient>>,
    }

    impl Transport<RoleClient> for ScriptedTransport {
        type Error = std::io::Error;

        fn send(
            &mut self,
            _item: TxJsonRpcMessage<RoleClient>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            std::future::ready(Ok(()))
        }

        fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
            // Yield the next scripted message, then signal end-of-stream.
            std::future::ready(self.incoming.pop_front())
        }

        fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            std::future::ready(Ok(()))
        }
    }

    /// A `notifications/progress` message for `token`, as the wire reader would
    /// hand to the wrapper.
    fn progress_message(token: i64, message: &str) -> RxJsonRpcMessage<RoleClient> {
        let params =
            ProgressNotificationParam::new(ProgressToken(NumberOrString::Number(token)), 1.0)
                .with_total(2.0)
                .with_message(message);
        JsonRpcMessage::notification(ServerNotification::ProgressNotification(Notification::new(
            params,
        )))
    }

    /// A progress notification is forwarded onto the broadcast *before*
    /// `receive` yields the message — the property that makes the call task's
    /// `select!` observe it ahead of the `tools/call` response.
    #[test]
    fn progress_is_forwarded_before_receive_yields() {
        crate::runtime::init().expect("runtime init");
        let (events, mut rx) = broadcast::channel(8);
        let mut wrapper = ProgressForwarding::new(
            ScriptedTransport {
                incoming: VecDeque::from([progress_message(7, "working")]),
            },
            events,
        );

        let (yielded, event) = crate::runtime::block_on(async {
            let msg = Transport::receive(&mut wrapper).await;
            // The broadcast must already hold the event the instant `receive`
            // resolves, without any separate task being scheduled.
            let event = rx.try_recv().expect("progress forwarded inline");
            (msg.is_some(), event)
        })
        .expect("block_on runs on the sidecar runtime");

        assert!(yielded, "the message is still returned to rmcp");
        let ServerEvent::Progress {
            token,
            progress,
            total,
            message,
        } = event;
        assert_eq!(token, ProgressToken(NumberOrString::Number(7)));
        assert_eq!(progress, 1.0);
        assert_eq!(total, Some(2.0));
        assert_eq!(message.as_deref(), Some("working"));
    }

    /// Every scripted message is forwarded in order, and an exhausted stream
    /// yields `None` without emitting an event.
    #[test]
    fn each_message_is_forwarded_then_the_stream_ends() {
        crate::runtime::init().expect("runtime init");
        let (events, mut rx) = broadcast::channel(8);
        let mut wrapper = ProgressForwarding::new(
            ScriptedTransport {
                incoming: VecDeque::from([progress_message(1, "one"), progress_message(2, "two")]),
            },
            events,
        );

        let (first, second, end) = crate::runtime::block_on(async {
            let _ = Transport::receive(&mut wrapper).await;
            let _ = Transport::receive(&mut wrapper).await;
            let end = Transport::receive(&mut wrapper).await;
            let first = rx.try_recv().expect("first forwarded inline");
            let second = rx.try_recv().expect("second forwarded inline");
            (first, second, end)
        })
        .expect("block_on runs on the sidecar runtime");

        let message = |event| match event {
            ServerEvent::Progress { message, .. } => message,
        };
        assert_eq!(message(first).as_deref(), Some("one"));
        assert_eq!(message(second).as_deref(), Some("two"));
        assert!(end.is_none(), "the scripted stream ends");
        assert!(
            rx.try_recv().is_err(),
            "no event for the end-of-stream read"
        );
    }
}
